//! Cross-reference data, object streams, and the reader that resolves one object at a time.
//!
//! Every cross-reference form a real document uses is handled: classic tables, cross-reference
//! streams, hybrid-reference files (`/XRefStm`) and the `/Prev` chain. Objects are read on
//! demand from their offset (or decoded out of their object stream), and every cache is bounded,
//! so memory depends on the caches — never on the size of the file.

use std::collections::HashMap;
use std::sync::Arc;

use pdfcraft_filters::{Filter, Params};

use crate::error::Error;
use crate::object::{self, Dict, ObjRef, Object, ParseError, StreamRef};
use crate::source::Source;

/// Largest cross-reference stream or object stream decoded at once (decompression-bomb bound).
const MAX_DECODED_STREAM: usize = 256 << 20;
/// Largest classic table window read while its subsections are still incomplete.
const MAX_TABLE_WINDOW: usize = 128 << 20;
/// Most revisions followed through `/Prev`.
const MAX_REVISIONS: usize = 4096;
/// Most entries one subsection may declare.
const MAX_SUBSECTION_ENTRIES: i64 = 50_000_000;
/// Parsed objects kept in memory before the cache is dropped wholesale.
const CACHE_OBJECTS: usize = 8192;
/// Decoded object streams kept in memory, in bytes.
const OBJSTM_CACHE_BYTES: usize = 32 << 20;

/// Where an object lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    Free,
    InFile { offset: u64, generation: u16 },
    InStream { container: u32, index: u32 },
}

/// The document's cross-reference index plus the trailer the newest revision carries.
pub(crate) struct Xref {
    entries: HashMap<u32, Entry>,
    trailer: Dict,
}

impl Xref {
    pub(crate) fn entry(&self, num: u32) -> Option<Entry> {
        self.entries.get(&num).copied()
    }

    /// True when the trailer names an `/Encrypt` dictionary: the objects are encrypted.
    pub(crate) fn is_encrypted(&self) -> bool {
        self.trailer.get(b"Encrypt").is_some_and(|v| !matches!(v, Object::Null))
    }

    pub(crate) fn root(&self) -> Option<ObjRef> {
        self.trailer.get_ref(b"Root")
    }
}

/// Read the cross-reference data, following `/Prev` back through the file's revisions.
pub(crate) fn read(source: &Source) -> Result<Xref, Error> {
    let mut cursor = start_offset(source)?;
    let mut entries: HashMap<u32, Entry> = HashMap::new();
    let mut trailer = Dict::new();
    let mut visited: Vec<u64> = Vec::new();
    for _ in 0..MAX_REVISIONS {
        if visited.contains(&cursor) {
            break;
        }
        visited.push(cursor);
        let head = source.read_vec(cursor, 8)?;
        let (found, dict) = if object::keyword_at(&head, 0, b"xref") { classic(source, cursor)? } else { cross_reference_stream(source, cursor)? };
        // The newest revision wins: its entry for a number replaces older ones.
        for (num, entry) in found {
            entries.entry(num).or_insert(entry);
        }
        // The newest trailer carries /Size; older ones may be the only place /Root appears.
        for (key, value) in dict.iter() {
            if trailer.get(key).is_none() {
                trailer.insert(key.to_vec(), value.clone());
            }
        }
        // A hybrid file keeps compressed entries in the stream named by /XRefStm.
        if let Some(offset) = dict.get_int(b"XRefStm").and_then(|v| u64::try_from(v).ok())
            && offset < source.len()
            && !visited.contains(&offset)
            && let Ok((found, dict)) = cross_reference_stream(source, offset)
        {
            for (num, entry) in found {
                entries.entry(num).or_insert(entry);
            }
            for (key, value) in dict.iter() {
                if trailer.get(key).is_none() {
                    trailer.insert(key.to_vec(), value.clone());
                }
            }
        }
        match dict.get_int(b"Prev").and_then(|v| u64::try_from(v).ok()) {
            Some(previous) if previous < source.len() => cursor = previous,
            _ => break,
        }
    }
    Ok(Xref { entries, trailer })
}

/// The offset `startxref` names, read from the tail of the file.
fn start_offset(source: &Source) -> Result<u64, Error> {
    const MAX_TAIL: usize = 1 << 16;
    let mut window = 4096_usize;
    loop {
        let len = usize::try_from(source.len().min(window as u64)).unwrap_or(usize::MAX);
        let start = source.len() - len as u64;
        let tail = source.read_vec(start, len)?;
        if let Some(at) = rfind(&tail, b"startxref")
            && let Some((value, _)) = object::int_at(&tail, at + b"startxref".len())
            && let Ok(offset) = u64::try_from(value)
        {
            if offset < source.len() {
                return Ok(offset);
            }
            return Err(Error::Corrupt(format!("startxref punta al byte {offset}, oltre la fine del file")));
        }
        if len == usize::try_from(source.len()).unwrap_or(usize::MAX) || window >= MAX_TAIL {
            return Err(Error::Corrupt("nessun startxref negli ultimi 64 KiB del file".into()));
        }
        window = window.saturating_mul(4).min(MAX_TAIL);
    }
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).rev().find(|&i| haystack.get(i..i + needle.len()) == Some(needle))
}

/// Parse a classic cross-reference table and the trailer that follows it.
fn classic(source: &Source, offset: u64) -> Result<(Vec<(u32, Entry)>, Dict), Error> {
    let mut window = 1 << 16;
    loop {
        let data = source.read_vec(offset, window)?;
        match classic_in(&data, offset, source.len()) {
            Ok(parsed) => return Ok(parsed),
            Err(ParseError::Eof) => {
                let at_end = offset.saturating_add(data.len() as u64) >= source.len();
                if at_end || window >= MAX_TABLE_WINDOW {
                    return Err(Error::Corrupt(format!("la tabella cross-reference al byte {offset} è incompleta")));
                }
                window = window.saturating_mul(4).min(MAX_TABLE_WINDOW);
            }
            Err(ParseError::Bad(detail)) => {
                return Err(Error::Corrupt(format!("la tabella cross-reference al byte {offset} è malformata: {detail}")));
            }
        }
    }
}

fn classic_in(data: &[u8], base: u64, file_len: u64) -> Result<(Vec<(u32, Entry)>, Dict), ParseError> {
    let at = object::skip_ws_at(data, 0);
    if !object::keyword_at(data, at, b"xref") {
        return Err(ParseError::Bad("a section that is neither an xref table nor a cross-reference stream"));
    }
    let mut at = object::skip_ws_at(data, at + 4);
    let mut entries: Vec<(u32, Entry)> = Vec::new();
    loop {
        if object::keyword_at(data, at, b"trailer") {
            let dict_at = object::skip_ws_at(data, at + b"trailer".len());
            return match object::parse_at(data, dict_at, base, file_len) {
                Ok((Object::Dict(dict), _)) => Ok((entries, dict)),
                Ok(_) => Err(ParseError::Bad("a trailer that is not a dictionary")),
                Err(e) => Err(e),
            };
        }
        let Some((start, next)) = object::int_at(data, at) else {
            return Err(if base + at as u64 >= file_len { ParseError::Bad("an xref subsection header") } else { ParseError::Eof });
        };
        let Some((count, mut cursor)) = object::int_at(data, next) else {
            return Err(if base + next as u64 >= file_len { ParseError::Bad("an xref subsection count") } else { ParseError::Eof });
        };
        if !(0..=MAX_SUBSECTION_ENTRIES).contains(&count) {
            return Err(ParseError::Bad("an xref subsection with an impossible entry count"));
        }
        let start = u32::try_from(start).map_err(|_| ParseError::Bad("an xref subsection that starts past the object-number range"))?;
        for index in 0..count {
            let Some((entry, after)) = entry_at(data, cursor) else {
                return Err(if base + cursor as u64 >= file_len { ParseError::Bad("an xref entry") } else { ParseError::Eof });
            };
            let num = start.saturating_add(u32::try_from(index).unwrap_or(u32::MAX));
            entries.push((num, entry));
            cursor = after;
        }
        at = object::skip_ws_at(data, cursor);
    }
}

/// One 20-byte table entry: `nnnnnnnnnn ggggg n|f`.
fn entry_at(data: &[u8], at: usize) -> Option<(Entry, usize)> {
    let (offset, after_offset) = object::int_at(data, at)?;
    let (generation, after_generation) = object::int_at(data, after_offset)?;
    let kind_at = object::skip_ws_at(data, after_generation);
    let entry = match data.get(kind_at)? {
        b'n' => Entry::InFile { offset: u64::try_from(offset).ok()?, generation: u16::try_from(generation).unwrap_or(0) },
        b'f' => Entry::Free,
        _ => return None,
    };
    let mut after = kind_at + 1;
    while matches!(data.get(after), Some(b' ' | b'\r' | b'\n')) {
        after += 1;
    }
    Some((entry, after))
}

/// Parse a cross-reference stream (PDF 1.5+) and its own trailer dictionary.
fn cross_reference_stream(source: &Source, offset: u64) -> Result<(Vec<(u32, Entry)>, Dict), Error> {
    let (_, object) = object::read_indirect(source, offset)?;
    let stream = object.as_stream().ok_or_else(|| Error::Corrupt(format!("la cross-reference al byte {offset} non è uno stream")))?.clone();
    let dict = stream.dict.clone();
    let raw = raw_stream_direct(source, &stream, MAX_DECODED_STREAM)?;
    let chain = chain_direct(&dict)?;
    let data = pdfcraft_filters::decode(&chain, &raw, MAX_DECODED_STREAM)
        .map_err(|e| Error::Corrupt(format!("la cross-reference stream al byte {offset}: {e}")))?;

    let widths = field_widths(&dict)?;
    let size = dict.get_int(b"Size").unwrap_or(0);
    let mut sections: Vec<(i64, i64)> = Vec::new();
    match dict.get_array(b"Index") {
        Some(items) => {
            let mut pairs = items.iter().filter_map(Object::as_int);
            while let (Some(start), Some(count)) = (pairs.next(), pairs.next()) {
                sections.push((start, count));
            }
        }
        None => sections.push((0, size)),
    }

    let mut entries: Vec<(u32, Entry)> = Vec::new();
    let mut cursor = 0_usize;
    for (start, count) in sections {
        if !(0..=MAX_SUBSECTION_ENTRIES).contains(&count) {
            return Err(Error::Corrupt("una cross-reference stream con un numero di voci impossibile".into()));
        }
        for index in 0..count {
            let kind = if widths.0 == 0 { 1 } else { read_field(&data, &mut cursor, widths.0)? };
            let field2 = read_field(&data, &mut cursor, widths.1)?;
            let field3 = read_field(&data, &mut cursor, widths.2)?;
            let num = u32::try_from(start.saturating_add(index)).map_err(|_| Error::Corrupt("un numero di oggetto fuori intervallo".into()))?;
            let entry = match kind {
                0 => Entry::Free,
                1 => Entry::InFile { offset: field2, generation: u16::try_from(field3).unwrap_or(0) },
                2 => Entry::InStream { container: u32::try_from(field2).unwrap_or(u32::MAX), index: u32::try_from(field3).unwrap_or(u32::MAX) },
                _ => Entry::Free,
            };
            if entry != Entry::Free {
                entries.push((num, entry));
            }
        }
    }
    Ok((entries, dict))
}

/// `/W [w0 w1 w2]`: the byte width of each field of a cross-reference stream entry.
fn field_widths(dict: &Dict) -> Result<(usize, usize, usize), Error> {
    let widths = dict.get_array(b"W").ok_or_else(|| Error::Corrupt("una cross-reference stream senza /W".into()))?;
    let mut values = widths.iter().map(|v| v.as_int().unwrap_or(0));
    let (w0, w1, w2) = (values.next().unwrap_or(1), values.next().unwrap_or(0), values.next().unwrap_or(0));
    let width = |v: i64| -> Result<usize, Error> {
        if (0..=8).contains(&v) { Ok(v as usize) } else { Err(Error::Corrupt(format!("una larghezza di campo /W di {v} byte"))) }
    };
    Ok((width(w0)?, width(w1)?, width(w2)?))
}

fn read_field(data: &[u8], cursor: &mut usize, width: usize) -> Result<u64, Error> {
    if width == 0 {
        return Ok(0);
    }
    let slice = data
        .get(*cursor..cursor.saturating_add(width))
        .ok_or_else(|| Error::Corrupt("una cross-reference stream che finisce dentro una voce".into()))?;
    *cursor += width;
    Ok(slice.iter().fold(0_u64, |value, byte| (value << 8) | u64::from(*byte)))
}

/// The filter chain of a stream whose dictionary is fully direct (a cross-reference stream).
fn chain_direct(dict: &Dict) -> Result<Vec<(Filter, Params)>, Error> {
    let filter = dict.get(b"Filter");
    let parms = dict.get(b"DecodeParms");
    chain_from(filter, parms)
}

/// Build a filter chain from already-resolved `/Filter` and `/DecodeParms` values.
fn chain_from(filter: Option<&Object>, parms: Option<&Object>) -> Result<Vec<(Filter, Params)>, Error> {
    let names: Vec<Vec<u8>> = match filter {
        None | Some(Object::Null) => Vec::new(),
        Some(Object::Name(name)) => vec![name.clone()],
        Some(Object::Array(items)) => items
            .iter()
            .map(|item| item.as_name().map(<[u8]>::to_vec).ok_or_else(|| Error::Corrupt("un array /Filter che non è fatto di nomi".into())))
            .collect::<Result<_, Error>>()?,
        _ => return Err(Error::Corrupt("un /Filter che non è né un nome né un array di nomi".into())),
    };
    let parm_list: Vec<Option<&Dict>> = match parms {
        None | Some(Object::Null) => Vec::new(),
        Some(Object::Dict(dict)) => vec![Some(dict)],
        Some(Object::Array(items)) => items.iter().map(Object::as_dict).collect(),
        _ => return Err(Error::Corrupt("un /DecodeParms che non è né un dizionario né un array".into())),
    };
    Ok(names
        .into_iter()
        .enumerate()
        .map(|(index, name)| {
            let params = parm_list.get(index).copied().flatten();
            let get = |key: &[u8], default: i64| params.and_then(|d| d.get_int(key)).unwrap_or(default);
            let params = Params {
                predictor: get(b"Predictor", 1),
                colors: get(b"Colors", 1),
                bits_per_component: get(b"BitsPerComponent", 8),
                columns: get(b"Columns", 1),
                early_change: get(b"EarlyChange", 1),
            };
            (Filter::from_name(&name), params)
        })
        .collect())
}

/// A stream's raw bytes, using `/Length` when it is a direct integer and scanning otherwise.
fn raw_stream_direct(source: &Source, stream: &StreamRef, max: usize) -> Result<Vec<u8>, Error> {
    match stream.length {
        Some(length) => {
            let length = usize::try_from(length).map_err(|_| Error::Limit(format!("uno stream di {length} byte")))?;
            if length > max {
                return Err(Error::Limit(format!("uno stream di {length} byte (il limite è {max})")));
            }
            source.read_vec(stream.data_offset, length)
        }
        None => object::scan_stream_data(source, stream.data_offset, max),
    }
}

/// A decoded object stream as the cache holds it: its number, its bytes, its header pairs.
type CachedObjStm = (u32, Arc<Vec<u8>>, Arc<Vec<(u32, u32)>>);

/// An object stream as the reader hands it out: its bytes and its `(object number, offset)` pairs,
/// where each offset is absolute (already shifted by `/First`).
type ObjStmContents = (Arc<Vec<u8>>, Arc<Vec<(u32, u32)>>);

/// Parsed objects and decoded object streams, all bounded.
pub(crate) struct Cache {
    objects: HashMap<u32, Arc<Object>>,
    objstms: Vec<CachedObjStm>,
    objstm_bytes: usize,
}

impl Cache {
    pub(crate) fn new() -> Self {
        Self { objects: HashMap::new(), objstms: Vec::new(), objstm_bytes: 0 }
    }
}

/// Resolves objects, one at a time, out of the file or out of an object stream.
pub(crate) struct Reader<'a> {
    source: &'a Source,
    xref: &'a Xref,
    cache: &'a mut Cache,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(source: &'a Source, xref: &'a Xref, cache: &'a mut Cache) -> Self {
        Self { source, xref, cache }
    }

    /// The object an indirect reference names; a free or missing number reads as `null`, which is
    /// what a viewer shows for a dangling reference.
    pub(crate) fn get(&mut self, reference: ObjRef) -> Result<Arc<Object>, Error> {
        if let Some(cached) = self.cache.objects.get(&reference.num) {
            return Ok(Arc::clone(cached));
        }
        let value = match self.xref.entry(reference.num) {
            None | Some(Entry::Free) => Arc::new(Object::Null),
            Some(Entry::InFile { offset, .. }) => {
                let (found, value) = object::read_indirect(self.source, offset)?;
                // An index that points at a different object would hand back the wrong bytes
                // without a word, so the header at the offset is checked against the entry.
                if found.num != reference.num {
                    return Err(Error::Corrupt(format!(
                        "l'oggetto {} non è al byte {offset}, dove lo mette la cross-reference: a quel byte c'è l'oggetto {}",
                        reference.num, found.num
                    )));
                }
                Arc::new(value)
            }
            Some(Entry::InStream { container, index }) => {
                let (data, pairs) = self.objstm(container)?;
                let (member, start) = pairs.get(index as usize).copied().ok_or_else(|| {
                    Error::Corrupt(format!(
                        "l'oggetto {} è la voce {index} dell'object stream {container}, che ne contiene {}",
                        reference.num,
                        pairs.len()
                    ))
                })?;
                if member != reference.num {
                    return Err(Error::Corrupt(format!(
                        "l'oggetto {} è la voce {index} dell'object stream {container}, che lì elenca l'oggetto {member}",
                        reference.num
                    )));
                }
                let end = pairs.get(index as usize + 1).map(|(_, offset)| *offset).unwrap_or(data.len() as u32);
                let slice = data
                    .get(start as usize..end as usize)
                    .ok_or_else(|| Error::Corrupt(format!("l'oggetto {} è fuori dall'object stream {container}", reference.num)))?;
                let parsed = object::parse_object_in(slice, 0, slice.len() as u64)
                    .map_err(|e| Error::Corrupt(format!("l'oggetto {} nell'object stream {container}: {}", reference.num, describe(&e))))?;
                Arc::new(parsed)
            }
        };
        if self.cache.objects.len() >= CACHE_OBJECTS {
            self.cache.objects.clear();
        }
        self.cache.objects.insert(reference.num, Arc::clone(&value));
        Ok(value)
    }

    /// The file the objects come from, for the reads that stream rather than resolve.
    pub(crate) fn source(&self) -> &Source {
        self.source
    }

    /// Follow one indirect reference, when the value is one.
    pub(crate) fn resolve(&mut self, value: &Object) -> Result<Arc<Object>, Error> {
        match value {
            Object::Ref(reference) => self.get(*reference),
            other => Ok(Arc::new(other.clone())),
        }
    }

    /// Where a stream's data really starts and how long it is: `/Length` when the `endstream`
    /// keyword follows it, the keyword's own position otherwise.
    ///
    /// The length matters when a stream is copied into a part: the copy must be the data, not a
    /// byte more, and producers do write a `/Length` that is a little off.
    pub(crate) fn stream_layout(&mut self, stream: &StreamRef) -> Result<(u64, u64), Error> {
        let declared = match stream.length {
            Some(length) => Some(length),
            None => match stream.dict.get(b"Length") {
                Some(Object::Ref(reference)) => {
                    let reference = *reference;
                    self.get(reference)?.as_int().and_then(|value| u64::try_from(value).ok())
                }
                other => other.and_then(Object::as_int).and_then(|value| u64::try_from(value).ok()),
            },
        };
        if let Some(length) = declared {
            let end = stream.data_offset.saturating_add(length);
            if end <= self.source.len() && object::keyword_after_ws(self.source, end, b"endstream")? {
                return Ok((stream.data_offset, length));
            }
        }
        let length = object::scan_stream_len(self.source, stream.data_offset, usize::MAX)?;
        Ok((stream.data_offset, length))
    }

    /// An object stream's decoded bytes and its `(object number, offset)` header pairs.
    fn objstm(&mut self, container: u32) -> Result<ObjStmContents, Error> {
        if let Some((_, data, pairs)) = self.cache.objstms.iter().find(|(num, _, _)| *num == container) {
            // Both are shared, not copied: a page inside a thousand-member stream would otherwise
            // copy the header pairs a thousand times.
            return Ok((Arc::clone(data), Arc::clone(pairs)));
        }
        let stream = self
            .get(ObjRef { num: container, generation: 0 })?
            .as_stream()
            .ok_or_else(|| Error::Corrupt(format!("l'object stream {container} non è uno stream")))?
            .clone();
        let decoded = self.decoded_stream(&stream, MAX_DECODED_STREAM)?;
        let count = stream.dict.get_int(b"N").and_then(|v| usize::try_from(v).ok()).unwrap_or(0);
        // The header's offsets are relative to `/First`, the offset of the first object's data.
        let first = stream.dict.get_int(b"First").and_then(|v| u32::try_from(v).ok()).unwrap_or(0);
        let mut pairs = Vec::with_capacity(count.min(1 << 16));
        let mut at = 0_usize;
        for _ in 0..count {
            let Some((num, next)) = object::int_at(&decoded, at) else { break };
            let Some((offset, next)) = object::int_at(&decoded, next) else { break };
            match (u32::try_from(num), u32::try_from(offset)) {
                // An offset past the decoded data would slice nothing: such a member reads as absent.
                (Ok(num), Ok(offset)) => pairs.push((num, first.saturating_add(offset))),
                _ => break,
            }
            at = next;
        }
        let data = Arc::new(decoded);
        let pairs = Arc::new(pairs);
        self.cache.objstm_bytes += data.len();
        self.cache.objstms.push((container, Arc::clone(&data), Arc::clone(&pairs)));
        while self.cache.objstm_bytes > OBJSTM_CACHE_BYTES && self.cache.objstms.len() > 1 {
            if let Some((_, dropped, _)) = self.cache.objstms.first() {
                let dropped = dropped.len();
                self.cache.objstm_bytes = self.cache.objstm_bytes.saturating_sub(dropped);
            }
            self.cache.objstms.remove(0);
        }
        Ok((data, pairs))
    }

    /// A stream's decoded bytes: `/Length` (direct or indirect, else scanned), then its filters.
    pub(crate) fn decoded_stream(&mut self, stream: &StreamRef, max: usize) -> Result<Vec<u8>, Error> {
        let raw = match stream.length {
            Some(length) => {
                let length = usize::try_from(length).map_err(|_| Error::Limit(format!("uno stream di {length} byte")))?;
                if length > max {
                    return Err(Error::Limit(format!("uno stream di {length} byte (il limite è {max})")));
                }
                self.source.read_vec(stream.data_offset, length)?
            }
            None => match stream.dict.get(b"Length") {
                Some(Object::Ref(_)) => {
                    let length = self.resolve(stream.dict.get(b"Length").unwrap_or(&Object::Null))?.as_int();
                    match length.and_then(|v| usize::try_from(v).ok()) {
                        Some(length) if length <= max => self.source.read_vec(stream.data_offset, length)?,
                        Some(length) => return Err(Error::Limit(format!("uno stream di {length} byte (il limite è {max})"))),
                        None => object::scan_stream_data(self.source, stream.data_offset, max)?,
                    }
                }
                _ => object::scan_stream_data(self.source, stream.data_offset, max)?,
            },
        };
        let filter = stream.dict.get(b"Filter").map(|value| self.resolve(value)).transpose()?;
        let parms = stream.dict.get(b"DecodeParms").map(|value| self.resolve(value)).transpose()?;
        let chain = chain_from(filter.as_deref(), parms.as_deref())?;
        if chain.iter().any(|(filter, _)| filter.is_image_codec()) {
            return Err(Error::Corrupt(format!("lo stream al byte {} usa un codec per immagini, che resta codificato", stream.data_offset)));
        }
        pdfcraft_filters::decode(&chain, &raw, max).map_err(|e| Error::Corrupt(format!("lo stream al byte {}: {e}", stream.data_offset)))
    }
}

/// The message of a parse failure, for a `Corrupt` error that names the object.
pub(crate) fn describe(error: &ParseError) -> String {
    match error {
        ParseError::Eof => "its bytes are incomplete".to_string(),
        ParseError::Bad(detail) => (*detail).to_string(),
    }
}
