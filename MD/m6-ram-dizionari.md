# I dizionari di una parte, senza tenerli in RAM

Progetto per togliere `PlannedPart.objects` dal picco di memoria, senza cambiare l'architettura:
il piano resta finito prima del primo byte, la numerazione resta quella, l'output resta lo stesso
byte per byte.

## 1. Il problema in numeri

Misurato con il binario `release-min` su `many.pdf` (200 000 pagine, 44,6 MB), campionando il
working set ogni 25 ms e leggendo `/Size` dalla parte scritta:

| parti | oggetti della parte | picco |
|---|---|---|
| 1 | 402 004 | 391,4 MB |
| 2 | 201 004 | 266,4 MB |
| 4 | 100 504 | 201,8 MB |

La pendenza è la struttura: (391,4 − 266,4) MB su 201 000 oggetti = **652 B per oggetto**, e
(266,4 − 201,8) MB su 100 500 = 674 B. La base fissa, circa 135 MB, sono `page_list`, l'indice
della cross-reference, `document_pages`, i buffer di copia e la frammentazione dell'allocatore.
`numbers: HashMap<u32, u32>` e `document_pages: HashSet<u32>` costano 8-12 B per pagina: non sono
loro.

Il costo è quindi `oggetti della parte × 650 B` e non ha tetto: su un sorgente da 8 GB con 400 000
oggetti una parte sola si porta circa 260 MB di soli dizionari. Servono i dizionari per intero solo
perché `write` li itera per scriverli, e perché `body_size` e `required_version` li scandiscono
prima.

## 2. Cosa deve restare vero

1. La numerazione è decisa prima di scrivere: `catalog`, `/Pages`, le pagine, poi ciò che le
   pagine raggiungono. Un riferimento a un oggetto non ancora visto riceve il suo numero nel
   momento in cui lo si incontra, e l'ordine di scoperta è quello di oggi (`intern` con coda LIFO).
2. La versione PDF si sceglie prima di scrivere `%PDF-x.y`, quindi la scansione delle feature di
   tutti gli oggetti deve avvenire nel piano.
3. La forma della cross-reference (classica o stream) si sceglie dalla stima dei byte: anche quella
   si accumula nel piano.
4. Gli oggetti che vanno dentro un object stream si raggruppano a 100 per stream, e la decisione
   «dentro o fuori» dipende dal tipo di oggetto: anche quella va annotata nel piano.
5. Un riferimento a una pagina fuori dalla parte diventa `null`; un `/Parent` dell'albero non si
   segue. Sono decisioni già prese nel piano e non cambiano.
6. Annullamento, spazio su disco, file `.part` e rename restano dove sono.

Nessuno dei sei punti richiede il *corpo* dell'oggetto: richiede solo la sua *classe*.

## 3. Il progetto: due passate, un solo walk

La prima passata fa esattamente ciò che fa `PartBuilder::drain` oggi — legge, classifica, annota,
accoda i riferimenti — ma invece di conservare il dizionario riscritto conserva **8 byte**:

```rust
/// Un oggetto del piano ridotto a ciò che serve per riscriverlo: dove sta, non com'è fatto.
#[derive(Clone, Copy)]
enum Slot {
    /// Un oggetto del sorgente, riletto quando la parte si scrive. `packable` dice se può
    /// finire dentro un object stream: vero per dizionari e array, falso per gli stream.
    Source { num: u32, packable: bool },
    /// Una pagina di questa parte: `page_dict` la ricostruisce dal sorgente a ogni scrittura.
    Page { num: u32 },
    /// Una pagina del documento che questa parte non tiene.
    Null,
    /// Qualcosa che la parte aggiunge da sé, tenuto in `synthetic`: catalogo, `/Pages`,
    /// voci dell'outline, livelli.
    Ready { index: u32 },
}
```

`slots: Vec<Slot>` è indicizzato per numero d'uscita, che è contiguo da 1 a `max`: la
`BTreeMap<u32, OutObject>` sparisce del tutto e con lei il controllo «buco nella numerazione», che
diventa `slots.len() == max + 1`.

```rust
pub(crate) struct PlannedPart {
    slots: Vec<Slot>,
    /// I pochi oggetti che la parte costruisce: catalogo, albero, outline, livelli.
    synthetic: Vec<OutObject>,
    catalog: u32,
    bookmarks: usize,
    notes: Vec<String>,
    /// Accumulati durante il piano: la stima dei byte e la versione che le pagine richiedono.
    body_size: u64,
    required_version: Option<Version>,
}
```

`PartBuilder` perde `objects` e guadagna `slots` e i due accumulatori; `drain` cambia di tre righe:

```rust
match &*value {
    Object::Stream(stream) => {
        let stream = stream.clone();
        let (_, length) = self.reader.stream_layout(&stream)?;
        scan_dict_features(&stream.dict, &mut self.required_version);
        self.body_size = self.body_size.saturating_add(length.saturating_add(1024));
        self.slots[number as usize] = Slot::Source { num: reference.num, packable: false };
    }
    other => {
        // Stessa trasformazione di oggi, ma il risultato si butta: ad interning già fatto,
        // `value` serve solo a scoprire i riferimenti e le feature.
        let copied = self.value(other)?;
        scan_features(&copied, &mut self.required_version);
        self.body_size = self.body_size.saturating_add(1024);
        self.slots[number as usize] = Slot::Source { num: reference.num, packable: true };
    }
}
```

La seconda passata scrive in ordine di numero, come fa `write` oggi, ricostruendo il corpo:

```rust
for number in 1..=max {
    match plan.slots[number as usize] {
        Slot::Source { num, .. } => match &*reader.get(ObjRef { num, generation: 0 })? {
            Object::Stream(stream) => write_stream(source, settings, &mut out, &mut reader, stream)?,
            other => write_value(&mut out, &builder.value_existing(other)?)?,
        },
        Slot::Page { num } => {
            let entry = &pages[page_of[num]];
            let dict = page_dict(reader.get(..)?.as_dict().., entry, plan.pages_root)?;
            write_value(&mut out, &Object::Dict(dict))?;
        }
        Slot::Null => out.put_str("null")?,
        Slot::Ready { index } => write_value(&mut out, &plan.synthetic[index])?,
    }
}
```

`value` guadagna un gemello che non internа nulla:

```rust
/// Come `value`, ma un riferimento che il piano non conosce è un errore: la seconda passata
/// non scopre oggetti, li riscrive. Un numero nuovo qui vuol dire un piano incompleto.
fn value_existing(&mut self, value: &Object) -> Result<Object, Error> {
    ...
    Object::Ref(target) => reference(self.lookup(*target)?),
```

`lookup` sostituisce `intern` nella seconda passata. `page_dict`, `dict`, `form`, `destination_leaves`
e `parent_is_tree` sono già funzioni pure di (oggetto del sorgente, `numbers`, `own_pages`,
`document_pages`): si richiamano e danno lo stesso risultato. È il punto su cui poggia tutto il
progetto, e va tenuto così: nessuna di queste funzioni deve dipendere da un corpo già memorizzato.

Gli object stream si riempiono dalla lista dei numeri `packable`, che il piano conosce senza i
corpi; i loro membri si rileggono uno per uno quando lo stream si scrive (`write_object_stream`
prende il `Reader` invece del piano).

## 4. Cosa costa

**Memoria.** Da 650 B a circa 28 B per oggetto (8 per lo slot, ~20 per `numbers` e la sua
capacità doppia). Su 402 004 oggetti: 260 MB → circa 11 MB.

| parti | oggi | dopo (stima) |
|---|---|---|
| 1 | 391,4 MB | ~150 MB |
| 2 | 266,4 MB | ~145 MB |
| 4 | 201,8 MB | ~143 MB |

Il picco smette di crescere con gli oggetti e resta la parte fissa, dominata da `document_pages`,
dall'indice xref e dalla `page_list`.

**Tempo.** Ogni dizionario si legge e si analizza due volte invece di una. Un proxy misurato è la
camminata dell'albero su `many.pdf`: 200 007 oggetti in 1866 ms, cioè 9,3 µs per oggetto (lettura
posizionale da 4 KB più analisi). Su questa base: circa +0,9 s per parte nel fixture da 4 parti
(+13% sul totale di 28-30 s), e circa +1,5 s per parte sui 26 s del tuo 6 GB in 2 parti (+6%). È
una stima costruita su una misura, non una misura dell'implementazione: la prima cosa da fare dopo
il codice è passarla nello stesso probe.

**Opzione B, se sul tuo file il +6% dà fastidio.** Il piano può tenere i corpi *serializzati* in un
arena di byte con un tetto (per esempio 32 MB): finché c'è spazio, l'oggetto si scrive dall'arena e
la seconda passata non rilegge niente; esaurita l'arena, si passa agli `Slot` e si rilegge. Il picco
diventa `base + 32 MB`, indipendente dal numero di oggetti, e l'I/O in più tocca solo la coda del
documento. Costa una `Vec<u8>` in più e due rami nel piano: è la variante da tenere in tasca se la
misura non convince.

## 5. Le trappole, una per una

- **`body_size` e `required_version`** oggi scandiscono gli oggetti memorizzati. Devono diventare
  accumulatori nel piano, altrimenti spariscono con i corpi. `body_size` è una stima grossolana
  (1024 B per oggetto più la lunghezza degli stream): identica a oggi se si somma negli stessi punti.
- **`packs_into_object_stream`** guarda il tipo dell'oggetto: il piano lo riduce al bit `packable`,
  e `object_stream_groups` itera gli slot. Il catalogo resta fuori come oggi.
- **Il `/Length` degli stream** dipende dalla disposizione nel sorgente, non dal corpo: si ricalcola
  con `stream_layout`, che è già chiamato nel piano e verrà richiamato in scrittura. Vale la pena
  leggere la lunghezza una volta sola e metterla in uno slot dedicato (`Source { num, len: u64 }`)
  se la seconda lettura del dizionario si rivela cara.
- **La verifica finale** della parte riletta dal disco non tocca il piano e resta com'è.
- **L'annullamento** va controllato anche nella seconda passata, oltre che nella prima: su 74 000
  pagine sono decine di secondi.

## 6. Passi di lavoro

1. `Slot`, `slots`, `synthetic`, gli accumulatori; `drain` che riempie gli slot e butta i corpi.
   Test: `tests.rs` compila e passa tutto senza modifiche (il piano è ancora completo).
2. Seconda passata in `write`: iterazione per numero, `value_existing`, stream riletti,
   object stream riempiti dalla lista `packable`. Criterio di accettazione: **stesso file byte per
   byte** di oggi sui fixture (impronta SHA-256 dei PDF prodotti da `parti.pdf`, dal fixture del
   modulo e da quello con le annotazioni, prima e dopo).
3. Le scansioni spostate (`body_size`, `required_version`) con un test che le confronta tra piano e
   vecchia implementazione su un documento con immagini JPX e trasparenze, dove la versione cambia.
4. Prova di picco con `target/probe/peak.ps1` su `many.pdf` con 1, 2 e 4 parti, e tempo totale:
   si accetta solo se il picco scende sotto i 160 MB e il tempo non supera il +15%.
5. Se il tempo non passa, l'opzione B dell'arena da 32 MB, misurata con lo stesso probe.

## 7. Alternative scartate

- **Un solo passaggio che scrive mentre cammina.** Sarebbe il minimo assoluto (nessuna rilettura),
  ma la numerazione contigua, la versione nell'header e il raggruppamento degli object stream alla
  fine richiedono di conoscere il piano prima. È un cambio di architettura, escluso dai paletti.
- **Scaricare i corpi su un file temporaneo** (spool) invece di rileggere il sorgente: risparmia il
  re-parse, ma aggiunge un file temporaneo, il suo spazio su disco, la sua pulizia e i suoi errori.
  Con i dizionari a 650 B per oggetto lo spool sarebbe di circa 260 MB per un file da 8 GB: più
  I/O della rilettura, e nuovi modi di fallire. Scartato, tenuto come idea se il sorgente fosse su
  rete e la rilettura costasse più di un file locale.
- **Un tetto a `PlannedPart.objects` che scrive a blocchi**: rompe la numerazione contigua o
  costringe a due file.

## 8. Cosa non cambia

Nessuna dipendenza nuova, niente `unsafe`, niente async, nessun cambio di GUI o di CLI, nessuna
modifica alla copia grezza degli stream. `PlannedPart` resta la descrizione completa e definitiva
della parte; cambia solo dove stanno i corpi, e la risposta è «nel sorgente, finché servono».
