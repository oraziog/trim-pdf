//! The work the window hands to a thread: what the user picked and what comes back.
//!
//! Nothing here knows about a screen. A [`Setup`] is what the window filled in, [`run`] is the
//! whole job — open, search if needed, plan, write — and [`Event`] is everything the window is
//! told while it happens. That split is what lets the job be tested without a window at all.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use splitter::{Canceller, Document, Error, Job, SplitReport, Summary, Update};

// What a cut is asked for lives in the core, where the command line fills it the same way: the
// window and the command it prints then cannot describe two different jobs.
pub use splitter::{Mode, Naming, Setup};

/// What the worker tells the window.
#[derive(Debug)]
pub enum Event {
    /// What the file is, for the opening panel.
    Info(Result<Summary, String>),
    /// The job has reached a new phase, or moved within the one it is in.
    Progress(Update),
    Done(Box<SplitReport>),
    Failed(String),
    Cancelled,
}

/// Read what the file is: size, version, pages, bookmarks, encryption.
pub fn read_info(path: PathBuf, sender: Sender<Event>) {
    let outcome = Document::open(&path).and_then(|mut document| document.summary()).map_err(|error| error.to_string());
    let _ = sender.send(Event::Info(outcome));
}

/// Cut the file the way `setup` says, reporting every phase to `sender`.
///
/// `stop` is the window's cancel button: the job notices it within one copy chunk and leaves no
/// file behind, half written parts included.
pub fn run(setup: Setup, sender: Sender<Event>, stop: Canceller) {
    let reporter = sender.clone();
    let job = Job::stopping(
        move |update| {
            let _ = reporter.send(Event::Progress(update));
        },
        stop,
    );
    let event = match setup.run(&job) {
        Ok(report) => Event::Done(Box::new(report)),
        Err(Error::Cancelled) => Event::Cancelled,
        Err(error) => Event::Failed(error.to_string()),
    };
    let _ = sender.send(event);
}

/// Documents and folders the tests of this crate cut: files written by hand, so a test never
/// depends on anything the workspace does not carry.
#[cfg(test)]
pub(crate) mod fixtures {
    use std::path::PathBuf;

    /// A folder of this process's own, emptied first.
    pub(crate) fn folder(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("pdfsplit-gui-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn add(out: &mut Vec<u8>, offsets: &mut Vec<u32>, body: &str) -> u32 {
        let number = u32::try_from(offsets.len()).unwrap() + 1;
        offsets.push(u32::try_from(out.len()).unwrap());
        out.extend_from_slice(
            format!(
                "{number} 0 obj
{body}
endobj
"
            )
            .as_bytes(),
        );
        number
    }

    fn add_stream(out: &mut Vec<u8>, offsets: &mut Vec<u32>, data: &str) -> u32 {
        let number = u32::try_from(offsets.len()).unwrap() + 1;
        offsets.push(u32::try_from(out.len()).unwrap());
        out.extend_from_slice(
            format!(
                "{number} 0 obj
<< /Length {} >>
stream
{data}
endstream
endobj
",
                data.len()
            )
            .as_bytes(),
        );
        number
    }

    /// A two-page document: a cover, then the page that holds the text a cut looks for. `cover` is
    /// what the first page says; an empty one is what a scanned page looks like to a text cut.
    fn document(directory: &std::path::Path, cover: &str, name: &str) -> PathBuf {
        let mut out = b"%PDF-1.7
"
        .to_vec();
        let mut offsets: Vec<u32> = Vec::new();
        add(&mut out, &mut offsets, "<< /Type /Catalog /Pages 2 0 R >>");
        add(&mut out, &mut offsets, "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>");
        add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /Font << /F1 7 0 R >> >> /Contents 5 0 R >>");
        add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /Font << /F1 7 0 R >> >> /Contents 6 0 R >>");
        add_stream(&mut out, &mut offsets, &format!("BT /F1 12 Tf 72 720 Td ({cover}) Tj ET"));
        add_stream(&mut out, &mut offsets, "BT /F1 12 Tf 72 720 Td (Fattura n. 500) Tj ET");
        add(&mut out, &mut offsets, "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>");
        let xref = out.len();
        out.extend_from_slice(
            format!(
                "xref
0 {}
0000000000 65535 f 
",
                offsets.len() + 1
            )
            .as_bytes(),
        );
        for offset in &offsets {
            out.extend_from_slice(
                format!(
                    "{offset:010} 00000 n 
"
                )
                .as_bytes(),
            );
        }
        out.extend_from_slice(
            format!(
                "trailer
<< /Size {} /Root 1 0 R >>
startxref
{xref}
%%EOF
",
                offsets.len() + 1
            )
            .as_bytes(),
        );
        let path = directory.join(name);
        std::fs::write(&path, &out).unwrap();
        path
    }

    /// The document with a cover that holds text.
    pub(crate) fn sample(directory: &std::path::Path) -> PathBuf {
        document(directory, "Copertina", "sample.pdf")
    }

    /// The same document with a cover that says nothing: a scan, as far as a text cut can tell.
    pub(crate) fn scan(directory: &std::path::Path) -> PathBuf {
        document(directory, "", "scan.pdf")
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{folder, sample};
    use super::*;
    use splitter::{Compression, NameTemplate, PartName, Stage, Version};
    use std::sync::mpsc::{Receiver, channel};

    fn setup(directory: &std::path::Path, mode: Mode) -> Setup {
        Setup {
            input: sample(directory),
            out_dir: Some(directory.join("out")),
            mode,
            naming: Naming::Sequence,
            keep_bookmarks: true,
            overwrite: false,
            template: NameTemplate::default(),
            compression: Compression::Neutral,
            version: None,
        }
    }

    /// Run a job to its end and hand back every event it produced.
    fn drain(setup: Setup, stop: Canceller) -> (Vec<Event>, Receiver<Event>) {
        let (sender, receiver) = channel();
        run(setup, sender, stop);
        (receiver.try_iter().collect(), receiver)
    }

    #[test]
    fn the_worker_cuts_the_file_and_says_so() {
        let directory = folder("parts");
        let events = drain(setup(&directory, Mode::Parts(2)), Canceller::default()).0;
        let report = events
            .iter()
            .find_map(|event| match event {
                Event::Done(report) => Some(report),
                _ => None,
            })
            .expect("the job reported its parts");
        assert_eq!(report.parts.len(), 2);
        assert_eq!(report.parts[0].pages, 1);
        assert!(report.parts.iter().all(|part| part.path.exists()));
        // The writing was reported in bytes, and so was the verification.
        assert!(events.iter().any(|event| matches!(event, Event::Progress(update) if update.stage == Stage::Write)));
        assert!(events.iter().any(|event| matches!(event, Event::Progress(update) if update.stage == Stage::Verify)));
    }

    #[test]
    fn a_text_cut_opens_a_part_at_the_page_that_holds_the_text() {
        let directory = folder("text");
        let mode = Mode::Text { text: "Fattura".to_string(), case_sensitive: false, regex: false, close_after: false };
        let events = drain(setup(&directory, mode), Canceller::default()).0;
        let report = events
            .iter()
            .find_map(|event| match event {
                Event::Done(report) => Some(report),
                _ => None,
            })
            .expect("the job reported its parts");
        assert_eq!(report.parts.len(), 2, "page 1 opens the second part");
        assert!(events.iter().any(|event| matches!(event, Event::Progress(update) if update.stage == Stage::Search)));
    }

    #[test]
    fn a_text_cut_says_which_pages_have_no_text_to_search() {
        let directory = folder("no-text");
        let mut base = setup(&directory, Mode::Text { text: "Fattura".to_string(), case_sensitive: false, regex: false, close_after: false });
        base.input = super::fixtures::scan(&directory);
        let events = drain(base, Canceller::default()).0;
        let report = events
            .iter()
            .find_map(|event| match event {
                Event::Done(report) => Some(report),
                _ => None,
            })
            .expect("the job reported its parts");
        // A cut by text on a scan matches nothing and says nothing: now the note says it.
        assert_eq!(report.found.as_ref().map(|found| found.without_text.clone()), Some(vec![1]), "the cover has no text");
        assert!(report.warnings.iter().any(|note| note.contains("1 pagina è senza testo")), "{:?}", report.warnings);
    }

    #[test]
    fn a_compressed_job_writes_parts_the_writer_can_read_back() {
        let directory = folder("compress");
        let mut base = setup(&directory, Mode::Parts(2));
        base.compression = Compression::Compress;
        let events = drain(base, Canceller::default()).0;
        let report = events
            .iter()
            .find_map(|event| match event {
                Event::Done(report) => Some(report),
                _ => None,
            })
            .expect("the job reported its parts");
        // The source is a 1.7 and nothing older was asked for: the parts stay 1.7 and are packed
        // anyway, since the object stream needs 1.5, not 1.7.
        assert_eq!(report.version, Version::PDF_1_7);
        for part in &report.parts {
            let text = String::from_utf8_lossy(&std::fs::read(&part.path).unwrap()).into_owned();
            assert!(text.starts_with("%PDF-1.7"), "{}", part.path.display());
            assert!(text.contains("/ObjStm") && text.contains("/Type /XRef"));
        }
    }

    #[test]
    fn the_setup_carries_the_format_and_the_name_the_window_chose() {
        let directory = folder("format");
        let mut base = setup(&directory, Mode::Parts(2));
        assert_eq!(base.options().compression, Compression::Neutral);
        base.compression = Compression::Compress;
        // Nothing asked: the source's own version, and never older than the object streams.
        assert_eq!(base.options().wanted_version(Version::PDF_1_7), Version::PDF_1_7);
        assert_eq!(base.options().wanted_version(Version::PDF_1_4), Version::PDF_1_5);
        base.version = Some(Version::PDF_2_0);
        assert_eq!(base.options().wanted_version(Version::PDF_1_7), Version::PDF_2_0);
        base.template.prefix = "Parte ".to_string();
        base.template.width = 4;
        assert_eq!(base.options().template.width_for(3), 4);
        // A number of ten digits is refused before anything is written.
        base.template.width = 10;
        assert!(base.problem().is_some());
    }

    #[test]
    fn cancelling_before_the_first_byte_leaves_nothing_behind() {
        let directory = folder("cancel");
        let stop = Canceller::default();
        stop.cancel();
        let events = drain(setup(&directory, Mode::Parts(2)), stop).0;
        assert!(matches!(events.last(), Some(Event::Cancelled)), "the last word is the cancellation");
        let left: Vec<PathBuf> = std::fs::read_dir(directory.join("out"))
            .map(|entries| entries.filter_map(|entry| entry.ok().map(|entry| entry.path())).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "a cancelled job left {left:?}");
    }

    #[test]
    fn a_page_that_is_not_there_is_a_message_not_a_panic() {
        let directory = folder("bad-pages");
        let mode = Mode::Pages { list: "1-9".to_string(), leftover: false };
        let events = drain(setup(&directory, mode), Canceller::default()).0;
        match events.last() {
            Some(Event::Failed(message)) => assert!(message.contains("2 pagine"), "{message}"),
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    #[test]
    fn a_setup_that_cannot_run_says_why() {
        let directory = folder("problems");
        let mut base = setup(&directory, Mode::Parts(0));
        assert!(base.problem().is_some());
        base.mode = Mode::Parts(12);
        assert!(base.problem().is_none());
        base.mode = Mode::Pages { list: "   ".to_string(), leftover: false };
        assert!(base.problem().is_some());
        base.mode = Mode::Text { text: String::new(), case_sensitive: false, regex: false, close_after: false };
        assert!(base.problem().is_some());
        base.mode = Mode::Bookmarks { level: 0, by_title: false };
        assert!(base.problem().is_some());
        base.mode = Mode::Bookmarks { level: 1, by_title: true };
        assert!(base.problem().is_none());
        assert_eq!(base.part_names(), PartName::Bookmark);
    }

    #[test]
    fn the_phases_of_a_job_follow_its_mode() {
        let directory = folder("stages");
        let mut base = setup(&directory, Mode::Parts(2));
        assert_eq!(base.stages(), [Stage::Structure, Stage::Plan, Stage::Write, Stage::Verify]);
        base.mode = Mode::Text { text: "x".to_string(), case_sensitive: true, regex: false, close_after: false };
        assert_eq!(base.stages(), [Stage::Structure, Stage::Search, Stage::Plan, Stage::Write, Stage::Verify]);
    }
}
