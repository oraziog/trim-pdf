# M3 — taglio per stringa, taglio per segnalibri, nomi da testo

## Cosa c'è ora

- [`crates/splitter/src/text.rs`](../crates/splitter/src/text.rs): l'estrazione del testo, una pagina
  alla volta. Legge i content stream (uno o l'array di più), segue i form XObject fino a 8 livelli,
  gestisce i font semplici e compositi (`/ToUnicode` con `bfchar`, `bfrange` e codespace), e i
  separatori di parola (`Td`, `TD`, `T*`, `Tm`, `ET`) perché un nome letto dal testo non resti
  incollato a quello di prima. Non è un renderer: non guarda le coordinate, non impagina.
- [`crates/splitter/src/search.rs`](../crates/splitter/src/search.rs): la ricerca.
  `Query::literal` / `Query::pattern`, `ignoring_case()` (con `(?i)` inline per le regex, così le
  classi restano valide). `read_texts` legge le pagine su più thread (fino a 4, un lettore e una
  cache per thread) e tiene il testo di ognuna: **una passata sola** serve la ricerca, il nome delle
  parti e il taglio. `Search` riporta le pagine trovate, quelle **senza testo** (immagini, o glifi
  che nessun font riporta a caratteri), quante ne ha lette e il tempo.
- [`crates/splitter/src/naming.rs`](../crates/splitter/src/naming.rs): i nomi.
  `LabelRule::new("Codice Fiscale")` legge il valore dopo i due punti (regex `(?i)etichetta\s*:\s*(\S+)`,
  punteggiatura finale tolta); `LabelRule::matching(etichetta, regex)` legge quello che dice la tua
  espressione (primo gruppo, o l'intera corrispondenza se non ci sono gruppi). `sanitize` rende il
  nome scrivibile (caratteri vietati da Windows, nomi di dispositivo, punto finale, 120 caratteri) e
  `unique` evita le collisioni con `-02`, `-03`… Prima occorrenza nelle pagine della parte; se non
  c'è, `<nome input>-NN`.
- [`crates/splitter/src/plan.rs`](../crates/splitter/src/plan.rs): `SplitPlan::from_marks(pagine, tot, close_after)`
  e `SplitPlan::from_bookmarks(segnalibri, livello, tot)`. La pagina trovata **apre** la parte, o la
  **chiude** con `close_after`; le pagine prima del primo match formano la parte iniziale, l'ultima
  va a fine documento, e un match a pagina 1 non lascia una parte vuota.
- [`apps/pdfsplit/src/main.rs`](../apps/pdfsplit/src/main.rs):
  `pdfsplit search <file> <testo> [--case-sensitive] [--regex]` (`scanned`, `matches`, `page`, `without_text`, `elapsed_ms`),
  `pdfsplit text <file> --page N`, e
  `pdfsplit split <file> --string "…" | --bookmarks N` con `--case-sensitive`, `--regex`,
  `--close-after`, `--name-label "…"`, `--name-pattern "…"`, `--name-from-title`, `--always-number`.
  Le pagine senza testo su cui la ricerca non può rispondere finiscono su stderr, con l'elenco.

## Compilare e testare

```sh
cargo build --profile release-min -p pdfsplit     # target/release-min/pdfsplit.exe (478 KiB)
cargo test --workspace                            # 48 unit + fixture 128 MB + doctest
cargo test -p splitter -- --ignored               # + apertura e taglio del fixture da 6,2 GB
cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all --check
```

I test unitari coprono: `Tj`/`TJ`/`'`/`"`, i form XObject, i CMap (`bfchar`, `bfrange`, codespace a
due byte), un font composito **senza** `/ToUnicode` (i codici grezzi vengono letti e la pagina è
segnalata), la regex e la ricerca case-sensitive, `from_marks` nei casi limite (pagina 1, ultima
pagina, ripetuti, fuori range), `from_bookmarks` su livelli 1 e 2, i nomi da testo, da segnalibro,
con sequenza sempre e il fallback che non può cadere sul file di input.

## Misure (Windows, disco locale, `release-min`)

Ricerca di `"Page 2"` sul fixture sintetico (pagina = content stream non compresso da 3,1 GB a 6 GB):

| Input | `search` (processo) | Picco RAM |
|---|---|---|
| 128 MB | 1,24 s | 5,6 MB |
| 512 MB | 2,62 s | 5,6 MB |
| 1 GB | 5,33 s | 5,6 MB |
| 2 GB | 10,70 s | 5,6 MB |
| **6,2 GB** | **9,7–10,8 s** in cache di sistema, **30,4 s** a freddo | **5,6 MB** |

Il tempo è la lettura del file (~190 MB/s a freddo, più veloce quando è in cache): la ricerca non ha
altro costo. La RAM non cambia con il file — 5,6 MB sono il processo più i blocchi di lettura: le
pagine sono lette in blocchi da 1 MiB e il testo di una pagina è tenuto al massimo a 4 MiB.

Taglio per stringa sullo stesso file da 6,2 GB (`--string "Page 2"`, due parti da 3,1 GB):

| | |
|---|---|
| parti | 2 (`1`, `2` pagine) |
| scrittura | 6.200.001.823 B (595 B più del sorgente: albero pagine, font e xref duplicati) |
| tempo dentro `pdfsplit` | 36,8 s (di cui ~40% la rilettura di verifica di ogni parte) |
| tempo del processo | 48,0 s |
| picco RAM | **15,7 MB** |

## Validazione indipendente (pypdf)

[`target/benchmark/m3_check.py`](../target/benchmark/m3_check.py) fa scrivere il documento d'ingresso
a pypdf — copertina, tre fatture, una quarta **in minuscolo**, un allegato, una nota, due segnalibri
di primo livello e uno annidato — taglia con `pdfsplit` e **riapre ogni parte con pypdf in strict
mode**, confrontando pagine, testo, outline e nomi dei file:

| taglio | parti | nomi |
|---|---|---|
| `--string "Fattura n." --name-label "Codice Fiscale"` | 1 / 2-3 / 4 / 5-6 / 7-8 | `<input>-01`, `AAA111`, `BBB222`, `CCC333`, `DDD444` |
| `--string "Fattura n." --case-sensitive` | 1 / 2-3 / 4 / 5-8 | quattro nomi di fallback |
| `--string "Fattura n\. 10[12]" --regex` | 1-3 / 4 / 5-8 | tre nomi di fallback |
| `--string "Fattura n." --close-after` | 1-2 / 3-4 / 5 / 6-7 / 8 | cinque nomi di fallback |
| `--bookmarks 1 --name-from-title` | 1-3 / 4-8 | `Fattura 100`, `Fattura 101` |
| `--bookmarks 2` | 1-2 / 3-8 | due nomi di fallback |

Il testo estratto da ogni parte è quello delle sue pagine, e l'outline di ogni parte contiene
**esattamente** i segnalibri del sorgente che puntano alle pagine che quella parte ha.

## Limiti, dichiarati

- `--string` è **case-insensitive per default** (la specifica elenca "case sensitive" tra le opzioni, quindi
  è l'opzione che accende la sensibilità): la fattura scritta in minuscolo a pagina 7 entra nel taglio,
  come mostra la prima riga della tabella qui sopra.
- Un content stream **filtrato** (Flate, LZW) è decodificato al massimo per 64 MiB: oltre, la pagina
  è segnalata come troncata e non si finge di averla letta. Gli stream **non** filtrati sono invece
  percorsi a blocchi da 1 MiB, quindi una pagina da 3,1 GB si legge a RAM costante (è il caso del
  fixture).
- PDF scansionati: nessun OCR, la pagina finisce in `without_text` e il comando lo dice.
- Font compositi senza `/ToUnicode`: i codici vengono letti come byte e la pagina è segnalata
  (`unmapped`); il testo reale richiederebbe la tabella del font.
- Il nome da testo guarda la **prima** occorrenza dell'etichetta nelle pagine della parte, come dice
  la specifica: se due parti portano lo stesso codice, la seconda prende `-02`.
- Una cartella di output che non esiste viene creata (prima falliva la scrittura del `.part`: trovato
  dalla prova sul fixture da 6,2 GB e corretto).

## Prossimo

M4: finestra egui, avanzamento per fase, ETA e annulla. Il core espone già le fasi come chiamate
separate (`page_count`, `bookmarks`, `search`, `split`), quindi la GUI le orchestra su un thread di
lavoro senza cambiare il motore.
