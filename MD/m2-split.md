# M2 — taglio per pagine e N parti uguali

## Cosa c'è ora

- [`crates/splitter/src/plan.rs`](../crates/splitter/src/plan.rs): il piano di taglio.
  `SplitPlan::equal_parts(pagine, parti)` (il resto va alle prime parti), `SplitPlan::from_page_list("1-10, 11, 13-20")`,
  `with_leftover(pagine)` (le pagine che nessuna voce copre finiscono in un ultimo file) e
  `validate(pagine)`: pagine fuori range, una pagina in due parti, parti vuote, più di 10.000 parti.
- [`crates/splitter/src/write.rs`](../crates/splitter/src/write.rs): la scrittura di una parte.
  - Una parte contiene solo ciò che le sue pagine raggiungono (contenuti, risorse, font, immagini,
    annotazioni), con gli oggetti rinumerati da 1: catalogo, albero pagine, pagine, poi il resto.
  - Gli stream sono copiati **byte per byte** dall'input all'output in blocchi da 4 MB: nessuna
    decodifica, nessuna ricodifica, niente del documento in RAM. `/Length` viene riscritto con i byte
    realmente copiati; se il `/Length` del sorgente è inaffidabile vale la posizione di `endstream`.
  - **Attributi ereditati**: `/Resources`, `/MediaBox`, `/CropBox` e `/Rotate` presi dagli antenati
    nell'albero e scritti nella pagina, perché l'albero di una parte è piatto (una pagina che non li
    ereditasse perderebbe contenuti e formato).
  - Nuova **xref classica** + trailer, intestazione `%PDF-x.y` con la versione del sorgente.
  - **Segnalibri**: in ogni parte quelli che puntano alle sue pagine, con la gerarchia originale
    (un segnalibro il cui genitore è altrove diventa una radice di quella parte), destinazione
    `[pagina /Fit]`, titoli riscritti in ASCII o UTF-16BE. Ogni item è "aperto" (`/Count` positivo).
  - Il catalogo porta anche `/AcroForm` e `/OutputIntents` del sorgente: i widget restano legati al
    loro campo e il profilo colore resta accanto ai contenuti che ne hanno bisogno.
- **Scrittura atomica**: ogni parte va su `<nome>.part`, viene **riaperta e contata**, e solo allora
  prende il suo nome. Su errore i `.part` vengono cancellati. I nomi già occupati fermano tutto
  *prima* di scrivere (senza `--overwrite`), con l'elenco dei conflitti.
- `apps/pdfsplit split <file> --parts N | --pages "1-10, 11" [--leftover] [--out DIR] [--overwrite] [--no-bookmarks]`.
  Nomi: `<input>-01.pdf`, larghezza = cifre di N, minimo 2. `--parts 1` = riscrittura senza taglio.

## Compilare e testare

```sh
cargo build --profile release-min -p pdfsplit     # target/release-min/pdfsplit.exe (332 KiB)
cargo test --workspace                            # 29 unit + piano + fixture 128 MB + 1 doctest
cargo test -p splitter -- --ignored               # + apertura e taglio del fixture da 6,2 GB
cargo clippy --workspace --all-targets -- -D warnings
```

## Misure (Windows, disco locale, `release-min`, `--parts 2`)

| Input | Tempo dentro `pdfsplit` | Tempo totale processo | Picco working set / commit | Riferimento: picco di `cos` (M0) |
|---|---|---|---|---|
| 128 MB | 0,72 s | 1,32 s | 15,5 MB / 12,7 MB | 644 MB |
| 512 MB | 2,89 s | 3,61 s | 15,5 MB / 12,6 MB | — |
| 1 GB | 6,9 s | 7,7 s | 15,5 MB / 12,6 MB | 1.026 MB |
| **6,2 GB** | **76,3 s** | **77,1 s** | **15,5 MB / 12,6 MB** | 5.913 MB |

La RAM non dipende dalla dimensione: è il buffer di copia (4 MB) più l'Output BufWriter (8 MB), il
piano degli oggetti e l'indice. Il tempo è quasi tutto I/O: 6,2 GB letti + 6,2 GB scritti + 6,2 GB
riletti per la verifica, cioè ~245 MB/s sul disco di questo PC. Del tempo totale, ~40% è la
riapertura e il conteggio di ogni parte.

Parti del fixture da 6,2 GB: 3.100.001.018 + 3.100.000.805 byte, somma **595 byte più del sorgente**
(albero pagine, font e xref duplicati in ogni parte). Restano in `target/benchmark/out-split-6gb/`:
si possono cancellare quando vuoi.

## Validazione indipendente (pypdf, non codice nostro)

In `target/benchmark/pycheck` (venv locale, fuori dal repo) c'è pypdf 6.21.2; lo script
[`independent_check.py`](../target/benchmark/independent_check.py) scrive con pypdf un PDF di 4 pagine
con font, testo e segnalibri annidati, lo taglia con `pdfsplit` in tre modi e rilegge **ogni parte con
`PdfReader(strict=True)`**:

- `--pages "1-2, 3, 4"` → 3 parti, 2+1+1 pagine, testo `Pagina 1-2` / `Pagina 3` / `Pagina 4`,
  segnalibri `Uno`+`Due` nella prima, `Due.a` nella seconda;
- `--parts 2` → testo `Pagina 1-2` / `Pagina 3-4`, segnalibri al posto giusto;
- `--parts 1` → 4 pagine, testo intatto, gerarchia `Uno`, `Due`, `Due.a` annidata come nel sorgente;
- fixture da 128 MB → due parti da 67 MB, testo `Page 1` e `Page 2` estraibile da entrambe: il content
  stream da 67 MB è arrivato intero.

Nessuna delle parti ha fatto scattare lo strict mode di pypdf, che controlla la coerenza della xref.

## Cosa manca (e cosa ho deciso)

- **Parti oltre ~9,3 GiB**: la xref classica ha offset a 10 cifre. Sopra quella soglia `split` si ferma
  con un errore chiaro; serve la xref stream (M5). Il fixture da 6,2 GB non ci arriva.
- **Controllo preventivo dello spazio su disco**: non c'è. Rust non ha un'API di spazio libero e
  servirebbe una dipendenza nuova (`fs4`/`fs2`, piccole ma da approvare). Oggi l'errore arriva dal
  sistema operativo al primo `write` fallito, con il nome del file. Dimmi se aggiungo la dipendenza o
  se la facciamo in M5 con le altre rifiniture.
- **N parti "per dimensione stimata"**: M2 fa N parti uguali per numero di pagine. La variante per
  dimensione stimata richiede di pesare le risorse condivise, la metto quando la vuoi.
- **Page labels** non ricostruiti, **pagine residue non contigue** vanno bene in un unico file
  (l'ultimo), ma solo nella modalità elenco pagine.
- Non ci sono ancora: ricerca testo, nome da testo e taglio per stringa/segnalibri (M3), GUI (M4),
  compressione/versione/nomi configurabili (M5).
