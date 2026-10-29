//! pdfsplit — the command line of the splitting app (the desktop window follows in M4).
//!
//! `info`, `pages`, `bookmarks`, `search` and `text` answer questions about a file read at
//! offsets and streams, never loaded. `split` writes the parts — objects renumbered, streams
//! copied byte for byte, names taken from the input, from the pages' own text or from a bookmark.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use splitter::{Compression, Document, Error, Job, Mode, NameTemplate, Naming, Query, SequencePosition, Setup, Version};

const USAGE: &str = r#"pdfsplit — divide PDF molto grandi

uso:
  pdfsplit info <file.pdf>            che cos'è il file: peso, versione, pagine, segnalibri, cifratura
  pdfsplit pages <file.pdf>           solo il numero di pagine
  pdfsplit bookmarks <file.pdf>       l'indice come righe `livello<TAB>pagina<TAB>titolo`
  pdfsplit search <file.pdf> <testo>  quali pagine contengono il testo
  pdfsplit text <file.pdf> --page N   che cosa dice una pagina
  pdfsplit split <file.pdf>           taglia il file in parti
  pdfsplit --help                     questo testo

opzioni di split:
  --parts N                         N parti il più possibile uguali (predefinito 2); 1 riscrive
                                    il file intero
  --pages "1-10, 11, 13-20"         una parte per ogni intervallo, pagine numerate da 1
  --string "Fattura"                una nuova parte a ogni pagina che contiene il testo
  --bookmarks LIVELLO               una nuova parte a ogni segnalibro di quel livello (1 = primo)
  --leftover                        con --pages: le pagine che nessun intervallo copre, in
                                    un'ultima parte
  --case-sensitive                  non ignora le maiuscole quando cerca --string
  --regex                           legge --string come espressione regolare
  --close-after                     con --string: la pagina trovata chiude la parte invece di
                                    aprirla
  --name-label "Codice Fiscale"     dà il nome a ogni parte dal valore che segue questa scritta
                                    nelle sue pagine (i due punti dopo la scritta sono
                                    facoltativi)
  --name-pattern "..."              legge quel valore con un'espressione regolare propria
  --name-from-title                 dà il nome a ogni parte dal segnalibro che l'ha aperta
  --always-number                   mette la sequenza sul nome di ogni parte
  --prefix TESTO                    scritto prima del nome; dopo il numero, quando questo viene
                                    per primo
  --suffix TESTO                    scritto dopo il nome; prima del numero, quando questo viene
                                    per ultimo
  --width N                         cifre del numero (predefinito: quante ne servono per le parti)
  --start N                         il numero che prende la prima parte (predefinito 1)
  --number-first                    il numero sta prima del nome invece che dopo
  --out CARTELLA                    dove finiscono le parti (predefinito: accanto al file di input)
  --overwrite                       sostituisce i file che ci sono già
  --no-bookmarks                    scrive le parti senza indice
  --compress                        comprime i flussi che il sorgente lasciava non compressi e
                                    impacchetta gli oggetti della parte dietro un cross-reference
                                    stream (è il predefinito: pesa meno e costa poco)
  --neutral                         copia ogni flusso esattamente come lo tiene il sorgente: la
                                    scrittura più rapida, e la più pesante
  --pdf-version X.Y                 la versione da scrivere (predefinito: quella del sorgente, e
                                    mai più vecchia di 1.5, che è quanto serve alla
                                    compressione); una parte le cui pagine ne chiedono una più
                                    alta la alza da sé e lo dice

opzioni di search:
  --case-sensitive                  non ignora le maiuscole
  --regex                           legge il testo come espressione regolare

`info` e `bookmarks` non caricano mai il file: leggono solo i dati di cross-reference e la
struttura degli oggetti che serve, così la memoria non cresce con il documento. Un taglio comprime
per difetto: Flate sui flussi che non hanno già un filtro proprio, mai su un'immagine. --neutral
copia invece ogni flusso byte per byte. Le parti mantengono la versione PDF del sorgente, alzata a
1.5 quando il sorgente è più vecchio, perché la compressione scrive un object stream. Il file non
viene mai letto tutto in memoria, qualunque sia la sua dimensione. Il testo è letto una volta per
pagina e tenuto: una ricerca, un nome preso dal testo e il taglio condividono un solo passaggio.

esempi:

  # Il taglio più semplice: le pagine 1-35308 in una parte, tutte le residue in un'ultima parte.
  # Le parti finiscono accanto al file di ingresso; l'estensione .pdf si può anche omettere.
  pdfsplit split "2026_FILE_TEST_X_TRIM-PDF.pdf" --pages "1-35308" --leftover --overwrite

  # Una parte per ogni pagina che contiene quell'avviso; ogni parte prende il nome dal numero che
  # segue la scritta `Documento N.` nella sua prima pagina.
  pdfsplit split "2026_FILE_TEST_X_TRIM-PDF.pdf" --string "AVVISO DI PAGAMENTO TASSA RIFIUTI" --name-label "Documento N."

  # Lo stesso taglio, ma in una cartella di uscita scelta: il percorso ha uno spazio, quindi va tra
  # virgolette. --always-number numera anche una parte sola; --compress è il predefinito, detto per
  # chiarezza.
  pdfsplit split "Fattura 101.pdf" --string "Codice Fiscale:" --name-label "Codice Fiscale:" --always-number --out ".\cartella-out" --compress

  # Una parte per capitolo, cioè per ogni segnalibro del primo livello, con il prefisso sul nome.
  pdfsplit split "Manuale.pdf" --bookmarks 1 --prefix "Capitolo-"

  # Quattro parti uguali, senza comprimere: il modo più rapido per spezzare un archivio enorme.
  pdfsplit split "Archivio 2026.pdf" --parts 4 --neutral

Ogni comando riporta su standard output una riga per parte, con peso e percorso, e alla fine i
byte scritti e il tempo impiegato; un avviso (riga `note`) spiega una versione alzata o una risorsa
condivisa copiata in più parti."#;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliError::Message(message)) => {
            eprintln!("pdfsplit: {message}");
            ExitCode::FAILURE
        }
        Err(CliError::Stdout(source)) => {
            // A closed pipe (the output went to `head`, say) is not a failure.
            if source.kind() == std::io::ErrorKind::BrokenPipe { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
    }
}

#[derive(Debug)]
enum CliError {
    Message(String),
    Stdout(std::io::Error),
}

impl From<String> for CliError {
    fn from(message: String) -> Self {
        CliError::Message(message)
    }
}

impl From<std::io::Error> for CliError {
    fn from(source: std::io::Error) -> Self {
        CliError::Stdout(source)
    }
}

impl From<Error> for CliError {
    fn from(error: Error) -> Self {
        CliError::Message(error.to_string())
    }
}

fn run(args: &[String]) -> Result<(), CliError> {
    let command = match args.first().map(String::as_str) {
        None | Some("--help" | "-h" | "help") => {
            println!("{USAGE}");
            return Ok(());
        }
        Some("--version") => {
            println!("pdfsplit {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some(command) => command.to_string(),
    };
    let path = args.get(1).ok_or_else(|| format!("{command}: manca il percorso del PDF (vedi `pdfsplit --help`)"))?;
    let path = Path::new(path);
    let rest = args.get(2..).unwrap_or(&[]);

    match command.as_str() {
        "info" => info(path),
        "pages" => pages(path),
        "bookmarks" => bookmarks(path),
        "search" => search(path, rest),
        "text" => text(path, rest),
        "split" => split(path, rest),
        other => Err(format!("comando sconosciuto «{other}» (vedi `pdfsplit --help`)").into()),
    }
}

/// Open the document and print its header facts and structure counts.
fn info(path: &Path) -> Result<(), CliError> {
    let started = Instant::now();
    let mut document = Document::open(path)?;
    let summary = document.summary()?;
    let elapsed = started.elapsed();

    let mut out = std::io::stdout().lock();
    let show = |value: Option<usize>| value.map_or_else(|| "-".to_string(), |value| value.to_string());
    write_line(&mut out, &format!("file\t{}", path.display()))?;
    write_line(&mut out, &format!("size_bytes\t{}", summary.file_size))?;
    write_line(&mut out, &format!("size_human\t{}", human_size(summary.file_size)))?;
    write_line(&mut out, &format!("pdf_version\t{}", summary.pdf_version))?;
    write_line(&mut out, &format!("encrypted\t{}", if summary.encrypted { "yes" } else { "no" }))?;
    write_line(&mut out, &format!("pages\t{}", show(summary.pages)))?;
    write_line(&mut out, &format!("bookmarks\t{}", show(summary.bookmarks)))?;
    write_line(&mut out, &format!("read_ms\t{}", elapsed.as_millis()))?;
    if summary.encrypted {
        write_line(&mut out, "note\tfile protetto da password: le pagine ed i segnalibri ne hanno bisogno")?;
    }
    Ok(())
}

fn pages(path: &Path) -> Result<(), CliError> {
    let mut document = Document::open(path)?;
    println!("{}", document.page_count()?);
    Ok(())
}

fn bookmarks(path: &Path) -> Result<(), CliError> {
    let mut document = Document::open(path)?;
    let mut out = std::io::stdout().lock();
    for bookmark in document.bookmarks()? {
        let page = bookmark.page.map_or_else(String::new, |page| page.to_string());
        write_line(&mut out, &format!("{}\t{page}\t{}", bookmark.level, bookmark.title))?;
    }
    Ok(())
}

/// Which pages hold the text, and which pages have no text to hold it.
fn search(path: &Path, options: &[String]) -> Result<(), CliError> {
    let mut text: Option<String> = None;
    let mut case_sensitive = false;
    let mut regex = false;
    let mut index = 0_usize;
    while let Some(argument) = options.get(index) {
        match argument.as_str() {
            "--case-sensitive" | "-c" => {
                case_sensitive = true;
                index += 1;
            }
            "--regex" | "-r" => {
                regex = true;
                index += 1;
            }
            other if other.starts_with('-') => return Err(format!("opzione sconosciuta «{other}» (vedi `pdfsplit --help`)").into()),
            other => {
                text = Some(other.to_string());
                index += 1;
            }
        }
    }
    let Some(text) = text else { return Err("search: manca il testo da cercare".to_string().into()) };
    let mut document = Document::open(path)?;
    let query = if regex { Query::pattern(text.clone()) } else { Query::literal(text.clone()) };
    let query = if case_sensitive { query } else { query.ignoring_case() };
    let found = document.search(&query)?;

    let mut out = std::io::stdout().lock();
    write_line(&mut out, &format!("file\t{}", path.display()))?;
    write_line(&mut out, &format!("query\t{text}"))?;
    write_line(&mut out, &format!("regex\t{}", if regex { "yes" } else { "no" }))?;
    write_line(&mut out, &format!("case_sensitive\t{}", if case_sensitive { "yes" } else { "no" }))?;
    write_line(&mut out, &format!("scanned\t{}", found.scanned))?;
    write_line(&mut out, &format!("matches\t{}", found.matches()))?;
    for page in &found.pages {
        write_line(&mut out, &format!("page\t{page}"))?;
    }
    write_line(&mut out, &format!("without_text\t{}", found.without_text.len()))?;
    for page in found.without_text.iter().take(20) {
        write_line(&mut out, &format!("no_text_page\t{page}"))?;
    }
    write_line(&mut out, &format!("elapsed_ms\t{}", found.elapsed.as_millis()))?;
    Ok(())
}

/// What one page says, and why it may say nothing.
fn text(path: &Path, options: &[String]) -> Result<(), CliError> {
    let mut page_number: Option<usize> = None;
    let mut index = 0_usize;
    while let Some(argument) = options.get(index) {
        match argument.as_str() {
            "--page" | "-p" => {
                let value = options.get(index + 1).ok_or_else(|| "--page richiede un numero di pagina".to_string())?;
                page_number = Some(value.parse::<usize>().map_err(|_| format!("«{value}» non è un numero di pagina"))?);
                index += 2;
            }
            other => return Err(format!("opzione sconosciuta «{other}» (vedi `pdfsplit --help`)").into()),
        }
    }
    let Some(page_number) = page_number else { return Err("text: manca --page".to_string().into()) };
    let mut document = Document::open(path)?;
    let page = document.page_text(page_number)?;

    let mut out = std::io::stdout().lock();
    write_line(&mut out, &format!("page\t{page_number}"))?;
    write_line(&mut out, &format!("characters\t{}", page.text.chars().count()))?;
    write_line(&mut out, &format!("text\t{}", page.text))?;
    if page.unmapped {
        write_line(&mut out, "note\tla pagina scrive caratteri senza font inclusi: si tratta di un font ridotto senza /ToUnicode")?;
    }
    if page.truncated {
        write_line(&mut out, "note\tla pagina contiene più contenuto di quanto una pagina di solo testo ne possa contenere")?;
    }
    if page.is_empty() {
        write_line(&mut out, "note\tla pagina è senza testo: è un'immagine, oppure non ha contenuto")?;
    }
    Ok(())
}

/// Cut the file into parts and report every one of them.
fn split(path: &Path, options: &[String]) -> Result<(), CliError> {
    let flags = SplitFlags::parse(options)?;
    // The same setup the window fills: one pipeline for both, or the command the window prints
    // would not be the job the window does.
    let setup = flags.setup(path)?;
    let report = setup.run(&Job::silent())?;

    let width = report.parts.len().to_string().len().max(2);
    let mut out = std::io::stdout().lock();
    write_line(&mut out, &format!("file\t{}", path.display()))?;
    write_line(&mut out, &format!("size_bytes\t{}", report.source_size))?;
    write_line(&mut out, &format!("pages\t{}", report.pages))?;
    if let Some(found) = &report.found {
        write_line(&mut out, &format!("matching_pages\t{}", found.matches()))?;
        write_line(&mut out, &format!("pages_without_text\t{}", found.without_text.len()))?;
    }
    if let Some(level) = flags.bookmarks {
        write_line(&mut out, &format!("bookmark_level\t{level}"))?;
    }
    write_line(&mut out, &format!("compression\t{}", setup.compression))?;
    write_line(&mut out, &format!("pdf_version\t{}", report.version))?;
    write_line(&mut out, &format!("parts\t{}", report.parts.len()))?;
    for (index, part) in report.parts.iter().enumerate() {
        write_line(
            &mut out,
            &format!("part\t{:0width$}\t{}\t{}\t{}\t{}\t{}", index + 1, part.label, part.pages, part.bookmarks, part.bytes, part.path.display()),
        )?;
    }
    write_line(&mut out, &format!("written_bytes\t{}", report.written_bytes()))?;
    write_line(&mut out, &format!("written_human\t{}", human_size(report.written_bytes())))?;
    write_line(&mut out, &format!("elapsed_ms\t{}", report.elapsed.as_millis()))?;
    for note in &report.warnings {
        write_line(&mut out, &format!("note\t{note}"))?;
    }
    if report.resources_were_duplicated() {
        let extra = report.written_bytes().saturating_sub(report.source_size);
        write_line(
            &mut out,
            &format!(
                "note\tle parti in pdf tagliate pesano in totale {} più del sorgente: il più delle volte sono font e immagini copiati in ogni file che ne ha bisogno",
                human_size(extra)
            ),
        )?;
    }
    Ok(())
}

/// What the `split` command was asked for.
#[derive(Default)]
struct SplitFlags {
    parts: Option<usize>,
    pages: Option<String>,
    string: Option<String>,
    bookmarks: Option<usize>,
    out: Option<PathBuf>,
    overwrite: bool,
    no_bookmarks: bool,
    leftover: bool,
    case_sensitive: bool,
    regex: bool,
    close_after: bool,
    name_label: Option<String>,
    name_pattern: Option<String>,
    name_from_title: bool,
    always_number: bool,
    prefix: String,
    suffix: String,
    width: Option<usize>,
    start: Option<usize>,
    number_first: bool,
    compress: bool,
    neutral: bool,
    pdf_version: Option<Version>,
}

impl SplitFlags {
    fn parse(args: &[String]) -> Result<Self, CliError> {
        let mut flags = Self::default();
        let mut index = 0_usize;
        while let Some(argument) = args.get(index) {
            match argument.as_str() {
                "--parts" | "-n" => {
                    let value = args.get(index + 1).ok_or_else(|| "--parts richiede il numero delle parti".to_string())?;
                    flags.parts = Some(value.parse::<usize>().map_err(|_| format!("«{value}» non è un numero di parti"))?);
                    index += 2;
                }
                "--pages" | "-p" => {
                    let value = args.get(index + 1).ok_or_else(|| "--pages richiede un elenco di pagine, per esempio \"1-10, 11\"".to_string())?;
                    flags.pages = Some(value.clone());
                    index += 2;
                }
                "--string" | "-s" => {
                    let value = args.get(index + 1).ok_or_else(|| "--string richiede il testo da cercare".to_string())?;
                    flags.string = Some(value.clone());
                    index += 2;
                }
                "--bookmarks" | "-b" => {
                    let value = args.get(index + 1).ok_or_else(|| "--bookmarks richiede un livello".to_string())?;
                    flags.bookmarks = Some(value.parse::<usize>().map_err(|_| format!("«{value}» non è un livello di segnalibro"))?);
                    index += 2;
                }
                "--out" | "-o" => {
                    let value = args.get(index + 1).ok_or_else(|| "--out richiede una cartella".to_string())?;
                    flags.out = Some(PathBuf::from(value));
                    index += 2;
                }
                "--name-label" => {
                    let value = args.get(index + 1).ok_or_else(|| "--name-label richiede l'etichetta da cercare".to_string())?;
                    flags.name_label = Some(value.clone());
                    index += 2;
                }
                "--name-pattern" => {
                    let value = args.get(index + 1).ok_or_else(|| "--name-pattern richiede un'espressione regolare".to_string())?;
                    flags.name_pattern = Some(value.clone());
                    index += 2;
                }
                "--overwrite" | "-y" => {
                    flags.overwrite = true;
                    index += 1;
                }
                "--no-bookmarks" => {
                    flags.no_bookmarks = true;
                    index += 1;
                }
                "--leftover" => {
                    flags.leftover = true;
                    index += 1;
                }
                "--case-sensitive" | "-c" => {
                    flags.case_sensitive = true;
                    index += 1;
                }
                "--regex" | "-r" => {
                    flags.regex = true;
                    index += 1;
                }
                "--close-after" => {
                    flags.close_after = true;
                    index += 1;
                }
                "--name-from-title" => {
                    flags.name_from_title = true;
                    index += 1;
                }
                "--always-number" => {
                    flags.always_number = true;
                    index += 1;
                }
                "--prefix" => {
                    flags.prefix = args.get(index + 1).ok_or_else(|| "--prefix richiede il testo da scrivere".to_string())?.clone();
                    index += 2;
                }
                "--suffix" => {
                    flags.suffix = args.get(index + 1).ok_or_else(|| "--suffix richiede il testo da scrivere".to_string())?.clone();
                    index += 2;
                }
                "--width" => {
                    let value = args.get(index + 1).ok_or_else(|| "--width richiede un numero di cifre".to_string())?;
                    let width = value.parse::<usize>().map_err(|_| format!("«{value}» non è un numero di cifre"))?;
                    if width > 9 {
                        return Err("una sequenza non può avere più di nove cifre".to_string().into());
                    }
                    flags.width = Some(width);
                    index += 2;
                }
                "--start" => {
                    let value = args.get(index + 1).ok_or_else(|| "--start richiede il numero da cui parte la prima parte".to_string())?;
                    flags.start = Some(value.parse::<usize>().map_err(|_| format!("«{value}» non è un numero"))?);
                    index += 2;
                }
                "--number-first" => {
                    flags.number_first = true;
                    index += 1;
                }
                "--compress" => {
                    flags.compress = true;
                    index += 1;
                }
                "--neutral" => {
                    flags.neutral = true;
                    index += 1;
                }
                "--pdf-version" => {
                    let value = args.get(index + 1).ok_or_else(|| "--pdf-version richiede una versione, per esempio 1.7".to_string())?;
                    let Some(version) = Version::parse(value) else {
                        return Err(format!("«{value}» non è una versione PDF (prova 1.5, 1.6, 1.7 o 2.0)").into());
                    };
                    if !Version::OFFERED.contains(&version) {
                        let offered: Vec<String> = Version::OFFERED.iter().map(ToString::to_string).collect();
                        return Err(format!(
                            "le parti possono essere scritte nelle seguenti versioni di PDF: {}; «{value}» è più vecchia di quella che il programma produce",
                            offered.join(", ")
                        )
                        .into());
                    }
                    flags.pdf_version = Some(version);
                    index += 2;
                }
                other => return Err(format!("opzione sconosciuta «{other}» (vedi `pdfsplit --help`)").into()),
            }
        }
        Ok(flags)
    }

    /// The cut these flags ask for, or why they cannot ask for it.
    fn setup(&self, path: &Path) -> Result<Setup, CliError> {
        let mut ways = 0;
        for chosen in [self.parts.is_some(), self.pages.is_some(), self.string.is_some(), self.bookmarks.is_some()] {
            ways += usize::from(chosen);
        }
        if ways > 1 {
            return Err("scegli un solo modo di tagliare: --parts, --pages, --string o --bookmarks".to_string().into());
        }
        if self.close_after && self.string.is_none() {
            return Err("--close-after va usato con --string".to_string().into());
        }
        if self.regex && self.string.is_none() {
            return Err("--regex va usato con --string".to_string().into());
        }
        if self.compress && self.neutral {
            return Err("scegli un solo modo di scrivere i flussi: --compress o --neutral".to_string().into());
        }
        if self.name_from_title && self.name_label.is_some() {
            return Err("scegli un solo modo di nominare le parti: --name-label o --name-from-title".to_string().into());
        }
        let mode = if let Some(text) = &self.string {
            Mode::Text { text: text.clone(), case_sensitive: self.case_sensitive, regex: self.regex, close_after: self.close_after }
        } else if let Some(level) = self.bookmarks {
            Mode::Bookmarks { level, by_title: self.name_from_title }
        } else if let Some(list) = &self.pages {
            Mode::Pages { list: list.clone(), leftover: self.leftover }
        } else {
            Mode::Parts(self.parts.unwrap_or(2))
        };
        let naming = match &self.name_label {
            Some(label) => Naming::Text { label: label.clone(), pattern: self.name_pattern.clone().unwrap_or_default() },
            None => Naming::Sequence,
        };
        Ok(Setup {
            input: path.to_path_buf(),
            out_dir: self.out.clone(),
            mode,
            naming,
            keep_bookmarks: !self.no_bookmarks,
            overwrite: self.overwrite,
            template: NameTemplate {
                prefix: self.prefix.clone(),
                suffix: self.suffix.clone(),
                sequence: if self.number_first { SequencePosition::Start } else { SequencePosition::End },
                width: self.width.unwrap_or(0),
                start: self.start.unwrap_or(1),
                always: self.always_number,
            },
            compression: if self.neutral { Compression::Neutral } else { Compression::Compress },
            version: self.pdf_version,
        })
    }
}

fn write_line(out: &mut impl std::io::Write, line: &str) -> Result<(), CliError> {
    writeln!(out, "{line}")?;
    Ok(())
}

/// A size a person reads, with the byte count beside it for machine use.
fn human_size(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10), ("B", 1)];
    for (unit, size) in UNITS {
        if bytes >= size {
            let value = bytes as f64 / size as f64;
            return if unit == "B" { format!("{bytes} B") } else { format!("{value:.1} {unit}") };
        }
    }
    "0 B".to_string()
}
