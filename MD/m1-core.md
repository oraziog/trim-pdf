# M1 — core da CLI (apertura, pagine, segnalibri a RAM costante)

## Cosa c'è ora

- `crates/splitter`: il core. Apre il file leggendo solo header e xref (`startxref`, catena `/Prev`,
  xref classica, xref stream, ibridi `/XRefStm`, object stream), conta le pagine dal page tree e
  legge i segnalibri (`/Outlines`, `/Dest`, `/A /GoTo`, destinazioni con nome in `/Dests` e
  `/Names /Dests`). Nessun `unsafe`, ogni cache è limitata, ogni limite ha un errore chiaro.
- `apps/pdfsplit`: la CLI della stessa CLI-thin dell'app finale.
  - `pdfsplit info <file.pdf>` — dimensione, versione PDF, cifrato sì/no, pagine, segnalibri.
  - `pdfsplit pages <file.pdf>` — solo il numero di pagine.
  - `pdfsplit bookmarks <file.pdf>` — `livello<TAB>pagina<TAB>titolo`.
- `vendor/pdfcraft-filters`: Flate/predictor/lzw copy di `storytold/pdfcraft` (MIT OR Apache-2.0),
  con `NOTICE` e README di provenienza. Uniche dipendenze: `pdfcraft-filters` + `thiserror`.

## Compilare e testare

```sh
cargo build --profile release-min -p pdfsplit    # target/release-min/pdfsplit.exe
cargo test --workspace                           # 12 unit + fixture 128 MB + 1 doctest
cargo test -p splitter -- --ignored              # aggiunge la prova sul fixture da 6 GB
```

## Misure (Windows, disco locale; picco = PeakWorkingSet64 / commit)

| Input | `pdfsplit info` (per esecuzione) | Picco RAM | Percorso `cos` di riferimento |
|---|---|---|---|
| 128 MB | ~48 ms | 3,7 MB | 644 MB (M0) |
| 512 MB | ~46 ms | 3,6 MB | — |
| 1 GB | ~39 ms | 3,6 MB | 1.026 MB / 5,4 s |
| 2 GB | ~39 ms | 3,6 MB | 2.050 MB (M0) |
| **6,2 GB** | **~43 ms** | **3,6 MB working set / 0,6 MB commit** | 5.913 MB / 105 s |

I tempi sono la media di 20 esecuzioni complete del processo (0,86–0,96 s per 20 lanci sul file da
6,2 GB come su quello da 128 MB): sono dominati dall'avvio del processo, non dal file. Il `read_ms`
che la CLI riporta è 0 a ogni dimensione. La RAM non cresce con il file: il costo che cresce è solo l'indice
delle xref (voci da 24 byte circa, non i byte del documento).

`pdfsplit.exe` con `release-min`: **261 KiB** (il core è 1,40 MB nel build M0 con le dipendenze
cos/organize; qui la CLI+core è 262 KiB perché non porta dietro nulla di inutile).

## Nota sui fixture

Il generatore di M0 scriveva le voci xref nell'ordine di scrittura degli oggetti, non per numero:
il file aveva quindi offset sbagliati (ma `cos` li tollerava). Corretto in M1; i fixture sono stati
rigenerati con le stesse dimensioni e il percorso `cos` rimisurato conferma gli stessi numeri
(6,2 GB → 5.913 MB di picco). Un test nuovo verifica che un xref che punta all'oggetto sbagliato
produca un errore chiaro invece di dati sbagliati.

## Non c'è ancora

Taglio e riscrittura delle parti (M2), ricerca testo (M3), GUI (M4), compressione/versioni/nomi
(M5), file cifrati (errore esplicito, nessuna decrittazione).
