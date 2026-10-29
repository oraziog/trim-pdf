//! What a part is called: a name taken from the text or from a bookmark, made safe to write.
//!
//! A name that comes out of a document is not a file name yet. It may carry a colon, a slash, a
//! trailing dot or a device name Windows keeps for itself; it may repeat; it may be missing
//! entirely. Every case here ends in a name that can be written, and says which one it fell back
//! to.

use std::collections::HashSet;

use regex_lite::Regex;

use crate::error::Error;

/// Where a part's name comes from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PartName {
    /// The input's own name and a sequence: `big-01.pdf`.
    #[default]
    Sequence,
    /// The value after a label in the part's own pages.
    Text(LabelRule),
    /// The title of the bookmark that opened the part.
    Bookmark,
}

/// How to read a value out of a page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LabelRule {
    /// The text that introduces the value, as the interface's "Codice Fiscale" would.
    pub label: String,
    /// A regular expression whose first capture group — or whole match, when it has none — is the
    /// value. `None` reads what follows the label's colon up to the next space.
    pub pattern: Option<String>,
}

impl LabelRule {
    /// The interface's rule: "Label: value", up to the next space.
    pub fn new(label: impl Into<String>) -> Self {
        Self { label: label.into(), pattern: None }
    }

    /// Read the value with a pattern of your own instead.
    pub fn matching(label: impl Into<String>, pattern: impl Into<String>) -> Self {
        Self { label: label.into(), pattern: Some(pattern.into()) }
    }
}

/// A label rule, compiled once so a thousand pages do not compile it a thousand times.
pub(crate) struct ValueReader {
    pattern: Regex,
}

impl ValueReader {
    pub(crate) fn new(rule: &LabelRule) -> Result<Self, Error> {
        let expression = match &rule.pattern {
            Some(pattern) => pattern.clone(),
            // "Label: value" — the colon and the spaces around it are not part of the value, and a
            // value ends at the first space, as the interface describes. The colon is optional:
            // producers write "Documento N. 123" as often as they write "Codice Fiscale: RSS", and
            // a label typed with its own colon keeps working.
            None => format!(r"(?i){}\s*:?\s*([^\s]+)", escape(&rule.label)),
        };
        let pattern =
            Regex::new(&expression).map_err(|error| Error::Invalid(format!("«{expression}» non è un'espressione regolare per il nome: {error}")))?;
        Ok(Self { pattern })
    }

    /// The value the rule reads from a page's text, if the page carries it.
    pub(crate) fn read(&self, text: &str) -> Option<String> {
        let captures = self.pattern.captures(text)?;
        let value = captures.get(1).or_else(|| captures.get(0))?.as_str();
        let value = value.trim().trim_end_matches(['.', ',', ';', ':', ')']);
        (!value.is_empty()).then(|| value.to_string())
    }
}

/// Everything that ends a name in a regular expression.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for character in text.chars() {
        if "\\^$.|?*+()[]{}".contains(character) {
            out.push('\\');
        }
        out.push(character);
    }
    out
}

/// A name made safe to write, or `None` when nothing usable is left.
///
/// The characters Windows refuses become `-`; control characters go; runs of spaces and dashes
/// collapse; a trailing dot or space — which Windows silently drops — is removed; a reserved
/// device name gets an underscore; and the length stays under what most file systems hold.
pub fn sanitize(name: &str) -> Option<String> {
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5",
        "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    const MAX_CHARS: usize = 120;
    let mut out = String::with_capacity(name.len());
    let mut previous = ' ';
    for character in name.chars() {
        let replaced =
            if character.is_control() || matches!(character, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '\t') { '-' } else { character };
        // Two separators in a row are one, and no space before one.
        if matches!(replaced, ' ' | '-') && matches!(previous, ' ' | '-') {
            continue;
        }
        out.push(replaced);
        previous = replaced;
    }
    let out = out.trim_matches(|character: char| character == ' ' || character == '.').to_string();
    let mut out: String = out.chars().take(MAX_CHARS).collect();
    if out.trim().is_empty() {
        return None;
    }
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        out.push('_');
    }
    Some(out)
}

/// Where the number goes in a part's name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SequencePosition {
    /// `01-Fattura.pdf`
    Start,
    /// `Fattura-01.pdf`
    #[default]
    End,
}

/// How a part's name is put together: what stands before the base, what after it, and the number.
///
/// The base is the input's own name, or the value a label reads out of the part's pages, or the
/// title of the bookmark that opened it. Around it sit the fixed prefix and suffix; the number
/// goes at whichever end was chosen, with the width and starting value the person asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameTemplate {
    /// Written before the base, after the sequence when the sequence comes first.
    pub prefix: String,
    /// Written after the base, before the sequence when the sequence comes last.
    pub suffix: String,
    pub sequence: SequencePosition,
    /// Digits of the number. `0` asks for as many as the part count needs, and never fewer than
    /// the two the specification asks for.
    pub width: usize,
    /// The number the first part takes.
    pub start: usize,
    /// Number every part, even one whose own name is unique in the set.
    pub always: bool,
}

impl Default for NameTemplate {
    fn default() -> Self {
        Self { prefix: String::new(), suffix: String::new(), sequence: SequencePosition::End, width: 0, start: 1, always: false }
    }
}

impl NameTemplate {
    /// How wide the number is for a cut of `parts` parts.
    #[must_use]
    pub fn width_for(&self, parts: usize) -> usize {
        if self.width > 0 { self.width } else { parts.max(1).to_string().len().max(2) }
    }

    /// The number of the part at `index`, counted from zero.
    #[must_use]
    pub fn number(&self, index: usize) -> usize {
        self.start.saturating_add(index)
    }

    /// The name of a part called `base`, numbered for `index` in a cut of `parts`.
    #[must_use]
    pub fn numbered(&self, base: &str, index: usize, parts: usize) -> String {
        self.compose(base, Some(self.number(index)), self.width_for(parts))
    }

    /// The name of a part that carries no number: the base with what surrounds it.
    #[must_use]
    pub fn plain(&self, base: &str) -> String {
        self.compose(base, None, 0)
    }

    fn compose(&self, base: &str, number: Option<usize>, width: usize) -> String {
        let prefix = &self.prefix;
        let suffix = &self.suffix;
        match (number, self.sequence) {
            (Some(number), SequencePosition::Start) => format!("{prefix}{number:0width$}-{base}{suffix}"),
            (Some(number), SequencePosition::End) => format!("{prefix}{base}{suffix}-{number:0width$}"),
            (None, _) => format!("{prefix}{base}{suffix}"),
        }
    }
}

/// A name no earlier part took: the base itself, then `base-02`, `base-03`, …
pub(crate) fn unique(base: &str, taken: &mut HashSet<String>) -> String {
    if taken.insert(base.to_lowercase()) {
        return base.to_string();
    }
    for suffix in 2..=9_999_u32 {
        let candidate = format!("{base}-{suffix:02}");
        if taken.insert(candidate.to_lowercase()) {
            return candidate;
        }
    }
    base.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_characters_windows_refuses_are_replaced() {
        assert_eq!(sanitize("a/b:c*d?e\"f<g>h|i\\j").as_deref(), Some("a-b-c-d-e-f-g-h-i-j"));
        assert_eq!(sanitize("  ..  ").as_deref(), None);
        assert_eq!(sanitize("").as_deref(), None);
        assert_eq!(sanitize("Fattura  n.  12").as_deref(), Some("Fattura n. 12"));
        // A trailing dot would be dropped by Windows, so it never gets there.
        assert_eq!(sanitize("Codice.").as_deref(), Some("Codice"));
    }

    #[test]
    fn a_device_name_gets_a_suffix() {
        assert_eq!(sanitize("CON").as_deref(), Some("CON_"));
        assert_eq!(sanitize("nul.txt").as_deref(), Some("nul.txt_"));
        assert_eq!(sanitize("CONSOLE").as_deref(), Some("CONSOLE"));
    }

    #[test]
    fn the_template_puts_the_number_where_it_was_asked() {
        let mut template = NameTemplate::default();
        // The default is what the earlier milestones wrote: the base and a number of two digits.
        assert_eq!(template.numbered("Fattura", 0, 2), "Fattura-01");
        assert_eq!(template.numbered("Fattura", 9, 12), "Fattura-10");
        template.sequence = SequencePosition::Start;
        assert_eq!(template.numbered("Fattura", 0, 2), "01-Fattura");
        assert_eq!(template.plain("Fattura"), "Fattura");
    }

    #[test]
    fn the_template_takes_a_prefix_a_suffix_and_a_width() {
        let template = NameTemplate { prefix: "April".to_string(), suffix: "-cliente".to_string(), width: 4, start: 100, ..NameTemplate::default() };
        assert_eq!(template.numbered("Rossi", 0, 3), "AprilRossi-cliente-0100");
        assert_eq!(template.numbered("Rossi", 2, 3), "AprilRossi-cliente-0102");
        assert_eq!(template.width_for(3), 4);
        // A width of zero follows the part count, and never goes below two digits.
        assert_eq!(NameTemplate::default().width_for(3), 2);
        assert_eq!(NameTemplate::default().width_for(1200), 4);
    }

    #[test]
    fn a_repeated_name_gets_a_sequence() {
        let mut taken = HashSet::new();
        assert_eq!(unique("Fattura", &mut taken), "Fattura");
        assert_eq!(unique("Fattura", &mut taken), "Fattura-02");
        assert_eq!(unique("fattura", &mut taken), "fattura-03");
    }

    #[test]
    fn the_label_rule_reads_what_follows_the_colon() {
        let reader = ValueReader::new(&LabelRule::new("Codice Fiscale")).unwrap();
        assert_eq!(reader.read("Cliente: Mario Rossi\nCodice Fiscale: RSSMRA80A01H501U\n").as_deref(), Some("RSSMRA80A01H501U"));
        assert_eq!(reader.read("codice fiscale: xyz.").as_deref(), Some("xyz"));
        assert_eq!(reader.read("niente da leggere"), None);
        // The colon is optional, and a label typed with one keeps working.
        let dotted = ValueReader::new(&LabelRule::new("Documento N.")).unwrap();
        assert_eq!(dotted.read("Documento N. 123 del 2026").as_deref(), Some("123"));
        let with_colon = ValueReader::new(&LabelRule::new("Codice Fiscale:")).unwrap();
        assert_eq!(with_colon.read("Codice Fiscale: RSSMRA80A01H501U").as_deref(), Some("RSSMRA80A01H501U"));
    }

    #[test]
    fn a_pattern_of_its_own_reads_what_it_names() {
        let reader = ValueReader::new(&LabelRule::matching("Fattura", r"Fattura n\.\s*(\S+)")).unwrap();
        assert_eq!(reader.read("Fattura n. 4210 del 3/4").as_deref(), Some("4210"));
        assert_eq!(reader.read("Fattura n. 42/2026").as_deref(), Some("42/2026"));
        // A pattern that captures digits alone captures digits alone.
        let digits = ValueReader::new(&LabelRule::matching("Fattura", r"Fattura n\.\s*(\d+)")).unwrap();
        assert_eq!(digits.read("Fattura n. 42/2026").as_deref(), Some("42"));
    }
}
