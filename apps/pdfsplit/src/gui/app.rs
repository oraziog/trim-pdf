//! The window: what the user picks, what it does with it, and what it says while it happens.
//!
//! The window holds no PDF. It fills a [`work::Setup`], hands it to a thread and draws what that
//! thread reports. [`App::ui`] takes an [`egui::Context`] and nothing else, so a test can run the
//! whole window frame by frame without a screen.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Instant;

use eframe::egui;
use splitter::{Canceller, Compression, NameTemplate, SequencePosition, SplitReport, Summary, Unit, Update, Version, sanitize};

use super::meter::Meter;
use super::work::{self, Event, Mode, Naming, Setup};
use super::{human_duration, human_size, stage_label};

/// The window's palette: an orange title, yellow headings, a green button that starts the work.
const ORANGE: egui::Color32 = egui::Color32::from_rgb(230, 126, 34);
const YELLOW: egui::Color32 = egui::Color32::from_rgb(241, 196, 15);
const GREEN: egui::Color32 = egui::Color32::from_rgb(46, 160, 67);

/// A heading of the window: upper case, bold and yellow.
fn heading(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text.to_uppercase()).strong().color(YELLOW));
}

/// A number field with the two buttons a `DragValue` does not have: a drag is fine for a jump,
/// not for a single step.
fn stepped(ui: &mut egui::Ui, value: &mut usize, range: std::ops::RangeInclusive<usize>) -> bool {
    let low = *range.start();
    let high = *range.end();
    let mut changed = ui.add(egui::DragValue::new(value).range(range).speed(1.0)).changed();
    if triangle(ui, true, *value < high) {
        *value = (*value + 1).min(high);
        changed = true;
    }
    if triangle(ui, false, *value > low) {
        *value = value.saturating_sub(1).max(low);
        changed = true;
    }
    changed
}

/// A small button whose face is a triangle: no font the window carries has that glyph, so the
/// shape is drawn rather than written.
fn triangle(ui: &mut egui::Ui, up: bool, enabled: bool) -> bool {
    let sense = if enabled { egui::Sense::click() } else { egui::Sense::hover() };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), sense);
    let response = response.on_hover_text(if up { "uno in più" } else { "uno in meno" });
    let visuals = *ui.style().interact(&response);
    ui.painter().rect_filled(rect, 3.0, visuals.weak_bg_fill);
    ui.painter().rect_stroke(rect, 3.0, visuals.bg_stroke, egui::StrokeKind::Inside);
    let center = rect.center();
    let (half, rise) = (4.0, 3.0);
    let points = if up {
        vec![center + egui::vec2(0.0, -rise), center + egui::vec2(half, rise), center + egui::vec2(-half, rise)]
    } else {
        vec![center + egui::vec2(0.0, rise), center + egui::vec2(half, -rise), center + egui::vec2(-half, -rise)]
    };
    ui.painter().add(egui::Shape::convex_polygon(points, visuals.fg_stroke.color, egui::Stroke::NONE));
    enabled && response.clicked()
}

/// The ways of cutting the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// N parts as equal as possible.
    Parts,
    /// One part per entry of a page list.
    Pages,
    /// A new part at every page holding the text.
    Text,
    /// A new part at every bookmark of one level.
    Bookmarks,
}

/// The window and everything it has been told.
pub struct App {
    input: String,
    out_dir: String,
    choice: Choice,
    parts_count: usize,
    page_list: String,
    leftover: bool,
    text: String,
    case_sensitive: bool,
    regex: bool,
    close_after: bool,
    level: usize,
    by_title: bool,
    naming_from_text: bool,
    label: String,
    pattern: String,
    prefix: String,
    suffix: String,
    number_first: bool,
    width: usize,
    start: usize,
    always_number: bool,
    compress: bool,
    /// The version to write; `None` keeps the source's own version.
    pdf_version: Option<Version>,
    keep_bookmarks: bool,
    overwrite: bool,
    /// Whether the window is drawn dark. Dark is what it opens as, and the toggle in the corner
    /// is the only thing that changes it.
    dark: bool,

    info: Option<Summary>,
    info_problem: Option<String>,
    info_rx: Option<Receiver<Event>>,

    job_rx: Option<Receiver<Event>>,
    cancel: Option<Canceller>,
    meter: Option<Meter>,
    update: Option<Update>,
    report: Option<SplitReport>,
    failure: Option<String>,
    cancelled: bool,
    started: Option<Instant>,
    /// What became of the last "Crea batch".
    batch_note: Option<String>,
}

impl Default for App {
    fn default() -> Self {
        Self {
            input: String::new(),
            out_dir: String::new(),
            choice: Choice::Parts,
            parts_count: 2,
            page_list: String::new(),
            // The benchmark chose the defaults: the leftover pages always have a home, and the
            // sequence is always there, so two parts can never be mistaken for one another.
            leftover: true,
            text: String::new(),
            case_sensitive: false,
            regex: false,
            close_after: false,
            level: 1,
            by_title: false,
            naming_from_text: false,
            label: String::new(),
            pattern: String::new(),
            prefix: String::new(),
            suffix: String::new(),
            number_first: false,
            width: 0,
            start: 1,
            always_number: true,
            // Compression at 1.5 is what the benchmark measured as the best trade: 7.5 % smaller
            // than a neutral split, at about a quarter more time, and no newer version pays more.
            // The version itself costs nothing, so a part keeps the one of the file it came from.
            compress: true,
            pdf_version: None,
            keep_bookmarks: true,
            overwrite: false,
            dark: true,
            info: None,
            info_problem: None,
            info_rx: None,
            job_rx: None,
            cancel: None,
            meter: None,
            update: None,
            report: None,
            failure: None,
            cancelled: false,
            started: None,
            batch_note: None,
        }
    }
}

impl App {
    /// The window as the application opens it.
    #[must_use]
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        Self::default()
    }

    /// True while a job is running.
    #[must_use]
    pub fn running(&self) -> bool {
        self.job_rx.is_some()
    }

    /// Take a file the user picked or dropped, and read what it is.
    pub fn choose(&mut self, path: &Path) {
        // Whether the folder still follows the input is asked before the new file takes its place:
        // comparing afterwards would always be against the file just chosen, and a folder picked
        // for one document would follow another.
        let follows_input = self.out_dir.trim().is_empty() || self.out_dir_matches(&self.input);
        self.input = path.display().to_string();
        if follows_input {
            // The parts go beside the input, as the specification asks.
            self.out_dir = path.parent().map_or_else(String::new, |parent| parent.display().to_string());
        }
        self.clear_results();
        self.info = None;
        self.info_problem = None;
        let (sender, receiver) = channel();
        self.info_rx = Some(receiver);
        let path = path.to_path_buf();
        std::thread::spawn(move || work::read_info(path, sender));
    }

    /// True when the output folder is still the one the input suggested.
    fn out_dir_matches(&self, input: &str) -> bool {
        Path::new(input).parent().is_some_and(|parent| parent.display().to_string() == self.out_dir)
    }

    /// Everything the window has filled in, ready for a thread.
    fn setup(&self) -> Result<Setup, String> {
        let input = PathBuf::from(self.input.trim());
        if input.as_os_str().is_empty() {
            return Err("scegli il file da tagliare".to_string());
        }
        if !input.is_file() {
            return Err(format!("{} non c'è", input.display()));
        }
        if self.info.as_ref().is_some_and(|info| info.encrypted) {
            return Err("il file è protetto da una password: la decrittazione non è supportata".to_string());
        }
        let setup = Setup {
            input,
            out_dir: (!self.out_dir.trim().is_empty()).then(|| PathBuf::from(self.out_dir.trim())),
            mode: self.mode(),
            naming: if self.naming_from_text { Naming::Text { label: self.label.clone(), pattern: self.pattern.clone() } } else { Naming::Sequence },
            keep_bookmarks: self.keep_bookmarks,
            overwrite: self.overwrite,
            template: self.template(),
            compression: if self.compress { Compression::Compress } else { Compression::Neutral },
            version: self.pdf_version,
        };
        match setup.problem() {
            Some(message) => Err(message),
            None => Ok(setup),
        }
    }

    /// How the parts are named, as the fields say.
    fn template(&self) -> NameTemplate {
        NameTemplate {
            prefix: self.prefix.clone(),
            suffix: self.suffix.clone(),
            sequence: if self.number_first { SequencePosition::Start } else { SequencePosition::End },
            width: self.width,
            start: self.start,
            always: self.always_number,
        }
    }

    /// The names the fields would give the first and the last part, so the effect of a prefix is
    /// seen before the cut.
    fn name_previews(&self) -> (String, String) {
        let typed = self.input.trim();
        let base = Path::new(typed)
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .filter(|stem| !stem.is_empty())
            .unwrap_or_else(|| "documento".to_string());
        let parts = match self.choice {
            Choice::Parts => self.parts_count.max(1),
            _ => 2,
        };
        let template = self.template();
        let name = |index: usize| format!("{}.pdf", sanitize(&template.numbered(&base, index, parts)).unwrap_or_else(|| base.clone()));
        let first = name(0);
        let last = if parts > 1 { name(parts - 1) } else { first.clone() };
        (first, last)
    }

    /// The preview as one line: one name when there is one part, two when there are more.
    fn name_preview_line(&self) -> String {
        let (first, last) = self.name_previews();
        if first == last { format!("esempio: {first}") } else { format!("esempio: primo {first} · ultimo {last}") }
    }

    /// The version the chosen file declares, as the version combo names it; `?` while the file's
    /// facts are still being read.
    fn source_version(&self) -> String {
        self.info.as_ref().and_then(|info| Version::of_source(&info.pdf_version)).map_or_else(|| "?".to_string(), |version| version.to_string())
    }

    /// The way of cutting the window is set to.
    fn mode(&self) -> Mode {
        match self.choice {
            Choice::Parts => Mode::Parts(self.parts_count),
            Choice::Pages => Mode::Pages { list: self.page_list.clone(), leftover: self.leftover },
            Choice::Text => {
                Mode::Text { text: self.text.clone(), case_sensitive: self.case_sensitive, regex: self.regex, close_after: self.close_after }
            }
            Choice::Bookmarks => Mode::Bookmarks { level: self.level, by_title: self.by_title },
        }
    }

    /// Start the cut the window describes. A setup that cannot run says why instead.
    pub fn start(&mut self) {
        if self.running() {
            return;
        }
        let setup = match self.setup() {
            Ok(setup) => setup,
            Err(message) => {
                self.clear_results();
                self.failure = Some(message);
                return;
            }
        };
        self.clear_results();
        let stop = Canceller::default();
        self.cancel = Some(stop.clone());
        self.meter = Some(Meter::new(&setup.stages(), Instant::now()));
        self.started = Some(Instant::now());
        let (sender, receiver) = channel();
        self.job_rx = Some(receiver);
        std::thread::spawn(move || work::run(setup, sender, stop));
    }

    /// Ask the running job to stop. Its temporary files are removed before it reports back.
    pub fn cancel_job(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
    }

    /// Forget the last job's outcome, keeping what the window describes.
    fn clear_results(&mut self) {
        self.report = None;
        self.failure = None;
        self.cancelled = false;
        self.meter = None;
        self.update = None;
        self.started = None;
    }

    /// Drain what the threads have said.
    ///
    /// A thread that ends without a word — a panic, an abort — closes its channel instead of
    /// sending a last event. That has to count as the end of the job: a channel nobody reads to
    /// the end leaves the window saying "in corso" forever, with the button dead.
    pub fn poll(&mut self) {
        if let Some(receiver) = &self.job_rx {
            let (events, gone) = take(receiver);
            for event in events {
                self.handle(event);
            }
            if gone && self.running() {
                self.failure = Some("lavoro interrotto: il thread del taglio è finito senza dire come".to_string());
                self.finish();
            }
        }
        if let Some(receiver) = &self.info_rx {
            let (events, gone) = take(receiver);
            for event in events {
                self.handle(event);
            }
            if gone && self.info_rx.is_some() {
                self.info = None;
                self.info_problem = Some("non ho potuto leggere il file: la lettura è stata interrotta".to_string());
                self.info_rx = None;
            }
        }
    }

    /// Take one thing a thread said.
    fn handle(&mut self, event: Event) {
        match event {
            Event::Info(Ok(summary)) => {
                self.info = Some(summary);
                self.info_problem = None;
                self.info_rx = None;
            }
            Event::Info(Err(message)) => {
                self.info = None;
                self.info_problem = Some(message);
                self.info_rx = None;
            }
            Event::Progress(update) => {
                if let Some(meter) = &mut self.meter {
                    meter.feed(update, Instant::now());
                }
                self.update = Some(update);
            }
            Event::Done(report) => {
                self.report = Some(*report);
                self.finish();
            }
            Event::Failed(message) => {
                self.failure = Some(message);
                self.finish();
            }
            Event::Cancelled => {
                self.cancelled = true;
                self.finish();
            }
        }
    }

    /// The job is over, however it ended.
    fn finish(&mut self) {
        self.job_rx = None;
        self.cancel = None;
        self.update = None;
    }

    /// What happens once before each frame: what the threads said, and a file dropped on the
    /// window. A frame that only draws can skip it.
    pub fn before_frame(&mut self, ctx: &egui::Context) {
        self.poll();
        self.take_dropped(ctx);
    }

    /// The window's content, drawn into `ui`.
    pub fn contents(&mut self, ui: &mut egui::Ui) {
        let running = self.running();
        // The theme is the window's own, not the system's: it opens dark and only the toggle in the
        // corner changes it, so a machine set to light does not get a light window by surprise.
        ui.ctx().set_theme(if self.dark { egui::ThemePreference::Dark } else { egui::ThemePreference::Light });
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Taglia un PDF").size(26.0).strong().color(ORANGE));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.toggle_value(&mut self.dark, "Tema").on_hover_text("scuro quando è acceso, chiaro quando è spento");
                });
            });
            ui.add_space(4.0);
            self.file_section(ui, running);
            ui.separator();
            self.mode_section(ui, running);
            ui.separator();
            self.naming_section(ui, running);
            ui.separator();
            self.format_section(ui, running);
            ui.separator();
            self.output_section(ui, running);
            ui.add_space(8.0);
            self.actions(ui, running);
            self.command_section(ui, running);
            self.progress_section(ui);
            self.result_section(ui);
            ui.add_space(12.0);
            ui.label(egui::RichText::new(format!("pdfsplit {}", env!("CARGO_PKG_VERSION"))).small().weak());
        });
    }

    /// A file dropped on the window is the file chosen.
    fn take_dropped(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|input| input.raw.dropped_files.first().map(|file| file.path().to_path_buf()));
        if let Some(path) = dropped
            && !self.running()
        {
            self.choose(&path);
        }
    }

    fn file_section(&mut self, ui: &mut egui::Ui, running: bool) {
        ui.horizontal(|ui| {
            ui.label("File PDF:");
            let field = ui.add_enabled(
                !running,
                egui::TextEdit::singleline(&mut self.input).hint_text("trascina qui un PDF, o scegli il file").desired_width(420.0),
            );
            if field.lost_focus() && !self.input.trim().is_empty() {
                let path = PathBuf::from(self.input.trim());
                if path.is_file() {
                    self.choose(&path);
                }
            }
            if ui.add_enabled(!running, egui::Button::new("Scegli…")).clicked()
                && let Some(path) = rfd::FileDialog::new().add_filter("PDF", &["pdf"]).pick_file()
            {
                self.choose(&path);
            }
        });
        if let Some(info) = &self.info {
            let pages = info.pages.map_or_else(|| "?".to_string(), |pages| pages.to_string());
            let bookmarks = info.bookmarks.map_or_else(|| "?".to_string(), |bookmarks| bookmarks.to_string());
            ui.label(format!("{} pagine · {} segnalibri · PDF {} · {}", pages, bookmarks, info.pdf_version, human_size(info.file_size)));
            if info.encrypted {
                ui.colored_label(egui::Color32::from_rgb(190, 60, 60), "protetto da password: non supportato");
            }
        }
        if let Some(problem) = &self.info_problem {
            ui.colored_label(egui::Color32::from_rgb(190, 60, 60), problem.clone());
        }
    }

    fn mode_section(&mut self, ui: &mut egui::Ui, running: bool) {
        heading(ui, "Come tagliare");
        ui.add_enabled_ui(!running, |ui| {
            ui.radio_value(&mut self.choice, Choice::Parts, "N parti uguali");
            ui.horizontal(|ui| {
                ui.add_space(20.0);
                if stepped(ui, &mut self.parts_count, 1..=10_000) {
                    self.choice = Choice::Parts;
                }
                ui.label("parti");
            });
            ui.radio_value(&mut self.choice, Choice::Pages, "Elenco pagine");
            ui.horizontal(|ui| {
                ui.add_space(20.0);
                ui.add(egui::TextEdit::singleline(&mut self.page_list).hint_text("1-10, 11, 13-20").desired_width(260.0));
                ui.checkbox(&mut self.leftover, "pagine residue in un ultimo file");
            });
            ui.radio_value(&mut self.choice, Choice::Text, "Stringa di taglio");
            ui.horizontal(|ui| {
                ui.add_space(20.0);
                ui.add(egui::TextEdit::singleline(&mut self.text).hint_text("testo da cercare").desired_width(260.0));
            });
            ui.horizontal(|ui| {
                ui.add_space(20.0);
                ui.checkbox(&mut self.case_sensitive, "maiuscole/minuscole");
                ui.checkbox(&mut self.regex, "espressione regolare");
                ui.checkbox(&mut self.close_after, "chiudi dopo la pagina trovata");
            });
            ui.radio_value(&mut self.choice, Choice::Bookmarks, "Segnalibri");
            ui.horizontal(|ui| {
                ui.add_space(20.0);
                ui.label("livello");
                stepped(ui, &mut self.level, 1..=20);
                ui.checkbox(&mut self.by_title, "nome dal titolo del segnalibro");
            });
        });
    }

    fn naming_section(&mut self, ui: &mut egui::Ui, running: bool) {
        heading(ui, "Nome delle parti");
        ui.add_enabled_ui(!running, |ui| {
            ui.checkbox(&mut self.naming_from_text, "nome dal testo delle pagine");
            ui.horizontal(|ui| {
                ui.add_space(20.0);
                ui.label("etichetta");
                ui.add_enabled(self.naming_from_text, egui::TextEdit::singleline(&mut self.label).hint_text("Codice Fiscale").desired_width(180.0));
                ui.label("espressione (facoltativa)");
                ui.add_enabled(
                    self.naming_from_text,
                    egui::TextEdit::singleline(&mut self.pattern).hint_text(r"Fattura n\.\s*(\S+)").desired_width(180.0),
                );
            });
            ui.checkbox(&mut self.always_number, "aggiungi sempre la sequenza");
            ui.horizontal(|ui| {
                ui.label("prefisso");
                ui.add(egui::TextEdit::singleline(&mut self.prefix).hint_text("niente").desired_width(110.0));
                ui.label("suffisso");
                ui.add(egui::TextEdit::singleline(&mut self.suffix).hint_text("niente").desired_width(110.0));
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.number_first, "numero prima del nome");
                ui.label("cifre").on_hover_text("0 segue il numero delle parti, mai meno di due cifre");
                stepped(ui, &mut self.width, 0..=9);
                ui.label("inizia da");
                ui.add(egui::DragValue::new(&mut self.start).range(0..=9_999).speed(1.0));
            });
            ui.label(egui::RichText::new(self.name_preview_line()).weak());
        });
    }

    /// How the parts are written: compressed or copied, and in which PDF version.
    fn format_section(&mut self, ui: &mut egui::Ui, running: bool) {
        heading(ui, "Come scrivere i file");
        ui.add_enabled_ui(!running, |ui| {
            ui.checkbox(&mut self.compress, "comprimi i flussi e impacchetta gli oggetti").on_hover_text(
                "comprime solo i flussi che non lo sono già: un'immagine non viene mai toccata.\nTrascinare gli stream così come sono è più veloce.",
            );
            ui.horizontal(|ui| {
                ui.label("versione PDF");
                let source = self.source_version();
                let chosen = self.pdf_version.map_or_else(|| format!("come il sorgente ({source})"), |version| version.to_string());
                egui::ComboBox::from_id_salt("pdf-version")
                    .selected_text(chosen)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.pdf_version, None, format!("come il sorgente ({source})"));
                        for version in Version::OFFERED {
                            ui.selectable_value(&mut self.pdf_version, Some(version), version.to_string());
                        }
                    })
                    .response
                    .on_hover_text(
                        "come il sorgente tiene le parti nella stessa generazione del file da cui \
                         vengono: la versione non cambia il peso, quindi non c'è motivo di alzarla.\n\nSe il \
                         sorgente è più vecchio della 1.5 le parti vanno alla 1.5, che è la prima a portare \
                         l'object stream e la xref stream della compressione.\n\nLa 2.0 non pesa meno e i \
                         lettori vecchi la conoscono peggio.",
                    );
            });
            ui.label(egui::RichText::new("una parte le cui pagine chiedono di più viene scritta più nuova, e la finestra lo dice").weak());
        });
    }

    fn output_section(&mut self, ui: &mut egui::Ui, running: bool) {
        heading(ui, "Cartella di uscita");
        ui.add_enabled_ui(!running, |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.out_dir).hint_text("accanto al file").desired_width(420.0));
                if ui.button("Sfoglia…").clicked()
                    && let Some(folder) = rfd::FileDialog::new().pick_folder()
                {
                    self.out_dir = folder.display().to_string();
                }
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.keep_bookmarks, "mantieni i segnalibri nelle parti");
                ui.checkbox(&mut self.overwrite, "sovrascrivi i file esistenti");
            });
        });
    }

    fn actions(&mut self, ui: &mut egui::Ui, running: bool) {
        ui.horizontal(|ui| {
            let start =
                egui::Button::new(egui::RichText::new("AVVIA").strong().color(egui::Color32::WHITE)).fill(GREEN).min_size(egui::vec2(130.0, 32.0));
            if ui.add_enabled(!running, start).clicked() {
                self.start();
            }
            if ui.add_enabled(running, egui::Button::new("Annulla")).clicked() {
                self.cancel_job();
            }
        });
    }

    /// The command the button would run, as a shell would take it, and a `.cmd` that runs it
    /// again tomorrow without the window.
    fn command_section(&mut self, ui: &mut egui::Ui, running: bool) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("comando equivalente").weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(!running, egui::Button::new("Crea batch")).clicked() {
                    self.create_batch();
                }
            });
        });
        ui.add(egui::Label::new(egui::RichText::new(self.command_line()).monospace()).wrap());
        if let Some(note) = &self.batch_note {
            ui.label(egui::RichText::new(note.clone()).weak());
        }
    }

    /// The command line the form describes: the command-line program and every argument.
    fn command_line(&self) -> String {
        let mut parts = vec![quote(&self.program())];
        parts.extend(self.cli_arguments().iter().map(|argument| quote(argument)));
        parts.join(" ")
    }

    /// The command-line program as the line names it: the one beside the window, else the one a
    /// shell finds in `PATH`.
    fn program(&self) -> String {
        cli_program().unwrap_or_else(|| "pdfsplit".to_string())
    }

    /// What the command line would be given for the choices on screen.
    fn cli_arguments(&self) -> Vec<String> {
        let mut args = vec!["split".to_string(), self.input.trim().to_string()];
        match self.choice {
            Choice::Parts => {
                args.push("--parts".to_string());
                args.push(self.parts_count.max(1).to_string());
            }
            Choice::Pages => {
                if !self.page_list.trim().is_empty() {
                    args.push("--pages".to_string());
                    args.push(self.page_list.trim().to_string());
                }
                if self.leftover {
                    args.push("--leftover".to_string());
                }
            }
            Choice::Text => {
                if !self.text.is_empty() {
                    args.push("--string".to_string());
                    args.push(self.text.clone());
                }
                if self.case_sensitive {
                    args.push("--case-sensitive".to_string());
                }
                if self.regex {
                    args.push("--regex".to_string());
                }
                if self.close_after {
                    args.push("--close-after".to_string());
                }
            }
            Choice::Bookmarks => {
                args.push("--bookmarks".to_string());
                args.push(self.level.to_string());
            }
        }
        // The same choice the job makes: a bookmark's own title wins over a label taken from the
        // text, so the printed command cannot name the parts differently from the window.
        if self.by_title && self.choice == Choice::Bookmarks {
            args.push("--name-from-title".to_string());
        } else if self.naming_from_text && !self.label.trim().is_empty() {
            args.push("--name-label".to_string());
            args.push(self.label.trim().to_string());
            if !self.pattern.trim().is_empty() {
                args.push("--name-pattern".to_string());
                args.push(self.pattern.trim().to_string());
            }
        }
        if self.always_number {
            args.push("--always-number".to_string());
        }
        if !self.prefix.is_empty() {
            args.push("--prefix".to_string());
            args.push(self.prefix.clone());
        }
        if !self.suffix.is_empty() {
            args.push("--suffix".to_string());
            args.push(self.suffix.clone());
        }
        if self.width > 0 {
            args.push("--width".to_string());
            args.push(self.width.to_string());
        }
        if self.start != 1 {
            args.push("--start".to_string());
            args.push(self.start.to_string());
        }
        if self.number_first {
            args.push("--number-first".to_string());
        }
        if !self.out_dir.trim().is_empty() {
            args.push("--out".to_string());
            args.push(self.out_dir.trim().to_string());
        }
        if self.overwrite {
            args.push("--overwrite".to_string());
        }
        if !self.keep_bookmarks {
            args.push("--no-bookmarks".to_string());
        }
        if self.compress {
            args.push("--compress".to_string());
        }
        if let Some(version) = self.pdf_version {
            args.push("--pdf-version".to_string());
            args.push(version.to_string());
        }
        args
    }

    /// The `.cmd` that repeats this cut: the same program, the same arguments, no window.
    fn batch_text(&self) -> String {
        let mut parts = vec![quote_for_batch(&self.program())];
        parts.extend(self.cli_arguments().iter().map(|argument| quote_for_batch(argument)));
        let mut text = String::from("@echo off\r\nchcp 65001 >nul\r\nrem creato da pdfsplit: rifa senza la finestra lo stesso taglio\r\n");
        if cli_program().is_none() {
            text.push_str("rem pdfsplit.exe non e accanto alla finestra: il comando lo cerca nel PATH\r\n");
        }
        text.push_str(&parts.join(" "));
        text.push_str("\r\nif errorlevel 1 echo Errore: il taglio non e stato completato.\r\n");
        text.push_str("pause\r\n");
        text
    }

    /// Ask where the `.cmd` goes and write it.
    fn create_batch(&mut self) {
        let mut dialog = rfd::FileDialog::new().set_file_name("avvia-pdfsplit.cmd").add_filter("comando", &["cmd", "bat"]);
        let folder = self.out_dir.trim();
        if !folder.is_empty() && Path::new(folder).is_dir() {
            dialog = dialog.set_directory(folder);
        }
        let Some(path) = dialog.save_file() else { return };
        self.batch_note = Some(match std::fs::write(&path, self.batch_text()) {
            Ok(()) => format!("comando salvato in {}", path.display()),
            Err(error) => format!("non ho potuto scrivere {}: {error}", path.display()),
        });
    }

    fn progress_section(&mut self, ui: &mut egui::Ui) {
        let Some(meter) = &self.meter else { return };
        let now = Instant::now();
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if self.running() {
                ui.add(egui::Spinner::new().size(16.0));
            }
            ui.label(egui::RichText::new(stage_label(meter.stage())).strong());
        });
        ui.add(egui::ProgressBar::new(meter.overall()).show_percentage().desired_width(520.0));
        if let Some(update) = self.update {
            ui.label(detail(update));
        }
        ui.label(progress_line(meter, now, self.running()));
    }

    fn result_section(&mut self, ui: &mut egui::Ui) {
        if let Some(message) = &self.failure {
            ui.add_space(10.0);
            ui.colored_label(egui::Color32::from_rgb(190, 60, 60), format!("Non fatto: {message}"));
        }
        if self.cancelled {
            ui.add_space(10.0);
            ui.colored_label(egui::Color32::from_rgb(150, 110, 20), "Annullato: nessun file è stato scritto.");
        }
        let Some(report) = &self.report else { return };
        ui.add_space(10.0);
        ui.separator();
        ui.label(egui::RichText::new(format!("{} parti scritte", report.parts.len())).strong());
        for part in &report.parts {
            ui.horizontal(|ui| {
                ui.label(part.path.file_name().map_or_else(|| part.path.display().to_string(), |name| name.to_string_lossy().into_owned()));
                ui.label(format!("· {} pagine · {}", part.pages, human_size(part.bytes)));
            });
        }
        ui.label(format!("totale {} in {} · PDF {}", human_size(report.written_bytes()), human_duration(report.elapsed), report.version));
        for note in &report.warnings {
            ui.colored_label(egui::Color32::from_rgb(150, 110, 20), note.clone());
        }
    }
}

/// The command-line program beside the window, when it is there.
///
/// Never the window's own executable: a `.cmd` naming it would open the form again and hand it a
/// command line it does not read as one.
fn cli_program() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let folder = exe.parent()?;
    ["pdfsplit.exe", "pdfsplit"].iter().map(|name| folder.join(name)).find(|path| path.is_file()).map(|path| path.display().to_string())
}

/// An argument as a shell takes it.
///
/// Quoted when it holds anything cmd reads itself — a space, and `& | < > ^ ( ) % !` — because a
/// name like `Fattura A&B` would otherwise split the line in two. Inside quotes those are all
/// literal except `%`, which a batch file still expands: the `.cmd` doubles it separately.
fn quote(argument: &str) -> String {
    let needs_quotes = argument.is_empty() || argument.chars().any(|c| c.is_whitespace() || "\"&|<>^()%!".contains(c));
    if !needs_quotes {
        return argument.to_string();
    }
    // A path cannot hold a quote on Windows, so one that is there is dropped rather than escaping
    // the whole command.
    let mut inner = argument.replace('"', "");
    // A closing quote right after a backslash is read as an escaped quote: `"C:\parti\"` would
    // lose its end and swallow whatever came next.
    if inner.ends_with('\\') {
        inner.push('\\');
    }
    format!("\"{inner}\"")
}

/// The same argument inside a `.cmd`, where `%` is expanded even between quotes.
fn quote_for_batch(argument: &str) -> String {
    quote(argument).replace('%', "%%")
}

/// Everything a thread has said so far, and whether it has ended without saying how.
fn take(receiver: &Receiver<Event>) -> (Vec<Event>, bool) {
    let mut events = Vec::new();
    loop {
        match receiver.try_recv() {
            Ok(event) => events.push(event),
            Err(TryRecvError::Empty) => return (events, false),
            Err(TryRecvError::Disconnected) => return (events, true),
        }
    }
}

/// The line under the bar: how long the job has run, and — only while it runs — how long is left.
///
/// An estimate belongs to a job still going. Once it has ended, the elapsed time here and the total
/// in the result are the truth, and a countdown left over on screen reads as a mistake.
fn progress_line(meter: &Meter, now: Instant, running: bool) -> String {
    let mut line = format!("trascorsi {}", human_duration(meter.elapsed(now)));
    if running && let Some(eta) = meter.last_eta() {
        line.push_str(&format!(" · circa {} rimasti", human_duration(eta)));
    }
    line
}

/// What one report counts, as a person reads it.
fn detail(update: Update) -> String {
    match update.unit {
        Unit::Parts => format!("parte {} di {}", update.part.max(1), update.parts),
        Unit::Bytes => {
            format!("parte {} di {} · {} di {}", update.part.max(1), update.parts.max(1), human_size(update.done), human_size(update.total))
        }
        Unit::Pages => format!("pagina {} di {}", update.done, update.total),
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.before_frame(&ctx);
        if self.running() || self.info_rx.is_some() {
            // The work reports from another thread: keep drawing while it does.
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        // The ui eframe hands over has no margin or background: a central panel gives the window
        // both, and keeps the layout the same as the frames the tests draw.
        egui::CentralPanel::default().show(ui, |ui| self.contents(ui));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::work::fixtures;

    use splitter::Stage;

    /// Run a frame of the window without a screen: egui lays out, draws nothing. Returns the theme
    /// the frame ended with, so a test can see what the window asked for.
    fn frame(app: &mut App) -> egui::ThemePreference {
        let ctx = egui::Context::default();
        let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 700.0))), ..Default::default() };
        let mut output = ctx.run_ui(input, |ui| {
            ui.set_max_size(egui::vec2(900.0, 700.0));
            app.before_frame(ui.ctx());
            egui::CentralPanel::default().show(ui, |ui| app.contents(ui));
        });
        // Nothing draws these frames: the textures a real backend would upload are dropped, and
        // epaint wants that said out loud.
        output.textures_delta.clear();
        ctx.options(|options| options.theme_preference)
    }

    #[test]
    fn the_window_opens_dark_and_the_toggle_changes_it() {
        let mut app = App::default();
        assert!(app.dark, "the window should open dark");
        assert_eq!(frame(&mut app), egui::ThemePreference::Dark);
        app.dark = false;
        assert_eq!(frame(&mut app), egui::ThemePreference::Light, "the toggle did not reach the window");
    }

    #[test]
    fn every_way_of_cutting_draws() {
        for choice in [Choice::Parts, Choice::Pages, Choice::Text, Choice::Bookmarks] {
            let mut app = App { choice, ..App::default() };
            frame(&mut app);
            // Twice: the second frame sees the state the first one left.
            frame(&mut app);
        }
    }

    #[test]
    fn a_file_that_is_not_there_is_a_message_not_a_crash() {
        let mut app = App { input: std::env::temp_dir().join("pdfsplit-gui-missing.pdf").display().to_string(), ..App::default() };
        app.start();
        assert!(!app.running(), "nothing was started");
        assert!(app.failure.is_some());
        frame(&mut app);
    }

    #[test]
    fn the_window_takes_what_the_threads_say() {
        let mut app = App { meter: Some(Meter::new(&[Stage::Write, Stage::Verify], Instant::now())), ..App::default() };
        app.handle(Event::Progress(Update::of_part(Stage::Write, 1, 2, 50, 100, Unit::Bytes)));
        assert_eq!(app.update.map(|update| update.stage), Some(Stage::Write));
        app.handle(Event::Failed("un guasto".to_string()));
        assert_eq!(app.failure.as_deref(), Some("un guasto"));
        assert!(!app.running());
        app.handle(Event::Cancelled);
        assert!(app.cancelled);
        frame(&mut app);
    }

    #[test]
    fn a_finished_job_leaves_no_countdown_on_screen() {
        let start = Instant::now();
        let mut meter = Meter::new(&[Stage::Write, Stage::Verify], start);
        meter.feed(Update::of_part(Stage::Write, 1, 2, 50, 100, Unit::Bytes), start);
        meter.feed(Update::of_part(Stage::Write, 2, 2, 100, 100, Unit::Bytes), start);
        assert!(meter.last_eta().is_some(), "there was nothing to estimate from");
        let later = start + std::time::Duration::from_secs(2);
        assert!(progress_line(&meter, later, true).contains("rimasti"));
        // The same meter once the job has ended: the time stays, the estimate goes.
        let finished = progress_line(&meter, later, false);
        assert!(!finished.contains("rimasti"), "a countdown outlived the job: {finished}");
        assert!(finished.starts_with("trascorsi"), "the elapsed time was lost: {finished}");
    }

    #[test]
    fn a_finished_job_lists_its_parts_in_the_window() {
        let directory = fixtures::folder("app-run");
        let source = fixtures::sample(&directory);
        let mut app = App::default();
        app.choose(&source);
        // The facts of the file, read by the worker thread.
        for _ in 0..200 {
            app.poll();
            if app.info.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(app.info.is_some(), "the file's facts never arrived");
        assert_eq!(app.info.as_ref().and_then(|info| info.pages), Some(2));

        app.text = "Fattura".to_string();
        app.choice = Choice::Text;
        app.out_dir = directory.join("out").display().to_string();
        app.start();
        assert!(app.running());
        for _ in 0..600 {
            app.poll();
            frame(&mut app);
            if !app.running() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!app.running(), "the job never finished");
        let report = app.report.as_ref().expect("the parts of the finished job");
        assert_eq!(report.parts.len(), 2);
        assert!(report.parts.iter().all(|part| part.path.exists()));
        // The window is still usable after a job.
        frame(&mut app);
    }

    #[test]
    fn the_name_fields_show_what_the_parts_will_be_called() {
        let mut app = App { input: "C:\\documenti\\Fatture 2026.pdf".to_string(), ..App::default() };
        // Two parts by default: the first and the last name are both shown.
        assert_eq!(app.name_previews(), ("Fatture 2026-01.pdf".to_string(), "Fatture 2026-02.pdf".to_string()));
        assert_eq!(app.name_preview_line(), "esempio: primo Fatture 2026-01.pdf · ultimo Fatture 2026-02.pdf");
        app.parts_count = 120;
        assert_eq!(app.name_previews().1, "Fatture 2026-120.pdf", "the width follows the part count");
        app.parts_count = 2;
        app.prefix = "Cliente Rossi ".to_string();
        app.width = 3;
        app.start = 5;
        assert_eq!(app.name_previews().0, "Cliente Rossi Fatture 2026-005.pdf");
        app.number_first = true;
        app.suffix = " (copia)".to_string();
        assert_eq!(
            app.name_previews(),
            ("Cliente Rossi 005-Fatture 2026 (copia).pdf".to_string(), "Cliente Rossi 006-Fatture 2026 (copia).pdf".to_string())
        );
        // One part is one name.
        app.choice = Choice::Pages;
        app.page_list = "1-5".to_string();
        assert!(app.name_preview_line().starts_with("esempio: primo"), "two parts from a list");
        app.parts_count = 1;
        app.choice = Choice::Parts;
        assert_eq!(app.name_preview_line(), "esempio: Cliente Rossi 005-Fatture 2026 (copia).pdf");
        // A folder with no file yet still shows a name, from the word the window falls back to.
        let empty = App { prefix: "A/B".to_string(), ..App::default() };
        assert_eq!(empty.name_previews().0, "A-Bdocumento-01.pdf");
    }

    #[test]
    fn the_window_hands_the_writer_what_was_chosen() {
        let directory = fixtures::folder("format");
        let source = fixtures::sample(&directory);
        let mut app = App { input: source.display().to_string(), ..App::default() };
        // The defaults: compression, and the version of the file being cut.
        let setup = app.setup().expect("the file is there");
        assert_eq!(setup.options().compression, Compression::Compress);
        assert_eq!(setup.options().version, None, "no version is asked for: the source's own is written");
        assert_eq!(setup.options().wanted_version(Version::PDF_1_7), Version::PDF_1_7);
        assert_eq!(setup.options().wanted_version(Version::PDF_2_0), Version::PDF_2_0);
        // A source older than the object streams is raised, because compression needs them.
        assert_eq!(setup.options().wanted_version(Version::PDF_1_4), Version::PDF_1_5);
        // Copied streams keep the source's version as well.
        app.compress = false;
        assert_eq!(app.setup().unwrap().options().wanted_version(Version::PDF_1_7), Version::PDF_1_7);
        app.pdf_version = Some(Version::PDF_2_0);
        assert_eq!(app.setup().unwrap().options().wanted_version(Version::PDF_1_7), Version::PDF_2_0);
        // The window names the version of the file it has read.
        assert_eq!(app.source_version(), "?", "no facts yet");
        app.info = Some(splitter::Summary { file_size: 1, pdf_version: "1.7".to_string(), encrypted: false, pages: Some(1), bookmarks: Some(0) });
        assert_eq!(app.source_version(), "1.7");
        frame(&mut app);
    }

    #[test]
    fn the_window_shows_the_command_it_would_run() {
        let mut app = App { input: "C:\\documenti\\Fatture 2026.pdf".to_string(), out_dir: "C:\\documenti\\parti".to_string(), ..App::default() };
        app.choice = Choice::Pages;
        app.page_list = "1-35308".to_string();
        let args = app.cli_arguments();
        let wanted = [
            "split",
            "C:\\documenti\\Fatture 2026.pdf",
            "--pages",
            "1-35308",
            "--leftover",
            "--always-number",
            "--out",
            "C:\\documenti\\parti",
            "--compress",
        ];
        assert_eq!(args, wanted.iter().map(|argument| (*argument).to_string()).collect::<Vec<String>>());
        // No version on the line: the parts keep the source's, which is the command's default too.
        assert!(!args.iter().any(|argument| argument == "--pdf-version"));
        // A path with a space is quoted, so the line can be pasted into a shell as it is.
        assert!(app.command_line().contains("\"C:\\documenti\\Fatture 2026.pdf\""));
        let batch = app.batch_text();
        assert!(batch.starts_with("@echo off"));
        assert!(batch.contains("--compress"), "{batch}");
        assert!(batch.contains("\r\n"), "a .cmd needs CRLF line endings");
        // A version the person picked is written out, so the batch repeats it.
        app.pdf_version = Some(Version::PDF_1_6);
        assert!(app.batch_text().contains("--pdf-version 1.6"));
    }

    #[test]
    fn the_form_cannot_run_without_a_file() {
        let app = App::default();
        let problem = app.setup().unwrap_err();
        assert!(problem.contains("scegli"), "{problem}");
    }

    #[test]
    fn the_output_folder_follows_the_file_it_belongs_to() {
        let directory = fixtures::folder("out-dir");
        let nested = directory.join("secondo");
        std::fs::create_dir_all(&nested).unwrap();
        let first = directory.join("primo.pdf");
        let second = nested.join("secondo.pdf");
        let mut app = App::default();
        app.choose(&first);
        assert_eq!(app.out_dir, directory.display().to_string(), "the parts go beside the file");
        app.choose(&second);
        assert_eq!(app.out_dir, nested.display().to_string(), "the folder follows the new file");
        // A folder chosen by hand stays where the person put it.
        app.out_dir = "C:\\parti".to_string();
        app.choose(&first);
        assert_eq!(app.out_dir, "C:\\parti");
    }

    #[test]
    fn a_worker_that_dies_without_a_word_is_not_a_job_in_progress() {
        let (sender, receiver) = channel();
        let mut app = App { job_rx: Some(receiver), meter: Some(Meter::new(&[Stage::Write], Instant::now())), ..App::default() };
        // The thread vanishes — a panic drops its sender without a last event — and the channel
        // turns into an error rather than into silence: the window must not wait forever.
        drop(sender);
        app.poll();
        assert!(!app.running(), "a closed channel is the end of the job");
        assert!(app.failure.as_deref().is_some_and(|message| message.contains("interrotto")), "{:?}", app.failure);
        frame(&mut app);
    }

    #[test]
    fn a_batch_survives_the_characters_a_shell_reads() {
        let mut app = App { input: "C:\\a&b\\100%.pdf".to_string(), out_dir: "C:\\parti".to_string(), ..App::default() };
        app.prefix = "A&B ".to_string();
        let batch = app.batch_text();
        assert!(batch.contains("\"C:\\a&b\\100%%.pdf\""), "{batch}");
        assert!(batch.contains("\"A&B \""), "{batch}");
        assert!(!batch.to_lowercase().contains("pdfsplit-gui"), "a batch never opens the window: {batch}");
        assert!(batch.contains("\r\n"));
        // The line for pasting into a shell keeps the % as it is: doubling is the .cmd's business.
        assert!(app.command_line().contains("\"C:\\a&b\\100%.pdf\""), "{}", app.command_line());
        // A folder typed with its separator keeps it: the closing quote is not escaped by it.
        app.out_dir = "C:\\parti mie\\".to_string();
        assert!(app.command_line().contains("\"C:\\parti mie\\\\\""), "{}", app.command_line());
    }

    #[test]
    fn the_command_names_the_parts_the_way_the_job_does() {
        let directory = fixtures::folder("naming");
        let source = fixtures::sample(&directory);
        let mut app = App { input: source.display().to_string(), ..App::default() };
        app.choice = Choice::Bookmarks;
        app.by_title = true;
        app.naming_from_text = true;
        app.label = "Codice Fiscale".to_string();
        // The job takes the chapter's own title, so the printed command asks for that and not for
        // the label it would ignore.
        let args = app.cli_arguments();
        assert!(args.iter().any(|argument| argument == "--name-from-title"), "{args:?}");
        assert!(!args.iter().any(|argument| argument == "--name-label"), "{args:?}");
        assert_eq!(app.setup().unwrap().part_names(), splitter::PartName::Bookmark);
    }
}
