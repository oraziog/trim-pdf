//! Document structure: the page tree and the outline (bookmarks).
//!
//! Both walks are depth- and cycle-guarded, and the maps they build are bounded by the number of
//! pages or bookmarks — never by the size of the file. A 6 GB scan with no bookmarks is never
//! walked at all: its page count comes from the page tree's own `/Count`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::error::Error;
use crate::object::{Dict, ObjRef, Object};
use crate::xref::Reader;

/// A bookmark: its title, its depth in the outline, and the page it opens (1-based).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bookmark {
    pub title: String,
    pub level: usize,
    pub page: Option<usize>,
}

/// Deepest page-tree or outline nesting accepted.
const MAX_DEPTH: usize = 64;
/// Most pages one walk may visit.
const MAX_PAGES: usize = 5_000_000;
/// Most bookmarks read.
const MAX_BOOKMARKS: usize = 200_000;
/// Most visited nodes remembered while walking (cycle guard).
const MAX_VISITED: usize = 4_000_000;
/// Most named destinations collected.
const MAX_NAMED_DESTS: usize = 200_000;
/// The attributes a page may inherit from the page tree above it (ISO 32000-1 Table 29).
/// A page written into a part is moved to a flat tree, so it must carry them itself.
pub(crate) const INHERITABLE: [&[u8]; 4] = [b"Resources", b"MediaBox", b"CropBox", b"Rotate"];
/// Page trees up to this size are walked, so the count is exact even when `/Count` is wrong.
/// Larger ones trust `/Count`: a scanned 6 GB document can hold hundreds of thousands of pages,
/// and walking each of them would cost one read per page for no gain.
const WALK_LIMIT: usize = 20_000;

/// The `/Pages` node the catalog names.
fn pages_root(reader: &mut Reader<'_>, root: ObjRef) -> Result<ObjRef, Error> {
    let catalog = reader.get(root)?;
    let dict = catalog.as_dict().ok_or_else(|| Error::Corrupt("la voce /Root del trailer non è un dizionario".into()))?;
    dict.get_ref(b"Pages").ok_or_else(|| Error::Corrupt("il catalogo non ha la voce /Pages".into()))
}

/// The number of pages in the document.
pub(crate) fn page_count(reader: &mut Reader<'_>, root: ObjRef) -> Result<usize, Error> {
    let pages = pages_root(reader, root)?;
    let node = reader.get(pages)?;
    let dict = node.as_dict().ok_or_else(|| Error::Corrupt("la radice dell'albero delle pagine non è un dizionario".into()))?;
    let declared = dict.get_int(b"Count").and_then(|value| usize::try_from(value).ok());
    if let Some(count) = declared {
        // A wrong /Count would silently produce wrong parts, so small trees are counted exactly.
        if (1..=WALK_LIMIT as i64).contains(&(count as i64)) {
            let mut visited = HashSet::new();
            let exact = walk_count(reader, pages, 0, &mut visited)?;
            if exact > 0 {
                return Ok(exact);
            }
        }
        if count > 0 {
            return Ok(count);
        }
    }
    let mut visited = HashSet::new();
    match walk_count(reader, pages, 0, &mut visited)? {
        0 => Err(Error::Corrupt("l'albero delle pagine non contiene pagine".into())),
        count => Ok(count),
    }
}

/// Count the leaf pages under a node, walking the tree in order.
fn walk_count(reader: &mut Reader<'_>, node: ObjRef, depth: usize, visited: &mut HashSet<ObjRef>) -> Result<usize, Error> {
    if depth > MAX_DEPTH {
        return Err(Error::Limit(format!("l'albero delle pagine è annidato oltre {MAX_DEPTH} livelli")));
    }
    if visited.len() >= MAX_VISITED {
        return Err(Error::Limit("l'albero delle pagine visita più nodi di quanti ne consenta il limite di sicurezza".into()));
    }
    if !visited.insert(node) {
        // A cycle, or a node shared by two parents: it was already counted.
        return Ok(0);
    }
    let value = reader.get(node)?;
    let Some(dict) = value.as_dict() else { return Ok(0) };
    let Some(kids) = dict.get_array(b"Kids") else {
        return Ok(usize::from(is_page(dict)));
    };
    let mut total = 0_usize;
    for kid in kids {
        if let Some(reference) = kid.as_ref() {
            total = total.saturating_add(walk_count(reader, reference, depth + 1, visited)?);
        }
        if total > MAX_PAGES {
            return Err(Error::Limit(format!("il documento ha più di {MAX_PAGES} pagine")));
        }
    }
    Ok(total)
}

/// Whether a node without `/Kids` is a page (a leaf) rather than a damaged intermediate node.
fn is_page(dict: &Dict) -> bool {
    match dict.get_name(b"Type") {
        Some(name) => name == b"Page",
        // Lenient, as viewers are: a leaf with no /Type is a page.
        None => true,
    }
}

/// One page of the document: its object, and the attributes it inherits from the tree above it.
#[derive(Clone, Debug)]
pub(crate) struct PageEntry {
    pub(crate) reference: ObjRef,
    /// `/Resources`, `/MediaBox`, `/CropBox` and `/Rotate` taken from the ancestors that hold
    /// them, for the keys the page itself does not have.
    ///
    /// Shared: the same values are inherited by every page under a node, and a scanned document
    /// with a hundred thousand pages must not hold a hundred thousand copies of one `/Resources`.
    pub(crate) inherited: Arc<Dict>,
}

/// Every page, in document order, with what each one inherits.
///
/// The one full walk of the tree: a split needs every page in order, and a bookmark's
/// destination becomes a page number through the same order.
pub(crate) fn page_list(reader: &mut Reader<'_>, root: ObjRef) -> Result<Vec<PageEntry>, Error> {
    let pages = pages_root(reader, root)?;
    let mut list = Vec::new();
    let mut visited = HashSet::new();
    collect_page_list(reader, pages, &Arc::new(Dict::new()), 0, &mut visited, &mut list)?;
    if list.is_empty() {
        return Err(Error::Corrupt("l'albero delle pagine non contiene pagine".into()));
    }
    Ok(list)
}

#[allow(clippy::too_many_arguments)]
fn collect_page_list(
    reader: &mut Reader<'_>,
    node: ObjRef,
    inherited: &Arc<Dict>,
    depth: usize,
    visited: &mut HashSet<ObjRef>,
    list: &mut Vec<PageEntry>,
) -> Result<(), Error> {
    if depth > MAX_DEPTH {
        return Err(Error::Limit(format!("l'albero delle pagine è annidato oltre {MAX_DEPTH} livelli")));
    }
    if !visited.insert(node) {
        return Ok(());
    }
    let value = reader.get(node)?;
    let Some(dict) = value.as_dict() else { return Ok(()) };
    // A copy is made only by a node that adds an attribute: the pages under it then share that
    // one dictionary instead of carrying a copy each.
    let mut carried = Arc::clone(inherited);
    for key in INHERITABLE {
        if let Some(value) = dict.get(key) {
            Arc::make_mut(&mut carried).insert(key.to_vec(), value.clone());
        }
    }
    let Some(kids) = dict.get_array(b"Kids") else {
        if !is_page(dict) {
            return Ok(());
        }
        if list.len() >= MAX_PAGES {
            return Err(Error::Limit(format!("il documento ha più di {MAX_PAGES} pagine")));
        }
        // The page's own entries are written after these and win, so the whole inherited set can
        // be shared as it is.
        list.push(PageEntry { reference: node, inherited: Arc::clone(&carried) });
        return Ok(());
    };
    let kids = kids.to_vec();
    for kid in &kids {
        if let Some(reference) = kid.as_ref() {
            collect_page_list(reader, reference, &carried, depth + 1, visited, list)?;
        }
    }
    Ok(())
}

/// Map every page to its 0-based index, in the order [`page_list`] returns.
pub(crate) fn page_map(reader: &mut Reader<'_>, root: ObjRef) -> Result<HashMap<ObjRef, usize>, Error> {
    Ok(page_list(reader, root)?.into_iter().enumerate().map(|(index, page)| (page.reference, index)).collect())
}

/// Every bookmark in the document, in outline order, with its depth.
pub(crate) fn bookmarks(reader: &mut Reader<'_>, root: ObjRef, pages: &HashMap<ObjRef, usize>) -> Result<Vec<Bookmark>, Error> {
    let value = reader.get(root)?;
    let Some(catalog) = value.as_dict() else { return Ok(Vec::new()) };
    let outlines = match catalog.get(b"Outlines") {
        None | Some(Object::Null) => return Ok(Vec::new()),
        Some(Object::Ref(reference)) => {
            let node = reader.get(*reference)?;
            match node.as_dict() {
                Some(dict) => dict.clone(),
                None => return Ok(Vec::new()),
            }
        }
        Some(Object::Dict(dict)) => dict.clone(),
        _ => return Ok(Vec::new()),
    };
    let Some(first) = outlines.get_ref(b"First") else { return Ok(Vec::new()) };
    let mut names = NamedDestinations { root, map: None };
    let mut out = Vec::new();
    let mut visited = HashSet::new();
    walk_outline(reader, first, 0, &mut out, pages, &mut names, &mut visited)?;
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn walk_outline(
    reader: &mut Reader<'_>,
    first: ObjRef,
    level: usize,
    out: &mut Vec<Bookmark>,
    pages: &HashMap<ObjRef, usize>,
    names: &mut NamedDestinations,
    visited: &mut HashSet<ObjRef>,
) -> Result<(), Error> {
    if level > MAX_DEPTH {
        return Err(Error::Limit(format!("l'indice è annidato oltre {MAX_DEPTH} livelli")));
    }
    let mut current = Some(first);
    while let Some(reference) = current {
        if out.len() >= MAX_BOOKMARKS {
            return Err(Error::Limit(format!("l'indice contiene più di {MAX_BOOKMARKS} voci")));
        }
        if !visited.insert(reference) {
            break;
        }
        let value = reader.get(reference)?;
        let Some(dict) = value.as_dict() else { break };
        let title = dict.get(b"Title").and_then(Object::as_string).map(pdf_text).unwrap_or_default();
        let page = destination(reader, dict, pages, names)?;
        out.push(Bookmark { title, level, page });
        if let Some(child) = dict.get_ref(b"First") {
            walk_outline(reader, child, level + 1, out, pages, names, visited)?;
        }
        current = dict.get_ref(b"Next");
    }
    Ok(())
}

/// The page an outline item points at, from `/Dest` or from a `/GoTo` action.
fn destination(reader: &mut Reader<'_>, item: &Dict, pages: &HashMap<ObjRef, usize>, names: &mut NamedDestinations) -> Result<Option<usize>, Error> {
    if let Some(dest) = item.get(b"Dest") {
        return resolve_destination(reader, dest, pages, names);
    }
    let Some(action) = item.get(b"A") else { return Ok(None) };
    let action = reader.resolve(action)?;
    let Some(dict) = action.as_dict() else { return Ok(None) };
    // Only a GoTo action moves inside this document; a URI or a remote go-to does not.
    if dict.get_name(b"S") != Some(b"GoTo") {
        return Ok(None);
    }
    match dict.get(b"D") {
        Some(dest) => resolve_destination(reader, dest, pages, names),
        None => Ok(None),
    }
}

/// A destination: an array whose first entry is a page, or a name resolved in the destinations
/// of the catalog (`/Dests` or the `/Names /Dests` tree).
fn resolve_destination(
    reader: &mut Reader<'_>,
    dest: &Object,
    pages: &HashMap<ObjRef, usize>,
    names: &mut NamedDestinations,
) -> Result<Option<usize>, Error> {
    match dest {
        Object::Array(items) => Ok(page_number(pages, items.first())),
        // A named destination is a name; producers also write it as a string, and both are
        // looked up in the catalog's destinations.
        Object::Name(name) | Object::String(name) => {
            let Some(value) = names.lookup(reader, name)? else { return Ok(None) };
            match value {
                Object::Array(items) => Ok(page_number(pages, items.first())),
                Object::Dict(dict) => match dict.get(b"D") {
                    Some(inner) => {
                        let inner = inner.clone();
                        resolve_destination(reader, &inner, pages, names)
                    }
                    None => Ok(None),
                },
                _ => Ok(None),
            }
        }
        Object::Ref(reference) => {
            let target = reader.get(*reference)?;
            match target.as_dict() {
                // A destination dictionary, or an array behind a reference.
                Some(dict) => match dict.get(b"D") {
                    Some(inner) => {
                        let inner = inner.clone();
                        resolve_destination(reader, &inner, pages, names)
                    }
                    None => Ok(None),
                },
                None => Ok(None),
            }
        }
        _ => Ok(None),
    }
}

/// The 1-based page a destination's first entry names, when it is a page of this document.
fn page_number(pages: &HashMap<ObjRef, usize>, first: Option<&Object>) -> Option<usize> {
    let reference = first.and_then(Object::as_ref)?;
    pages.get(&reference).map(|index| index + 1)
}

/// The catalog's named destinations, read at most once per walk.
struct NamedDestinations {
    /// The catalog they are read from.
    root: ObjRef,
    map: Option<HashMap<Vec<u8>, Object>>,
}

impl NamedDestinations {
    fn lookup(&mut self, reader: &mut Reader<'_>, name: &[u8]) -> Result<Option<Object>, Error> {
        if self.map.is_none() {
            self.map = Some(collect_named_destinations(reader, self.root)?);
        }
        Ok(self.map.as_ref().and_then(|map| map.get(name).cloned()))
    }
}

fn collect_named_destinations(reader: &mut Reader<'_>, root: ObjRef) -> Result<HashMap<Vec<u8>, Object>, Error> {
    let mut map = HashMap::new();
    let value = reader.get(root)?;
    let Some(catalog) = value.as_dict() else { return Ok(map) };
    // The old form: /Dests is a dictionary keyed by name.
    if let Some(dests) = catalog.get(b"Dests") {
        let dests = reader.resolve(dests)?;
        if let Some(dict) = dests.as_dict() {
            for (key, value) in dict.iter() {
                if map.len() >= MAX_NAMED_DESTS {
                    return Err(Error::Limit("il documento ha più destinazioni con nome di quante ne consenta il limite di sicurezza".into()));
                }
                map.insert(key.to_vec(), value.clone());
            }
        }
    }
    // The modern form: /Names /Dests is a name tree.
    let names = catalog.get(b"Names").and_then(Object::as_dict).and_then(|dict| dict.get(b"Dests")).cloned();
    if let Some(names) = names {
        let names = reader.resolve(&names)?;
        if let Some(dict) = names.as_dict() {
            let dict = dict.clone();
            let mut visited = HashSet::new();
            collect_name_tree(reader, &dict, 0, &mut visited, &mut map)?;
        }
    }
    Ok(map)
}

fn collect_name_tree(
    reader: &mut Reader<'_>,
    node: &Dict,
    depth: usize,
    visited: &mut HashSet<ObjRef>,
    map: &mut HashMap<Vec<u8>, Object>,
) -> Result<(), Error> {
    if depth > MAX_DEPTH {
        return Err(Error::Limit(format!("l'albero delle destinazioni è annidato oltre {MAX_DEPTH} livelli")));
    }
    if let Some(pairs) = node.get_array(b"Names") {
        let mut items = pairs.iter();
        while let (Some(key), Some(value)) = (items.next(), items.next()) {
            if map.len() >= MAX_NAMED_DESTS {
                return Err(Error::Limit("il documento ha più destinazioni con nome di quante ne consenta il limite di sicurezza".into()));
            }
            if let Some(bytes) = key.as_string() {
                map.insert(bytes.to_vec(), value.clone());
                // A tree key is a string; store its text too, since an outline may name it either way.
                map.insert(pdf_text(bytes).into_bytes(), value.clone());
            }
        }
    }
    if let Some(kids) = node.get_array(b"Kids") {
        let kids = kids.to_vec();
        for kid in &kids {
            let Some(reference) = kid.as_ref() else { continue };
            if !visited.insert(reference) {
                continue;
            }
            let child = reader.get(reference)?;
            if let Some(dict) = child.as_dict() {
                let dict = dict.clone();
                collect_name_tree(reader, &dict, depth + 1, visited, map)?;
            }
        }
    }
    Ok(())
}

/// Decode a PDF text string: UTF-16BE with a BOM, UTF-8 with a BOM, or PDFDocEncoding.
pub(crate) fn pdf_text(bytes: &[u8]) -> String {
    match bytes {
        [0xFE, 0xFF, rest @ ..] => {
            let units: Vec<u16> = rest.as_chunks::<2>().0.iter().map(|pair| u16::from_be_bytes(*pair)).collect();
            String::from_utf16_lossy(&units)
        }
        [0xEF, 0xBB, 0xBF, rest @ ..] => String::from_utf8_lossy(rest).into_owned(),
        other => match std::str::from_utf8(other) {
            Ok(text) if !other.is_ascii() => text.to_owned(),
            _ => other.iter().map(|&byte| pdfdoc_char(byte)).collect(),
        },
    }
}

/// One PDFDocEncoding byte as a character. The range 0x80–0x9F differs from Latin-1; the rest
/// matches it, and the table here covers the positions real titles use.
pub(crate) fn pdfdoc_char(byte: u8) -> char {
    const HIGH: [char; 32] = [
        '\u{2022}', '\u{2020}', '\u{2021}', '\u{2026}', '\u{2014}', '\u{2013}', '\u{0192}', '\u{2044}', '\u{2039}', '\u{203A}', '\u{2212}',
        '\u{2030}', '\u{201E}', '\u{201C}', '\u{201D}', '\u{2018}', '\u{2019}', '\u{201A}', '\u{2122}', '\u{FB01}', '\u{FB02}', '\u{0141}',
        '\u{0152}', '\u{0160}', '\u{0178}', '\u{017D}', '\u{0131}', '\u{0142}', '\u{0153}', '\u{0161}', '\u{017E}', '\u{0000}',
    ];
    match byte {
        0x18 => '\u{02D8}',
        0x19 => '\u{02C7}',
        0x1A => '\u{02C6}',
        0x1B => '\u{02D9}',
        0x1C => '\u{02DD}',
        0x1D => '\u{02DB}',
        0x1E => '\u{02DA}',
        0x1F => '\u{02DC}',
        0x80..=0x9F => HIGH[usize::from(byte - 0x80)],
        // 0xA0 and above are Latin-1; below 0x18 the byte is its own character.
        other => char::from(other),
    }
}
