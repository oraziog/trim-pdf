//! Finding text: which pages match a query, and which pages have no text at all.
//!
//! A page's text is read once, by [`read_texts`], and every later question — does it match, what
//! value does it carry, what is this part called — is answered from that one read. Pages are read
//! a few at a time: each thread walks its own share of the page tree with its own caches, so the
//! work scales with the cores and the memory still does not scale with the document.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use regex_lite::Regex;

use crate::error::Error;
use crate::progress::{Job, Stage, Unit};
use crate::source::Source;
use crate::structure::PageEntry;
use crate::text::{self, PageText};
use crate::xref::{Cache, Reader, Xref};

/// Most pages read at once. Each adds its caches, so the count is small on purpose.
const MAX_THREADS: usize = 4;

/// What to look for in the pages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    /// What to look for: plain characters, or a regular expression.
    pub text: String,
    /// Read `text` as a regular expression (regex-lite syntax).
    pub pattern: bool,
    pub case_sensitive: bool,
}

impl Query {
    /// Look for these characters, exactly as typed.
    pub fn literal(text: impl Into<String>) -> Self {
        Self { text: text.into(), pattern: false, case_sensitive: true }
    }

    /// Look for a regular expression.
    pub fn pattern(text: impl Into<String>) -> Self {
        Self { text: text.into(), pattern: true, case_sensitive: true }
    }

    /// Match whatever the case. A pattern gets the flag inline, so its classes still work.
    pub fn ignoring_case(mut self) -> Self {
        self.case_sensitive = false;
        self
    }
}

/// The pages a query matches, and the pages that could not be read for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Search {
    /// 1-based pages whose text holds the query, in document order.
    pub pages: Vec<usize>,
    /// 1-based pages with no text to search: an image, or glyphs no font maps back to characters.
    pub without_text: Vec<usize>,
    /// How many pages were read.
    pub scanned: usize,
    pub elapsed: Duration,
}

impl Search {
    /// How many pages hold the query.
    pub fn matches(&self) -> usize {
        self.pages.len()
    }
}

/// A query compiled for scanning.
pub(crate) struct Matcher {
    /// The text to find, already folded when the case does not matter.
    literal: Option<String>,
    pattern: Option<Regex>,
    case_sensitive: bool,
}

impl Matcher {
    pub(crate) fn new(query: &Query) -> Result<Self, Error> {
        if query.text.is_empty() {
            return Err(Error::Invalid("il testo da cercare è vuoto".to_string()));
        }
        if query.pattern {
            let expression = if query.case_sensitive { query.text.clone() } else { format!("(?i){}", query.text) };
            let pattern = Regex::new(&expression)
                .map_err(|error| Error::Invalid(format!("«{text}» non è un'espressione regolare: {error}", text = query.text)))?;
            return Ok(Self { literal: None, pattern: Some(pattern), case_sensitive: query.case_sensitive });
        }
        let literal = if query.case_sensitive { query.text.clone() } else { query.text.to_lowercase() };
        Ok(Self { literal: Some(literal), pattern: None, case_sensitive: query.case_sensitive })
    }

    pub(crate) fn matches(&self, text: &str) -> bool {
        match (&self.literal, &self.pattern) {
            (Some(literal), _) if self.case_sensitive => text.contains(literal.as_str()),
            (Some(literal), _) => text.to_lowercase().contains(literal.as_str()),
            (None, Some(pattern)) => pattern.is_match(text),
            (None, None) => false,
        }
    }
}

/// How many pages to read at once, for a document of `pages` pages.
pub(crate) fn thread_count(pages: usize) -> usize {
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    cores.clamp(1, MAX_THREADS).min(pages.max(1))
}
/// Read the text of `pages`, in order, reporting each page to `job`.
///
/// `first` is how many pages of the whole scan came before this run and `total` how many the
/// whole scan holds: a scan reads its pages in blocks, and the progress bar is about the document.
pub(crate) fn read_texts(
    source: &Source,
    xref: &Xref,
    pages: &[PageEntry],
    threads: usize,
    job: &Job,
    first: u64,
    total: usize,
) -> Result<Vec<PageText>, Error> {
    let threads = threads.clamp(1, MAX_THREADS).min(pages.len().max(1));
    // Pages are counted as they finish, by whichever thread read them: the bar moves with the
    // work and not with the order of the pages.
    let read = AtomicU64::new(0);
    if threads <= 1 || pages.len() < 2 {
        return chunk(source, xref, pages, first, total, job, &read);
    }
    let size = pages.len().div_ceil(threads);
    let mut texts = Vec::with_capacity(pages.len());
    // Each thread opens its own reader over the same file: the index is shared read-only, the
    // caches are not shared at all.
    std::thread::scope(|scope| -> Result<(), Error> {
        let read = &read;
        let mut handles = Vec::with_capacity(threads);
        for slice in pages.chunks(size) {
            handles.push(scope.spawn(move || chunk(source, xref, slice, first, total, job, read)));
        }
        for handle in handles {
            let read = handle.join().map_err(|_| Error::Corrupt("un thread di scansione del testo si è fermato inaspettatamente".to_string()))??;
            texts.extend(read);
        }
        Ok(())
    })?;
    Ok(texts)
}

/// The text of one run of pages, read with one reader.
#[allow(clippy::too_many_arguments)]
fn chunk(source: &Source, xref: &Xref, pages: &[PageEntry], first: u64, total: usize, job: &Job, read: &AtomicU64) -> Result<Vec<PageText>, Error> {
    let mut cache = Cache::new();
    let mut reader = Reader::new(source, xref, &mut cache);
    let mut texts = Vec::with_capacity(pages.len());
    for page in pages {
        job.check()?;
        let value = reader.get(page.reference)?;
        let dict = value
            .as_dict()
            .ok_or_else(|| Error::Corrupt(format!("l'oggetto {} è nell'albero delle pagine ma non è una pagina", page.reference.num)))?;
        texts.push(text::page_text(&mut reader, dict, &page.inherited)?);
        let done = first.saturating_add(read.fetch_add(1, Ordering::Relaxed)).saturating_add(1);
        job.part(Stage::Search, 0, 0, done, total as u64, Unit::Pages);
    }
    Ok(texts)
}
