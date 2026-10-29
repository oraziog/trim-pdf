# M5 — compressione, versione PDF e nomi configurabili

Il core, la CLI e la finestra espongono ora la compressione Flate lossless, la versione PDF d’uscita e un template di nome con prefisso, suffisso, posizione della sequenza, larghezza, valore iniziale e opzione per numerare sempre. La compressione tocca solo stream senza filtro; immagini e stream già compressi restano raw. Object stream e xref stream sono usati nella modalità compressa. La versione richiesta viene aumentata se le strutture o le funzionalità delle pagine lo richiedono, con avviso nel report.

## Regole di default

Il benchmark su 6 GB ha misurato che comprimere costa poco (296 s → 399 s sul taglio in due parti, circa un minuto e mezzo) e rende il 7,5 % in meno, mentre la versione PDF non cambia il peso di un solo byte: a parità di compressione le quattro versioni scrivono esattamente gli stessi byte. Da qui le due regole che valgono in finestra e in CLI:

- **si comprime sempre**, salvo chiedere il contrario (`--neutral` in CLI, la casella nella finestra);
- **la versione delle parti è quella del sorgente**, così un taglio non consegna a un lettore un documento più nuovo di quello da cui è nato. Se il sorgente è più vecchio della 1.5 le parti vanno alla 1.5, la prima che porta l'object stream e la xref stream della compressione, e il report lo dice.

Il file di prova è una 1.5: con queste regole le parti escono 1.5 compresse, cioè 5.801 MiB invece di 6.271 MiB, in circa sette minuti.

## Verifica

- `cargo test -p splitter`: 67 test unitari e 2 fixture test passati; 2 test da 6 GB sono ignorati per default.
- `cargo test -p pdfsplit --features gui`: 22 test passati.
- CLI compilata; `info` sul PDF reale `target/benchmark/2026_FILE_TEST_X_TRIM-PDF.pdf`: 6,062,905,985 byte (5.6 GiB), PDF 1.5, 73,868 pagine, non cifrato, nessun segnalibro; apertura/analisi in 8.429 s.
- Tentativo di riscrittura compressa integrale del PDF reale con nome `M5-…-test-007.pdf`: non completato entro il limite di 10 minuti; il processo ha lasciato un `.part` da 1.5 GiB, rimosso dopo il controllo. Non si dichiara quindi validata la compressione sull’intero file da 6 GB. Il writer attuale costruisce il piano degli oggetti raggiungibili per tutte le pagine in memoria; il requisito di memoria costante per questa riscrittura resta un limite noto.

## Limiti

La riscrittura di documenti enormi può essere molto lenta e il piano degli oggetti non è a RAM costante; il test reale ha raggiunto il timeout prima del completamento. La compressione indipendente su PDF sintetici è coperta dai test del core; il test pypdf richiede un run separato se serve una conferma indipendente.
