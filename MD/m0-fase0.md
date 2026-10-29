# M0 — Fase 0: benchmark su PDF ≥ 6 GB e scelta del motore

Data: 2026-10-27 · Riferimento: `storytold/pdfcraft` @ `1dd2737` (clone shallow)

## 1. Fixture sintetico

`target/benchmark/pdfsplit-6gb.pdf` — 6.200.001.228 byte, 2 pagine (testo + immagine XObject 3×1),
1 segnalibro, xref classica. Gli stream di pagina sono byte reali (testo + riempimento `%`), quindi
gli offset xref e la scansione di `endstream` sono quelli di un file vero. Generatore:
`target/benchmark/generate_fixture.py` (accetta dimensione, numero pagine). Fixture di scala:
128 MB, 512 MB, 1 GB, 2 GB.

Macchina di prova: Windows 11, i7-4702MQ, **7,93 GB RAM**, limite di commit 11,94 GB, 31 GB disco liberi.

## 2. Tempi e RAM di picco con `cos` + `organize`

Probe: `target/benchmark/pdfcraft/apps/sizecheck` (`info` = apertura + conteggio pagine + segnalibri,
`split` = split + `write_full` + riapertura di verifica). Picco = `PeakWorkingSet64` campionato ogni 200 ms.

| Input | Apertura (tempo / picco) | Split in 2 parti (tempo / picco) |
|---|---|---|
| 128 MB | — | 7,9 s / 644 MB |
| 1 GB | 17,0 s / 1.026 MB | 59,7 s / 5.116 MB |
| 2 GB | 33,6 s / 2.050 MB | non eseguito (≈ 9,6 GB attesi > limite di commit) |
| **6,2 GB** | **45,0 s / 5.913 MB** | **non eseguito (≈ 30 GB attesi)** |

- Apertura ≈ **1× la dimensione del file**; split ≈ **4,8×** (sorgente + parti + output + verifica).
- Il parsing dell'xref è irrilevante (15 ms); il tempo è tutto I/O di lettura (~140 MB/s).
- Coerente con quanto dichiara upstream in `docs/gaps.md` §13: “a 1 GiB file opens at 1.0 GiB peak”.
- **`pdfcraft-cli split` non compilabile in questo clone**: la revisione pinnata di `craft-libs` non
  espone più `craft-menu` (`no matching package named craft-menu`). La sua pipeline è comunque la
  stessa misurata (`engine::Session::split` → `organize::split_ranges` + salvataggio completo).

Verdetto: **il percorso attuale non soddisfa il requisito** di RAM privata < 1 GB: `cos` conserva
l'intero file in un `Arc<Vec<u8>>` e ogni parte ne duplica gli stream. A 6–8 GB servono 6–8 GB per
l'apertura e ~30–38 GB per lo split.

## 3. Peso dell'eseguibile con `pdfcraft-render` (profilo `release-min`)

| Build | Dimensione |
|---|---|
| core (`cos` + `organize` + `filters` + `crypt`) | **1,40 MB** (1.464.832 B) |
| + `pdfcraft-render` (hayro, lopdf, decodificatori immagine) | **3,92 MB** (4.112.896 B) |

Delta: **+2,53 MB (+181%)**. L'estrazione testo funziona (pagina 1 → “Page 1”, 2 s su 2 pagine), ma
il renderer **legge una seconda copia intera del file** (picco 325 MB su un fixture da 134 MB).
→ `render` va messo dietro feature opzionale, attiva solo per i criteri “stringa”/“nome da testo”.

## 4. Cosa è riusabile (10 righe)

1. `cos`: catena `/Prev`, xref stream, file ibridi, object stream, repair e scrittura xref (classica e stream), cifratura — come logica di riferimento; il buffer unico resta il limite.
2. `organize`: `split_ranges`, `extract_pages`, segnalibri (`/Outlines`, `/Dest`, `/A GoTo`, named destinations), page labels, `prune_unused_xobjects`.
3. `filters`: Flate e filtri immagine, per la modalità “Comprimi” e per gli stream di oggetto.
4. `crypt`: solo per rilevare un PDF cifrato e rifiutarlo con errore chiaro (v1).
5. `render`: estrazione testo per pagina (parallela), dietro feature; costa +2,5 MB e +1 copia del file.
6. `platform::staging`: scrittura atomica (file temporaneo + rename) già pronta.
7. `ui-egui`: il dialogo “Split document” è solo riferimento di stile, non codice riusabile.
8. `cos`/`organize` sono L1 senza dipendenze UI: il nuovo core può dipenderne senza rompere il layering.
9. Il workspace impone `unsafe_code = "forbid"`: niente mmap se non con eccezione esplicita.
10. Upstream dichiara `ByteSource`/`core.lazy-loading` (apertura di file GB con budget) **non ancora fatto**.

## 5. Motore consigliato: (c) su lettore posizionale safe — scarto (a) da solo e (b)

- **(a) da solo non basta**: `cos::Document` riceve i byte completi; rendere “a range” ogni reader
  significa toccare tutti i moduli e anche `hayro` (che parsa un buffer contiguo) — non è una
  modifica additiva e minima, ed è lavoro che upstream ha in piano ma non ha.
- **(b) mmap scartato**: richiede `unsafe` (vietato dal workspace), non riduce in modo affidabile il
  working set su Windows e complica il controllo “spazio su disco” e l'annullamento.
- **(c) consigliato**: nuova crate `splitter` con `ByteSource` posizionale in codice safe
  (`File` + `seek`/`read_exact` su buffer limitati) che parsa **solo** xref/trailer/page tree, poi
  copia gli stream **raw** con `BufWriter` da 8 MB, rinumerando gli oggetti raggiungibili e
  costruendo la nuova xref. RAM = indice xref + page tree + buffer (pochi MB), indipendente dal file.
  Per i segnalibri si riusa la logica di lettura di `organize` su un modello leggero; per “Comprimi”
  si usa `filters` solo sugli stream non compressi.

Stima di picco attesa: **< 100 MB** su 6–8 GB, con tempo dominato dall'I/O (una lettura + una
scrittura sequenziale per parte).

## 6. Come riprodurre

```bash
python target/benchmark/generate_fixture.py target/benchmark/pdfsplit-6gb.pdf 3100000000 2
cd target/benchmark/pdfcraft && cargo build -p pdfcraft-sizecheck --profile release-min
powershell -File target/benchmark/measure.ps1 -Exe <exe> -Arguments "info <pdf>"
powershell -File target/benchmark/measure.ps1 -Exe <exe> -Arguments "split <pdf> 2 <out-dir>"
```

Nota: il clone di benchmark è in `target/` (ignorato da Git) e ha `Cargo.toml` con membri ridotti
per compilare solo i crate necessari; l'app vera non è ancora stata toccata.
