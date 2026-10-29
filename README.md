# pdfsplit

Taglia PDF enormi — decine di migliaia di pagine, diversi gigabyte — in parti, senza mai caricare
il documento in memoria.

Serve per i casi in cui gli strumenti normali si arrendono: un unico PDF da 6 GB con 73.868 pagine
diviso in due parti in meno di un minuto, con il picco di memoria sotto il gigabyte.

## Cosa fa

- **Legge la struttura a incrementi.** Tabelle `xref` e `xref stream`, object stream, file con più
  sezioni: ogni oggetto è letto quando serve, non tutti insieme.
- **Copia gli stream così come sono.** Nessuna ricodifica delle immagini o dei font: le parti si
  scrivono copiando i byte, quindi il tempo dipende dal disco e non dal contenuto.
- **Ricostruisce i segnalibri in ogni parte**, ricalcolando le destinazioni sulle pagine che la
  parte contiene davvero. Un link che punta a una pagina finita altrove diventa nullo, invece di
  trascinarsi dentro il resto del documento.
- **Quattro modi di tagliare**: elenco di pagine, N parti uguali, testo trovato nelle pagine,
  livello dei segnalibri.
- **Due interfacce**: `pdfsplit`, da console, e `pdfsplit-gui`, una finestra con fasi, avanzamento,
  stima del tempo rimasto e annulla.

## Compilare

Serve Rust 1.95 o successivo.

```sh
cargo build --profile release-min                      # console
cargo build --profile release-min --features gui       # finestra
```

Il profilo `release-min` è quello della distribuzione: `opt-level = "s"`, LTO, `strip`, `panic
abort`. Produce `target/release-min/pdfsplit.exe` (circa 555 KiB) e, con la feature `gui`,
`pdfsplit-gui.exe` (circa 8 MiB, quasi tutto egui e wgpu).

I test:

```sh
cargo test --workspace
cargo test -p pdfsplit --features gui
```

## Uso

```
pdfsplit info <file.pdf>             che cos'è il file: peso, versione, pagine, segnalibri, cifratura
pdfsplit pages <file.pdf>            solo il numero di pagine
pdfsplit bookmarks <file.pdf>        l'indice come righe `livello<TAB>pagina<TAB>titolo`
pdfsplit search <file.pdf> <testo>   quali pagine contengono il testo
pdfsplit text <file.pdf> --page N    che cosa dice una pagina
pdfsplit split <file.pdf>            taglia il file in parti
pdfsplit --help                      questo testo
```

Esempi:

```sh
# un documento per ogni intestazione trovata nel testo, con il nome preso dalla riga
pdfsplit split "FILE_TEST_X_TRIM-PDF.pdf" --string "AVVISO DI PAGAMENTO TASSA RIFIUTI" --name-label "Documento N."

# le prime 35.308 pagine, il resto in un ultimo file, sostituendo quel che c'è già
pdfsplit split "FILE_TEST_X_TRIM-PDF.pdf" --pages 1-35308 --leftover --overwrite

# taglio per segnalibro di primo livello, comprimendo i flussi
pdfsplit split "Fattura 101.pdf" --bookmarks 1 --name-from-title --compress --out .\cartella-out
```

Le parti si chiamano, per esempio, `FILE_TEST_X_TRIM-PDF-01.pdf`, `-02.pdf`. Con `--prefix`,
`--suffix`, `--width` e `--start` si cambiano prefisso, suffisso, cifre e numero di partenza; con
`--always-number` la sequenza si aggiunge anche quando il nome viene dal testo.

## Prestazioni misurate

Su un PDF reale da 6,06 GB con 73.868 pagine, diviso in due parti da circa 37.000 pagine:

| | |
|---|---|
| durata | 00:00:49 |
| byte scritti | 6.104 MiB |

Il picco di memoria non cresce con il numero di pagine: su un fixture sintetico da 300.000 pagine
resta sotto i 700 MiB, perché il documento non viene mai letto tutto insieme.

La compressione dei flussi costa circa un minuto e mezzo in più sullo stesso file, perché ogni
stream va scompattato e ricompresso invece di essere copiato. Per questo le parti si scrivono, per
difetto, con la stessa versione PDF del sorgente.

## Limiti noti

- I riferimenti a pagine che finiscono in un'altra parte vengono azzerati: un link interno al
  documento non sopravvive al taglio.
- Del catalogo si portano le pagine, i segnalibri e `OCProperties`; `Info`, `Metadata`,
  `PageLabels`, `Names` e `StructTreeRoot` non vengono riscritti.
- Una parte non può essere scritta in una versione PDF più vecchia di 1.5 quando usa object stream
  e xref stream: in quel caso la versione del sorgente viene alzata e il report lo dice.

## Licenza

MIT oppure Apache-2.0, a scelta. Vedi [LICENSE-MIT](LICENSE-MIT) e [LICENSE-APACHE](LICENSE-APACHE),
e [NOTICE](NOTICE) per il codice di terzi incluso (`vendor/pdfcraft-filters`, derivato da
[PdfCraft](https://github.com/storytold/pdfcraft)).
