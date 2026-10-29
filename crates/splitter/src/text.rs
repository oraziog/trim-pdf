//! What a page says: the text its content streams show.
//!
//! Nothing is rendered. The scanner walks a content stream's operators and collects the strings
//! `Tj`, `TJ`, `'` and `"` carry, decoding them through the font that was current when they were
//! shown: the font's `/ToUnicode` map when it has one, its base encoding otherwise.
//!
//! A content stream is never held whole. An unfiltered one is read in windows and scanned as it
//! arrives, so a page whose content is three gigabytes costs one window; a filtered one is
//! decoded with a bound, and a page past that bound is reported rather than guessed at.
//!
//! What this cannot do is OCR: a scanned page has no text, and a subsetted font with no
//! `/ToUnicode` cannot be read back to characters. Both cases are reported, not invented.

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::Error;
use crate::object::{Dict, ObjRef, Object, StreamRef, is_white};
use crate::structure::pdfdoc_char;
use crate::xref::Reader;

/// Largest decoded content a page may need: past this the page is reported as too large.
const MAX_DECODED_CONTENT: usize = 64 << 20;
/// Bytes read at a time from an unfiltered content stream.
const CONTENT_CHUNK: usize = 1 << 20;
/// Deepest `/Form` XObject nesting read.
const MAX_FORM_DEPTH: usize = 8;
/// Most fonts one resources dictionary may hold.
const MAX_FONTS: usize = 4096;
/// Most entries a `/ToUnicode` map may hold.
const MAX_CMAP: usize = 100_000;
/// Most text one page may contribute, so a hostile page cannot grow the string without end.
const MAX_TEXT: usize = 4 << 20;
/// Largest `/ToUnicode` stream decoded.
const MAX_CMAP_BYTES: usize = 16 << 20;

/// The text of a page.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PageText {
    pub text: String,
    /// True when the page showed glyphs that no font mapped back to characters: a subset font
    /// without `/ToUnicode`, which is the case OCR would be for.
    pub unmapped: bool,
    /// True when the page held more than the reader is willing to scan in one go.
    pub truncated: bool,
}

impl PageText {
    /// True when there is nothing to search: an image-only page, or a page with no content.
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }
}

/// Read the text of a page.
pub(crate) fn page_text(reader: &mut Reader<'_>, page: &Dict, inherited: &Dict) -> Result<PageText, Error> {
    let resources = match page.get(b"Resources") {
        Some(value) => Some(value.clone()),
        None => inherited.get(b"Resources").cloned(),
    };
    let resources = match resources {
        Some(value) => reader.resolve(&value)?.as_dict().cloned(),
        None => None,
    };
    let mut engine = Engine { reader, fonts: HashMap::new() };
    let mut text = PageText::default();
    let contents = page.get(b"Contents").cloned();
    engine.contents(contents.as_ref(), resources.as_ref(), &mut text, 0)?;
    text.text = tidy(&text.text);
    Ok(text)
}

/// Runs of white space collapsed to one space, so a name taken from text stays a name.
fn tidy(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for character in text.chars() {
        if character.is_whitespace() {
            space = !out.is_empty();
        } else {
            if space {
                out.push(' ');
                space = false;
            }
            out.push(character);
        }
    }
    out
}

/// A page's fonts, and the names its resources give them: `/F1` is the first entry.
type LevelFonts = (Vec<Arc<Font>>, HashMap<Vec<u8>, usize>);

/// Reads content streams, caching the fonts and their decoding as it goes.
struct Engine<'a, 'b> {
    reader: &'b mut Reader<'a>,
    /// Decoding per font object, so a document's fonts are read once.
    fonts: HashMap<u32, Arc<Font>>,
}

impl Engine<'_, '_> {
    /// The text of one `/Contents` entry, plus everything its form XObjects show.
    fn contents(&mut self, contents: Option<&Object>, resources: Option<&Dict>, text: &mut PageText, depth: usize) -> Result<(), Error> {
        let streams = self.streams(contents)?;
        self.streams_text(&streams, resources, text, depth)
    }

    /// The text of a list of content streams, in the resource scope they are drawn with.
    fn streams_text(&mut self, streams: &[StreamRef], resources: Option<&Dict>, text: &mut PageText, depth: usize) -> Result<(), Error> {
        if depth > MAX_FORM_DEPTH {
            return Err(Error::Limit(format!("form XObject annidati oltre {MAX_FORM_DEPTH} livelli")));
        }
        if streams.is_empty() {
            return Ok(());
        }
        let (fonts, names) = self.level_fonts(resources)?;
        let mut scanner = Scanner::new(&fonts, &names);
        for stream in streams {
            self.scan(stream, &mut scanner, text)?;
        }
        let forms = std::mem::take(&mut scanner.forms);
        text.unmapped |= scanner.unmapped;
        text.truncated |= scanner.truncated;
        text.text.push_str(&scanner.into_text());
        // A form is drawn by the page, so its text belongs to the page as well.
        for name in forms {
            self.form(&name, resources, text, depth)?;
        }
        Ok(())
    }

    /// One content stream, filtered or not, fed to the scanner as it is read.
    fn scan(&mut self, stream: &StreamRef, scanner: &mut Scanner<'_>, text: &mut PageText) -> Result<(), Error> {
        let filtered = stream.dict.get(b"Filter").is_some_and(|value| !matches!(value, Object::Null));
        if filtered {
            match self.reader.decoded_stream(stream, MAX_DECODED_CONTENT) {
                Ok(decoded) => scanner.feed(&decoded),
                // A page whose content is past the bound is reported, not silently half read.
                Err(Error::Limit(_)) => text.truncated = true,
                Err(error) => return Err(error),
            }
            return Ok(());
        }
        let (offset, length) = self.reader.stream_layout(stream)?;
        let mut buffer = vec![0_u8; CONTENT_CHUNK];
        let mut done = 0_u64;
        while done < length {
            let take = usize::try_from((length - done).min(buffer.len() as u64)).unwrap_or(buffer.len());
            let read = self.reader.source().read_into(offset.saturating_add(done), &mut buffer[..take])?;
            if read == 0 {
                break;
            }
            scanner.feed(&buffer[..read]);
            done = done.saturating_add(read as u64);
        }
        Ok(())
    }

    /// The text a form XObject draws. A form's own stream is its content, and its `/Resources`
    /// replace the page's for what it draws — or fall back to them when it has none.
    fn form(&mut self, name: &[u8], resources: Option<&Dict>, text: &mut PageText, depth: usize) -> Result<(), Error> {
        let Some(xobjects) = resources.and_then(|dict| dict.get(b"XObject")) else { return Ok(()) };
        let xobjects = self.reader.resolve(xobjects)?.as_dict().cloned();
        let Some(value) = xobjects.and_then(|dict| dict.get(name).cloned()) else { return Ok(()) };
        let object = self.reader.resolve(&value)?;
        let Some(stream) = object.as_stream().cloned() else { return Ok(()) };
        if stream.dict.get_name(b"Subtype") != Some(b"Form") {
            return Ok(());
        }
        let form_resources = match stream.dict.get(b"Resources").cloned() {
            Some(value) => self.reader.resolve(&value)?.as_dict().cloned(),
            None => resources.cloned(),
        };
        self.streams_text(&[stream], form_resources.as_ref(), text, depth + 1)
    }

    /// The streams of a `/Contents` entry: one, or the array a producer wrote instead.
    fn streams(&mut self, contents: Option<&Object>) -> Result<Vec<StreamRef>, Error> {
        let Some(contents) = contents else { return Ok(Vec::new()) };
        let value = self.reader.resolve(contents)?;
        let mut streams = Vec::new();
        match &*value {
            Object::Stream(stream) => streams.push(stream.clone()),
            Object::Array(items) => {
                for item in items {
                    let item = self.reader.resolve(item)?;
                    if let Object::Stream(stream) = &*item {
                        streams.push(stream.clone());
                    }
                }
            }
            _ => {}
        }
        Ok(streams)
    }

    /// The fonts one resources dictionary names, in the order the page reaches them.
    fn level_fonts(&mut self, resources: Option<&Dict>) -> Result<LevelFonts, Error> {
        let mut fonts = Vec::new();
        let mut names: HashMap<Vec<u8>, usize> = HashMap::new();
        let Some(resources) = resources else { return Ok((fonts, names)) };
        let Some(value) = resources.get(b"Font") else { return Ok((fonts, names)) };
        let dict = self.reader.resolve(value)?;
        let Some(dict) = dict.as_dict().cloned() else { return Ok((fonts, names)) };
        for (name, value) in dict.iter() {
            if fonts.len() >= MAX_FONTS {
                return Err(Error::Limit(format!("una pagina nomina più di {MAX_FONTS} font")));
            }
            let font = match value {
                Object::Ref(reference) => self.font(*reference)?,
                other => Arc::new(self.read_font(other)?),
            };
            names.insert(name.to_vec(), fonts.len());
            fonts.push(font);
        }
        Ok((fonts, names))
    }

    /// A font by its object number, read once per document.
    fn font(&mut self, reference: ObjRef) -> Result<Arc<Font>, Error> {
        if let Some(cached) = self.fonts.get(&reference.num) {
            return Ok(Arc::clone(cached));
        }
        let value = self.reader.get(reference)?;
        let font = Arc::new(self.read_font(&value)?);
        self.fonts.insert(reference.num, Arc::clone(&font));
        Ok(font)
    }

    /// How one font's character codes become text.
    fn read_font(&mut self, value: &Object) -> Result<Font, Error> {
        let dict = value.as_dict().ok_or_else(|| Error::Corrupt("una voce /Font che non è un dizionario".into()))?;
        let composite = dict.get_name(b"Subtype") == Some(b"Type0");
        let mut code_len = if composite { 2 } else { 1 };
        let map = match dict.get(b"ToUnicode") {
            Some(Object::Ref(reference)) => {
                let reference = *reference;
                let stream = self.reader.get(reference)?;
                self.unicode_map(&stream)?
            }
            Some(Object::Stream(stream)) => {
                let stream = stream.clone();
                self.unicode_map(&Object::Stream(stream))?
            }
            _ => None,
        };
        if let Some(parsed) = &map {
            code_len = code_len.max(parsed.code_len);
        }
        Ok(Font { code_len: code_len.min(2), map: map.map(|parsed| parsed.map), composite })
    }

    /// A font's `/ToUnicode` CMap: character codes to the text they stand for.
    fn unicode_map(&mut self, value: &Object) -> Result<Option<Cmap>, Error> {
        let Some(stream) = value.as_stream() else { return Ok(None) };
        let stream = stream.clone();
        let data = match self.reader.decoded_stream(&stream, MAX_CMAP_BYTES) {
            Ok(data) => data,
            Err(Error::Limit(_)) => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(parse_cmap(&data))
    }
}

/// One font's decoding.
struct Font {
    /// Bytes one character code takes: the widest the map's codespace describes.
    code_len: usize,
    /// Code to text, when the font carries a `/ToUnicode` map.
    map: Option<HashMap<u32, String>>,
    /// True for a composite font, whose codes are never single bytes of text on their own.
    composite: bool,
}

impl Font {
    /// The text a string of character codes stands for, and whether every code was mapped.
    fn decode(&self, raw: &[u8]) -> (String, bool) {
        let mut text = String::with_capacity(raw.len());
        let mut mapped = true;
        let mut at = 0;
        while at < raw.len() {
            let widest = self.code_len.clamp(1, raw.len() - at);
            let mut found = None;
            for width in (1..=widest).rev() {
                let code = raw[at..at + width].iter().fold(0_u32, |code, byte| (code << 8) | u32::from(*byte));
                if let Some(value) = self.map.as_ref().and_then(|map| map.get(&code)) {
                    found = Some((width, value));
                    break;
                }
            }
            match found {
                Some((width, value)) => {
                    text.push_str(value);
                    at += width;
                }
                None => {
                    if self.map.is_none() && !self.composite {
                        // A simple font with no map: the byte is its own character.
                        text.push(pdfdoc_char(raw[at]));
                    } else {
                        mapped = false;
                    }
                    at += 1;
                }
            }
        }
        (text, mapped)
    }
}

/// A parsed `/ToUnicode` map.
struct Cmap {
    map: HashMap<u32, String>,
    /// Bytes per code, from the map's own codespace ranges.
    code_len: usize,
}

/// Read a CMap: the `bfchar`, `bfrange` and `codespacerange` blocks that carry the mapping.
fn parse_cmap(data: &[u8]) -> Option<Cmap> {
    let items = tokenise(data);
    let mut map: HashMap<u32, String> = HashMap::new();
    let mut code_len = 0_usize;
    let mut at = 0;
    while let Some(item) = items.get(at) {
        match item {
            Item::Word(word) if word == "begincodespacerange" => {
                at += 1;
                while let (Some(Item::Hex(low)), Some(Item::Hex(_high))) = (items.get(at), items.get(at + 1)) {
                    code_len = code_len.max(low.len());
                    at += 2;
                }
            }
            Item::Word(word) if word == "beginbfchar" => {
                at += 1;
                while let (Some(Item::Hex(source)), Some(Item::Hex(target))) = (items.get(at), items.get(at + 1)) {
                    if map.len() >= MAX_CMAP {
                        break;
                    }
                    code_len = code_len.max(source.len());
                    map.insert(code(source), text_of(target));
                    at += 2;
                }
            }
            Item::Word(word) if word == "beginbfrange" => {
                at += 1;
                while let (Some(Item::Hex(low)), Some(Item::Hex(high)), Some(target)) = (items.get(at), items.get(at + 1), items.get(at + 2)) {
                    let (low, high) = (code(low), code(high));
                    if high >= low && high.saturating_sub(low) <= 65_535 {
                        code_len = code_len.max(items[at].len());
                        match target {
                            Item::Hex(start) => {
                                let base = text_of(start);
                                for offset in 0..=(high - low) {
                                    if map.len() >= MAX_CMAP {
                                        break;
                                    }
                                    map.insert(low.saturating_add(offset), shifted(&base, offset));
                                }
                            }
                            Item::Array(values) => {
                                for (offset, value) in values.iter().enumerate() {
                                    if map.len() >= MAX_CMAP {
                                        break;
                                    }
                                    if let Item::Hex(bytes) = value {
                                        map.insert(low.saturating_add(u32::try_from(offset).unwrap_or(u32::MAX)), text_of(bytes));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    at += 3;
                }
            }
            _ => at += 1,
        }
    }
    if map.is_empty() {
        return None;
    }
    Some(Cmap { map, code_len: code_len.max(1) })
}

/// What a CMap's bytes are made of: the parts of the mapping, and everything else ignored.
#[derive(Debug)]
enum Item {
    Hex(Vec<u8>),
    Word(String),
    Array(Vec<Item>),
    Other,
}

impl Item {
    fn len(&self) -> usize {
        match self {
            Item::Hex(bytes) => bytes.len(),
            _ => 0,
        }
    }
}

fn tokenise(data: &[u8]) -> Vec<Item> {
    let mut items = Vec::new();
    let mut at = 0;
    while at < data.len() {
        match data[at] {
            byte if is_white(byte) => at += 1,
            b'%' => {
                while at < data.len() && data[at] != b'\n' && data[at] != b'\r' {
                    at += 1;
                }
            }
            b'<' => {
                at += 1;
                let start = at;
                while at < data.len() && data[at] != b'>' {
                    at += 1;
                }
                items.push(Item::Hex(hex_bytes(&data[start..at])));
                at += 1;
            }
            b'[' => {
                let mut nested = Vec::new();
                at += 1;
                let mut depth = 1;
                while at < data.len() && depth > 0 {
                    match data[at] {
                        b'[' => {
                            depth += 1;
                            at += 1;
                        }
                        b']' => {
                            depth -= 1;
                            at += 1;
                        }
                        byte if is_white(byte) => at += 1,
                        b'<' => {
                            let start = at + 1;
                            at = start;
                            while at < data.len() && data[at] != b'>' {
                                at += 1;
                            }
                            nested.push(Item::Hex(hex_bytes(&data[start..at])));
                            at += 1;
                        }
                        _ => {
                            while at < data.len() && !is_white(data[at]) && !matches!(data[at], b']' | b'<' | b'[') {
                                at += 1;
                            }
                            nested.push(Item::Other);
                        }
                    }
                }
                items.push(Item::Array(nested));
            }
            byte if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'+' | b'.') => {
                let start = at;
                while at < data.len() && !is_white(data[at]) && !matches!(data[at], b'<' | b'[' | b']' | b'(' | b'/') {
                    at += 1;
                }
                // A '/' starts a name: it is not a keyword of the mapping.
                if data.get(start) != Some(&b'/') {
                    items.push(Item::Word(String::from_utf8_lossy(&data[start..at]).into_owned()));
                }
                at = at.max(start + 1);
            }
            _ => at += 1,
        }
    }
    items
}

fn hex_bytes(digits: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(digits.len() / 2 + 1);
    let mut high: Option<u8> = None;
    for &digit in digits {
        let Some(value) = char::from(digit).to_digit(16) else { continue };
        let value = u8::try_from(value).unwrap_or(0);
        match high {
            Some(top) => {
                out.push(top * 16 + value);
                high = None;
            }
            None => high = Some(value),
        }
    }
    if let Some(top) = high {
        out.push(top * 16);
    }
    out
}

fn code(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0_u32, |code, byte| (code << 8) | u32::from(*byte))
}

/// A CMap destination: UTF-16BE units, or the bytes themselves when they are not whole units.
fn text_of(bytes: &[u8]) -> String {
    if !bytes.len().is_multiple_of(2) || bytes.is_empty() {
        return bytes.iter().map(|&byte| pdfdoc_char(byte)).collect();
    }
    let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().map(|pair| u16::from_be_bytes(*pair)).collect();
    String::from_utf16_lossy(&units)
}

/// The destination of a `bfrange` entry, one code along.
fn shifted(base: &str, offset: u32) -> String {
    let mut characters: Vec<u32> = base.chars().map(u32::from).collect();
    if let Some(last) = characters.last_mut() {
        *last = last.saturating_add(offset);
    }
    characters.into_iter().filter_map(char::from_u32).collect()
}

/// The content-stream scanner: operators in, text out, chunk by chunk.
struct Scanner<'f> {
    fonts: &'f [Arc<Font>],
    names: &'f HashMap<Vec<u8>, usize>,
    current: Option<usize>,
    mode: Mode,
    token: Vec<u8>,
    string: Vec<u8>,
    /// Nesting of the string being read: a `(` inside a string is part of it.
    depth: usize,
    escaped: bool,
    octal: u8,
    octal_value: u16,
    /// The text of the strings seen since the last operator.
    pending: String,
    /// The text being built inside a `[ … ]` array, for `TJ`.
    array: Option<String>,
    /// Whether the last number in an array was a gap wide enough to be a space.
    gap: bool,
    last_number: f64,
    last_name: Vec<u8>,
    text: String,
    unmapped: bool,
    truncated: bool,
    /// `/XObject` names the page draws, whose text belongs to the page too.
    forms: Vec<Vec<u8>>,
}

/// Where the tokenizer is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Outside,
    String,
    Hex,
    Name,
    Number,
    Word,
}

impl<'f> Scanner<'f> {
    fn new(fonts: &'f [Arc<Font>], names: &'f HashMap<Vec<u8>, usize>) -> Self {
        Self {
            fonts,
            names,
            current: None,
            mode: Mode::Outside,
            token: Vec::new(),
            string: Vec::new(),
            depth: 1,
            escaped: false,
            octal: 0,
            octal_value: 0,
            pending: String::new(),
            array: None,
            gap: false,
            last_number: 0.0,
            last_name: Vec::new(),
            text: String::new(),
            unmapped: false,
            truncated: false,
            forms: Vec::new(),
        }
    }

    fn into_text(mut self) -> String {
        self.end();
        self.text
    }

    /// Scan one window of a content stream. The tokenizer keeps its state, so a token split
    /// across two windows is read on the second one.
    fn feed(&mut self, bytes: &[u8]) {
        let mut at = 0;
        while at < bytes.len() {
            let byte = bytes[at];
            match self.mode {
                Mode::Outside => match byte {
                    b if is_white(b) => at += 1,
                    b'%' => {
                        while at < bytes.len() && bytes[at] != b'\n' && bytes[at] != b'\r' {
                            at += 1;
                        }
                    }
                    b'(' => {
                        self.mode = Mode::String;
                        self.depth = 1;
                        self.escaped = false;
                        self.octal = 0;
                        self.string.clear();
                        at += 1;
                    }
                    b'<' => {
                        // A dictionary in a marked-content operator: skip both brackets.
                        if bytes.get(at + 1) == Some(&b'<') {
                            at += 2;
                        } else {
                            self.mode = Mode::Hex;
                            self.string.clear();
                            at += 1;
                        }
                    }
                    b']' => {
                        self.end_array();
                        at += 1;
                    }
                    b'[' => {
                        self.array = Some(String::new());
                        at += 1;
                    }
                    b'/' => {
                        self.mode = Mode::Name;
                        self.token.clear();
                        at += 1;
                    }
                    b'0'..=b'9' | b'+' | b'-' | b'.' => {
                        self.mode = Mode::Number;
                        self.token.clear();
                        self.token.push(byte);
                        at += 1;
                    }
                    b'A'..=b'Z' | b'a'..=b'z' | b'\'' | b'"' => {
                        self.mode = Mode::Word;
                        self.token.clear();
                        self.token.push(byte);
                        at += 1;
                    }
                    _ => at += 1,
                },
                Mode::String => {
                    at += 1;
                    if self.octal > 0 {
                        if let b'0'..=b'7' = byte {
                            self.octal_value = self.octal_value * 8 + u16::from(byte - b'0');
                            self.octal -= 1;
                            if self.octal == 0 {
                                self.string.push(u8::try_from(self.octal_value & 0xff).unwrap_or(0));
                            }
                            continue;
                        }
                        self.string.push(u8::try_from(self.octal_value & 0xff).unwrap_or(0));
                        self.octal = 0;
                    }
                    if self.escaped {
                        self.escaped = false;
                        match byte {
                            b'n' => self.string.push(b'\n'),
                            b'r' => self.string.push(b'\r'),
                            b't' => self.string.push(b'\t'),
                            b'b' => self.string.push(8),
                            b'f' => self.string.push(12),
                            b'0'..=b'7' => {
                                self.octal_value = u16::from(byte - b'0');
                                self.octal = 2;
                            }
                            // A backslash before an end of line continues the string across it.
                            b'\r' => {
                                if bytes.get(at) == Some(&b'\n') {
                                    at += 1;
                                }
                            }
                            b'\n' => {}
                            other => self.string.push(other),
                        }
                        continue;
                    }
                    match byte {
                        b'\\' => self.escaped = true,
                        b'(' => {
                            self.depth += 1;
                            self.string.push(byte);
                        }
                        b')' => {
                            self.depth -= 1;
                            if self.depth == 0 {
                                let string = std::mem::take(&mut self.string);
                                self.mode = Mode::Outside;
                                self.add_string(&string);
                            } else {
                                self.string.push(byte);
                            }
                        }
                        other => self.string.push(other),
                    }
                }
                Mode::Hex => {
                    at += 1;
                    match byte {
                        b'>' => {
                            let string = std::mem::take(&mut self.string);
                            self.mode = Mode::Outside;
                            self.add_string(&hex_bytes(&string));
                        }
                        b if is_white(b) => {}
                        other => self.string.push(other),
                    }
                }
                Mode::Name | Mode::Number | Mode::Word => {
                    let ends = is_white(byte) || matches!(byte, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%');
                    if ends {
                        self.finish_token();
                    } else {
                        self.token.push(byte);
                        at += 1;
                    }
                }
            }
        }
    }

    /// The text of everything scanned so far.
    fn end(&mut self) {
        self.finish_token();
    }

    fn finish_token(&mut self) {
        let token = std::mem::take(&mut self.token);
        match self.mode {
            Mode::Name => {
                self.last_name = token;
                self.mode = Mode::Outside;
            }
            Mode::Number => {
                if let Ok(value) = std::str::from_utf8(&token).unwrap_or("").parse::<f64>() {
                    self.last_number = value;
                    if self.array.is_some() && value <= -180.0 {
                        self.gap = true;
                    }
                }
                self.mode = Mode::Outside;
            }
            Mode::Word => {
                self.mode = Mode::Outside;
                self.operator(&token);
            }
            _ => self.mode = Mode::Outside,
        }
    }

    fn operator(&mut self, token: &[u8]) {
        match token {
            b"Tf" => {
                self.current = self.names.get(self.last_name.as_slice()).copied();
                self.last_name.clear();
            }
            b"Tj" | b"TJ" | b"'" | b"\"" => {
                // The array of a `TJ` has already been gathered into `pending` by its `]`.
                let pending = std::mem::take(&mut self.pending);
                self.show(&pending);
                self.last_name.clear();
            }
            b"Do" => {
                if !self.last_name.is_empty() && self.forms.len() < 4096 {
                    self.forms.push(std::mem::take(&mut self.last_name));
                }
            }
            // A new line or a new position: words on either side are separate words.
            b"Td" | b"TD" | b"T*" | b"Tm" => self.separate(),
            b"ET" => {
                self.separate();
                self.current = None;
            }
            _ => {
                self.pending.clear();
                self.last_name.clear();
            }
        }
    }

    /// A string of character codes shown by the page: decode it and keep the text.
    fn add_string(&mut self, raw: &[u8]) {
        let (decoded, mapped) = match self.current.and_then(|index| self.fonts.get(index)) {
            Some(font) => font.decode(raw),
            None => (raw.iter().map(|&byte| pdfdoc_char(byte)).collect(), true),
        };
        if !mapped {
            self.unmapped = true;
        }
        match self.array.as_mut() {
            Some(array) => {
                if std::mem::take(&mut self.gap) {
                    array.push(' ');
                }
                array.push_str(&decoded);
            }
            None => self.pending.push_str(&decoded),
        }
    }

    fn end_array(&mut self) {
        if let Some(array) = self.array.take() {
            self.pending.push_str(&array);
        }
    }

    /// Put text into the page's own string, within the bound a page may cost.
    fn show(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let room = MAX_TEXT.saturating_sub(self.text.len());
        if text.len() > room {
            self.truncated = true;
            let end = floor_char_boundary(text, room);
            self.text.push_str(&text[..end]);
            return;
        }
        self.text.push_str(text);
    }

    /// A gap between two blocks of text: a space, unless one is already there.
    fn separate(&mut self) {
        self.pending.clear();
        if !self.text.is_empty() && !self.text.ends_with(' ') {
            self.text.push(' ');
        }
    }
}

/// The largest index at or below `at` that is a character boundary of `text`.
fn floor_char_boundary(text: &str, at: usize) -> usize {
    let mut index = at.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}
