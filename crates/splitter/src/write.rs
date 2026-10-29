//! Writing a part: objects renumbered, streams copied byte for byte, a new cross-reference.
//!
//! Nothing of the source is held in memory. A part is planned first — every object its pages
//! reach, numbered afresh — and then written object by object, each stream streaming straight
//! from the input file to the output one. What is kept is the dictionaries: a page's content
//! stream, three gigabytes of it, goes past without ever being in a buffer of its own.
//!
//! A part holds what its pages reach and nothing else of the source: their resources, fonts,
//! images and annotations, the outlines that point at them, and the catalog entries that would
//! break without them.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::Bookmark;
use crate::error::Error;
use crate::format::{Compression, Version};
use crate::object::{Dict, ObjRef, Object};
use crate::progress::{Job, Stage, Unit};
use crate::source::Source;
use crate::structure::PageEntry;
use crate::xref::Reader;

/// Bytes copied at a time while a stream is written: the stream is never held whole in memory.
const COPY_CHUNK: usize = 4 << 20;
/// The output buffer, at least the 8 MB the specification asks for.
const OUT_BUFFER: usize = 8 << 20;
/// The largest offset a classic cross-reference table can address: ten decimal digits.
pub(crate) const MAX_CLASSIC_OFFSET: u64 = 9_999_999_999;
/// The bytes after `%PDF-x.y` that mark a file as binary for transfer programs.
const BINARY_MARKER: &[u8] = b"%\xE2\xE3\xCF\xD3\n";

/// A planned output object: bytes already renumbered, or a stream still in the source file.
enum OutObject {
    Value(Object),
    Stream { dict: Dict, offset: u64, length: u64 },
}

/// A part that is fully planned: every object numbered, with its page tree and its outline.
pub(crate) struct PlannedPart {
    objects: BTreeMap<u32, OutObject>,
    catalog: u32,
    bookmarks: usize,
    /// What the source had and this part does not carry.
    notes: Vec<String>,
}

/// Catalog entries a part cannot carry: their objects name pages, or describe the document as a
/// whole instead of the pages in it.
const LOST_CATALOG: [&[u8]; 4] = [b"PageLabels", b"Names", b"StructTreeRoot", b"Metadata"];

impl PlannedPart {
    /// How many bookmarks the part carries.
    pub(crate) fn bookmarks(&self) -> usize {
        self.bookmarks
    }
}

/// Plan one part: every object its pages reach, renumbered, plus its page tree and outline.
///
/// `selected` holds 0-based indices into `pages`, in the order the pages are written.
pub(crate) fn build_part(
    reader: Reader<'_>,
    root: ObjRef,
    pages: &[PageEntry],
    selected: &[usize],
    bookmarks: &[Bookmark],
    keep_bookmarks: bool,
) -> Result<PlannedPart, Error> {
    let mut builder = PartBuilder::new(reader);
    // The catalog and the page tree take the first numbers, then the pages: a part then reads
    // the way a document does.
    let catalog = builder.fresh();
    let pages_root = builder.fresh();
    let mut numbers = Vec::with_capacity(selected.len());
    for &index in selected {
        let page = pages.get(index).ok_or_else(|| Error::Invalid(format!("la pagina {} è oltre la fine del documento", index + 1)))?;
        numbers.push(builder.intern(page.reference));
        builder.hold_page(page.reference.num);
    }
    for (&index, &number) in selected.iter().zip(&numbers) {
        let entry = &pages[index];
        let value = builder.reader.get(entry.reference)?;
        let page = value
            .as_dict()
            .ok_or_else(|| Error::Corrupt(format!("l'oggetto {} è nell'albero delle pagine ma non è una pagina", entry.reference.num)))?;
        let dict = builder.page_dict(page, entry, pages_root)?;
        builder.put(number, OutObject::Value(Object::Dict(dict)));
    }
    // Everything those pages reach, and then the tree they hang from.
    builder.drain()?;
    builder.put(pages_root, OutObject::Value(Object::Dict(pages_tree(&numbers))));
    let outline = if keep_bookmarks { builder.outline(bookmarks, &positions(selected, &numbers)) } else { None };
    let mut catalog_dict = Dict::new();
    catalog_dict.insert(b"Type".to_vec(), Object::Name(b"Catalog".to_vec()));
    catalog_dict.insert(b"Pages".to_vec(), Object::Ref(ObjRef { num: pages_root, generation: 0 }));
    if let Some((outlines, _)) = outline {
        catalog_dict.insert(b"Outlines".to_vec(), Object::Ref(ObjRef { num: outlines, generation: 0 }));
    }
    // A form's field tree, a print profile's colour intent and the layer configuration reach
    // objects the pages use too: carrying them over keeps widgets attached to their fields, keeps
    // the colour intent beside the content that needs it, and keeps the layers as the source had
    // them — a layer the source hid must not come back visible in every part.
    let source = builder.reader.get(root)?;
    for key in [&b"AcroForm"[..], &b"OutputIntents"[..], &b"OCProperties"[..]] {
        let Some(value) = source.as_dict().and_then(|dict| dict.get(key)).cloned() else { continue };
        let copied = builder.value(&value)?;
        if !matches!(copied, Object::Null) {
            catalog_dict.insert(key.to_vec(), copied);
        }
    }
    // Whatever those entries reach is planned now: a layer's own object, say, is referenced from
    // nowhere else, and would otherwise be numbered and never written.
    builder.drain()?;
    let mut notes = Vec::new();
    if let Some(dict) = source.as_dict() {
        let lost: Vec<String> = LOST_CATALOG
            .iter()
            .filter(|key| dict.get(key).is_some_and(|value| !matches!(value, Object::Null)))
            .map(|key| format!("/{}", String::from_utf8_lossy(key)))
            .collect();
        if !lost.is_empty() {
            notes.push(format!("il sorgente ha {}: le parti non li portano", lost.join(", ")));
        }
    }
    builder.put(catalog, OutObject::Value(Object::Dict(catalog_dict)));
    Ok(PlannedPart { objects: builder.objects, catalog, bookmarks: outline.map_or(0, |(_, count)| count), notes })
}

/// The `/Pages` node of a part: its pages as direct children, in order.
fn pages_tree(numbers: &[u32]) -> Dict {
    let mut dict = Dict::new();
    dict.insert(b"Type".to_vec(), Object::Name(b"Pages".to_vec()));
    dict.insert(b"Kids".to_vec(), Object::Array(numbers.iter().map(|num| reference(*num)).collect()));
    dict.insert(b"Count".to_vec(), Object::Int(i64::try_from(numbers.len()).unwrap_or(i64::MAX)));
    dict
}

/// Where each page of the document went in this part.
fn positions(selected: &[usize], numbers: &[u32]) -> HashMap<usize, u32> {
    selected.iter().copied().zip(numbers.iter().copied()).collect()
}

/// A reference to one of the part's own objects: everything written here is generation 0.
fn reference(num: u32) -> Object {
    Object::Ref(ObjRef { num, generation: 0 })
}

/// An outline item while the part is still being planned.
struct Node {
    number: u32,
    page: u32,
    title: Vec<u8>,
    parent: Option<usize>,
    children: Vec<usize>,
}

/// Renumbers one part: every object it reaches gets a new number and a planned body.
struct PartBuilder<'a> {
    reader: Reader<'a>,
    /// Object number in the source → object number in this part.
    numbers: HashMap<u32, u32>,
    next: u32,
    /// Objects interned but not planned yet; the queue is what keeps the walk off the stack.
    queue: Vec<(u32, ObjRef)>,
    objects: BTreeMap<u32, OutObject>,
    /// The pages this part holds. Any other page the walk meets is dropped where it stands: its
    /// `/Parent` is the page tree, and the page tree holds every page of the document, with all
    /// their content. One link to a page elsewhere would otherwise pull the whole file in.
    own_pages: HashSet<u32>,
}

impl<'a> PartBuilder<'a> {
    fn new(reader: Reader<'a>) -> Self {
        Self { reader, numbers: HashMap::new(), next: 1, queue: Vec::new(), objects: BTreeMap::new(), own_pages: HashSet::new() }
    }

    /// Remember that this part holds `page`, so a reference to it is followed rather than cut.
    fn hold_page(&mut self, page: u32) {
        self.own_pages.insert(page);
    }

    /// A number no object has taken yet, for something the part adds itself.
    fn fresh(&mut self) -> u32 {
        let number = self.next;
        self.next = self.next.saturating_add(1);
        number
    }

    /// The new number of a source object, planning it the first time it is seen.
    fn intern(&mut self, reference: ObjRef) -> u32 {
        if let Some(&number) = self.numbers.get(&reference.num) {
            return number;
        }
        let number = self.fresh();
        self.numbers.insert(reference.num, number);
        self.queue.push((number, reference));
        number
    }

    fn put(&mut self, number: u32, object: OutObject) {
        self.objects.insert(number, object);
    }

    /// Resolve and plan every object interned so far, and everything those reach in turn. The
    /// queue keeps the walk flat, so a file whose objects refer to each other in a long chain
    /// cannot run the stack out.
    fn drain(&mut self) -> Result<(), Error> {
        while let Some((number, reference)) = self.queue.pop() {
            // A page was planned as this part writes it, not as the source holds it.
            if self.objects.contains_key(&number) {
                continue;
            }
            let value = self.reader.get(reference)?;
            // A page this part does not hold is dropped before its dictionary is copied: copying
            // it would intern its `/Parent`, that is the whole page tree, and with the tree every
            // page of the document. A destination pointing here reads as a null, which is what a
            // viewer shows for a link that goes nowhere.
            if !self.own_pages.contains(&reference.num) && is_page_object(&value) {
                self.objects.insert(number, OutObject::Value(Object::Null));
                continue;
            }
            match &*value {
                Object::Stream(stream) => {
                    let stream = stream.clone();
                    let (offset, length) = self.reader.stream_layout(&stream)?;
                    // The length is the number of bytes this part actually copies, not what the
                    // source declared: it is the one value an output must get right.
                    let mut dict = self.dict(&stream.dict, &[b"Length"])?;
                    dict.insert(b"Length".to_vec(), Object::Int(i64::try_from(length).unwrap_or(i64::MAX)));
                    self.objects.insert(number, OutObject::Stream { dict, offset, length });
                }
                other => {
                    let copied = self.value(other)?;
                    self.objects.insert(number, OutObject::Value(copied));
                }
            }
        }
        Ok(())
    }

    /// A value with every reference renumbered.
    fn value(&mut self, value: &Object) -> Result<Object, Error> {
        Ok(match value {
            Object::Array(items) => Object::Array(items.iter().map(|item| self.value(item)).collect::<Result<Vec<_>, _>>()?),
            Object::Dict(dict) => Object::Dict(self.dict(dict, &[])?),
            Object::Ref(target) => reference(self.intern(*target)),
            // A stream is always an indirect object: one inside a dictionary would have no
            // number for a reader to reach it by.
            Object::Stream(_) => return Err(Error::Corrupt("uno stream usato come oggetto diretto".to_string())),
            other => other.clone(),
        })
    }

    /// Whether an object is a page of the document that this part does not hold.
    fn is_foreign_page(&mut self, reference: ObjRef) -> Result<bool, Error> {
        if self.own_pages.contains(&reference.num) {
            return Ok(false);
        }
        let value = self.reader.get(reference)?;
        Ok(is_page_object(&value))
    }

    /// True when a destination names a page this part does not hold.
    fn destination_leaves(&mut self, destination: &Object) -> Result<bool, Error> {
        let resolved = self.reader.resolve(destination)?;
        let Object::Array(items) = &*resolved else { return Ok(false) };
        let Some(page) = items.first().and_then(Object::as_ref) else { return Ok(false) };
        self.is_foreign_page(page)
    }

    /// True when an action is a `/GoTo` into a page this part does not hold.
    fn action_leaves(&mut self, action: &Object) -> Result<bool, Error> {
        let resolved = self.reader.resolve(action)?;
        let Some(dict) = resolved.as_dict() else { return Ok(false) };
        if dict.get_name(b"S") != Some(b"GoTo") {
            return Ok(false);
        }
        let Some(destination) = dict.get(b"D").cloned() else { return Ok(false) };
        self.destination_leaves(&destination)
    }

    /// True when a `/Parent` points at a page-tree node. A part builds its own tree, and following
    /// the source's would carry every page of the document into the part.
    fn parent_is_tree(&mut self, value: &Object) -> Result<bool, Error> {
        let Object::Ref(target) = value else { return Ok(false) };
        let node = self.reader.get(*target)?;
        let Some(dict) = node.as_dict() else { return Ok(false) };
        Ok(dict.get_name(b"Type") == Some(b"Pages") || (dict.get_name(b"Type") != Some(b"Page") && dict.get_array(b"Kids").is_some()))
    }

    /// A copy of `dict` with every reference renumbered, minus the keys the part replaces and the
    /// ones that would point outside it.
    fn dict(&mut self, dict: &Dict, skip: &[&[u8]]) -> Result<Dict, Error> {
        let mut out = Dict::new();
        for (key, value) in dict.iter() {
            if skip.contains(&key) {
                continue;
            }
            // A destination into another part is dropped, not written as a dead link: the array
            // would hold a null where a page belongs, and a reader would complain.
            if key == b"Dest" && self.destination_leaves(value)? {
                continue;
            }
            if key == b"A" && self.action_leaves(value)? {
                continue;
            }
            if key == b"Parent" && self.parent_is_tree(value)? {
                continue;
            }
            let copied = self.value(value)?;
            out.insert(key.to_vec(), copied);
        }
        Ok(out)
    }

    /// The page as the part writes it: its own entries, the attributes it inherited, and this
    /// part's page tree as its parent.
    fn page_dict(&mut self, page: &Dict, entry: &PageEntry, pages_root: u32) -> Result<Dict, Error> {
        let mut dict = Dict::new();
        dict.insert(b"Type".to_vec(), Object::Name(b"Page".to_vec()));
        dict.insert(b"Parent".to_vec(), reference(pages_root));
        // A page whose resources or box live on an ancestor would lose them in a flat tree.
        for (key, value) in entry.inherited.iter() {
            let copied = self.value(value)?;
            dict.insert(key.to_vec(), copied);
        }
        for (key, value) in page.iter() {
            if key == b"Parent" || key == b"Type" {
                continue;
            }
            let copied = self.value(value)?;
            dict.insert(key.to_vec(), copied);
        }
        Ok(dict)
    }

    /// The outline of the part: the bookmarks whose pages it holds, in their original nesting.
    /// `None` when no kept bookmark points into this part.
    fn outline(&mut self, bookmarks: &[Bookmark], positions: &HashMap<usize, u32>) -> Option<(u32, usize)> {
        // A bookmark whose page is elsewhere would point at a page that is not in this file.
        let kept: Vec<(usize, u32, Vec<u8>)> = bookmarks
            .iter()
            .filter_map(|bookmark| {
                let page = bookmark.page.and_then(|page| positions.get(&page.saturating_sub(1)).copied())?;
                Some((bookmark.level, page, encode_text(&bookmark.title)))
            })
            .collect();
        if kept.is_empty() {
            return None;
        }
        let root = self.fresh();
        let mut nodes: Vec<Node> = Vec::with_capacity(kept.len());
        let mut stack: Vec<(usize, usize)> = Vec::new();
        let mut roots: Vec<usize> = Vec::new();
        for (level, page, title) in kept {
            // A level deeper than the one before hangs from it; a level that comes back up
            // closes everything below it.
            while stack.last().is_some_and(|&(open, _)| open >= level) {
                stack.pop();
            }
            let parent = stack.last().map(|&(_, index)| index);
            let index = nodes.len();
            nodes.push(Node { number: self.fresh(), page, title, parent, children: Vec::new() });
            match parent {
                Some(parent) => nodes[parent].children.push(index),
                None => roots.push(index),
            }
            stack.push((level, index));
        }
        // An item's `/Count` is how many items are open below it. A child always has a larger
        // index than its parent, so one pass from the end settles them all.
        let mut counts = vec![0_usize; nodes.len()];
        for index in (0..nodes.len()).rev() {
            counts[index] = nodes[index].children.iter().fold(0_usize, |total, &child| total.saturating_add(counts[child].saturating_add(1)));
        }
        for index in 0..nodes.len() {
            let node = &nodes[index];
            let siblings: &[usize] = node.parent.map_or(&roots, |parent| &nodes[parent].children);
            let at = siblings.iter().position(|&sibling| sibling == index).unwrap_or(0);
            let parent = node.parent.map_or(root, |parent| nodes[parent].number);
            let mut dict = Dict::new();
            dict.insert(b"Title".to_vec(), Object::String(node.title.clone()));
            dict.insert(b"Parent".to_vec(), reference(parent));
            // The destination is the page itself: a part moves its pages, so a coordinate the
            // source carried would no longer point where it did.
            dict.insert(b"Dest".to_vec(), Object::Array(vec![reference(node.page), Object::Name(b"Fit".to_vec())]));
            if let (Some(&first), Some(&last)) = (node.children.first(), node.children.last()) {
                dict.insert(b"First".to_vec(), reference(nodes[first].number));
                dict.insert(b"Last".to_vec(), reference(nodes[last].number));
            }
            if counts[index] > 0 {
                dict.insert(b"Count".to_vec(), Object::Int(i64::try_from(counts[index]).unwrap_or(i64::MAX)));
            }
            if let Some(&next) = siblings.get(at.saturating_add(1)) {
                dict.insert(b"Next".to_vec(), reference(nodes[next].number));
            }
            if at > 0
                && let Some(&previous) = siblings.get(at - 1)
            {
                dict.insert(b"Prev".to_vec(), reference(nodes[previous].number));
            }
            self.put(node.number, OutObject::Value(Object::Dict(dict)));
        }
        let total = roots.iter().fold(0_usize, |sum, &index| sum.saturating_add(counts[index].saturating_add(1)));
        let mut dict = Dict::new();
        dict.insert(b"Type".to_vec(), Object::Name(b"Outlines".to_vec()));
        if let (Some(&first), Some(&last)) = (roots.first(), roots.last()) {
            dict.insert(b"First".to_vec(), reference(nodes[first].number));
            dict.insert(b"Last".to_vec(), reference(nodes[last].number));
            dict.insert(b"Count".to_vec(), Object::Int(i64::try_from(total).unwrap_or(i64::MAX)));
        }
        self.put(root, OutObject::Value(Object::Dict(dict)));
        Some((root, nodes.len()))
    }
}

/// A bookmark title as PDF bytes: ASCII stays readable in the file, anything else becomes
/// UTF-16BE with its byte-order mark, which any reader of a 1.5 document understands.
fn encode_text(text: &str) -> Vec<u8> {
    if text.is_ascii() {
        return text.as_bytes().to_vec();
    }
    let mut bytes = vec![0xFE, 0xFF];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_be_bytes());
    }
    bytes
}

/// Where a part sits in the whole job: what the progress numbers are measured against.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Placement {
    /// Bytes the parts before this one already wrote.
    pub before: u64,
    /// The size of the whole source, which is what the parts copy.
    pub total: u64,
    /// This part's place in the sequence, 1-based.
    pub part: usize,
    pub parts: usize,
}

/// How a part is written: what the caller asked for, and the limit a classic table has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Settings {
    pub compression: Compression,
    pub version: Version,
    /// The largest offset a classic cross-reference table can hold: ten decimal digits.
    pub classic_limit: u64,
}

/// What writing one part produced.
pub(crate) struct Written {
    pub bytes: u64,
    pub version: Version,
    /// Things worth telling the person about this part.
    pub notes: Vec<String>,
}

/// Where one object of a part lives in the output.
#[derive(Clone, Copy, Debug)]
enum Entry {
    /// As an object of its own, at this offset.
    Offset(u64),
    /// Inside an object stream, at this index.
    Packed { stream: u32, index: u32 },
}

/// Ten digits, so the length of a compressed stream can be written over them once it is known.
const LENGTH_PLACEHOLDER: &[u8] = b"0000000000";

/// Write a planned part to `path`, which the caller gives as a temporary name: it is renamed into
/// place once the part has been read back.
pub(crate) fn write(
    source: &Source,
    plan: &PlannedPart,
    settings: &Settings,
    path: &Path,
    job: &Job,
    placement: Placement,
) -> Result<Written, Error> {
    let file = File::create(path).map_err(|error| Error::io(path, error))?;
    let mut out = PartOut::new(file, path, job, placement);
    let max = plan.objects.keys().next_back().copied().unwrap_or(0);
    if plan.objects.len() != usize::try_from(max).unwrap_or(usize::MAX) {
        return Err(Error::Corrupt("il piano ha un buco nella numerazione degli oggetti".to_string()));
    }

    // What the output must be able to say about itself: what its pages need, and what an object
    // stream or a cross-reference stream needs.
    let mut version = settings.version;
    let mut notes = Vec::new();
    if let Some(needed) = plan.required_version()
        && needed > version
    {
        notes.push(format!("le pagine richiedono la versione PDF {needed}: la parte è scritta in versione PDF {needed}"));
        version = needed;
    }
    let packed = settings.compression.structures_objects();
    let structured = packed || plan.body_size() > settings.classic_limit;
    if structured && version < Version::PDF_1_5 {
        notes.push(format!(
            "un object stream ed una cross-reference stream necessitano della versione PDF 1.5, quindi le parti non saranno scritte nella versione PDF {version}"
        ));
        version = Version::PDF_1_5;
    }
    notes.extend(plan.notes.iter().cloned());

    // The numbers the part takes for itself: the object streams, then the cross-reference stream.
    // One stream holds a few dozen dictionaries, not all of them: a reader that wants one object
    // should not have to decode the whole file to reach it.
    let groups = object_stream_groups(plan, packed);
    let mut next = max;
    let objstms: Vec<u32> = (0..groups.len())
        .map(|_| {
            next = next.saturating_add(1);
            next
        })
        .collect();
    let xref_number = if structured {
        next = next.saturating_add(1);
        Some(next)
    } else {
        None
    };
    let size = next.saturating_add(1);

    out.put_str(&format!("%PDF-{version}\n"))?;
    out.put(BINARY_MARKER)?;

    let mut entries: BTreeMap<u32, Entry> = BTreeMap::new();
    for (number, object) in &plan.objects {
        // A packed object is written inside the object stream further down.
        if packed && packs_into_object_stream(*number, object, plan.catalog) {
            continue;
        }
        entries.insert(*number, Entry::Offset(out.offset()));
        out.put_str(&format!("{number} 0 obj\n"))?;
        match object {
            OutObject::Value(item) => write_value(&mut out, item)?,
            OutObject::Stream { dict, offset, length } => {
                let flate = settings.compression == Compression::Compress && !filtered(dict);
                if flate {
                    let placeholder = write_stream_dict(&mut out, dict, true)?;
                    out.put(b"\nstream\n")?;
                    let written = out.copy_flate(source, *offset, *length)?;
                    out.put(b"\nendstream")?;
                    // The dictionary was written with ten zeros where the length goes.
                    out.patch_length(placeholder, written)?;
                } else {
                    write_stream_dict(&mut out, dict, false)?;
                    out.put(b"\nstream\n")?;
                    out.copy_stream(source, *offset, *length)?;
                    out.put(b"\nendstream")?;
                }
            }
        }
        out.put(b"\nendobj\n")?;
    }

    for (members, number) in groups.iter().zip(&objstms) {
        write_object_stream(&mut out, plan, *number, members, &mut entries)?;
    }

    match xref_number {
        Some(number) => write_cross_reference_stream(&mut out, plan, size, number, &mut entries)?,
        None => write_cross_reference_table(&mut out, plan, size, settings.classic_limit, &entries)?,
    }

    out.flush()?;
    Ok(Written { bytes: out.offset(), version, notes })
}

/// Whether an object is one page: a dictionary that says so, or — as lenient readers accept — one
/// with no `/Type`, no `/Kids` and content of its own.
fn is_page_object(value: &Object) -> bool {
    let Some(dict) = value.as_dict() else { return false };
    match dict.get_name(b"Type") {
        Some(name) => name == b"Page",
        None => dict.get_array(b"Kids").is_none() && dict.get(b"Contents").is_some(),
    }
}

/// True for an object that goes inside the object stream: a dictionary or an array, but never a
/// stream, and never the catalog, which every reader is entitled to find as an object of its own.
fn packs_into_object_stream(number: u32, object: &OutObject, catalog: u32) -> bool {
    number != catalog && matches!(object, OutObject::Value(_))
}

/// True when the dictionary declares a filter: those streams are copied, never recompressed, so
/// nothing lossy can happen to an image.
fn filtered(dict: &Dict) -> bool {
    dict.get(b"Filter").is_some_and(|value| !matches!(value, Object::Null))
}

/// Most dictionaries one object stream holds. A reader decodes a whole stream to reach a single
/// member, so the count is kept low: the price of a smaller number is a few more streams, the price
/// of a larger one is a slower first page in every viewer.
const OBJECTS_PER_STREAM: usize = 100;

/// The objects that go inside object streams, cut into groups of [`OBJECTS_PER_STREAM`].
fn object_stream_groups(plan: &PlannedPart, packed: bool) -> Vec<Vec<u32>> {
    if !packed {
        return Vec::new();
    }
    let numbers: Vec<u32> =
        plan.objects.keys().copied().filter(|number| packs_into_object_stream(*number, &plan.objects[number], plan.catalog)).collect();
    numbers.chunks(OBJECTS_PER_STREAM).map(<[u32]>::to_vec).collect()
}

/// One object stream: the dictionaries of `members`, with the header that locates them.
fn write_object_stream(
    out: &mut PartOut<'_>,
    plan: &PlannedPart,
    number: u32,
    members: &[u32],
    entries: &mut BTreeMap<u32, Entry>,
) -> Result<(), Error> {
    let mut pairs = String::new();
    let mut bodies: Vec<u8> = Vec::new();
    let mut index: u32 = 0;
    for object_number in members {
        let Some(object) = plan.objects.get(object_number) else { continue };
        let OutObject::Value(item) = object else { continue };
        pairs.push_str(&format!("{object_number} {}\n", bodies.len()));
        write_value(&mut bodies, item)?;
        bodies.push(b'\n');
        entries.insert(*object_number, Entry::Packed { stream: number, index });
        index = index.saturating_add(1);
    }
    // Where the bodies start, counted from the first byte of the stream's own data.
    let first = pairs.len();
    let mut data = pairs.into_bytes();
    data.extend_from_slice(&bodies);
    let compressed = pdfcraft_filters::encode_flate(&data);
    entries.insert(number, Entry::Offset(out.offset()));
    out.put_str(&format!("{number} 0 obj\n"))?;
    out.put_str(&format!("<< /Type /ObjStm /N {index} /First {first} /Filter /FlateDecode /Length {} >>\nstream\n", compressed.len()))?;
    out.put(&compressed)?;
    out.put_str("\nendstream\nendobj\n")
}

/// The cross-reference stream: the only form that can hold an offset past ten digits, and the
/// only one that can say "this object lives inside the object stream".
fn write_cross_reference_stream(
    out: &mut PartOut<'_>,
    plan: &PlannedPart,
    size: u32,
    number: u32,
    entries: &mut BTreeMap<u32, Entry>,
) -> Result<(), Error> {
    let at = out.offset();
    entries.insert(number, Entry::Offset(at));
    // One row per object: a kind byte, an eight-byte field and a four-byte one, so no part can
    // outgrow the offsets it has to write.
    let mut rows: Vec<u8> = Vec::with_capacity(usize::try_from(size).unwrap_or_default() * 13);
    for object in 0..size {
        let (kind, field, third): (u8, u64, u32) = if object == 0 {
            (0, 0, 65_535)
        } else {
            match entries.get(&object) {
                Some(Entry::Offset(offset)) => (1, *offset, 0),
                Some(Entry::Packed { stream, index }) => (2, u64::from(*stream), *index),
                None => (0, 0, 0),
            }
        };
        rows.push(kind);
        rows.extend_from_slice(&field.to_be_bytes());
        rows.extend_from_slice(&third.to_be_bytes());
    }
    let compressed = pdfcraft_filters::encode_flate(&rows);
    out.put_str(&format!("{number} 0 obj\n"))?;
    out.put_str(&format!(
        "<< /Type /XRef /Size {size} /W [1 8 4] /Root {} 0 R /Filter /FlateDecode /Length {} >>\nstream\n",
        plan.catalog,
        compressed.len()
    ))?;
    out.put(&compressed)?;
    out.put_str(&format!("\nendstream\nendobj\nstartxref\n{at}\n%%EOF\n"))
}

/// The classic table, which no part may use once it passes ten digits.
fn write_cross_reference_table(
    out: &mut PartOut<'_>,
    plan: &PlannedPart,
    size: u32,
    limit: u64,
    entries: &BTreeMap<u32, Entry>,
) -> Result<(), Error> {
    let xref = out.offset();
    if xref > limit {
        return Err(too_large());
    }
    out.put_str(&format!("xref\n0 {size}\n"))?;
    out.put_str("0000000000 65535 f \n")?;
    for number in 1..size {
        let Some(Entry::Offset(offset)) = entries.get(&number) else {
            return Err(Error::Corrupt(format!("l'oggetto {number} non è mai stato scritto")));
        };
        if *offset > limit {
            return Err(too_large());
        }
        out.put_str(&format!("{offset:010} 00000 n \n"))?;
    }
    out.put_str(&format!("trailer\n<< /Size {size} /Root {} 0 R >>\nstartxref\n{xref}\n%%EOF\n", plan.catalog))
}

/// A classic table cannot hold an offset past ten digits: such a part needs a cross-reference
/// stream, which compression or a smaller part count would give it.
fn too_large() -> Error {
    Error::Limit("una parte oltre circa 9,3 GiB richiede una cross-reference stream: chiedi la compressione".to_string())
}

/// The dictionary of a stream. A compressed stream gets `/Filter /FlateDecode` and ten zeros in
/// place of its `/Length`, which is where the real length is written once the encoder is done;
/// the return value is that position.
fn write_stream_dict<E: Emit>(out: &mut E, dict: &Dict, flate: bool) -> Result<u64, Error> {
    if !flate {
        write_dict(out, dict)?;
        return Ok(0);
    }
    out.emit(b"<<")?;
    for (key, item) in dict.iter() {
        if key == b"Length" || key == b"Filter" {
            continue;
        }
        out.emit(b" ")?;
        write_name(out, key)?;
        out.emit(b" ")?;
        write_value(out, item)?;
    }
    out.emit(b" /Filter /FlateDecode /Length ")?;
    let at = out.offset();
    out.emit(LENGTH_PLACEHOLDER)?;
    out.emit(b" >>")?;
    Ok(at)
}

/// Where a written object goes: the part's own file, or a buffer when it is being packed.
trait Emit {
    fn emit(&mut self, bytes: &[u8]) -> Result<(), Error>;
    fn offset(&self) -> u64;
}

impl Emit for Vec<u8> {
    fn emit(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.extend_from_slice(bytes);
        Ok(())
    }

    fn offset(&self) -> u64 {
        self.len() as u64
    }
}

impl Emit for PartOut<'_> {
    fn emit(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.put(bytes)
    }

    fn offset(&self) -> u64 {
        self.written
    }
}

/// One PDF object (ISO 32000-2 §7.3.2 onwards).
fn write_value<E: Emit>(out: &mut E, value: &Object) -> Result<(), Error> {
    match value {
        Object::Null => out.emit(b"null"),
        Object::Bool(true) => out.emit(b"true"),
        Object::Bool(false) => out.emit(b"false"),
        Object::Int(number) => out.emit(number.to_string().as_bytes()),
        Object::Real(number) => out.emit(real_text(*number).as_bytes()),
        Object::String(bytes) => write_string(out, bytes),
        Object::Name(name) => write_name(out, name),
        Object::Array(items) => {
            out.emit(b"[")?;
            for item in items {
                out.emit(b" ")?;
                write_value(out, item)?;
            }
            out.emit(b"]")
        }
        Object::Dict(dict) => write_dict(out, dict),
        Object::Ref(target) => out.emit(target.to_string().as_bytes()),
        Object::Stream(_) => Err(Error::Corrupt("uno stream scritto come oggetto diretto".to_string())),
    }
}

fn write_dict<E: Emit>(out: &mut E, dict: &Dict) -> Result<(), Error> {
    out.emit(b"<<")?;
    for (key, value) in dict.iter() {
        out.emit(b" ")?;
        write_name(out, key)?;
        out.emit(b" ")?;
        write_value(out, value)?;
    }
    out.emit(b" >>")
}

/// A name, with every byte that is not a plain one written as `#xx`.
fn write_name<E: Emit>(out: &mut E, name: &[u8]) -> Result<(), Error> {
    let mut bytes = Vec::with_capacity(name.len() + 1);
    bytes.push(b'/');
    for &byte in name {
        if plain_name_byte(byte) {
            bytes.push(byte);
        } else {
            bytes.extend_from_slice(format!("#{byte:02X}").as_bytes());
        }
    }
    out.emit(&bytes)
}

/// A literal string, escaping only what must be escaped: the bytes stay as they were, so a title
/// in PDFDocEncoding or in UTF-16BE is written back unchanged.
fn write_string<E: Emit>(out: &mut E, bytes: &[u8]) -> Result<(), Error> {
    let mut escaped = Vec::with_capacity(bytes.len() + 2);
    escaped.push(b'(');
    for &byte in bytes {
        match byte {
            b'(' | b')' | b'\\' => {
                escaped.push(b'\\');
                escaped.push(byte);
            }
            b'\n' => escaped.extend_from_slice(b"\\n"),
            b'\r' => escaped.extend_from_slice(b"\\r"),
            b'\t' => escaped.extend_from_slice(b"\\t"),
            8 => escaped.extend_from_slice(b"\\b"),
            12 => escaped.extend_from_slice(b"\\f"),
            other => escaped.push(other),
        }
    }
    escaped.push(b')');
    out.emit(&escaped)
}

impl PlannedPart {
    /// The least PDF version this part needs, judged by the features a reader will meet in it:
    /// transparency arrived with 1.4, JPEG 2000 and a form's XFA with 1.5, rich media with 1.7.
    fn required_version(&self) -> Option<Version> {
        let mut needed: Option<Version> = None;
        {
            let mut note = |version: Version| {
                needed = Some(match needed {
                    Some(current) => current.max(version),
                    None => version,
                });
            };
            for object in self.objects.values() {
                match object {
                    OutObject::Value(item) => scan_features(item, &mut note),
                    OutObject::Stream { dict, .. } => scan_dict_features(dict, &mut note),
                }
            }
        }
        needed
    }

    /// A generous estimate of the bytes the part will take, used to choose the cross-reference
    /// form before the first byte is written.
    fn body_size(&self) -> u64 {
        self.objects.values().fold(0_u64, |total, object| {
            total.saturating_add(match object {
                OutObject::Stream { length, .. } => length.saturating_add(1024),
                OutObject::Value(_) => 1024,
            })
        })
    }
}

/// Look through a value for the features that raise the version a reader needs.
fn scan_features(value: &Object, note: &mut impl FnMut(Version)) {
    match value {
        Object::Dict(dict) => scan_dict_features(dict, note),
        Object::Array(items) => {
            for item in items {
                scan_features(item, note);
            }
        }
        _ => {}
    }
}

fn scan_dict_features(dict: &Dict, note: &mut impl FnMut(Version)) {
    if dict.get(b"SMask").is_some() || dict.get(b"CA").is_some() || dict.get(b"ca").is_some() {
        note(Version::PDF_1_4);
    }
    if dict.get(b"XFA").is_some() {
        note(Version::PDF_1_5);
    }
    if dict.get(b"RichMedia").is_some() {
        note(Version::PDF_1_7);
    }
    if let Some(Object::Name(name)) = dict.get(b"Filter")
        && name == b"JPXDecode"
    {
        note(Version::PDF_1_5);
    }
    for (_, item) in dict.iter() {
        scan_features(item, note);
    }
}

/// The output file: a buffered writer that also counts what it has written, since every object's
/// offset goes into the cross-reference table.
struct PartOut<'a> {
    inner: BufWriter<File>,
    path: PathBuf,
    written: u64,
    /// The copy buffer, kept here so a part does not allocate one per stream.
    chunk: Vec<u8>,
    job: &'a Job,
    placement: Placement,
    /// Bytes written since the last report, so a report comes once per copy chunk.
    unsent: u64,
}

impl<'a> PartOut<'a> {
    fn new(file: File, path: &Path, job: &'a Job, placement: Placement) -> Self {
        Self {
            inner: BufWriter::with_capacity(OUT_BUFFER, file),
            path: path.to_path_buf(),
            written: 0,
            chunk: vec![0_u8; COPY_CHUNK],
            job,
            placement,
            unsent: 0,
        }
    }

    fn offset(&self) -> u64 {
        self.written
    }

    /// Count what was written, and once per copy chunk tell the job where the writing is.
    ///
    /// The stop flag is read on every write, so a cancel is noticed within a chunk rather than at
    /// the end of a three-gigabyte stream.
    fn advance(&mut self, added: u64) -> Result<(), Error> {
        self.written = self.written.saturating_add(added);
        self.unsent = self.unsent.saturating_add(added);
        self.job.check()?;
        if self.unsent >= COPY_CHUNK as u64 {
            self.unsent = 0;
            let placement = self.placement;
            self.job.part(Stage::Write, placement.part, placement.parts, placement.before.saturating_add(self.written), placement.total, Unit::Bytes);
        }
        Ok(())
    }

    fn put(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.inner.write_all(bytes).map_err(|error| Error::io(&self.path, error))?;
        self.advance(bytes.len() as u64)
    }

    fn put_str(&mut self, text: &str) -> Result<(), Error> {
        self.put(text.as_bytes())
    }

    fn flush(&mut self) -> Result<(), Error> {
        self.inner.flush().map_err(|error| Error::io(&self.path, error))
    }

    /// Write a number over the ten zeros a compressed stream's dictionary left for it.
    fn patch_length(&mut self, at: u64, length: u64) -> Result<(), Error> {
        if length > 9_999_999_999 {
            return Err(Error::Limit(format!("uno stream compresso di {length} byte, oltre quanto un dizionario possa dichiarare")));
        }
        self.inner.flush().map_err(|error| Error::io(&self.path, error))?;
        let file = self.inner.get_mut();
        file.seek(SeekFrom::Start(at)).map_err(|error| Error::io(&self.path, error))?;
        file.write_all(format!("{length:010}").as_bytes()).map_err(|error| Error::io(&self.path, error))?;
        file.seek(SeekFrom::End(0)).map_err(|error| Error::io(&self.path, error))?;
        Ok(())
    }

    /// Copy a stream's bytes from the source file, a chunk at a time.
    fn copy_stream(&mut self, source: &Source, offset: u64, length: u64) -> Result<(), Error> {
        let mut done = 0_u64;
        while done < length {
            let take = usize::try_from((length - done).min(self.chunk.len() as u64)).unwrap_or(self.chunk.len());
            let read = source.read_into(offset.saturating_add(done), &mut self.chunk[..take])?;
            if read == 0 {
                return Err(Error::Corrupt(format!("il file sorgente finisce dentro lo stream al byte {offset}, {length} byte dopo")));
            }
            // `inner` and `chunk` are separate fields: taking them apart keeps their borrows apart.
            let Self { inner, path, chunk, .. } = self;
            inner.write_all(&chunk[..read]).map_err(|error| Error::io(&*path, error))?;
            self.advance(read as u64)?;
            done = done.saturating_add(read as u64);
        }
        Ok(())
    }

    /// Copy a stream through a Flate encoder, a chunk at a time, and report the bytes written.
    ///
    /// Nothing of the stream is held: what is in memory is the read buffer, the encoder's state
    /// and the write buffer, whatever the size of the stream.
    fn copy_flate(&mut self, source: &Source, offset: u64, length: u64) -> Result<u64, Error> {
        let start = self.written;
        // The copy buffer belongs to the part; taking it out lets the loop use it while the
        // encoder writes through `self`.
        let mut chunk = std::mem::take(&mut self.chunk);
        let outcome = self.compress_stream(source, offset, length, &mut chunk);
        self.chunk = chunk;
        outcome?;
        Ok(self.written.saturating_sub(start))
    }

    fn compress_stream(&mut self, source: &Source, offset: u64, length: u64, chunk: &mut [u8]) -> Result<(), Error> {
        let mut encoder = flate2::Compress::new(flate2::Compression::new(6), true);
        let mut buffer = vec![0_u8; COPY_CHUNK];
        let mut done = 0_u64;
        while done < length {
            let take = usize::try_from((length - done).min(chunk.len() as u64)).unwrap_or(chunk.len());
            let read = source.read_into(offset.saturating_add(done), &mut chunk[..take])?;
            if read == 0 {
                return Err(Error::Corrupt(format!("il file sorgente finisce dentro lo stream al byte {offset}, {length} byte dopo")));
            }
            let mut input = &chunk[..read];
            while !input.is_empty() {
                let before_in = encoder.total_in();
                let before_out = encoder.total_out();
                encoder
                    .compress(input, &mut buffer, flate2::FlushCompress::None)
                    .map_err(|error| Error::Corrupt(format!("non si riesce a comprimere lo stream al byte {offset}: {error}")))?;
                let consumed = usize::try_from(encoder.total_in().saturating_sub(before_in)).unwrap_or(0).min(input.len());
                let produced = usize::try_from(encoder.total_out().saturating_sub(before_out)).unwrap_or(0).min(buffer.len());
                if produced > 0 {
                    self.put(&buffer[..produced])?;
                }
                input = &input[consumed..];
                if consumed == 0 && produced == 0 {
                    return Err(Error::Corrupt(format!("non si riesce a comprimere lo stream al byte {offset} nel buffer")));
                }
            }
            done = done.saturating_add(read as u64);
        }
        // The end of the zlib stream: its checksum and the encoder's last bytes.
        loop {
            let before_out = encoder.total_out();
            let status = encoder
                .compress(&[], &mut buffer, flate2::FlushCompress::Finish)
                .map_err(|error| Error::Corrupt(format!("non si riesce a chiudere lo stream al byte {offset}: {error}")))?;
            let produced = usize::try_from(encoder.total_out().saturating_sub(before_out)).unwrap_or(0).min(buffer.len());
            if produced > 0 {
                self.put(&buffer[..produced])?;
            }
            if status == flate2::Status::StreamEnd || produced == 0 {
                return Ok(());
            }
        }
    }
}

/// True for a byte that can stand in a name as itself (ISO 32000-1 §7.3.5).
fn plain_name_byte(byte: u8) -> bool {
    (0x21..=0x7E).contains(&byte) && !matches!(byte, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%' | b'#')
}

/// A real number as text: `f64` prints `1` for `1.0`, and a real keeps its point.
fn real_text(value: f64) -> String {
    if !value.is_finite() {
        return "0.0".to_string();
    }
    let text = format!("{value}");
    if text.contains(['.', 'e', 'E']) { text } else { format!("{text}.0") }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A part of one blank page under a catalog: enough to write and to read back, nothing to
    /// resolve out of a source file.
    fn one_page_part(extra: Option<(u32, Object)>) -> PlannedPart {
        let mut objects = BTreeMap::new();
        let mut catalog = Dict::new();
        catalog.insert(b"Type".to_vec(), Object::Name(b"Catalog".to_vec()));
        catalog.insert(b"Pages".to_vec(), reference(2));
        let mut tree = Dict::new();
        tree.insert(b"Type".to_vec(), Object::Name(b"Pages".to_vec()));
        tree.insert(b"Kids".to_vec(), Object::Array(vec![reference(3)]));
        tree.insert(b"Count".to_vec(), Object::Int(1));
        let mut page = Dict::new();
        page.insert(b"Type".to_vec(), Object::Name(b"Page".to_vec()));
        page.insert(b"Parent".to_vec(), reference(2));
        page.insert(b"MediaBox".to_vec(), Object::Array(vec![Object::Int(0), Object::Int(0), Object::Int(595), Object::Int(842)]));
        objects.insert(1, OutObject::Value(Object::Dict(catalog)));
        objects.insert(2, OutObject::Value(Object::Dict(tree)));
        objects.insert(3, OutObject::Value(Object::Dict(page)));
        if let Some((number, item)) = extra {
            objects.insert(number, OutObject::Value(item));
        }
        PlannedPart { objects, catalog: 1, bookmarks: 0, notes: Vec::new() }
    }

    fn write_to(name: &str, plan: &PlannedPart, settings: Settings) -> (Written, PathBuf) {
        let directory = std::env::temp_dir().join(format!("pdfsplit-write-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        // A part with nothing to copy still needs a source: the bytes are only ever read when a
        // plan carries a stream.
        let source_path = directory.join("source.pdf");
        std::fs::write(&source_path, b"%PDF-1.7\n%%EOF\n").unwrap();
        let source = Source::open(&source_path).unwrap();
        let path = directory.join(name);
        let placement = Placement { before: 0, total: 1, part: 1, parts: 1 };
        let written = write(&source, plan, &settings, &path, &Job::silent(), placement).unwrap();
        (written, path)
    }

    #[test]
    fn a_part_too_big_for_a_classic_table_writes_a_cross_reference_stream() {
        let plan = one_page_part(None);
        // A classic table may not go past ten digits; a limit of zero stands in for that here.
        let settings = Settings { compression: Compression::Neutral, version: Version::PDF_1_4, classic_limit: 0 };
        let (written, path) = write_to("stream.pdf", &plan, settings);
        assert_eq!(written.version, Version::PDF_1_5, "a cross-reference stream needs 1.5");
        assert!(written.notes.iter().any(|note| note.contains("1.5")), "{:?}", written.notes);
        let text = String::from_utf8_lossy(&std::fs::read(&path).unwrap()).into_owned();
        assert!(text.starts_with("%PDF-1.5"));
        assert!(text.contains("/Type /XRef"), "{text}");
        assert!(text.contains("startxref"), "{text}");
        // The part still reads as a document: the cross-reference stream says where every object is.
        let mut reopened = crate::Document::open(&path).unwrap();
        assert_eq!(reopened.page_count().unwrap(), 1);
    }

    #[test]
    fn a_small_part_keeps_the_classic_table() {
        let plan = one_page_part(None);
        let settings = Settings { compression: Compression::Neutral, version: Version::PDF_1_4, classic_limit: MAX_CLASSIC_OFFSET };
        let (written, path) = write_to("classic.pdf", &plan, settings);
        assert_eq!(written.version, Version::PDF_1_4);
        assert!(written.notes.is_empty(), "{:?}", written.notes);
        let text = String::from_utf8_lossy(&std::fs::read(&path).unwrap()).into_owned();
        assert!(text.starts_with("%PDF-1.4"));
        assert!(!text.contains("/Type /XRef"));
        assert!(text.contains("xref\n0 4\n"), "{text}");
    }

    #[test]
    fn a_page_that_needs_a_later_version_raises_the_header_and_says_so() {
        // Transparency arrived with 1.4: a part asking for 1.3 cannot be written as 1.3.
        let mut soft = Dict::new();
        soft.insert(b"SMask".to_vec(), reference(9));
        let plan = one_page_part(Some((4, Object::Dict(soft))));
        let settings = Settings { compression: Compression::Neutral, version: Version::PDF_1_3, classic_limit: MAX_CLASSIC_OFFSET };
        let (written, path) = write_to("raised.pdf", &plan, settings);
        assert_eq!(written.version, Version::PDF_1_4);
        assert!(written.notes.iter().any(|note| note.contains("1.4")), "{:?}", written.notes);
        assert!(String::from_utf8_lossy(&std::fs::read(&path).unwrap()).starts_with("%PDF-1.4"));
    }
}
