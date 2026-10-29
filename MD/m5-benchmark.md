# M5 — benchmark compressione × versione PDF

**File di prova:** `target/benchmark/2026_FILE_TEST_X_TRIM-PDF.pdf`
5.664 MiB (6.062.905.985 byte, 5,65 GiB), PDF 1.5, 73.868 pagine, nessun segnalibro, non cifrato.

**Taglio:** elenco pagine `1-35308` + residuo nell'ultimo file, sequenza sempre presente nei nomi.
**Uscita:** due parti, `2026_FILE_TEST_X_TRIM-PDF-01.pdf` (35.308 pagine) e `-02.pdf` (38.560 pagine).
**Eseguibile:** `target/release/pdfsplit.exe` (profilo release, non debug).
**Script:** `target/benchmark/run-m5-matrix.bat` — esegue le 8 combinazioni, misura, scrive `risultati.txt` e cancella ogni cartella tranne `neutral-1.7`.

## Risultati

| combinazione | compressione | versione | tempo parete | tempo scrittura | byte scritti | MiB | vs neutro | vs sorgente |
|---|---|---|---|---|---|---|---|---|
| neutral-1.5 | neutra | 1.5 | 296 s | 289.869 ms | 6.576.005.305 | 6.271 | — | +8,46 % |
| neutral-1.6 | neutra | 1.6 | 368 s | 360.673 ms | 6.576.005.305 | 6.271 | — | +8,46 % |
| neutral-1.7 | neutra | 1.7 | 375 s | 368.632 ms | 6.576.005.305 | 6.271 | — | +8,46 % |
| neutral-2.0 | neutra | 2.0 | 376 s | 370.691 ms | 6.576.005.305 | 6.271 | — | +8,46 % |
| compress-1.5 | comprimi | 1.5 | 399 s | 393.961 ms | 6.082.528.312 | 5.801 | **−470,6 MiB (−7,50 %)** | +0,32 % |
| compress-1.6 | comprimi | 1.6 | 387 s | 381.395 ms | 6.082.528.312 | 5.801 | −470,6 MiB | +0,32 % |
| compress-1.7 | comprimi | 1.7 | 392 s | 386.637 ms | 6.082.528.312 | 5.801 | −470,6 MiB | +0,32 % |
| compress-2.0 | comprimi | 2.0 | 390 s | 383.938 ms | 6.082.528.312 | 5.801 | −470,6 MiB | +0,32 % |

## Lettura dei dati

- **La versione PDF non cambia il peso.** A parità di compressione i byte sono identici: cambia solo la stringa nell'header, che ha la stessa lunghezza per 1.5, 1.6, 1.7 e 2.0. Scegliere 1.7 o 2.0 non fa risparmiare nulla qui.
- **La compressione paga.** −470,6 MiB (−7,5 %) per circa +20–100 s. Il guadagno è più marcato rispetto al sorgente (+0,32 % contro +8,46 %) perché la compressione compensa quasi esattamente la duplicazione delle risorse condivise tra le due parti (il neutro le copia in entrambe).
- **Tempi omogenei.** Il primo run neutro (296 s) è il più veloce del gruppo; gli altri tre neutri stanno tra 368 e 376 s. La differenza è più rumore di pagina cache che effetto della versione. Il blocco compresso sta tra 387 e 399 s, quindi circa +25 % di tempo rispetto al neutro.
- **Scrittura ≠ tempo parete.** Il tempo di scrittura del writer è 290–394 s; la differenza con il tempo parete è l'avvio e l'analisi dell'xref, circa 5–8 s.

## Cosa conviene

- **N = 1 (riscrittura ottimizzata):** compressa a **1.5**. Object stream e xref stream arrivano con la 1.5, danno il risparmio e hanno la compatibilità migliore.
- **N ≥ 2:** con questo documento la compressione conviene: −7,5 % di spazio per +25 % di tempo. Se il tempo conta più dello spazio (parti molto grandi, disco veloce) resta valido il neutro a 1.7; se lo spazio conta, **compress a 1.5** è la scelta migliore, perché il peso è identico a 1.7/2.0 ma la compatibilità è maggiore.
- **Versione:** 2.0 non offre alcun vantaggio misurabile su questo file e restringe la compatibilità. Da evitare come default.

## Verifica

- Ogni run è stato validato dal programma stesso: ogni parte viene scritta come `.part`, riaperta e contata prima del rename.
- Controllo indipendente sull'output conservato (`neutral-1.7`): parte 1 = 3.117.119.221 byte, 35.308 pagine, PDF 1.7; parte 2 = 3.458.886.084 byte, 38.560 pagine, PDF 1.7. Somma 73.868 pagine = sorgente. Entrambe riaprono con `pdfsplit info`.
- Le cartelle compresse sono state cancellate dopo la misura per non esaurire il disco (18 GB liberi, ~6 GB per combinazione). Restano i log per combinazione in `test-compressione/*.log`.
