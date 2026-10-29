//! What a person asked for — from the window or from the command line — and the job that does it.
//!
//! Both front ends describe a cut with a [`Setup`] and hand it to [`Setup::run`]: one pipeline,
//! two ways of asking. A difference between them would show as a command the window prints that
//! does not do what the window does, which is what having this here prevents.

use std::path::PathBuf;

use crate::{
    Compression, Document, Error, Job, LabelRule, NameTemplate, PartName, Query, Search, SplitOptions, SplitPlan, SplitReport, Stage, Version,
};

/// How the file is cut.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// N parts as equal as possible, by page count.
    Parts(usize),
    /// One part per entry of a page list, and the pages nothing covers in a last part.
    Pages { list: String, leftover: bool },
    /// A new part at every page holding the text.
    Text { text: String, case_sensitive: bool, regex: bool, close_after: bool },
    /// A new part at every bookmark of one level, 1 being the top.
    Bookmarks { level: usize, by_title: bool },
}

/// Where a part's name comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Naming {
    /// The input's name and the sequence.
    Sequence,
    /// The value after a label in the part's pages.
    Text { label: String, pattern: String },
}

/// Everything a cut was asked for, in one value a thread can own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Setup {
    pub input: PathBuf,
    pub out_dir: Option<PathBuf>,
    pub mode: Mode,
    pub naming: Naming,
    pub keep_bookmarks: bool,
    pub overwrite: bool,
    /// The prefix, the suffix, where the number goes, how wide it is and where it starts.
    pub template: NameTemplate,
    /// Whether the streams are compressed and the objects packed.
    pub compression: Compression,
    /// The version to write; `None` follows the compression.
    pub version: Option<Version>,
}

impl Setup {
    /// Why this setup cannot run, when it cannot.
    #[must_use]
    pub fn problem(&self) -> Option<String> {
        match &self.mode {
            Mode::Parts(0) => Some("una parte è il minimo: chiedine almeno una".to_string()),
            Mode::Parts(count) if *count > 10_000 => Some(format!("{count} parti sono troppe: il limite è 10.000")),
            Mode::Pages { list, .. } if list.trim().is_empty() => Some("scrivi le pagine da tenere, per esempio \"1-10, 11\"".to_string()),
            Mode::Text { text, .. } if text.is_empty() => Some("scrivi il testo da cercare".to_string()),
            Mode::Text { regex: true, text, .. } if text.trim().is_empty() => Some("scrivi l'espressione regolare da cercare".to_string()),
            Mode::Bookmarks { level: 0, .. } => Some("i livelli dei segnalibri partono da 1".to_string()),
            // A number of more than nine digits is longer than any file name is worth.
            _ if self.template.width > 9 => Some("il numero delle parti arriva al massimo a nove cifre".to_string()),
            _ => None,
        }
    }

    /// The phases this job goes through, in order.
    #[must_use]
    pub fn stages(&self) -> Vec<Stage> {
        let mut stages = vec![Stage::Structure, Stage::Plan, Stage::Write, Stage::Verify];
        if matches!(self.mode, Mode::Text { .. }) {
            stages.insert(1, Stage::Search);
        }
        stages
    }

    /// Where the parts' names come from.
    ///
    /// A bookmark's own title wins over a label taken from the text: a part that was opened by a
    /// chapter is called after the chapter, whatever else the form says.
    #[must_use]
    pub fn part_names(&self) -> PartName {
        if let Mode::Bookmarks { by_title: true, .. } = self.mode {
            return PartName::Bookmark;
        }
        match &self.naming {
            Naming::Text { label, pattern } if !label.trim().is_empty() => {
                let label = label.trim().to_string();
                if pattern.trim().is_empty() {
                    PartName::Text(LabelRule::new(label))
                } else {
                    PartName::Text(LabelRule::matching(label, pattern.trim().to_string()))
                }
            }
            _ => PartName::Sequence,
        }
    }

    /// How the parts are written.
    #[must_use]
    pub fn options(&self) -> SplitOptions {
        SplitOptions {
            out_dir: self.out_dir.clone(),
            overwrite: self.overwrite,
            keep_bookmarks: self.keep_bookmarks,
            names: self.part_names(),
            template: self.template.clone(),
            compression: self.compression,
            version: self.version,
        }
    }

    /// Open the file, work out the parts, write them: the whole job, reporting each phase to `job`.
    pub fn run(&self, job: &Job) -> Result<SplitReport, Error> {
        if let Some(message) = self.problem() {
            return Err(Error::Invalid(message));
        }
        let mut document = Document::open(&self.input)?;
        // The walk's own count, not the page tree's `/Count`: a `/Count` written too low would
        // leave the last pages out of every plan without a word.
        let pages = document.page_count_exact()?;
        let mut found: Option<Search> = None;
        let plan = match &self.mode {
            Mode::Parts(count) => SplitPlan::equal_parts(pages, (*count).max(1))?,
            Mode::Pages { list, leftover } => {
                let plan = SplitPlan::from_page_list(list)?;
                if *leftover { plan.with_leftover(pages)? } else { plan }
            }
            Mode::Text { text, case_sensitive, regex, close_after } => {
                let query = if *regex { Query::pattern(text.clone()) } else { Query::literal(text.clone()) };
                let query = if *case_sensitive { query } else { query.ignoring_case() };
                let outcome = document.search_with(&query, job)?;
                let plan = SplitPlan::from_marks(&outcome.pages, pages, *close_after)?;
                found = Some(outcome);
                plan
            }
            Mode::Bookmarks { level, .. } => {
                let bookmarks = document.bookmarks()?.to_vec();
                SplitPlan::from_bookmarks(&bookmarks, *level, pages)?
            }
        };
        let mut report = document.split_with(&plan, &self.options(), job)?;
        if let Some(search) = found {
            // A page with no text is the one thing a cut by text can get wrong without saying so:
            // on a scan nothing matches, and every page goes to the same part.
            if !search.without_text.is_empty() {
                let alone = search.without_text.len();
                let head = if alone == 1 { "1 pagina è senza testo".to_string() } else { format!("{alone} pagine sono senza testo") };
                report.warnings.insert(0, format!("{head}: {}", page_list(&search.without_text)));
            }
            report.found = Some(search);
        }
        Ok(report)
    }
}

/// The pages as a short list for a message: the first ten, then how many are left.
fn page_list(pages: &[usize]) -> String {
    let shown: Vec<String> = pages.iter().take(10).map(usize::to_string).collect();
    let hidden = pages.len() - shown.len();
    let rest = match hidden {
        0 => return shown.join(", "),
        1 => "e un'altra".to_string(),
        _ => format!("e altre {hidden}"),
    };
    format!("{} {rest}", shown.join(", "))
}
