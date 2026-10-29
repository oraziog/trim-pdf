# M4 — finestra egui, avanzamento per fase, ETA, annulla

## Cosa c'è ora

### Nel core

- [`crates/splitter/src/progress.rs`](../crates/splitter/src/progress.rs): `Stage` (apertura, struttura,
  ricerca, piano, scrittura, verifica), `Unit` (pagine, parti, byte), `Update` (fase, fatto, totale,
  unità, parte), `Job` (dove riportare + la bandierina di stop) e `Canceller` (una maniglia
  clonabile, tenuta dalla finestra, dal lavoro e — quando serve — dal reporter stesso).
  `Error::Cancelled` è l'unico modo in cui un lavoro fermato esce.
- `Document::search_with(query, job)` e `Document::split_with(plan, options, job)`: le stesse funzioni
  di M1–M3 con un `Job` in più. `search`/`split` restano, e sono un `Job` silenzioso: la CLI non
  cambia comportamento.
- **Dove si guarda la bandierina**: ogni pagina letta, ogni parte prima di essere pianificata, ogni
  blocco di 4 MB copiato. Un annullo durante la copia di uno stream da 3 GB arriva quindi entro un
  blocco, non a fine stream.
- **Le fasi non si intrecciano**: prima si scrivono tutte le parti, poi si riaprono una a una per
  contarle. Prima di M4 la verifica era dentro il ciclo di scrittura, e la barra tornava indietro di
  fase a ogni parte.
- **La scrittura riporta i byte**: `write::write` riceve la posizione della parte nel lavoro
  (`Placement`) e chiama il reporter ogni 4 MB, così una parte da 3,1 GB muove la barra ~780 volte
  invece di restare ferma un minuto.

### La finestra

- [`apps/pdfsplit/src/bin/pdfsplit-gui.rs`](../apps/pdfsplit/src/bin/pdfsplit-gui.rs): il programma.
  Compilato solo con `--features gui` (la CLI resta un binario senza grafica e senza egui), con
  `windows_subsystem = "windows"` in release: niente console accanto alla finestra. Prova **Direct3D
  12** e, se la finestra non si apre, **OpenGL** (vedi "Perché due backend").
- [`apps/pdfsplit/src/gui/app.rs`](../apps/pdfsplit/src/gui/app.rs): lo stato e i pannelli.
  - File: campo percorso, pulsante "Scegli…", **drag & drop**; sotto, le notizie lette dal file
    (pagine, segnalibri, versione, dimensione) e l'avviso se è protetto da password.
  - Quattro modi: N parti uguali (contatore 1–10.000), elenco pagine con "pagine residue in un
    ultimo file", stringa di taglio con maiuscole/minuscole, espressione regolare e "chiudi dopo la
    pagina trovata", segnalibri con livello e "nome dal titolo".
  - Nome delle parti: sequenza, oppure etichetta + espressione facoltativa, con "aggiungi sempre la
    sequenza".
  - Cartella di uscita (con "Sfoglia…"), "mantieni i segnalibri", "sovrascrivi".
  - "Dividi" e "Annulla"; durante un lavoro il modulo è disabilitato e la finestra resta viva.
  - Avanzamento: nome della fase, barra complessiva con percentuale, riga di dettaglio
    (`parte 2 di 3 · 3,1 GiB di 5,8 GiB`, `pagina 120 di 456`, `parte 2 di 3`), tempo trascorso e
    stima mancante. Alla fine l'elenco delle parti scritte con pagine e dimensione.
  - Errori in rosso con il messaggio del motore; "Annullato: nessun file è stato scritto." quando
    l'annullo ha vinto.
- [`apps/pdfsplit/src/gui/work.rs`](../apps/pdfsplit/src/gui/work.rs): il lavoro, senza sapere niente
  di schermo. `Setup` (tutto quello che la finestra ha deciso), `Mode`, `Naming`, `Event` e
  `run(setup, sender, stop)` — apre, cerca se serve, pianifica, scrive. È il pezzo che i test
  provano direttamente.
- [`apps/pdfsplit/src/gui/meter.rs`](../apps/pdfsplit/src/gui/meter.rs): una barra e una stima. Le
  fasi pesano diversamente (scrittura 14, verifica 4, ricerca 6, struttura 1, piano 1, apertura 0,5);
  la stima è tempo trascorso × (1 − avanzamento) / avanzamento, mediata 60/40 con la precedente per
  non tremare. I pesi vengono dalle misure di M2 e M3 (la verifica è circa un quinto del tempo).
- [`apps/pdfsplit/src/gui/mod.rs`](../apps/pdfsplit/src/gui/mod.rs): le etichette italiane delle fasi,
  le dimensioni (`5.8 GiB`) e i tempi (`1 min 20 s`) come li legge una persona.

## Compilare e testare

```sh
# la CLI, come prima: 481 KiB, nessuna grafica
cargo build --profile release-min -p pdfsplit

# la finestra (richiede la feature: un `cargo build` normale non tira dentro egui)
cargo build -p pdfsplit --features gui --bin pdfsplit-gui

cargo test --workspace                                        # 55 unit + 2 fixture + doctest
cargo test -p pdfsplit --features gui --bin pdfsplit-gui      # 18 test della finestra
cargo test -p splitter --test large_fixtures -- --ignored     # + i due test sul file da 6,2 GB
cargo clippy --workspace --all-targets --features pdfsplit/gui -- -D warnings
cargo fmt --all --check
```

I 18 test della finestra non hanno bisogno di uno schermo: `App::before_frame` + `App::contents`
girano su un `egui::Context` nudo (`ctx.run_ui`), quindi si prova davvero il disegno di tutte e
quattro le modalità, la macchina a stati con eventi veri (`Done`, `Failed`, `Cancelled`,
`Progress`), il rifiuto di un file che non c'è e un lavoro completo: file scelto → notizie lette dal
thread → "Dividi" → parti scritte e riportate nell'elenco.

## Misure (Windows 11, sessione RDP, `release`)

| | |
|---|---|
| `pdfsplit.exe` (CLI, `release-min`) | **481 KiB** (492.544 B; +3 KiB da M3 per `progress.rs`) |
| `pdfsplit-gui.exe` (`release-min`, entrambi i backend) | **7,7 MiB** (8.050.688 B) — 5 min 38 s di build, a dipendenze già compilate |
| `pdfsplit-gui.exe` (`release`, thin LTO, entrambi i backend) | 13,2 MiB (13.832.192 B) |
| `pdfsplit-gui.exe` (`release-min`, **sola glow**, prima di wgpu) | 4,5 MiB (4.759.040 B) |
| finestra aperta e ferma | 52,6 MB di working set, 9 thread |
| annullo di uno split da 6,2 GB | **53,9 ms** dal flag, **nessun file lasciato** |
| split da 6,2 GB con il percorso nuovo | 60,5 s dentro `pdfsplit`, 63,3 s il processo, **15,6 MB di picco** |
| split da 128 MB | ≥ 30 aggiornamenti di byte (uno ogni 4 MB), monotoni |

Il tempo sul file da 6,2 GB dipende dalla cache del sistema operativo: 63 s con il file appena letto,
77 s a freddo (M2). Il picco di RAM non cambia: 15,6 MB, come prima dell'avanzamento.

## Perché due backend, e perché Vulkan non c'è

Su questa macchina, una sessione RDP con "Microsoft Basic Display Adapter", OpenGL si ferma a 1.1:

```
Error: OpenGL(PainterError("egui_glow requires opengl 2.0+. "))
```

Cioè: la finestra **non si apriva affatto**. wgpu apre la finestra anche qui. Restano compilati
entrambi: wgpu per primo, glow come ripiego per una macchina che ha OpenGL ma nessun dispositivo per
wgpu. Il costo è la dimensione: 7,7 MiB con entrambi i backend, 4,5 MiB con la sola glow (e una
finestra che qui non si apre).

**Vulkan è escluso di proposito.** I backend predefiniti di wgpu su Windows sono Vulkan e Direct3D
12, e su un Windows 11 con grafica Intel il runtime Vulkan (`igvk64.dll`) chiude il processo con
una access violation (`c0000005`) appena si crea l'istanza: nessun `Err`, nessun ripiego, la
finestra e il processo spariscono. Non c'è modo di intercettarlo da Rust, quindi il driver non va
proprio caricato: su Windows wgpu gira con **D3D12 soltanto**.

```
Firma del problema
Nome evento problema:  APPCRASH
Nome modulo con errori: igvk64.dll
Codice eccezione:      c0000005
```

L'ordine è: D3D12 su Windows (o la scelta di wgpu altrove), poi glow. `PDFSPLIT_RENDERER=dx12 |
glow | wgpu | vulkan` impone un solo tentativo, senza ripieghi, per una macchina che si comporta
male. Se non si apre nessuna delle due vie, ora compare una finestra di messaggio con il motivo per
esteso: prima un doppio clic non produceva niente e non lasciava traccia.

## Cosa non c'è ancora (M5)

Prefisso/suffisso/posizione e larghezza della sequenza, valore iniziale, compressione, versione PDF
di uscita, apertura della cartella al termine, "elimina segnalibri" come interruttore separato (oggi
è "mantieni i segnalibri"), e la scelta del backend a un solo motore grafico.

## Nota su cosa è stato verificato e cosa no

- Verificato con test: la macchina a stati della finestra, i frame di tutte le modalità, il lavoro
  completo su un PDF di prova, l'annullo (anche su 6,2 GB), la granularità dell'avanzamento, i pesi
  e la stima della barra.
- Verificato a mano: il processo `pdfsplit-gui.exe` parte, apre la finestra e resta vivo su questa
  macchina (wgpu). **Non** è stato possibile guardare la finestra disegnata né cliccare i pulsanti:
  qui non c'è un display da fotografare. Un controllo visivo vero richiede una macchina con schermo.
