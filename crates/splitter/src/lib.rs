//! splitter — the core of pdfsplit: open a PDF of any size, count its pages, read its bookmarks,
//! and cut it into parts.
//!
//! The document is never loaded. Bytes are read at an offset ([`crate::source`]), the
//! cross-reference data is indexed ([`crate::xref`]), and structure is walked object by object
//! ([`crate::structure`]). Everything that is cached is bounded, so a 6–8 GB file costs the same
//! memory as a small one: what grows with the document is the cross-reference index and a page's
//! own dictionaries, not a copy of it.
//!
//! A split ([`Document::split`]) writes one file per part, with the objects of each part
//! renumbered and its streams copied byte for byte ([`crate::write`]).
//!
//! ```no_run
//! let mut doc = splitter::Document::open(std::path::Path::new("big.pdf"))?;
//! println!("{} pagine", doc.page_count()?);
//! for bookmark in doc.bookmarks()? {
//!     println!("{}{:?} {:?}", "  ".repeat(bookmark.level), bookmark.page, bookmark.title);
//! }
//! let plan = splitter::SplitPlan::equal_parts(doc.page_count()?, 3)?;
//! let report = doc.split(&plan, &splitter::SplitOptions::default())?;
//! for part in &report.parts {
//!     println!("{} {} pagine", part.path.display(), part.pages);
//! }
//! # Ok::<(), splitter::Error>(())
//! ```
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod error;
mod format;
mod naming;
mod object;
mod plan;
mod progress;
mod search;
mod setup;
mod source;
mod structure;
#[cfg(test)]
mod tests;
mod text;
mod write;
mod xref;

pub use error::Error;
pub use format::{Compression, Version};
pub use naming::{LabelRule, NameTemplate, PartName, SequencePosition, sanitize};
pub use object::ObjRef;
pub use plan::SplitPlan;
pub use progress::{Canceller, Job, Stage, Unit, Update};
pub use search::{Query, Search};
pub use setup::{Mode, Naming, Prepared, Setup};
pub use structure::Bookmark;
pub use text::PageText;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use search::Matcher;
use source::Source;
use structure::PageEntry;
use xref::{Cache, Reader, Xref};

/// Most page text kept for later questions: past this, pages are read again rather than held.
const MAX_CACHED_TEXT: usize = 128 << 20;
/// Pages whose text is read before it is matched and dropped: what a scan holds at one time is
/// this many pages' worth, not the document's.
const SCAN_BLOCK: usize = 4096;

/// What the interface shows about the input file before a split is planned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Summary {
    pub file_size: u64,
    pub pdf_version: String,
    pub encrypted: bool,
    /// `None` for an encrypted document, whose pages only a password opens.
    pub pages: Option<usize>,
    /// `None` for an encrypted document, whose outline only a password opens.
    pub bookmarks: Option<usize>,
}

/// How a split writes its parts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplitOptions {
    /// Where the parts go; `None` writes them beside the input file.
    pub out_dir: Option<PathBuf>,
    /// Replace files that are already there. When this is off a conflict stops the split before
    /// anything is written, and the error names every file in the way.
    pub overwrite: bool,
    /// Rebuild the bookmarks of each part, pointing at that part's own pages.
    pub keep_bookmarks: bool,
    /// Where each part's name comes from.
    pub names: PartName,
    /// How those names are put together: the prefix, the suffix, where the number goes, how wide
    /// it is and where it starts.
    pub template: NameTemplate,
    /// Whether the streams are copied as they are or compressed.
    pub compression: Compression,
    /// The PDF version to write. `None` keeps the source's own version, raised to 1.5 when the
    /// source is older: compression writes an object stream and a cross-reference stream, and both
    /// arrive with 1.5.
    pub version: Option<Version>,
}

impl Default for SplitOptions {
    fn default() -> Self {
        Self {
            out_dir: None,
            overwrite: false,
            keep_bookmarks: true,
            names: PartName::Sequence,
            template: NameTemplate::default(),
            compression: Compression::Neutral,
            version: None,
        }
    }
}

impl SplitOptions {
    /// The version these options ask for, given what the source declares.
    ///
    /// A person's choice wins. Without one, every part keeps the version of the file it came from:
    /// the bytes are the same whichever version is written, so splitting need not hand a reader a
    /// newer document than the one it started from. Only a source older than 1.5 is raised, because
    /// compression writes an object stream and the cross-reference stream that indexes it, and both
    /// arrive with 1.5.
    #[must_use]
    pub fn wanted_version(&self, source: Version) -> Version {
        self.version.unwrap_or_else(|| source.max(Version::PDF_1_5))
    }
}

/// One written part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    pub path: PathBuf,
    /// The pages it holds, as the interface writes them: `1-10`.
    pub label: String,
    pub pages: usize,
    pub bookmarks: usize,
    pub bytes: u64,
}

/// What a split did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplitReport {
    pub parts: Vec<Part>,
    /// How many pages the document turned out to have.
    pub pages: usize,
    pub source_size: u64,
    pub elapsed: Duration,
    /// The PDF version actually written: the highest of the parts, since a part whose pages need
    /// more is raised on its own.
    pub version: Version,
    /// What the person should know about how the parts were written: a version raised because the
    /// pages needed it, and nothing twice.
    pub warnings: Vec<String>,
    /// What a cut that looked for text found: the pages that matched, and the pages with no text
    /// at all. `None` for every other way of cutting.
    pub found: Option<Search>,
}

impl SplitReport {
    /// The bytes of every part together.
    pub fn written_bytes(&self) -> u64 {
        self.parts.iter().map(|part| part.bytes).sum()
    }

    /// True when the parts together are larger than the source. That is what happens when pages
    /// of different parts share a font or an image: each part carries its own copy.
    pub fn resources_were_duplicated(&self) -> bool {
        self.written_bytes() > self.source_size
    }
}

/// The bytes a split can take, estimated from the share of the source its pages are.
///
/// Pages are not all the same size, so the figure is only as good as the average page in `source_size`;
/// it is exact in the two ends that matter, the whole document and no page at all, and it never
/// guesses more than what a part of every page would cost.
fn estimated_output(source_size: u64, covered: u64, pages: u64) -> u64 {
    if pages == 0 {
        return source_size;
    }
    let estimate = u128::from(source_size).saturating_mul(u128::from(covered)) / u128::from(pages);
    u64::try_from(estimate).unwrap_or(u64::MAX)
}

/// An open PDF: its index, its version, and caches for the objects structure reads.
pub struct Document {
    path: PathBuf,
    source: Source,
    xref: Xref,
    version: String,
    cache: Cache,
    pages: Option<usize>,
    /// Every page with what it inherits, built once by the first walk that needs it.
    page_list: Option<Vec<PageEntry>>,
    bookmarks: Option<Vec<Bookmark>>,
    /// Text read so far, by 1-based page. One read answers every later question: does the page
    /// match, what value does it carry, what is this part called.
    texts: HashMap<usize, PageText>,
    /// How much of that text is being held, so a huge document does not hold all of it.
    text_bytes: usize,
}

impl std::fmt::Debug for Document {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Document")
            .field("file_size", &self.source.len())
            .field("pdf_version", &self.version)
            .field("encrypted", &self.is_encrypted())
            .finish_non_exhaustive()
    }
}

impl Document {
    /// Open a PDF by reading only its header and its cross-reference data.
    pub fn open(path: &Path) -> Result<Self, Error> {
        let source = Source::open(path)?;
        let (_, version) = header(&source)?;
        let xref = xref::read(&source)?;
        Ok(Self {
            path: path.to_path_buf(),
            source,
            xref,
            version,
            cache: Cache::new(),
            pages: None,
            page_list: None,
            bookmarks: None,
            texts: HashMap::new(),
            text_bytes: 0,
        })
    }

    pub fn pdf_version(&self) -> &str {
        &self.version
    }

    pub fn file_size(&self) -> u64 {
        self.source.len()
    }

    /// The file this document was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// True when the trailer names an `/Encrypt` dictionary.
    pub fn is_encrypted(&self) -> bool {
        self.xref.is_encrypted()
    }

    /// The number of pages in the document.
    pub fn page_count(&mut self) -> Result<usize, Error> {
        if let Some(pages) = self.pages {
            return Ok(pages);
        }
        self.require_unencrypted()?;
        let root = self.root()?;
        let pages = {
            let mut reader = Reader::new(&self.source, &self.xref, &mut self.cache);
            structure::page_count(&mut reader, root)?
        };
        self.pages = Some(pages);
        Ok(pages)
    }

    /// The number of pages, counted by walking the tree.
    ///
    /// [`page_count`](Self::page_count) may answer from the page tree's own `/Count`, which a
    /// damaged producer can write lower than the truth; a cut planned on that number would leave
    /// the last pages out without a word. This walks the tree, and it is the list a split needs
    /// anyway, so the walk is not paid for twice.
    pub fn page_count_exact(&mut self) -> Result<usize, Error> {
        let pages = self.page_list()?.len();
        self.pages = Some(pages);
        Ok(pages)
    }

    /// Every bookmark, in outline order. Empty when the document has no outline.
    pub fn bookmarks(&mut self) -> Result<&[Bookmark], Error> {
        if self.bookmarks.is_none() {
            self.require_unencrypted()?;
            let root = self.root()?;
            // A split lists the page tree before it reads the outline, and that list is the map.
            let listed = self.page_list.as_deref().map(index_of);
            let (found, pages) = {
                let mut reader = Reader::new(&self.source, &self.xref, &mut self.cache);
                let catalog = reader.get(root)?;
                let has_outline =
                    catalog.as_dict().and_then(|dict| dict.get(b"Outlines")).is_some_and(|value| !matches!(value, object::Object::Null));
                if has_outline {
                    // A destination becomes a page number only through the page tree, so the
                    // tree is mapped once here — the one walk a document with bookmarks pays for.
                    let map = match listed {
                        Some(map) => map,
                        None => structure::page_map(&mut reader, root)?,
                    };
                    let list = structure::bookmarks(&mut reader, root, &map)?;
                    (list, map.len())
                } else {
                    (Vec::new(), 0)
                }
            };
            if pages > 0 {
                self.pages.get_or_insert(pages);
            }
            self.bookmarks = Some(found);
        }
        Ok(self.bookmarks.as_deref().unwrap_or(&[]))
    }

    /// Read every page and report which ones hold `query`.
    ///
    /// The pages' text is kept, so a name taken from the text — or a second question about the
    /// same pages — does not read the document again. Pages with no text are reported rather than
    /// guessed at: a scan is a job for OCR, which this version does not do.
    pub fn search(&mut self, query: &Query) -> Result<Search, Error> {
        self.search_with(query, &Job::silent())
    }

    /// The same search, reporting each page to `job` and stopping when it is asked to.
    pub fn search_with(&mut self, query: &Query, job: &Job) -> Result<Search, Error> {
        let started = Instant::now();
        job.check()?;
        self.require_unencrypted()?;
        self.page_list()?;
        let pages_in_document = self.pages.unwrap_or_default() as u64;
        job.phase(Stage::Structure, pages_in_document, pages_in_document, Unit::Pages);
        let matcher = Matcher::new(query)?;
        let pages = self.page_list.take().unwrap_or_default();
        let outcome = self.scan_pages(&pages, &matcher, job);
        self.page_list = Some(pages);
        let mut report = outcome?;
        report.elapsed = started.elapsed();
        Ok(report)
    }

    /// Read every page's text once and answer the query from it.
    ///
    /// The pages are read a block at a time: the text of a whole document held at once is how a
    /// scan of a hundred thousand pages grows without a bound. What is kept afterwards is only
    /// what fits under [`MAX_CACHED_TEXT`], the rest is read again if a name needs it.
    fn scan_pages(&mut self, pages: &[PageEntry], matcher: &Matcher, job: &Job) -> Result<Search, Error> {
        let threads = search::thread_count(pages.len());
        let mut found = Search { pages: Vec::new(), without_text: Vec::new(), scanned: 0, elapsed: Duration::ZERO };
        let total = pages.len();
        for block in pages.chunks(SCAN_BLOCK) {
            job.check()?;
            let first = found.scanned as u64;
            let texts = search::read_texts(&self.source, &self.xref, block, threads, job, first, total)?;
            for text in texts {
                found.scanned += 1;
                let page = found.scanned;
                if matcher.matches(&text.text) {
                    found.pages.push(page);
                }
                if text.is_empty() {
                    found.without_text.push(page);
                }
                if self.text_bytes.saturating_add(text.text.len()) <= MAX_CACHED_TEXT {
                    self.text_bytes = self.text_bytes.saturating_add(text.text.len());
                    self.texts.insert(page, text);
                }
            }
        }
        Ok(found)
    }

    /// The text of one page (1-based), read once and kept.
    pub fn page_text(&mut self, page: usize) -> Result<&PageText, Error> {
        if !self.texts.contains_key(&page) {
            self.page_list()?;
            let entry = self.page_list.as_ref().and_then(|list| list.get(page.saturating_sub(1))).cloned();
            let Some(entry) = entry else {
                return Err(Error::Invalid(format!("la pagina {page} è oltre la fine del documento")));
            };
            let text = {
                let mut reader = Reader::new(&self.source, &self.xref, &mut self.cache);
                let value = reader.get(entry.reference)?;
                let dict = value
                    .as_dict()
                    .ok_or_else(|| Error::Corrupt(format!("l'oggetto {} è nell'albero delle pagine ma non è una pagina", entry.reference.num)))?;
                text::page_text(&mut reader, dict, &entry.inherited)?
            };
            self.text_bytes = self.text_bytes.saturating_add(text.text.len());
            self.texts.insert(page, text);
        }
        self.texts.get(&page).ok_or_else(|| Error::Corrupt(format!("il testo della pagina {page} non è stato conservato")))
    }

    /// Header facts and structure counts in one call, for the interface's opening panel.
    pub fn summary(&mut self) -> Result<Summary, Error> {
        let file_size = self.file_size();
        let pdf_version = self.version.clone();
        if self.is_encrypted() {
            return Ok(Summary { file_size, pdf_version, encrypted: true, pages: None, bookmarks: None });
        }
        let pages = self.page_count()?;
        let bookmarks = self.bookmarks()?.len();
        Ok(Summary { file_size, pdf_version, encrypted: false, pages: Some(pages), bookmarks: Some(bookmarks) })
    }

    /// Write `plan` as one file per part, in `options.out_dir` or beside the input file.
    ///
    /// Each part is written to a `.part` name, read back and counted, and only then given its
    /// real name: a failure removes the temporary files and leaves no half-written output. A name
    /// comes from the input, from the part's own text or from a bookmark, as `options.names` says.
    pub fn split(&mut self, plan: &SplitPlan, options: &SplitOptions) -> Result<SplitReport, Error> {
        self.split_with(plan, options, &Job::silent())
    }

    /// The same split, reporting each phase to `job` and stopping when it is asked to.
    pub fn split_with(&mut self, plan: &SplitPlan, options: &SplitOptions, job: &Job) -> Result<SplitReport, Error> {
        let started = Instant::now();
        job.check()?;
        self.require_unencrypted()?;
        self.page_list()?;
        let pages_in_document = self.pages.unwrap_or_default() as u64;
        job.phase(Stage::Structure, pages_in_document, pages_in_document, Unit::Pages);
        // The walk's own count is what the plan is checked against, so the list is taken out of
        // the document for the duration of the writing and put back afterwards.
        let pages = self.page_list.take().unwrap_or_default();
        let outcome = self.write_parts(&pages, plan, options, job);
        self.page_list = Some(pages);
        let mut report = outcome?;
        report.elapsed = started.elapsed();
        Ok(report)
    }

    /// Write every part of a validated plan.
    fn write_parts(&mut self, pages: &[PageEntry], plan: &SplitPlan, options: &SplitOptions, job: &Job) -> Result<SplitReport, Error> {
        plan.validate(pages.len())?;
        job.phase(Stage::Plan, 0, plan.len() as u64, Unit::Parts);
        let source_size = self.source.len();
        let directory = options.out_dir.clone().unwrap_or_else(|| self.directory());
        // A folder the user typed but the disk does not have yet is made, not refused.
        if let Err(error) = std::fs::create_dir_all(&directory) {
            return Err(Error::io(&directory, error));
        }
        // A part copies the streams its pages use, so the parts together are about as large as the
        // share of the source their pages are — the whole source only when the plan covers every
        // page. Asking the volume first turns a full disk into one message instead of a set of
        // parts with the last one cut short.
        let covered: u64 = (0..plan.len()).map(|index| plan.pages(index).len() as u64).sum();
        let needed = estimated_output(source_size, covered, pages.len() as u64);
        let mut space_warning = None;
        if let Ok(free) = fs2::available_space(&directory) {
            if free < needed {
                return Err(Error::Limit(format!(
                    "in {} ci sono {} liberi e questa suddivisione ne può occupare circa {}: libera spazio o scegli un'altra cartella di uscita",
                    directory.display(),
                    size_text(free),
                    size_text(needed)
                )));
            }
            // Fewer bytes free than the whole source is not an error when the plan keeps a small
            // share of it, but it is worth saying: pages of different parts share fonts and
            // images, and each part copies its own.
            if free < source_size {
                space_warning = Some(format!(
                    "in {} ci sono {} liberi: la stima per le pagine scelte è {}, ma font e immagini condivisi tra parti si copiano in ciascuna",
                    directory.display(),
                    size_text(free),
                    size_text(needed)
                ));
            }
        }
        let stem = self.file_stem();
        let (targets, unnamed) = self.part_paths(plan, &directory, &stem, options, job)?;
        // Every name is checked before the first byte is written, and the error lists them all.
        if !options.overwrite {
            let taken: Vec<String> = targets.iter().filter(|path| path.exists()).map(|path| path.display().to_string()).collect();
            if !taken.is_empty() {
                let head = if taken.len() == 1 {
                    "il file esiste già nella cartella di destinazione".to_string()
                } else {
                    format!("i {} file esistono già nella cartella di destinazione", taken.len())
                };
                let verb = if taken.len() == 1 { "sostituirlo" } else { "sostituirli" };
                return Err(Error::Invalid(format!("{head}: {} (usa il parametro --overwrite per {verb})", taken.join(", "))));
            }
        }
        // The outline is read once; every part rebuilds the share of it that points into it.
        let bookmarks: Vec<Bookmark> = if options.keep_bookmarks { self.bookmarks()?.to_vec() } else { Vec::new() };
        let root = self.root()?;
        // Every part is written the way the options ask, and each one may raise the version on its
        // own when its pages need more than the request. Without a request the source's own version
        // is kept, so a split does not look like an upgrade to a reader.
        let source_version = Version::of_source(&self.version).unwrap_or(Version::PDF_1_7);
        let settings = write::Settings {
            compression: options.compression,
            version: options.wanted_version(source_version),
            classic_limit: write::MAX_CLASSIC_OFFSET,
        };
        let mut version = settings.version;
        let mut warnings: Vec<String> = Vec::new();
        if let Some(note) = space_warning {
            warnings.push(note);
        }
        if options.version.is_none() && source_version < Version::PDF_1_5 {
            warnings.push(format!("il sorgente ha una versione pdf {source_version}: le parti sono scritte in {}", settings.version));
        }
        if unnamed > 0 {
            let head = if unnamed == 1 {
                "una parte non ha trovato il nome che le si voleva dare".to_string()
            } else {
                format!("{unnamed} parti non hanno trovato il nome che si voleva dare loro")
            };
            warnings.push(format!("{head}: si {} come il file di ingresso, con la sequenza", if unnamed == 1 { "chiama" } else { "chiamano" }));
        }
        // What the page-tree walk calls a page: the only pages the walk may write as null, and the
        // only objects the form's fields are cut down to. It does not change from part to part.
        let document_pages: HashSet<u32> = pages.iter().map(|page| page.reference.num).collect();
        let mut temporaries: Vec<PathBuf> = Vec::new();
        let mut parts: Vec<Part> = Vec::with_capacity(plan.len());
        let mut written = 0_u64;
        let parts_total = plan.len();
        for (index, target) in targets.iter().enumerate() {
            job.check()?;
            let selected: Vec<usize> = plan.pages(index).iter().map(|page| page.saturating_sub(1)).collect();
            let temporary = temporary_name(target);
            job.part(Stage::Write, index + 1, parts_total, written, source_size, Unit::Bytes);
            let planned = {
                let reader = Reader::new(&self.source, &self.xref, &mut self.cache);
                write::build_part(reader, root, pages, &document_pages, &selected, &bookmarks, options.keep_bookmarks)
            };
            let planned = planned.inspect_err(|_| discard(&temporaries))?;
            // The bytes of this part are reported as they are copied, so a part of three gigabytes
            // moves the bar instead of freezing it.
            let placement = write::Placement { before: written, total: source_size, part: index + 1, parts: parts_total };
            let outcome = write::write(&self.source, &planned, &settings, &temporary, job, placement).inspect_err(|_| {
                discard(&temporaries);
                let _ = std::fs::remove_file(&temporary);
            })?;
            let bytes = outcome.bytes;
            version = version.max(outcome.version);
            for note in outcome.notes {
                if !warnings.contains(&note) {
                    warnings.push(note);
                }
            }
            written = written.saturating_add(bytes);
            temporaries.push(temporary);
            parts.push(Part { path: target.clone(), label: plan.label(index), pages: selected.len(), bookmarks: planned.bookmarks(), bytes });
        }
        // Every part is written before any is checked, so the interface shows one phase at a time
        // and a cancel during the writing never leaves a part that passed for finished.
        job.phase(Stage::Verify, 0, parts_total as u64, Unit::Parts);
        for (index, temporary) in temporaries.iter().enumerate() {
            job.check()?;
            if let Err(error) = read_back(temporary, parts[index].pages) {
                discard(&temporaries);
                return Err(error);
            }
            job.part(Stage::Verify, index + 1, parts_total, index as u64 + 1, parts_total as u64, Unit::Parts);
        }
        job.check()?;
        let mut in_place: Vec<&PathBuf> = Vec::with_capacity(targets.len());
        for (temporary, target) in temporaries.iter().zip(&targets) {
            if let Err(error) = std::fs::rename(temporary, target) {
                // A rename can fail in the middle of the set — the destination went away, another
                // program holds the name. What was already put in place is taken back out: a set
                // of parts with a hole in it is worse than none.
                discard(&temporaries);
                for done in &in_place {
                    let _ = std::fs::remove_file(done);
                }
                return Err(Error::io(target, error));
            }
            in_place.push(target);
        }
        Ok(SplitReport { parts, pages: pages.len(), source_size, elapsed: Duration::ZERO, version, warnings, found: None })
    }

    /// Where every part goes: its name from the input, from the part's own text or from the
    /// bookmark that opened it, made safe and made distinct.
    fn part_paths(
        &mut self,
        plan: &SplitPlan,
        directory: &Path,
        stem: &str,
        options: &SplitOptions,
        job: &Job,
    ) -> Result<(Vec<PathBuf>, usize), Error> {
        let bookmarks: Vec<Bookmark> = if matches!(options.names, PartName::Bookmark) { self.bookmarks()?.to_vec() } else { Vec::new() };
        let rule = match &options.names {
            PartName::Text(rule) => Some(naming::ValueReader::new(rule)?),
            _ => None,
        };
        let template = &options.template;
        let mut taken: HashSet<String> = HashSet::new();
        let mut paths = Vec::with_capacity(plan.len());
        // How many parts had to fall back on the input's own name, so the report can say it.
        let mut unnamed = 0_usize;
        for index in 0..plan.len() {
            let found = match &options.names {
                PartName::Sequence => None,
                PartName::Bookmark => plan
                    .mark(index)
                    .and_then(|page| bookmarks.iter().find(|bookmark| bookmark.page == Some(page)))
                    .map(|bookmark| bookmark.title.clone()),
                PartName::Text(_) => self.value_in(plan.pages(index), rule.as_ref(), job)?,
            };
            let base = found.as_deref().and_then(naming::sanitize);
            if base.is_none() && !matches!(options.names, PartName::Sequence) {
                unnamed += 1;
            }
            let number = index + 1;
            // A name taken from the document is only numbered when it has to be; a name that is
            // not there at all falls back to the input's own name and always takes the sequence,
            // so it can never land on the input file itself.
            let name = match &base {
                Some(base) if !template.always => {
                    let plain = naming::sanitize(&template.plain(base)).unwrap_or_else(|| base.clone());
                    if taken.insert(plain.to_lowercase()) {
                        plain
                    } else {
                        let numbered = naming::sanitize(&template.numbered(base, index, plan.len())).unwrap_or_else(|| base.clone());
                        if taken.insert(numbered.to_lowercase()) { numbered } else { naming::unique(&numbered, &mut taken) }
                    }
                }
                other => {
                    // The template adds the prefix and the suffix itself, so the base is the bare
                    // name: composing it here would put them on twice.
                    let base = other.clone().unwrap_or_else(|| stem.to_string());
                    let numbered = naming::sanitize(&template.numbered(&base, index, plan.len())).unwrap_or_else(|| stem.to_string());
                    if taken.insert(numbered.to_lowercase()) { numbered } else { naming::unique(&numbered, &mut taken) }
                }
            };
            paths.push(directory.join(format!("{name}.pdf")));
            // How many names are settled, not how many parts there are: the row must start at zero
            // and climb, or the first thing a person reads is a job that looks already finished.
            job.part(Stage::Plan, number, plan.len(), (number - 1) as u64, plan.len() as u64, Unit::Parts);
        }
        job.phase(Stage::Plan, plan.len() as u64, plan.len() as u64, Unit::Parts);
        Ok((paths, unnamed))
    }

    /// The first value a rule reads in a part's pages, in page order.
    fn value_in(&mut self, part: &[usize], rule: Option<&naming::ValueReader>, job: &Job) -> Result<Option<String>, Error> {
        let Some(rule) = rule else { return Ok(None) };
        for page in part {
            job.check()?;
            let value = {
                let text = self.page_text(*page)?;
                rule.read(&text.text)
            };
            if value.is_some() {
                return Ok(value);
            }
        }
        Ok(None)
    }

    /// Every page in document order, with the attributes each one inherits from the page tree.
    fn page_list(&mut self) -> Result<&[PageEntry], Error> {
        if self.page_list.is_none() {
            self.require_unencrypted()?;
            let root = self.root()?;
            let list = {
                let mut reader = Reader::new(&self.source, &self.xref, &mut self.cache);
                structure::page_list(&mut reader, root)?
            };
            self.page_list = Some(list);
        }
        Ok(self.page_list.as_deref().unwrap_or(&[]))
    }

    /// Where the input file lives: the default home of the parts.
    fn directory(&self) -> PathBuf {
        self.path.parent().map_or_else(|| PathBuf::from("."), Path::to_path_buf)
    }

    /// The input's name without its extension: the base of every part's name.
    fn file_stem(&self) -> String {
        self.path.file_stem().map_or_else(|| "part".to_string(), |stem| stem.to_string_lossy().into_owned())
    }

    fn root(&self) -> Result<ObjRef, Error> {
        self.xref.root().ok_or_else(|| Error::Corrupt("il trailer non ha la voce /Root".into()))
    }

    fn require_unencrypted(&self) -> Result<(), Error> {
        if self.is_encrypted() { Err(Error::Encrypted) } else { Ok(()) }
    }
}

/// Where each listed page sits in the document, by its object.
fn index_of(pages: &[PageEntry]) -> HashMap<ObjRef, usize> {
    pages.iter().enumerate().map(|(index, page)| (page.reference, index)).collect()
}

/// Reopen a written part and count its pages: the check that the plan and the file agree.
fn read_back(path: &Path, expected: usize) -> Result<(), Error> {
    let name = path.display();
    let mut document = Document::open(path).map_err(|error| Error::Corrupt(format!("non si riesce a riaprire la parte scritta {name}: {error}")))?;
    let pages = document.page_count().map_err(|error| Error::Corrupt(format!("non si riesce a leggere la parte scritta {name}: {error}")))?;
    if pages != expected {
        return Err(Error::Corrupt(format!("la parte scritta {name} contiene {pages} pagine invece di {expected}")));
    }
    Ok(())
}

/// A size a person reads: `6.1 GiB`, `412 KiB`.
fn size_text(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10), ("B", 1)];
    for (unit, size) in UNITS {
        if bytes >= size {
            return if unit == "B" { format!("{bytes} B") } else { format!("{:.1} {unit}", bytes as f64 / size as f64) };
        }
    }
    "0 B".to_string()
}

/// The name a part is written under until it has been read back.
fn temporary_name(target: &Path) -> PathBuf {
    let mut name = target.as_os_str().to_os_string();
    name.push(".part");
    PathBuf::from(name)
}

/// Remove temporary files after a failure: a real name is never left half written.
fn discard(files: &[PathBuf]) {
    for file in files {
        let _ = std::fs::remove_file(file);
    }
}

/// The `%PDF-` header: its offset and the version it declares.
fn header(source: &Source) -> Result<(u64, String), Error> {
    let head = source.read_vec(0, 1024)?;
    let at = head.windows(5).position(|window| window == b"%PDF-").ok_or(Error::NotPdf)?;
    let version_at = at + 5;
    let version: String = head
        .get(version_at..version_at + 3)
        .map(|bytes| bytes.iter().map(|&byte| char::from(byte)).collect::<String>())
        .unwrap_or_default()
        .trim()
        .to_string();
    let version = if version.is_empty() { "1.4".to_string() } else { version };
    Ok((at as u64, version))
}
