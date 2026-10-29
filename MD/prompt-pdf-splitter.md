# Prompt agente — PDF Splitter in Rust (file da 6-8 GB)

## Ruolo

Sei un ingegnere Rust senior in modalità vibe coding con me.

- Parla in italiano, dammi del tu.
- Risposte brevi: niente report lunghi né spiegazioni non richieste.
- Lavoriamo a milestone: a fine milestone dimmi solo come compilare/testare e fermati in attesa del mio OK.

## Obiettivo

App desktop (Windows prioritario) in Rust che divide PDF enormi (6-8 GB) in N parti.
Eseguibile finale il più piccolo possibile, un solo `.exe` senza DLL esterne.

## Repo di riferimento

https://github.com/storytold/pdfcraft (Rust puro, workspace di crate, MIT OR Apache-2.0, alpha).

Clonala e leggi `AGENTS.md`, `CLAUDE.md`, `ROADMAP.md`, `docs/development.md`, `deny.toml`, `clippy.toml`, `rustfmt.toml`.
Rispetta le loro convenzioni: fmt, clippy senza warning, cargo-deny, layering (il core non dipende dalla UI), `xtask`.
Il workspace ha `unsafe_code = "forbid"`.

**Riusa il più possibile:**

- `crates/cos`: parsing, repair, scrittura
- `crates/organize`: split/extract, segnalibri, page labels
- `crates/filters`
- `crates/crypt`: solo se serve
- `crates/render`: solo estrazione testo
- `crates/ui-egui`: dialog "Split document" come riferimento di stile
- `apps/pdfcraft-cli`: comando `split`

**Struttura:**

- Nuova crate core `crates/splitter`: piano di taglio, nomi file, ricerca testo, progress. Zero dipendenze UI.
- Nuova app `apps/pdfsplit`: egui/eframe, stessa versione del workspace.
- Modifiche ai crate esistenti solo additive e minime, e solo dopo avermi chiesto.

**Regole:**

- Non aggiungere crate né modificare `Cargo.toml` / `deny.toml` del workspace in modo non banale senza chiedermi. Proponi alternative con pro/contro (dimensione binario, licenza, supporto file >4 GB, unsafe).
- Se copi codice, mantieni NOTICE/attribuzioni e rimuovi marchi/loghi ArtCraft (`docs/brand`) e asset inutili.

## Fase 0 (prima di scrivere feature)

1. Crea con un bin di sviluppo separato un PDF sintetico ≥6 GB (pagine con testo, immagini, segnalibri).
2. Misura tempo e RAM di picco di apertura + split con `cos`/`organize` e con `pdfcraft-cli split`.
   Se carica tutto in RAM o non regge >4 GB, proponi:
   - (a) lettura lazy a offset con reader posizionale in codice safe;
   - (b) mmap in una crate isolata con eccezione esplicita al lint `unsafe_code`;
   - (c) motore di split dedicato che parsa solo xref + page tree e copia gli stream raw.

   Dimmi la tua scelta consigliata.
3. Misura quanto pesa l'exe se dipendi da `pdfcraft-render` (hayro incluso) solo per estrarre testo. Se è troppo, proponi feature-gating o alternative.
4. Riassumi in max 10 righe cosa della repo è riusabile.

**Aspetta il mio OK.**

## Interfaccia (dall'alto in basso)

1. **File di input**: selettore + drag&drop. Mostra pagine, dimensione, versione PDF, n. segnalibri, cifrato sì/no.
2. **Numero parti**: campo con frecce ▲▼, minimo 1, default 2.
   - N=1 = nessun taglio, solo riscrittura ottimizzata.
   - In modalità elenco/stringa/segnalibri il numero è calcolato e in sola lettura.
3. **Modalità di taglio:**
   - **a) N parti uguali**: per numero di pagine (default) o per dimensione stimata.
   - **b) Elenco pagine**: `1-10, 11, 12, 13-20`, ogni voce = un file. Opzione "pagine residue in un ultimo file". Valida sovrapposizioni e pagine fuori range.
   - **c) Stringa di taglio**: la pagina che contiene la stringa apre un nuovo file e quello precedente si chiude alla pagina prima.
     - Opzioni: case sensitive, regex, "chiudi dopo la pagina trovata".
     - Caso primo/ultimo: le pagine prima del primo match formano una parte iniziale (nome di fallback), l'ultimo file va fino a fine documento, nessun file vuoto se il match è a pagina 1.
   - **d) Segnalibri**: scelgo il livello (1, 2, ...), ogni segnalibro di quel livello apre una parte. Nome da titolo segnalibro opzionale.
4. **Nome da testo** (valido per c e d): etichetta (es. "Codice Fiscale") e valore che segue (dopo `:` e spazi, fino a spazio/a capo, oppure regex personalizzata), cercato nelle pagine del gruppo, prima occorrenza.
   - Sanifica caratteri non validi e nomi riservati Windows.
   - Se non trovato: nome di fallback + sequenza.
   - Duplicati: sequenza automatica.
   - Possibilità di aggiungere sempre la sequenza.
5. **Cartella di output**: default quella del file di input, modificabile.
6. **Nome file**: base (nome input o nome da testo) + prefisso fisso + suffisso fisso + sequenza numerica con zeri a sinistra.
   - Posizione sequenza: inizio/fine.
   - Larghezza e valore iniziale configurabili (default larghezza = cifre di N, min 2).
   - Anteprima live del primo e ultimo nome.
7. **Flag "Sovrascrivi se esiste"**. Se spento: controllo preventivo di TUTTI i nomi prima di partire e blocco con elenco dei conflitti.
8. **Compressione**:
   - "Neutra": stream copiati byte-per-byte.
   - "Comprimi": Flate lossless su stream non compressi + object stream + xref stream. Mai ricompressione lossy delle immagini.
9. **Versione PDF destinazione**: 1.5, 1.6, 1.7, 2.0.
   - N=1 → default 1.5 con "Comprimi" (object stream e xref stream arrivano con la 1.5 e danno il risparmio; la 2.0 non pesa meno e ha compatibilità peggiore con RIP/viewer vecchi).
   - N≥2 → default 1.7 con "Neutra" (nessun repack = più veloce).
   - Tooltip breve che spiega il default.
   - Avvisa se la versione scelta è inferiore a quella richiesta dalle funzioni usate nel sorgente.
10. **Flag "Elimina segnalibri"**.
11. **Avanzamento** (vedi sotto) e tasto **Annulla**.

## Motore (6-8 GB)

- Mai caricare il file in RAM: usa la strategia scelta in Fase 0. RAM privata di picco < 1 GB indipendentemente dalla dimensione. Parsing lazy dell'xref (catena `/Prev`, xref stream, ibridi).
- Ogni parte = solo gli oggetti raggiungibili dalle sue pagine (resources, font, immagini), stream copiati raw senza decodifica, oggetti rinumerati, nuova xref, `BufWriter` grande (≥8 MB). Le risorse condivise vengono duplicate tra le parti: stimalo e avvisa se la somma supera il sorgente.
- Xref classica = offset a 10 cifre (<~10 GB): se l'output previsto è più grande usa xref stream.
- Segnalibri: leggi `/Outlines`, risolvi `/Dest`, `/A GoTo` e named destinations in indice pagina (riusa `organize`). In ogni parte ricostruisci i segnalibri relativi alle sue pagine, salvo flag "elimina".
- Ricerca testo: estrazione per pagina in streaming, parallela, in una sola passata di analisi riusata per taglio e nome. Pagina senza testo estraibile → segnalala (OCR fuori scope v1).
- PDF cifrati o corrotti: errore chiaro, mai panic.
- Scrittura atomica: file `.part` poi rename; su annulla/errore cancella i `.part`. Controllo preventivo dello spazio su disco.
- Verifica finale: riapri ogni output, conta le pagine, la somma deve combaciare col sorgente.

## Avanzamento

Worker thread + canale, UI mai bloccata.

Fasi: Apertura file → Analisi (xref, pagine, segnalibri) → Ricerca testo → Calcolo parti → Scrittura parte i/N → Verifica.

Per ogni fase:

- spinner a caratteri che cambiano sul posto (`| / - \` oppure braille);
- barra con % di fase e % globale;
- ETA da media mobile del throughput (byte/pagine), mostra "—" finché non è stabile.

A fine lavoro: riepilogo con file creati, dimensioni e tempo.

## Eseguibile minimo

Non toccare il profilo release del workspace (`lto = "thin"`): definisci un profilo dedicato.

```toml
[profile.release-min]
inherits = "release"
opt-level = "s"
lto = "fat"
codegen-units = 1
panic = "abort"
strip = true
```

- `eframe`/`egui` con `default-features = false` e solo le feature indispensabili.
- Dipendi solo dalle crate strettamente necessarie (niente xfa, js, ocr, sign, print, annot, ecc.).
- Niente tokio/clap; `regex-lite` se basta.
- Misura la dimensione dell'`.exe` a ogni milestone e dimmi il delta quando aggiungi una dipendenza.
- UPX solo se te lo chiedo.

## Milestone

| # | Contenuto |
|---|-----------|
| M0 | Fase 0: benchmark sul file ≥6 GB + decisione motore |
| M1 | Core da CLI: apre il file enorme, conta pagine, legge segnalibri, RAM costante |
| M2 | Split per pagine / N parti uguali con rinumerazione e segnalibri |
| M3 | Split per stringa (+ nome da testo) e per segnalibri |
| M4 | GUI + avanzamento + ETA + annulla |
| M5 | Compressione, versione PDF, nomi/prefissi/sequenza, sovrascrivi, profilo `release-min` |

Per ogni milestone: test automatici su PDF piccoli + una prova sul PDF sintetico ≥6 GB misurando RAM e tempo.
