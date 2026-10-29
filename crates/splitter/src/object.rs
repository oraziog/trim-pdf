//! The PDF object model and a parser that reads one object out of a bounded window.
//!
//! Nothing here holds the whole document: an object is parsed from a window read at its own
//! offset, and the window grows only while the object still needs more bytes (up to
//! [`MAX_WINDOW`]). Streams are recognised by their dictionary and left on disk — the parser
//! records their absolute data offset and length instead of copying their bytes.

use crate::error::Error;
use crate::source::Source;

/// Deepest nesting accepted for arrays and dictionaries. Real documents stay far below this;
/// a file that exceeds it is damaged or hostile.
pub(crate) const MAX_DEPTH: usize = 64;
/// First window read for an indirect object; grows ×4 while the object needs more.
pub(crate) const FIRST_WINDOW: usize = 4096;
/// Largest window ever read for one object.
pub(crate) const MAX_WINDOW: usize = 16 << 20;

/// An indirect reference: object number and generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjRef {
    pub num: u32,
    pub generation: u16,
}

impl std::fmt::Display for ObjRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {} R", self.num, self.generation)
    }
}

/// A PDF object (ISO 32000-2 §7.3).
#[derive(Clone, Debug, PartialEq)]
pub enum Object {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    /// A string's raw bytes; whether it is PDFDocEncoding or UTF-16BE is decided on read.
    String(Vec<u8>),
    /// A name without its leading `/` and with `#xx` escapes decoded.
    Name(Vec<u8>),
    Array(Vec<Object>),
    Dict(Dict),
    Stream(StreamRef),
    Ref(ObjRef),
}

impl Object {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Object::Int(v) => Some(*v),
            Object::Real(v) if v.fract() == 0.0 && v.is_finite() => Some(*v as i64),
            _ => None,
        }
    }

    pub fn as_name(&self) -> Option<&[u8]> {
        match self {
            Object::Name(n) => Some(n),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&Dict> {
        match self {
            Object::Dict(d) => Some(d),
            Object::Stream(s) => Some(&s.dict),
            _ => None,
        }
    }

    pub fn as_stream(&self) -> Option<&StreamRef> {
        match self {
            Object::Stream(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_ref(&self) -> Option<ObjRef> {
        match self {
            Object::Ref(r) => Some(*r),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Object]> {
        match self {
            Object::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_string(&self) -> Option<&[u8]> {
        match self {
            Object::String(s) => Some(s),
            _ => None,
        }
    }
}

/// An ordered dictionary: entries keep their order, so a parsed dictionary reads like the file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dict {
    entries: Vec<(Vec<u8>, Object)>,
}

impl Dict {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &[u8]) -> Option<&Object> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn insert(&mut self, key: Vec<u8>, value: Object) {
        match self.entries.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => *v = value,
            None => self.entries.push((key, value)),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&[u8], &Object)> {
        self.entries.iter().map(|(k, v)| (k.as_slice(), v))
    }

    pub fn get_int(&self, key: &[u8]) -> Option<i64> {
        self.get(key).and_then(Object::as_int)
    }

    pub fn get_name(&self, key: &[u8]) -> Option<&[u8]> {
        self.get(key).and_then(Object::as_name)
    }

    pub fn get_array(&self, key: &[u8]) -> Option<&[Object]> {
        self.get(key).and_then(Object::as_array)
    }

    pub fn get_ref(&self, key: &[u8]) -> Option<ObjRef> {
        self.get(key).and_then(Object::as_ref)
    }
}

/// A stream: its dictionary, where its data starts, and its declared length.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamRef {
    pub dict: Dict,
    /// Absolute offset of the first byte of the stream's data.
    pub data_offset: u64,
    /// `/Length` when it is a direct integer; `None` when it is an indirect reference or absent.
    pub length: Option<u64>,
}

/// True for the six PDF white-space bytes (ISO 32000-2 Table 1).
pub(crate) fn is_white(b: u8) -> bool {
    matches!(b, 0 | b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

/// True for a PDF delimiter.
fn is_delimiter(b: u8) -> bool {
    matches!(b, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%')
}

/// Why a window could not be parsed.
#[derive(Debug)]
pub(crate) enum ParseError {
    /// The window ended in the middle of the object: read more bytes and retry.
    Eof,
    /// The bytes are not a PDF object.
    Bad(&'static str),
}

/// A parser over one window. `base` is the window's absolute offset in the file and `file_len`
/// lets it tell "the keyword is cut by the window" from "the keyword cannot exist".
pub(crate) struct Parser<'a> {
    data: &'a [u8],
    pos: usize,
    base: u64,
    file_len: u64,
    depth: usize,
}

enum Token {
    Int(i64),
    Real(f64),
}

impl<'a> Parser<'a> {
    pub(crate) fn new(data: &'a [u8], pos: usize, base: u64, file_len: u64) -> Self {
        Self { data, pos, base, file_len, depth: 0 }
    }

    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    fn byte(&self, at: usize) -> Option<u8> {
        self.data.get(at).copied()
    }

    /// True when `at` is at or past the end of the file (so no more bytes can arrive).
    fn is_past_file_end(&self, at: usize) -> bool {
        self.base.saturating_add(at as u64) >= self.file_len
    }

    pub(crate) fn skip_ws(&mut self) {
        loop {
            match self.byte(self.pos) {
                Some(b) if is_white(b) => self.pos += 1,
                Some(b'%') => {
                    // A comment runs to the end of the line.
                    self.pos += 1;
                    while let Some(b) = self.byte(self.pos) {
                        self.pos += 1;
                        if b == b'\n' || b == b'\r' {
                            break;
                        }
                    }
                }
                _ => return,
            }
        }
    }

    /// Parse one object.
    pub(crate) fn object(&mut self) -> Result<Object, ParseError> {
        self.skip_ws();
        let Some(b) = self.byte(self.pos) else { return Err(ParseError::Eof) };
        match b {
            b'/' => Ok(Object::Name(self.name()?)),
            b'(' => Ok(Object::String(self.literal_string()?)),
            b'[' => self.array(),
            b'<' => match self.byte(self.pos + 1) {
                None if self.is_past_file_end(self.pos + 1) => Err(ParseError::Bad("un '<' senza una stringa esadecimale o un dizionario dopo")),
                None => Err(ParseError::Eof),
                Some(b'<') => self.dictionary(),
                Some(_) => Ok(Object::String(self.hex_string()?)),
            },
            b't' => {
                self.eat_keyword_required(b"true")?;
                Ok(Object::Bool(true))
            }
            b'f' => {
                self.eat_keyword_required(b"false")?;
                Ok(Object::Bool(false))
            }
            b'n' => {
                self.eat_keyword_required(b"null")?;
                Ok(Object::Null)
            }
            b'+' | b'-' | b'.' | b'0'..=b'9' => self.number_or_ref(),
            _ => Err(ParseError::Bad("questo byte non può iniziare un oggetto")),
        }
    }

    fn eat_keyword_required(&mut self, keyword: &[u8]) -> Result<(), ParseError> {
        if self.eat_keyword(keyword)? { Ok(()) } else { Err(ParseError::Bad("un keyword sconosciuto")) }
    }

    /// Consume `keyword` when it starts here, and say whether it did.
    fn eat_keyword(&mut self, keyword: &[u8]) -> Result<bool, ParseError> {
        let end = self.pos.saturating_add(keyword.len());
        match self.data.get(self.pos..end) {
            Some(slice) if slice == keyword => match self.byte(end) {
                Some(b) if is_white(b) || is_delimiter(b) => {
                    self.pos = end;
                    Ok(true)
                }
                Some(_) => Ok(false),
                None if self.is_past_file_end(end) => {
                    self.pos = end;
                    Ok(true)
                }
                None => Err(ParseError::Eof),
            },
            Some(_) => Ok(false),
            None if self.is_past_file_end(end) => Ok(false),
            None => Err(ParseError::Eof),
        }
    }

    fn name(&mut self) -> Result<Vec<u8>, ParseError> {
        // The caller saw the '/'.
        self.pos += 1;
        let mut out = Vec::new();
        loop {
            match self.byte(self.pos) {
                Some(b) if is_white(b) || is_delimiter(b) => break,
                Some(b'#') => {
                    let (hi, lo) = (self.byte(self.pos + 1), self.byte(self.pos + 2));
                    match (hi, lo) {
                        (Some(hi), Some(lo)) => match (hex_value(hi), hex_value(lo)) {
                            (Some(hi), Some(lo)) => {
                                out.push(hi * 16 + lo);
                                self.pos += 3;
                            }
                            // A '#' that is not an escape is a literal '#' in real files.
                            _ => {
                                out.push(b'#');
                                self.pos += 1;
                            }
                        },
                        _ if self.is_past_file_end(self.pos + 2) => {
                            out.push(b'#');
                            self.pos += 1;
                        }
                        _ => return Err(ParseError::Eof),
                    }
                }
                Some(b) => {
                    out.push(b);
                    self.pos += 1;
                }
                None if self.is_past_file_end(self.pos) => break,
                None => return Err(ParseError::Eof),
            }
        }
        Ok(out)
    }

    fn literal_string(&mut self) -> Result<Vec<u8>, ParseError> {
        self.pos += 1; // '('
        let mut out = Vec::new();
        let mut nesting = 1_usize;
        loop {
            let Some(b) = self.byte(self.pos) else {
                return if self.is_past_file_end(self.pos) { Err(ParseError::Bad("una stringa non chiusa")) } else { Err(ParseError::Eof) };
            };
            self.pos += 1;
            match b {
                b'\\' => {
                    let Some(esc) = self.byte(self.pos) else {
                        return if self.is_past_file_end(self.pos) { Err(ParseError::Bad("un escape non chiuso")) } else { Err(ParseError::Eof) };
                    };
                    self.pos += 1;
                    match esc {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'(' | b')' | b'\\' => out.push(esc),
                        b'0'..=b'7' => {
                            let mut value = u32::from(esc - b'0');
                            for _ in 0..2 {
                                match self.byte(self.pos) {
                                    Some(d @ b'0'..=b'7') => {
                                        value = value * 8 + u32::from(d - b'0');
                                        self.pos += 1;
                                    }
                                    _ => break,
                                }
                            }
                            out.push(u8::try_from(value & 0xff).unwrap_or(0));
                        }
                        // A backslash before a newline is a line continuation: nothing is added.
                        b'\r' => {
                            if self.byte(self.pos) == Some(b'\n') {
                                self.pos += 1;
                            }
                        }
                        b'\n' => {}
                        other => out.push(other),
                    }
                }
                b'(' => {
                    nesting += 1;
                    out.push(b'(');
                }
                b')' => {
                    nesting -= 1;
                    if nesting == 0 {
                        return Ok(out);
                    }
                    out.push(b')');
                }
                other => out.push(other),
            }
        }
    }

    fn hex_string(&mut self) -> Result<Vec<u8>, ParseError> {
        self.pos += 1; // '<'
        let mut nibbles = Vec::new();
        loop {
            match self.byte(self.pos) {
                Some(b'>') => {
                    self.pos += 1;
                    break;
                }
                Some(b) if is_white(b) => self.pos += 1,
                Some(b) => match hex_value(b) {
                    Some(v) => {
                        nibbles.push(v);
                        self.pos += 1;
                    }
                    None => return Err(ParseError::Bad("una stringa esadecimale con un carattere non esadecimale")),
                },
                None if self.is_past_file_end(self.pos) => return Err(ParseError::Bad("una stringa esadecimale non chiusa")),
                None => return Err(ParseError::Eof),
            }
        }
        let mut out = Vec::with_capacity(nibbles.len() / 2 + 1);
        for pair in nibbles.chunks(2) {
            // An odd final digit means the last nibble is the low half of a byte (7.3.4.3).
            let hi = pair[0];
            let lo = pair.get(1).copied().unwrap_or(0);
            out.push(hi * 16 + lo);
        }
        Ok(out)
    }

    fn array(&mut self) -> Result<Object, ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(ParseError::Bad("array annidati oltre il limite di sicurezza"));
        }
        self.pos += 1; // '['
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            match self.byte(self.pos) {
                Some(b']') => {
                    self.pos += 1;
                    self.depth -= 1;
                    return Ok(Object::Array(items));
                }
                Some(_) => items.push(self.object()?),
                None if self.is_past_file_end(self.pos) => return Err(ParseError::Bad("un array non chiuso")),
                None => return Err(ParseError::Eof),
            }
        }
    }

    /// Parse `<< … >>` and, when `stream` follows it, record the stream instead of its data.
    fn dictionary(&mut self) -> Result<Object, ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(ParseError::Bad("dizionari annidati oltre il limite di sicurezza"));
        }
        self.pos += 2; // '<<'
        let mut dict = Dict::new();
        loop {
            self.skip_ws();
            match self.byte(self.pos) {
                None if self.is_past_file_end(self.pos) => return Err(ParseError::Bad("un dizionario non chiuso")),
                None => return Err(ParseError::Eof),
                Some(b'>') => {
                    if self.data.get(self.pos..self.pos + 2) != Some(b">>") {
                        return Err(ParseError::Bad("un '>' che non fa parte di '>>'"));
                    }
                    self.pos += 2;
                    self.depth -= 1;
                    return self.maybe_stream(dict);
                }
                Some(b'/') => {
                    let key = self.name()?;
                    let value = self.object()?;
                    dict.insert(key, value);
                }
                Some(_) => return Err(ParseError::Bad("una chiave di dizionario che non è un nome")),
            }
        }
    }

    /// After a dictionary's `>>`: a `stream` keyword turns it into a stream object.
    ///
    /// The keyword may sit on the next line, so white space (and a comment) is skipped first —
    /// but it is un-skipped again when there is no stream, since the caller's position must not
    /// move past bytes that are not part of the object.
    fn maybe_stream(&mut self, dict: Dict) -> Result<Object, ParseError> {
        let after_dict = self.pos;
        self.skip_ws();
        if !self.eat_keyword(b"stream")? {
            self.pos = after_dict;
            return Ok(Object::Dict(dict));
        }
        // The keyword is followed by CRLF or a single LF (7.3.8.1); be lenient about a bare CR.
        match self.byte(self.pos) {
            Some(b'\r') => {
                self.pos += 1;
                if self.byte(self.pos) == Some(b'\n') {
                    self.pos += 1;
                }
            }
            Some(b'\n') => self.pos += 1,
            Some(_) => {}
            None if self.is_past_file_end(self.pos) => return Err(ParseError::Bad("uno stream senza dati")),
            None => return Err(ParseError::Eof),
        }
        let length = dict.get_int(b"Length").and_then(|v| u64::try_from(v).ok());
        let data_offset = self.base.saturating_add(self.pos as u64);
        Ok(Object::Stream(StreamRef { dict, data_offset, length }))
    }

    /// A number, or an indirect reference when it is `<num> <generation> R`.
    fn number_or_ref(&mut self) -> Result<Object, ParseError> {
        let Some(token) = self.number_token()? else {
            return Err(ParseError::Bad("un numero non leggibile"));
        };
        // A real number is never a reference.
        let first = match token {
            Token::Real(value) => return Ok(Object::Real(value)),
            Token::Int(value) => value,
        };
        if (0..=i64::from(u32::MAX)).contains(&first) {
            self.skip_ws();
            let before_second = self.pos;
            match self.number_token()? {
                Some(Token::Int(generation)) if (0..=i64::from(u16::MAX)).contains(&generation) => {
                    self.skip_ws();
                    if self.eat_keyword(b"R")? {
                        return Ok(Object::Ref(ObjRef { num: first as u32, generation: generation as u16 }));
                    }
                    self.pos = before_second;
                }
                Some(Token::Real(_)) => self.pos = before_second,
                Some(Token::Int(_)) => self.pos = before_second,
                None => self.pos = before_second,
            }
        }
        Ok(Object::Int(first))
    }

    fn number_token(&mut self) -> Result<Option<Token>, ParseError> {
        self.skip_ws();
        let start = self.pos;
        let mut i = start;
        if matches!(self.byte(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let digital_start = i;
        while matches!(self.byte(i), Some(b'0'..=b'9')) {
            i += 1;
        }
        let mut is_real = false;
        if self.byte(i) == Some(b'.') {
            is_real = true;
            i += 1;
            while matches!(self.byte(i), Some(b'0'..=b'9')) {
                i += 1;
            }
        }
        if i == digital_start && self.byte(start) != Some(b'.') {
            return Ok(None);
        }
        // A number cut by the window edge cannot be trusted: ask for more bytes.
        if i == self.data.len() && !self.is_past_file_end(i) {
            return Err(ParseError::Eof);
        }
        let text = std::str::from_utf8(self.data.get(start..i).ok_or(ParseError::Bad("un numero non leggibile"))?)
            .map_err(|_| ParseError::Bad("un numero con byte non ASCII"))?;
        self.pos = i;
        if is_real {
            text.parse::<f64>().map(|v| Some(Token::Real(v))).map_err(|_| ParseError::Bad("un numero reale non leggibile"))
        } else {
            text.parse::<i64>().map(|v| Some(Token::Int(v))).map_err(|_| ParseError::Bad("un intero non leggibile"))
        }
    }
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Parse `num gen obj …` at `offset`, growing the window while the object needs more bytes.
pub(crate) fn read_indirect(source: &Source, offset: u64) -> Result<(ObjRef, Object), Error> {
    let file_len = source.len();
    let mut window = FIRST_WINDOW;
    loop {
        let data = source.read_vec(offset, window)?;
        if data.is_empty() {
            return Err(Error::Corrupt(format!("l'oggetto al byte {offset} è oltre la fine del file")));
        }
        let mut parser = Parser::new(&data, 0, offset, file_len);
        match indirect(&mut parser) {
            Ok(found) => return Ok(found),
            Err(ParseError::Eof) => {
                let reached_file_end = offset.saturating_add(data.len() as u64) >= file_len;
                if reached_file_end || window >= MAX_WINDOW {
                    return Err(Error::Corrupt(format!("l'oggetto al byte {offset} è incompleto")));
                }
                window = window.saturating_mul(4).min(MAX_WINDOW);
            }
            Err(ParseError::Bad(detail)) => return Err(Error::Corrupt(format!("l'oggetto al byte {offset} è malformato: {detail}"))),
        }
    }
}

fn indirect(parser: &mut Parser<'_>) -> Result<(ObjRef, Object), ParseError> {
    parser.skip_ws();
    let Some(Token::Int(num)) = parser.number_token()? else {
        return Err(ParseError::Bad("un oggetto che non inizia con un numero di oggetto"));
    };
    parser.skip_ws();
    let Some(Token::Int(generation)) = parser.number_token()? else {
        return Err(ParseError::Bad("un oggetto a cui manca il numero di generazione"));
    };
    let num = u32::try_from(num).map_err(|_| ParseError::Bad("un numero di oggetto fuori intervallo"))?;
    let generation = u16::try_from(generation).map_err(|_| ParseError::Bad("una generazione fuori intervallo"))?;
    parser.skip_ws();
    if !parser.eat_keyword(b"obj")? {
        return Err(ParseError::Bad("un oggetto a cui manca il keyword `obj`"));
    }
    let object = parser.object()?;
    Ok((ObjRef { num, generation }, object))
}

/// Parse a single object that has no `num gen obj` header (a member of an object stream).
pub(crate) fn parse_object_in(data: &[u8], base: u64, file_len: u64) -> Result<Object, ParseError> {
    let mut parser = Parser::new(data, 0, base, file_len);
    parser.object()
}

/// Parse one object at `pos` and report the position just after it.
pub(crate) fn parse_at(data: &[u8], pos: usize, base: u64, file_len: u64) -> Result<(Object, usize), ParseError> {
    let mut parser = Parser::new(data, pos, base, file_len);
    let object = parser.object()?;
    Ok((object, parser.pos()))
}

/// The first position at or after `pos` that is neither white space nor inside a comment.
pub(crate) fn skip_ws_at(data: &[u8], pos: usize) -> usize {
    let mut at = pos;
    loop {
        match data.get(at) {
            Some(b) if is_white(*b) => at += 1,
            Some(b'%') => {
                at += 1;
                while let Some(b) = data.get(at) {
                    at += 1;
                    if *b == b'\n' || *b == b'\r' {
                        break;
                    }
                }
            }
            _ => return at,
        }
    }
}

/// An integer at `pos` (white space and comments skipped first) and the position after it.
pub(crate) fn int_at(data: &[u8], pos: usize) -> Option<(i64, usize)> {
    let start = skip_ws_at(data, pos);
    let mut at = start;
    if matches!(data.get(at), Some(b'+' | b'-')) {
        at += 1;
    }
    let digits = at;
    while matches!(data.get(at), Some(b'0'..=b'9')) {
        at += 1;
    }
    if at == digits {
        return None;
    }
    std::str::from_utf8(data.get(start..at)?).ok()?.parse().ok().map(|value| (value, at))
}

/// Whether `keyword` starts at `pos` (white space and comments skipped first).
pub(crate) fn keyword_at(data: &[u8], pos: usize, keyword: &[u8]) -> bool {
    let at = skip_ws_at(data, pos);
    data.get(at..at.saturating_add(keyword.len())) == Some(keyword)
}

/// The bytes of a stream whose `/Length` cannot be used: read until the `endstream` keyword.
///
/// Bounded by `max`, which also bounds the buffer: a stream that never ends is an error, not a
/// growing allocation.
pub(crate) fn scan_stream_data(source: &Source, offset: u64, max: usize) -> Result<Vec<u8>, Error> {
    let length = scan_stream_len(source, offset, max)?;
    source.read_vec(offset, usize::try_from(length).unwrap_or(usize::MAX))
}

/// How long the data of a stream really is, when its `/Length` cannot be used: the position of
/// the `endstream` keyword, found by scanning without ever holding the data in memory.
///
/// `max` bounds the scan, so a stream that never ends is an error rather than a growing read.
pub(crate) fn scan_stream_len(source: &Source, offset: u64, max: usize) -> Result<u64, Error> {
    const NEEDLE: &[u8] = b"endstream";
    const CHUNK: usize = 1 << 20;
    let limit = source.len().min(offset.saturating_add(max as u64).saturating_add(NEEDLE.len() as u64));
    let mut buffer = vec![0_u8; CHUNK];
    // The tail of a read can be the beginning of a keyword split across two reads.
    let mut carry: Vec<u8> = Vec::new();
    // Bytes of the stream that lie before the window currently being searched.
    let mut scanned = 0_u64;
    let mut at = offset;
    while at < limit {
        let take = usize::try_from((limit - at).min(CHUNK as u64)).unwrap_or(CHUNK);
        let read = source.read_into(at, &mut buffer[..take])?;
        if read == 0 {
            break;
        }
        let mut window = std::mem::take(&mut carry);
        window.extend_from_slice(&buffer[..read]);
        if let Some(found) = find(&window, NEEDLE, 0) {
            return data_end(source, offset, scanned.saturating_add(found as u64));
        }
        let keep = NEEDLE.len().saturating_sub(1).min(window.len());
        let searched = window.len() - keep;
        scanned = scanned.saturating_add(searched as u64);
        carry = window.split_off(searched);
        at = at.saturating_add(read as u64);
    }
    Err(Error::Limit(format!("lo stream al byte {offset} non ha endstream entro {max} byte")))
}

/// The data length once `endstream` is found: the end-of-line marker before it is not data,
/// and everything before that marker is, however it looks.
fn data_end(source: &Source, offset: u64, end: u64) -> Result<u64, Error> {
    let byte_at = |at: u64| -> Result<Option<u8>, Error> { Ok(source.read_vec(offset.saturating_add(at), 1)?.first().copied()) };
    match byte_at(end)? {
        _ if end == 0 => Ok(0),
        Some(b'\n') => {
            let before_cr = if end >= 2 { byte_at(end - 2)? == Some(b'\r') } else { false };
            Ok(end - if before_cr { 2 } else { 1 })
        }
        Some(b'\r') => Ok(end - 1),
        _ => Ok(end),
    }
}

/// Whether `keyword` follows at `at`, after any white space: where a stream's data really ends.
pub(crate) fn keyword_after_ws(source: &Source, at: u64, keyword: &[u8]) -> Result<bool, Error> {
    let window = source.read_vec(at, keyword.len() + 16)?;
    let pos = skip_ws_at(&window, 0);
    Ok(window.get(pos..pos.saturating_add(keyword.len())) == Some(keyword))
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let last = haystack.len() - needle.len();
    (from..=last).find(|&i| haystack.get(i..i + needle.len()) == Some(needle))
}
