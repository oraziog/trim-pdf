//! The cut plan: which pages go into which part, and the checks that keep it honest.
//!
//! A part is a list of pages, in the order they are written. The interface builds a plan from a
//! number of equal parts or from a page list; the split itself only ever sees the plan. The plan
//! is checked against the document's real page count before a single byte is written.

use crate::error::Error;
use crate::structure::Bookmark;

/// Most parts one plan may ask for: every part is a file on disk.
pub(crate) const MAX_PARTS: usize = 10_000;

/// The parts of a split.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplitPlan {
    /// One entry per part, holding 1-based page numbers in the order they are written.
    parts: Vec<Vec<usize>>,
    /// One entry per part: the page that opened it, when a mark did. A part named after a
    /// bookmark needs to know which bookmark it belongs to.
    marks: Vec<Option<usize>>,
}

impl SplitPlan {
    /// `parts` parts as equal as possible: the pages that do not divide evenly go to the first
    /// parts, so the earlier files are never smaller than the later ones.
    pub fn equal_parts(pages: usize, parts: usize) -> Result<Self, Error> {
        if parts == 0 {
            return Err(Error::Invalid("per tagliare serve almeno una parte".to_string()));
        }
        if pages == 0 {
            return Err(Error::Invalid("il documento non ha pagine da tagliare".to_string()));
        }
        if parts > pages {
            return Err(Error::Invalid(format!("il documento ha {pages} pagine: non si può tagliare in {parts} parti senza crearne una vuota")));
        }
        let mut plan = Self::new();
        let size = pages / parts;
        let remainder = pages % parts;
        let mut page = 1_usize;
        for index in 0..parts {
            let count = size + usize::from(index < remainder);
            plan.add((page..page + count).collect(), None)?;
            page += count;
        }
        Ok(plan)
    }

    /// The plan an interface page list describes: `1-10, 11, 12, 13-20`, one part per entry.
    pub fn from_page_list(text: &str) -> Result<Self, Error> {
        let mut plan = Self::new();
        for item in text.split(',') {
            let item = item.trim();
            if item.is_empty() {
                return Err(Error::Invalid("l'elenco delle pagine ha una voce vuota (una virgola di troppo?)".to_string()));
            }
            let range = PageRange::parse(item)?;
            plan.add((range.first..=range.last).collect(), None)?;
        }
        if plan.parts.is_empty() {
            return Err(Error::Invalid("l'elenco delle pagine è vuoto".to_string()));
        }
        Ok(plan)
    }

    /// Add one last part holding every page no part covers, in page order. A document fully
    /// covered by the plan is left unchanged.
    #[must_use = "the plan with its last part is the return value"]
    pub fn with_leftover(mut self, pages: usize) -> Result<Self, Error> {
        self.validate(pages)?;
        let mut covered = vec![false; pages + 1];
        for part in &self.parts {
            for page in part {
                if let Some(slot) = covered.get_mut(*page) {
                    *slot = true;
                }
            }
        }
        let leftover: Vec<usize> = (1..=pages).filter(|page| !covered[*page]).collect();
        if !leftover.is_empty() {
            self.add(leftover, None)?;
        }
        Ok(self)
    }

    /// The plan a set of marks describes: each marked page opens a part, and the pages before it
    /// close in the part before.
    ///
    /// `close_after` turns that round, for a mark that ends a document rather than opening one:
    /// the marked page closes the part it is in, and the pages after the last mark form a last
    /// part. Pages before the first mark, or after the last, are never left out.
    pub fn from_marks(marks: &[usize], pages: usize, close_after: bool) -> Result<Self, Error> {
        let mut marks = marks.to_vec();
        marks.sort_unstable();
        marks.dedup();
        marks.retain(|mark| *mark >= 1 && *mark <= pages);
        if marks.is_empty() {
            return Err(Error::Invalid(if pages == 0 {
                "il documento non ha pagine".to_string()
            } else {
                "non è stata trovata nessuna pagina su cui tagliare".to_string()
            }));
        }
        let mut plan = Self::new();
        if close_after {
            let mut start = 1;
            for &mark in &marks {
                plan.add((start..=mark).collect(), Some(mark))?;
                start = mark.saturating_add(1);
            }
            if start <= pages {
                plan.add((start..=pages).collect(), None)?;
            }
        } else {
            if marks[0] > 1 {
                plan.add((1..marks[0]).collect(), None)?;
            }
            for (index, &mark) in marks.iter().enumerate() {
                let end = marks.get(index + 1).map_or(pages, |next| next.saturating_sub(1).min(pages));
                if end >= mark {
                    plan.add((mark..=end).collect(), Some(mark))?;
                }
            }
        }
        Ok(plan)
    }

    /// The plan a level of bookmarks describes: every bookmark at `level` (1 is the top) opens a
    /// part, and the pages no bookmark names go to the part around them.
    pub fn from_bookmarks(bookmarks: &[Bookmark], level: usize, pages: usize) -> Result<Self, Error> {
        if level == 0 {
            return Err(Error::Invalid("bookmark levels start at 1".to_string()));
        }
        let marks: Vec<usize> =
            bookmarks.iter().filter(|bookmark| bookmark.level.saturating_add(1) == level).filter_map(|bookmark| bookmark.page).collect();
        if marks.is_empty() {
            return Err(Error::Invalid(format!("nessun segnalibro è di livello {level}")));
        }
        Self::from_marks(&marks, pages, false)
    }

    /// The page that opened a part, when a mark did: the bookmark cut names its parts by it.
    pub fn mark(&self, index: usize) -> Option<usize> {
        self.marks.get(index).copied().flatten()
    }

    /// The pages one part holds, 1-based, in the order they are written.
    pub fn pages(&self, index: usize) -> &[usize] {
        self.parts.get(index).map_or(&[], Vec::as_slice)
    }

    /// How many parts the plan holds.
    pub fn len(&self) -> usize {
        self.parts.len()
    }

    /// True when the plan holds no part at all.
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// A part's pages as the interface shows them: `1-10`, or `1, 4-5` when they are not one run.
    pub fn label(&self, index: usize) -> String {
        describe(self.pages(index))
    }

    /// Check the plan against a document of `pages` pages: every page exists, and no page is in
    /// two parts. A page that is in no part is allowed — the interface's `leftover` option is
    /// what puts those in a last file.
    pub fn validate(&self, pages: usize) -> Result<(), Error> {
        if self.parts.is_empty() {
            return Err(Error::Invalid("non è stato chiesto nessun numero di parti".to_string()));
        }
        if self.parts.len() > MAX_PARTS {
            return Err(too_many_parts());
        }
        let mut seen = vec![false; pages + 1];
        for part in &self.parts {
            if part.is_empty() {
                return Err(Error::Invalid("una parte senza pagine".to_string()));
            }
            for page in part {
                if *page == 0 || *page > pages {
                    return Err(Error::Invalid(format!(
                        "dalle indicazioni dovremmo tagliare a pagina {page} ma il documento ha solo {pages} pagine"
                    )));
                }
                if std::mem::replace(&mut seen[*page], true) {
                    return Err(Error::Invalid(format!("la pagina {page} è in due parti")));
                }
            }
        }
        Ok(())
    }

    fn new() -> Self {
        Self { parts: Vec::new(), marks: Vec::new() }
    }

    /// Add one part, refusing to plan more parts than a directory should hold.
    fn add(&mut self, pages: Vec<usize>, mark: Option<usize>) -> Result<(), Error> {
        if self.parts.len() >= MAX_PARTS {
            return Err(too_many_parts());
        }
        self.parts.push(pages);
        self.marks.push(mark);
        Ok(())
    }
}

fn too_many_parts() -> Error {
    Error::Limit(format!("un taglio in più di {MAX_PARTS} parti"))
}

/// Pages as a short text: `1-10`, `1, 4-5`, `3`.
fn describe(pages: &[usize]) -> String {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for &page in pages {
        match runs.last_mut() {
            Some((_, last)) if last.saturating_add(1) == page => *last = page,
            _ => runs.push((page, page)),
        }
    }
    runs.iter().map(|(first, last)| if first == last { first.to_string() } else { format!("{first}-{last}") }).collect::<Vec<_>>().join(", ")
}

/// One entry of an interface page list: `7` or `7-12`, 1-based and inclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PageRange {
    first: usize,
    last: usize,
}

impl PageRange {
    fn parse(text: &str) -> Result<Self, Error> {
        let (first, last) = text.split_once('-').unwrap_or((text, text));
        let first = page_number(first, text)?;
        let last = page_number(last, text)?;
        if last < first {
            return Err(Error::Invalid(format!("l'intervallo «{text}» finisce prima di iniziare")));
        }
        Ok(Self { first, last })
    }
}

fn page_number(text: &str, whole: &str) -> Result<usize, Error> {
    match text.trim().parse::<usize>() {
        Ok(value) if value >= 1 => Ok(value),
        _ => Err(Error::Invalid(format!("«{whole}» non è una pagina né un intervallo di pagine"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_parts_give_the_remainder_to_the_first_files() {
        let plan = SplitPlan::equal_parts(10, 3).unwrap();
        assert_eq!(plan.len(), 3);
        assert_eq!(plan.pages(0), [1, 2, 3, 4]);
        assert_eq!(plan.pages(1), [5, 6, 7]);
        assert_eq!(plan.pages(2), [8, 9, 10]);
        assert_eq!(plan.pages(1).len(), 3);
    }

    #[test]
    fn one_part_is_the_whole_document() {
        let plan = SplitPlan::equal_parts(4, 1).unwrap();
        assert_eq!(plan.label(0), "1-4");
        assert!(plan.validate(4).is_ok());
    }

    #[test]
    fn a_page_list_is_read_one_part_per_entry() {
        let plan = SplitPlan::from_page_list("1-10, 11, 12, 13-20").unwrap();
        assert_eq!(plan.len(), 4);
        assert_eq!(plan.pages(0).len(), 10);
        assert_eq!(plan.label(1), "11");
        assert_eq!(plan.label(3), "13-20");
        assert!(plan.validate(20).is_ok());
    }

    #[test]
    fn a_page_list_that_is_not_pages_is_refused() {
        assert!(matches!(SplitPlan::from_page_list(""), Err(Error::Invalid(_))));
        assert!(matches!(SplitPlan::from_page_list("1,,3"), Err(Error::Invalid(_))));
        assert!(matches!(SplitPlan::from_page_list("x"), Err(Error::Invalid(_))));
        assert!(matches!(SplitPlan::from_page_list("5-2"), Err(Error::Invalid(_))));
        assert!(matches!(SplitPlan::from_page_list("0"), Err(Error::Invalid(_))));
    }

    #[test]
    fn a_page_in_two_parts_is_refused() {
        let plan = SplitPlan::from_page_list("1-5, 4-8").unwrap();
        let error = plan.validate(10).unwrap_err();
        assert!(matches!(error, Error::Invalid(_)));
        assert!(error.to_string().contains("due parti"), "{error}");
    }

    #[test]
    fn a_page_past_the_end_is_refused() {
        let plan = SplitPlan::from_page_list("1-5, 9").unwrap();
        let error = plan.validate(8).unwrap_err();
        assert!(error.to_string().contains("8 pagine"), "{error}");
    }

    #[test]
    fn more_parts_than_pages_is_refused() {
        let error = SplitPlan::equal_parts(3, 5).unwrap_err();
        assert!(error.to_string().contains("una vuota"), "{error}");
        assert!(SplitPlan::equal_parts(3, 0).is_err());
    }

    #[test]
    fn the_leftover_pages_become_one_last_part() {
        let plan = SplitPlan::from_page_list("3-4").unwrap().with_leftover(5).unwrap();
        assert_eq!(plan.len(), 2);
        // The pages no entry covers are in two runs, and one last file still holds them all.
        assert_eq!(plan.pages(1), [1, 2, 5]);
        assert_eq!(plan.label(1), "1-2, 5");
        assert!(plan.validate(5).is_ok());
    }

    #[test]
    fn a_plan_that_covers_everything_gains_no_part() {
        let plan = SplitPlan::from_page_list("1-3").unwrap().with_leftover(3).unwrap();
        assert_eq!(plan.len(), 1);
    }

    #[test]
    fn a_mark_opens_the_part_it_names() {
        let plan = SplitPlan::from_marks(&[3], 6, false).unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan.pages(0), [1, 2]);
        assert_eq!(plan.pages(1), [3, 4, 5, 6]);
        assert_eq!(plan.mark(1), Some(3));
        assert_eq!(plan.mark(0), None);
    }

    #[test]
    fn a_mark_on_the_first_page_leaves_no_empty_part() {
        let plan = SplitPlan::from_marks(&[1], 3, false).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan.label(0), "1-3");
    }

    #[test]
    fn marks_out_of_order_and_repeated_still_give_one_cut_each() {
        let plan = SplitPlan::from_marks(&[5, 3, 3], 6, false).unwrap();
        assert_eq!(plan.len(), 3);
        assert_eq!(plan.label(0), "1-2");
        assert_eq!(plan.label(1), "3-4");
        assert_eq!(plan.label(2), "5-6");
    }

    #[test]
    fn closing_after_a_mark_puts_the_marked_page_at_the_end_of_its_part() {
        let plan = SplitPlan::from_marks(&[2], 6, true).unwrap();
        assert_eq!(plan.pages(0), [1, 2]);
        assert_eq!(plan.pages(1), [3, 4, 5, 6]);
        // A mark on the last page closes the document: there is nothing after it.
        let plan = SplitPlan::from_marks(&[4], 4, true).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan.label(0), "1-4");
    }

    #[test]
    fn a_bookmark_level_gives_a_part_per_bookmark() {
        let bookmarks = [
            Bookmark { title: "Uno".to_string(), level: 0, page: Some(1) },
            Bookmark { title: "Uno.a".to_string(), level: 1, page: Some(2) },
            Bookmark { title: "Due".to_string(), level: 0, page: Some(4) },
        ];
        let plan = SplitPlan::from_bookmarks(&bookmarks, 1, 5).unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan.label(0), "1-3");
        assert_eq!(plan.label(1), "4-5");
        assert_eq!(plan.mark(1), Some(4));

        let plan = SplitPlan::from_bookmarks(&bookmarks, 2, 5).unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan.label(0), "1");
        assert!(SplitPlan::from_bookmarks(&bookmarks, 3, 5).is_err());
    }

    #[test]
    fn no_mark_is_an_error_rather_than_an_empty_cut() {
        assert!(SplitPlan::from_marks(&[], 5, false).is_err());
        assert!(SplitPlan::from_marks(&[0, 9], 5, false).is_err());
    }
}
