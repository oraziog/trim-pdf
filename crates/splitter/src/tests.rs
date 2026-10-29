//! Unit tests over synthetic documents, built in memory and written to a temporary file.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use pdfcraft_filters::{Filter, Params};

use crate::object::{Dict, Object};
use crate::*;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("pdfsplit-test-{}-{id}-{name}", std::process::id()));
    std::fs::write(&path, bytes).unwrap();
    path
}

fn open(name: &str, bytes: &[u8]) -> Document {
    Document::open(&temp_file(name, bytes)).unwrap()
}

fn add(out: &mut Vec<u8>, offsets: &mut Vec<u32>, body: &str) -> u32 {
    let num = offsets.len() as u32 + 1;
    offsets.push(u32::try_from(out.len()).unwrap());
    out.extend_from_slice(format!("{num} 0 obj\n{body}\nendobj\n").as_bytes());
    num
}

fn add_stream(out: &mut Vec<u8>, offsets: &mut Vec<u32>, dict: &str, data: &[u8]) -> u32 {
    let num = offsets.len() as u32 + 1;
    offsets.push(u32::try_from(out.len()).unwrap());
    out.extend_from_slice(format!("{num} 0 obj\n<< {dict} /Length {} >>\nstream\n", data.len()).as_bytes());
    out.extend_from_slice(data);
    out.extend_from_slice(b"\nendstream\nendobj\n");
    num
}

/// Close a document built object by object with a classic cross-reference table.
fn classic(body: Vec<u8>, offsets: &[u32], root: u32) -> Vec<u8> {
    let mut out = body;
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes());
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(format!("trailer\n<< /Size {} /Root {root} 0 R >>\nstartxref\n{xref}\n%%EOF\n", offsets.len() + 1).as_bytes());
    out
}

/// A three-page document whose outline nests one level and uses all three destination forms.
fn outline_document() -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets: Vec<u32> = Vec::new();
    add(&mut out, &mut offsets, "<< /Type /Catalog /Pages 2 0 R /Outlines 3 0 R /Dests << /Fine [4 0 R /Fit] >> >>");
    add(&mut out, &mut offsets, "<< /Type /Pages /Kids [4 0 R 5 0 R 6 0 R] /Count 3 >>");
    add(&mut out, &mut offsets, "<< /Type /Outlines /First 7 0 R /Last 10 0 R /Count 4 >>");
    add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>");
    add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>");
    add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>");
    add(&mut out, &mut offsets, "<< /Title (Uno) /Parent 3 0 R /First 8 0 R /Last 8 0 R /Count 2 /Next 9 0 R /Dest [5 0 R /Fit] >>");
    add(&mut out, &mut offsets, "<< /Title (Uno.a) /Parent 7 0 R /Dest [6 0 R /XYZ 0 0 0] >>");
    add(&mut out, &mut offsets, "<< /Title (Due) /Parent 3 0 R /Next 10 0 R /A << /S /GoTo /D /Fine >> >>");
    add(&mut out, &mut offsets, "<< /Title (Niente) /Parent 3 0 R >>");
    classic(out, &offsets, 1)
}

#[test]
fn a_classic_document_reports_its_pages_and_bookmarks() {
    let mut doc = open("outline.pdf", &outline_document());
    assert_eq!(doc.pdf_version(), "1.7");
    assert_eq!(doc.page_count().unwrap(), 3);
    let bookmarks = doc.bookmarks().unwrap();
    let seen: Vec<(usize, Option<usize>, &str)> = bookmarks.iter().map(|b| (b.level, b.page, b.title.as_str())).collect();
    assert_eq!(seen, vec![(0, Some(2), "Uno"), (1, Some(3), "Uno.a"), (0, Some(1), "Due"), (0, None, "Niente")]);
}

#[test]
fn the_summary_shows_what_the_panel_needs() {
    let mut doc = open("summary.pdf", &outline_document());
    let summary = doc.summary().unwrap();
    assert_eq!(summary.pdf_version, "1.7");
    assert!(!summary.encrypted);
    assert_eq!(summary.pages, Some(3));
    assert_eq!(summary.bookmarks, Some(4));
    assert!(summary.file_size > 0);
}

/// Pages and an outline item that live in an object stream, reached through a cross-reference
/// stream with a PNG predictor — the layout modern producers write.
fn object_stream_document() -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets: Vec<u32> = Vec::new();
    add(&mut out, &mut offsets, "<< /Type /Catalog /Pages 2 0 R /Outlines 3 0 R >>");
    add(&mut out, &mut offsets, "<< /Type /Pages /Kids [4 0 R 5 0 R] /Count 2 >>");
    add(&mut out, &mut offsets, "<< /Type /Outlines /First 7 0 R /Last 7 0 R /Count 1 >>");
    add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>");
    add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>");

    // Object 6: an object stream holding object 7, the outline item.
    let item = b"<< /Title (Capitolo) /Parent 3 0 R /Dest [5 0 R /Fit] >>";
    let header = b"7 0 ";
    let mut payload = header.to_vec();
    payload.extend_from_slice(item);
    let compressed = pdfcraft_filters::encode_flate(&payload);
    let objstm = add_stream(&mut out, &mut offsets, &format!("/Type /ObjStm /N 1 /First {} /Filter /FlateDecode", header.len()), &compressed);

    // Object 8: the cross-reference stream, with /W [1 4 2] and a PNG predictor.
    const WIDTHS: usize = 7;
    let mut entries: Vec<u8> = Vec::new();
    let mut entry = |kind: u8, field2: u32, field3: u32| {
        entries.push(kind);
        entries.extend_from_slice(&field2.to_be_bytes());
        entries.extend_from_slice(&u16::try_from(field3).unwrap().to_be_bytes());
    };
    entry(0, 0, 65535);
    for offset in &offsets {
        entry(1, *offset, 0);
    }
    // Object 7 lives at index 0 of the object stream; object 8 is this stream itself.
    entry(2, objstm, 0);
    let xref_offset = u32::try_from(out.len()).unwrap();
    entry(1, xref_offset, 0);

    let params = Params { predictor: 12, columns: WIDTHS as i64, ..Params::default() };
    let compressed = pdfcraft_filters::encode(&Filter::Flate, &params, &entries).unwrap();
    // Objects 0..8: the six written here, object 7 in the object stream, object 8 this stream.
    let size = offsets.len() + 3;
    out.extend_from_slice(
        format!(
            "8 0 obj\n<< /Type /XRef /Size {size} /Root 1 0 R /W [1 4 2] /Index [0 {size}] /Filter /FlateDecode /DecodeParms << /Predictor 12 /Columns {WIDTHS} >> /Length {} >>\nstream\n",
            compressed.len()
        )
        .as_bytes(),
    );
    out.extend_from_slice(&compressed);
    out.extend_from_slice(b"\nendstream\nendobj\n");
    out.extend_from_slice(format!("startxref\n{xref_offset}\n%%EOF\n").as_bytes());
    out
}

#[test]
fn objects_in_an_object_stream_are_found_through_a_cross_reference_stream() {
    let mut doc = open("objstm.pdf", &object_stream_document());
    assert_eq!(doc.page_count().unwrap(), 2);
    let bookmarks = doc.bookmarks().unwrap();
    assert_eq!(bookmarks.len(), 1);
    assert_eq!(bookmarks[0].title, "Capitolo");
    assert_eq!(bookmarks[0].page, Some(2));
}

#[test]
fn a_page_tree_with_a_wrong_count_is_counted_exactly() {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets: Vec<u32> = Vec::new();
    add(&mut out, &mut offsets, "<< /Type /Catalog /Pages 2 0 R >>");
    // /Count says 99, the tree holds two pages.
    add(&mut out, &mut offsets, "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 99 >>");
    add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>");
    add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>");
    let bytes = classic(out, &offsets, 1);
    let mut doc = open("wrong-count.pdf", &bytes);
    assert_eq!(doc.page_count().unwrap(), 2);
}

/// A document whose second revision grows the page tree from one page to two.
fn incremental_document() -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets: Vec<u32> = Vec::new();
    add(&mut out, &mut offsets, "<< /Type /Catalog /Pages 2 0 R >>");
    add(&mut out, &mut offsets, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>");
    add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>");
    let mut out = classic(out, &offsets, 1);

    // Where revision one's table starts, as its own `startxref` says.
    let marker = out.windows(b"startxref\n".len()).rposition(|window| window == b"startxref\n").unwrap() + b"startxref\n".len();
    let previous: u64 = std::str::from_utf8(&out[marker..]).unwrap().lines().next().unwrap().trim().parse().unwrap();

    // Revision two: the page tree grows, and the update's table lists only what changed.
    let pages_offset = u32::try_from(out.len()).unwrap();
    out.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>\nendobj\n");
    let page_offset = u32::try_from(out.len()).unwrap();
    out.extend_from_slice(b"4 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>\nendobj\n");
    let xref = out.len();
    // Two subsections: object 2 (the grown page tree) and object 4 (the new page).
    out.extend_from_slice(b"xref\n2 1\n");
    out.extend_from_slice(format!("{pages_offset:010} 00000 n \n").as_bytes());
    out.extend_from_slice(b"4 1\n");
    out.extend_from_slice(format!("{page_offset:010} 00000 n \n").as_bytes());
    out.extend_from_slice(format!("trailer\n<< /Size 5 /Root 1 0 R /Prev {previous} >>\nstartxref\n{xref}\n%%EOF\n").as_bytes());

    out
}

#[test]
fn an_incremental_update_wins_over_the_revision_before_it() {
    let mut doc = open("incremental.pdf", &incremental_document());
    assert_eq!(doc.page_count().unwrap(), 2);
}

/// A document whose trailer names an encryption dictionary.
fn encrypted_document() -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets: Vec<u32> = Vec::new();
    add(&mut out, &mut offsets, "<< /Type /Catalog /Pages 2 0 R >>");
    add(&mut out, &mut offsets, "<< /Type /Pages /Kids [] /Count 0 >>");
    add(&mut out, &mut offsets, "<< /Filter /Standard /V 1 /R 2 /O <00> /U <00> /P -1 >>");
    let mut bytes = out;
    let xref = bytes.len();
    bytes.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes());
    for offset in &offsets {
        bytes.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    bytes.extend_from_slice(
        format!("trailer\n<< /Size {} /Root 1 0 R /Encrypt 3 0 R /ID [<01> <01>] >>\nstartxref\n{xref}\n%%EOF\n", offsets.len() + 1).as_bytes(),
    );
    bytes
}

#[test]
fn an_encrypted_document_is_reported_and_not_read() {
    let bytes = encrypted_document();
    let mut doc = open("encrypted.pdf", &bytes);
    assert!(doc.is_encrypted());
    assert!(matches!(doc.page_count(), Err(Error::Encrypted)));
    assert!(matches!(doc.bookmarks(), Err(Error::Encrypted)));
    let summary = doc.summary().unwrap();
    assert!(summary.encrypted);
    assert_eq!(summary.pages, None);
    assert_eq!(summary.bookmarks, None);
}

#[test]
fn a_damaged_cross_reference_is_an_error_and_never_a_panic() {
    let bytes = b"%PDF-1.7\n1 0 obj\n<< /Type /Catalog >>\nendobj\ntrailer\n<< /Root 1 0 R >>\nstartxref\n999999\n%%EOF\n";
    let error = Document::open(&temp_file("damaged.pdf", bytes)).unwrap_err();
    assert!(matches!(error, Error::Corrupt(_)), "{error}");
}

#[test]
fn a_file_that_is_not_a_pdf_is_refused() {
    let error = Document::open(&temp_file("not-a-pdf.bin", b"just some text, no header at all")).unwrap_err();
    assert!(matches!(error, Error::NotPdf), "{error}");
}

#[test]
fn a_missing_file_reports_its_path() {
    let error = Document::open(std::path::Path::new("/nonexistent/pdfsplit/none.pdf")).unwrap_err();
    assert!(matches!(error, Error::Io { .. }), "{error}");
}

#[test]
fn a_destination_that_points_nowhere_keeps_its_bookmark() {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets: Vec<u32> = Vec::new();
    add(&mut out, &mut offsets, "<< /Type /Catalog /Pages 2 0 R /Outlines 3 0 R >>");
    add(&mut out, &mut offsets, "<< /Type /Pages /Kids [4 0 R] /Count 1 >>");
    add(&mut out, &mut offsets, "<< /Type /Outlines /First 5 0 R /Last 5 0 R /Count 1 >>");
    add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>");
    add(&mut out, &mut offsets, "<< /Title (Perso) /Parent 3 0 R /Dest [42 0 R /Fit] >>");
    let bytes = classic(out, &offsets, 1);
    let mut doc = open("dangling.pdf", &bytes);
    let bookmarks = doc.bookmarks().unwrap();
    assert_eq!(bookmarks.len(), 1);
    assert_eq!(bookmarks[0].page, None);
    assert_eq!(bookmarks[0].title, "Perso");
}

#[test]
fn bookmark_titles_decode_utf16_and_pdfdoc_encoding() {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets: Vec<u32> = Vec::new();
    add(&mut out, &mut offsets, "<< /Type /Catalog /Pages 2 0 R /Outlines 3 0 R >>");
    add(&mut out, &mut offsets, "<< /Type /Pages /Kids [4 0 R] /Count 1 >>");
    add(&mut out, &mut offsets, "<< /Type /Outlines /First 5 0 R /Last 6 0 R /Count 2 >>");
    add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>");
    add(&mut out, &mut offsets, "<< /Title <FEFF005300E9006300740069006F006E0065> /Parent 3 0 R /Dest [4 0 R /Fit] /Next 6 0 R >>");
    // \\204 is PDFDocEncoding 0x84, the em dash; \\351 is 0xE9, e-acute.
    add(&mut out, &mut offsets, "<< /Title (R\\351sum\\351 \\204 2026) /Parent 3 0 R /Dest [4 0 R /Fit] >>");
    let bytes = classic(out, &offsets, 1);
    let mut doc = open("titles.pdf", &bytes);
    let bookmarks = doc.bookmarks().unwrap();
    assert_eq!(bookmarks[0].title, "Séctione");
    assert_eq!(bookmarks[1].title, "Résumé — 2026");
}

/// Point the cross-reference entry for `num` at another object's offset, as a damaged file does.
fn misdirect(bytes: &[u8], num: usize, offset: u32) -> Vec<u8> {
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let xref = lines.iter().position(|line| line == "xref").unwrap();
    // "xref", the subsection header, the free entry, then object 1.
    lines[xref + 2 + num] = format!("{offset:010} 00000 n ");
    let mut out = lines.join("\n");
    out.push('\n');
    out.into_bytes()
}

#[test]
fn an_xref_entry_that_points_at_the_wrong_object_is_reported() {
    let bytes = outline_document();
    let outline_offset = bytes.windows(7).position(|window| window == b"4 0 obj").unwrap();
    // Object 3 is the outline root; its entry now points at page 4 instead, so the bytes would be
    // read as the wrong object unless the header is checked against the entry.
    let damaged = misdirect(&bytes, 3, u32::try_from(outline_offset).unwrap());
    let mut doc = open("misdirected.pdf", &damaged);
    assert_eq!(doc.page_count().unwrap(), 3);
    let error = doc.bookmarks().unwrap_err();
    assert!(error.to_string().contains("non è al byte"), "{error}");
}

// — M2: the split. ————————————————————————————————————————————————————————————————

/// A directory no other test shares, under the temporary directory.
fn temp_dir(name: &str) -> PathBuf {
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("pdfsplit-parts-{}-{id}-{name}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// The page dictionaries of a written part, read back the way any document is read.
fn part_pages(path: &std::path::Path) -> Vec<Dict> {
    let source = crate::source::Source::open(path).unwrap();
    let xref = crate::xref::read(&source).unwrap();
    let mut cache = crate::xref::Cache::new();
    let root = xref.root().unwrap();
    let mut reader = crate::xref::Reader::new(&source, &xref, &mut cache);
    let pages = crate::structure::page_list(&mut reader, root).unwrap();
    pages.iter().map(|page| reader.get(page.reference).unwrap().as_dict().unwrap().clone()).collect()
}

/// The bytes of a page's content stream as they were written into a part.
fn part_stream_bytes(path: &std::path::Path, page: usize) -> Vec<u8> {
    let source = crate::source::Source::open(path).unwrap();
    let xref = crate::xref::read(&source).unwrap();
    let mut cache = crate::xref::Cache::new();
    let root = xref.root().unwrap();
    let mut reader = crate::xref::Reader::new(&source, &xref, &mut cache);
    let pages = crate::structure::page_list(&mut reader, root).unwrap();
    let page = reader.get(pages[page].reference).unwrap();
    let contents = page.as_dict().unwrap().get(b"Contents").unwrap().as_ref().unwrap();
    let stream = reader.get(contents).unwrap().as_stream().unwrap().clone();
    let (offset, length) = reader.stream_layout(&stream).unwrap();
    source.read_vec(offset, usize::try_from(length).unwrap()).unwrap()
}

/// A document whose resources and box live on the page tree, with a binary content stream.
fn inheriting_document() -> (Vec<u8>, Vec<u8>) {
    // Bytes that have to survive the copy as they are: EOLs, delimiters, high bytes, a zero.
    let stream: Vec<u8> = (0..600_u32).map(|index| u8::try_from((index * 37 + 128) % 256).unwrap()).collect();
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets: Vec<u32> = Vec::new();
    add(&mut out, &mut offsets, "<< /Type /Catalog /Pages 2 0 R >>");
    add(&mut out, &mut offsets, "<< /Type /Pages /Kids [3 0 R] /Count 1 /MediaBox [0 0 595 842] /Resources << /Font << /F1 5 0 R >> >> >>");
    add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>");
    add_stream(&mut out, &mut offsets, "", &stream);
    add(&mut out, &mut offsets, "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>");
    (classic(out, &offsets, 1), stream)
}

#[test]
fn a_split_writes_one_file_per_part_with_the_pages_asked_for() {
    let directory = temp_dir("split-pages");
    let path = temp_file("split-pages.pdf", &outline_document());
    let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
    let mut doc = Document::open(&path).unwrap();
    let plan = SplitPlan::from_page_list("1-2, 3").unwrap();
    let options = SplitOptions { out_dir: Some(directory.clone()), ..SplitOptions::default() };
    let report = doc.split(&plan, &options).unwrap();

    assert_eq!(report.pages, 3);
    assert_eq!(report.parts.len(), 2);
    assert_eq!(report.parts[0].label, "1-2");
    assert_eq!(report.parts[0].pages, 2);
    assert_eq!(report.parts[1].label, "3");
    for (index, part) in report.parts.iter().enumerate() {
        assert_eq!(part.path.file_name().unwrap().to_string_lossy(), format!("{stem}-{:02}.pdf", index + 1));
        assert!(part.bytes > 0);
        let mut reopened = Document::open(&part.path).unwrap();
        assert_eq!(reopened.page_count().unwrap(), part.pages);
    }
    assert_eq!(report.written_bytes(), report.parts.iter().map(|part| part.bytes).sum::<u64>());
}

#[test]
fn a_part_carries_the_bookmarks_that_point_into_it() {
    let directory = temp_dir("split-bookmarks");
    let path = temp_file("split-bookmarks.pdf", &outline_document());
    let mut doc = Document::open(&path).unwrap();
    let plan = SplitPlan::from_page_list("1-2, 3").unwrap();
    let options = SplitOptions { out_dir: Some(directory), ..SplitOptions::default() };
    let report = doc.split(&plan, &options).unwrap();
    assert_eq!((report.parts[0].bookmarks, report.parts[1].bookmarks), (2, 1));

    let mut first = Document::open(&report.parts[0].path).unwrap();
    let seen: Vec<(usize, Option<usize>, &str)> = first.bookmarks().unwrap().iter().map(|b| (b.level, b.page, b.title.as_str())).collect();
    assert_eq!(seen, vec![(0, Some(2), "Uno"), (0, Some(1), "Due")]);

    // Its parent pointed elsewhere, so the bookmark that is left heads this part's outline, and
    // the level a reader reports is the one the part's own outline gives it.
    let mut second = Document::open(&report.parts[1].path).unwrap();
    let seen: Vec<(usize, Option<usize>, &str)> = second.bookmarks().unwrap().iter().map(|b| (b.level, b.page, b.title.as_str())).collect();
    assert_eq!(seen, vec![(0, Some(1), "Uno.a")]);
}

#[test]
fn a_part_can_be_written_without_an_outline() {
    let directory = temp_dir("split-no-outline");
    let path = temp_file("split-no-outline.pdf", &outline_document());
    let mut doc = Document::open(&path).unwrap();
    let options = SplitOptions { out_dir: Some(directory), keep_bookmarks: false, ..SplitOptions::default() };
    let report = doc.split(&SplitPlan::equal_parts(3, 2).unwrap(), &options).unwrap();
    assert_eq!(report.parts.iter().map(|part| part.bookmarks).sum::<usize>(), 0);
    let mut first = Document::open(&report.parts[0].path).unwrap();
    assert!(first.bookmarks().unwrap().is_empty());
}

#[test]
fn a_page_keeps_the_resources_and_the_box_it_inherited() {
    let (bytes, stream) = inheriting_document();
    let directory = temp_dir("split-inherited");
    let path = temp_file("inherited.pdf", &bytes);
    let mut doc = Document::open(&path).unwrap();
    let options = SplitOptions { out_dir: Some(directory), ..SplitOptions::default() };
    let report = doc.split(&SplitPlan::equal_parts(1, 1).unwrap(), &options).unwrap();

    let pages = part_pages(&report.parts[0].path);
    assert_eq!(pages.len(), 1);
    let box_values: Vec<i64> = pages[0].get(b"MediaBox").and_then(Object::as_array).unwrap().iter().filter_map(Object::as_int).collect();
    assert_eq!(box_values, vec![0, 0, 595, 842]);
    assert!(pages[0].get(b"Resources").is_some());
    // The stream came across byte for byte, not decoded and not re-encoded.
    assert_eq!(part_stream_bytes(&report.parts[0].path, 0), stream);
}

#[test]
fn pages_that_live_in_an_object_stream_are_written_out_as_plain_objects() {
    let directory = temp_dir("split-objstm");
    let path = temp_file("objstm-pages.pdf", &pages_in_object_stream_document());
    let mut doc = Document::open(&path).unwrap();
    assert_eq!(doc.page_count().unwrap(), 2);
    let options = SplitOptions { out_dir: Some(directory), ..SplitOptions::default() };
    let report = doc.split(&SplitPlan::from_page_list("1, 2").unwrap(), &options).unwrap();

    for part in &report.parts {
        let mut reopened = Document::open(&part.path).unwrap();
        assert_eq!(reopened.page_count().unwrap(), 1);
    }
    let pages = part_pages(&report.parts[1].path);
    assert_eq!(pages[0].get(b"Type").and_then(Object::as_name), Some(&b"Page"[..]));
    assert_eq!(pages[0].get(b"Parent").and_then(Object::as_ref).unwrap().num, 2);
}

#[test]
fn an_existing_name_stops_the_split_before_anything_is_written() {
    let directory = temp_dir("split-conflict");
    let path = temp_file("conflict.pdf", &outline_document());
    let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
    let taken = directory.join(format!("{stem}-02.pdf"));
    std::fs::write(&taken, b"already here").unwrap();

    let mut doc = Document::open(&path).unwrap();
    let options = SplitOptions { out_dir: Some(directory.clone()), ..SplitOptions::default() };
    let error = doc.split(&SplitPlan::from_page_list("1, 2, 3").unwrap(), &options).unwrap_err();
    assert!(error.to_string().contains(&taken.display().to_string()), "{error}");
    // No first part, and no temporary file left over: a conflict writes nothing at all.
    let left: Vec<String> =
        std::fs::read_dir(&directory).unwrap().filter_map(Result::ok).map(|entry| entry.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(left, vec![taken.file_name().unwrap().to_string_lossy().into_owned()]);
}

#[test]
fn a_plan_that_cuts_past_the_document_is_refused() {
    let directory = temp_dir("split-past-end");
    let path = temp_file("past-end.pdf", &outline_document());
    let mut doc = Document::open(&path).unwrap();
    let options = SplitOptions { out_dir: Some(directory), ..SplitOptions::default() };
    let error = doc.split(&SplitPlan::from_page_list("1, 9").unwrap(), &options).unwrap_err();
    assert!(matches!(error, Error::Invalid(_)), "{error}");
}

#[test]
fn an_encrypted_document_is_not_split() {
    let directory = temp_dir("split-encrypted");
    let path = temp_file("encrypted-split.pdf", &encrypted_document());
    let mut doc = Document::open(&path).unwrap();
    let options = SplitOptions { out_dir: Some(directory), ..SplitOptions::default() };
    let error = doc.split(&SplitPlan::equal_parts(1, 1).unwrap(), &options).unwrap_err();
    assert!(matches!(error, Error::Encrypted), "{error}");
}

/// A two-page document whose pages live inside an object stream, reached through a cross-reference
/// stream: the layout producers write, and the one a part has to write out flat.
fn pages_in_object_stream_document() -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets: Vec<u32> = Vec::new();
    add(&mut out, &mut offsets, "<< /Type /Catalog /Pages 2 0 R >>");
    add(&mut out, &mut offsets, "<< /Type /Pages /Kids [4 0 R 5 0 R] /Count 2 >>");

    let page = b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>";
    let header = format!("4 0 5 {} ", page.len() + 1);
    let mut payload = header.clone().into_bytes();
    payload.extend_from_slice(page);
    payload.push(b' ');
    payload.extend_from_slice(page);
    let compressed = pdfcraft_filters::encode_flate(&payload);
    let objstm = add_stream(&mut out, &mut offsets, &format!("/Type /ObjStm /N 2 /First {} /Filter /FlateDecode", header.len()), &compressed);

    const WIDTHS: usize = 7;
    let mut entries: Vec<u8> = Vec::new();
    let mut entry = |kind: u8, field2: u32, field3: u32| {
        entries.push(kind);
        entries.extend_from_slice(&field2.to_be_bytes());
        entries.extend_from_slice(&u16::try_from(field3).unwrap().to_be_bytes());
    };
    entry(0, 0, 65535);
    for offset in &offsets {
        entry(1, *offset, 0);
    }
    // Objects 4 and 5 are members of the object stream; object 6 is this stream itself.
    entry(2, objstm, 0);
    entry(2, objstm, 1);
    let xref_offset = u32::try_from(out.len()).unwrap();
    entry(1, xref_offset, 0);

    let params = Params { predictor: 12, columns: WIDTHS as i64, ..Params::default() };
    let compressed = pdfcraft_filters::encode(&Filter::Flate, &params, &entries).unwrap();
    let size = offsets.len() + 4;
    out.extend_from_slice(
        format!(
            "6 0 obj\n<< /Type /XRef /Size {size} /Root 1 0 R /W [1 4 2] /Index [0 {size}] /Filter /FlateDecode /DecodeParms << /Predictor 12 /Columns {WIDTHS} >> /Length {} >>\nstream\n",
            compressed.len()
        )
        .as_bytes(),
    );
    out.extend_from_slice(&compressed);
    out.extend_from_slice(b"\nendstream\nendobj\nstartxref\n");
    out.extend_from_slice(format!("{xref_offset}\n%%EOF\n").as_bytes());
    out
}

// — M3: text, so a cut can follow what the pages say. ————————————————————————————————————

/// Six pages that between them exercise the text reader: a plain string, a `TJ` array, a form
/// XObject, a composite font with a `/ToUnicode` map, one without, and a page with no content.
fn text_document() -> Vec<u8> {
    let mut out = b"%PDF-1.7
"
    .to_vec();
    let mut offsets: Vec<u32> = Vec::new();
    // The objects are written in this order, so their numbers are fixed: 1 catalog, 2 page tree,
    // 3-8 pages, 9-13 contents, 14 font, 15 form, 16 composite font, 17 its map, 18 a font with
    // no map, 19-21 the outline.
    add(&mut out, &mut offsets, "<< /Type /Catalog /Pages 2 0 R /Outlines 19 0 R >>");
    add(&mut out, &mut offsets, "<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R 6 0 R 7 0 R 8 0 R] /Count 6 >>");
    add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /Font << /F1 14 0 R >> >> /Contents 9 0 R >>");
    add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /Font << /F1 14 0 R >> >> /Contents 10 0 R >>");
    add(
        &mut out,
        &mut offsets,
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /XObject << /Fm0 15 0 R >> >> /Contents 11 0 R >>",
    );
    add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /Font << /F2 16 0 R >> >> /Contents 12 0 R >>");
    add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /Font << /F3 18 0 R >> >> /Contents 13 0 R >>");
    add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] >>");
    add_stream(&mut out, &mut offsets, "", b"BT /F1 12 Tf 72 720 Td (Fattura n. 100) Tj 0 -14 Td (Codice Fiscale: ABC123) Tj ET");
    add_stream(&mut out, &mut offsets, "", b"BT /F1 12 Tf 72 720 Td [(Fattura) -300 (n. 101)] TJ 0 -14 Td (Codice Fiscale: ABC123) Tj ET");
    add_stream(&mut out, &mut offsets, "", b"q /Fm0 Do Q");
    add_stream(&mut out, &mut offsets, "", b"BT /F2 12 Tf 72 700 Td <0001000200030004> Tj ET");
    // The same codes, drawn with a font that has no map at all.
    add_stream(&mut out, &mut offsets, "", b"BT /F3 12 Tf 72 700 Td <0001000200030004> Tj ET");
    add(&mut out, &mut offsets, "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>");
    add_stream(
        &mut out,
        &mut offsets,
        "/Type /XObject /Subtype /Form /BBox [0 0 595 842] /Resources << /Font << /F1 14 0 R >> >>",
        b"BT /F1 12 Tf 72 700 Td (Riga nel form) Tj ET",
    );
    add(&mut out, &mut offsets, "<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+Test /Encoding /Identity-H /ToUnicode 17 0 R >>");
    let cmap = b"/CIDInit /ProcSet findresource begin
12 dict begin
begincmap
/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def
/CMapName /Adobe-Identity-UCS def
/CMapType 2 def
1 begincodespacerange
<0000> <FFFF>
endcodespacerange
2 beginbfchar
<0001> <0041>
<0002> <0042>
endbfchar
1 beginbfrange
<0003> <0004> <0043>
endbfrange
endcmap
CMapName currentdict /CMap defineresource pop
end
end
";
    add_stream(&mut out, &mut offsets, "", cmap);
    add(&mut out, &mut offsets, "<< /Type /Font /Subtype /Type0 /BaseFont /GHIJKL+Test /Encoding /Identity-H >>");
    add(&mut out, &mut offsets, "<< /Type /Outlines /First 20 0 R /Last 21 0 R /Count 2 >>");
    add(&mut out, &mut offsets, "<< /Title (Primo) /Parent 19 0 R /Next 21 0 R /Dest [3 0 R /Fit] >>");
    add(&mut out, &mut offsets, "<< /Title (Secondo) /Parent 19 0 R /Dest [4 0 R /Fit] >>");
    classic(out, &offsets, 1)
}

#[test]
fn a_page_says_what_its_content_streams_show() {
    let mut doc = open("text.pdf", &text_document());
    assert_eq!(doc.page_text(1).unwrap().text, "Fattura n. 100 Codice Fiscale: ABC123");
    // A `TJ` array is read as one string, with the gap its numbers ask for.
    assert_eq!(doc.page_text(2).unwrap().text, "Fattura n. 101 Codice Fiscale: ABC123");
    // A form is drawn by the page, so its text belongs to the page.
    assert_eq!(doc.page_text(3).unwrap().text, "Riga nel form");
    // A composite font is read back through its `/ToUnicode` map, two-byte codes and all.
    assert_eq!(doc.page_text(4).unwrap().text, "ABCD");
    // Without such a map the glyphs cannot become characters, and that is said rather than
    // papers over with the raw bytes.
    let unmapped = doc.page_text(5).unwrap();
    assert_eq!(unmapped.text, "");
    assert!(unmapped.unmapped);
    assert!(!unmapped.truncated);
    // A page with no content at all is empty, not unreadable.
    let empty = doc.page_text(6).unwrap();
    assert!(empty.is_empty());
    assert!(!empty.unmapped);
}

#[test]
fn search_reports_the_pages_that_hold_the_text() {
    let mut doc = open("search.pdf", &text_document());
    let found = doc.search(&Query::literal("Codice Fiscale")).unwrap();
    assert_eq!(found.pages, [1, 2]);
    assert_eq!(found.scanned, 6);
    assert_eq!(found.without_text, [5, 6]);

    // Case is ignored when it is asked to be, and not otherwise.
    assert_eq!(doc.search(&Query::literal("fattura").ignoring_case()).unwrap().pages, [1, 2]);
    assert!(doc.search(&Query::literal("fATTURA")).unwrap().pages.is_empty());
    assert_eq!(doc.search(&Query::literal("FATtura").ignoring_case()).unwrap().pages, [1, 2]);

    // A regular expression, and text that came out of a form XObject.
    assert_eq!(doc.search(&Query::pattern(r"n\. 10[01]")).unwrap().pages, [1, 2]);
    assert_eq!(doc.search(&Query::literal("Riga nel form")).unwrap().pages, [3]);
    // A pattern that does not compile is reported as such, not as a failure to read the file.
    let error = doc.search(&Query::pattern("(")).unwrap_err();
    assert!(error.to_string().contains("espressione regolare"), "{error}");
}

#[test]
fn the_string_mode_opens_a_part_at_every_page_a_mark_names() {
    let directory = temp_dir("split-string");
    let path = temp_file("split-string.pdf", &text_document());
    let mut doc = Document::open(&path).unwrap();
    let found = doc.search(&Query::literal("Fattura")).unwrap();
    assert_eq!(found.pages, [1, 2]);

    let plan = SplitPlan::from_marks(&found.pages, 6, false).unwrap();
    let options = SplitOptions { out_dir: Some(directory), ..SplitOptions::default() };
    let report = doc.split(&plan, &options).unwrap();
    assert_eq!(report.parts.len(), 2);
    assert_eq!(report.parts[0].label, "1");
    assert_eq!(report.parts[1].label, "2-6");
    // Each part opens on the page that matched, and its text is the page's own.
    let mut first = Document::open(&report.parts[0].path).unwrap();
    assert_eq!(first.page_text(1).unwrap().text, "Fattura n. 100 Codice Fiscale: ABC123");
    let mut second = Document::open(&report.parts[1].path).unwrap();
    assert_eq!(second.page_text(1).unwrap().text, "Fattura n. 101 Codice Fiscale: ABC123");
}

#[test]
fn a_part_can_be_named_after_the_text_of_its_pages() {
    let directory = temp_dir("split-names-text");
    let path = temp_file("names.pdf", &text_document());
    let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
    let mut doc = Document::open(&path).unwrap();
    let plan = SplitPlan::from_marks(&[1, 2, 3, 4], 6, false).unwrap();
    let rule = LabelRule::new("Codice Fiscale");
    let options = SplitOptions { out_dir: Some(directory), names: PartName::Text(rule), ..SplitOptions::default() };
    let report = doc.split(&plan, &options).unwrap();
    let names: Vec<String> = report.parts.iter().map(|part| part.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
    // Two pages carry the same code; the third and fourth carry none, so they fall back to the
    // input's name and the sequence.
    let expected: Vec<String> = vec!["ABC123.pdf".to_string(), "ABC123-02.pdf".to_string(), format!("{stem}-03.pdf"), format!("{stem}-04.pdf")];
    assert_eq!(names, expected);
}

#[test]
fn the_sequence_can_be_asked_for_on_every_name() {
    let directory = temp_dir("split-names-always");
    let path = temp_file("named.pdf", &text_document());
    let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
    let mut doc = Document::open(&path).unwrap();
    let plan = SplitPlan::from_marks(&[1, 2], 6, false).unwrap();
    let options = SplitOptions {
        out_dir: Some(directory),
        names: PartName::Text(LabelRule::new("Codice Fiscale")),
        template: NameTemplate { always: true, ..NameTemplate::default() },
        ..SplitOptions::default()
    };
    let report = doc.split(&plan, &options).unwrap();
    let names: Vec<String> = report.parts.iter().map(|part| part.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
    assert_eq!(names, vec!["ABC123-01.pdf", "ABC123-02.pdf"]);
    assert!(!stem.is_empty());
}

#[test]
fn a_part_can_be_named_after_the_bookmark_that_opened_it() {
    let directory = temp_dir("split-names-title");
    let path = temp_file("titles.pdf", &text_document());
    let mut doc = Document::open(&path).unwrap();
    let bookmarks = doc.bookmarks().unwrap().to_vec();
    let plan = SplitPlan::from_bookmarks(&bookmarks, 1, 6).unwrap();
    assert_eq!(plan.len(), 2);
    assert_eq!(plan.pages(1), [2, 3, 4, 5, 6]);

    let options = SplitOptions { out_dir: Some(directory), names: PartName::Bookmark, ..SplitOptions::default() };
    let report = doc.split(&plan, &options).unwrap();
    let names: Vec<String> = report.parts.iter().map(|part| part.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
    assert_eq!(names, vec!["Primo.pdf", "Secondo.pdf"]);
}

#[test]
fn a_name_from_text_cannot_land_on_the_input_file() {
    let path = temp_file("fallback.pdf", &text_document());
    let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
    let mut doc = Document::open(&path).unwrap();
    // A label no page carries: every part falls back to the input's name, numbered.
    let plan = SplitPlan::from_marks(&[1], 6, false).unwrap();
    let options = SplitOptions {
        out_dir: path.parent().map(std::path::Path::to_path_buf),
        names: PartName::Text(LabelRule::new("Non c'è")),
        ..SplitOptions::default()
    };
    let report = doc.split(&plan, &options).unwrap();
    let name = report.parts[0].path.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(name, format!("{stem}-01.pdf"));
    assert!(report.parts[0].path.exists());
    std::fs::remove_file(&report.parts[0].path).unwrap();
}

#[test]
fn cancelling_during_the_write_leaves_no_file_behind() {
    let directory = temp_dir("cancel-write");
    let path = temp_file("cancel.pdf", &text_document());
    let mut doc = Document::open(&path).unwrap();
    let plan = SplitPlan::equal_parts(6, 3).unwrap();
    // The second part has begun: the first is already written, and neither may survive.
    let stop = Canceller::default();
    let hit = stop.clone();
    let job = Job::stopping(
        move |update| {
            if update.stage == Stage::Write && update.part == 2 {
                hit.cancel();
            }
        },
        stop,
    );
    let options = SplitOptions { out_dir: Some(directory.clone()), ..SplitOptions::default() };
    let error = doc.split_with(&plan, &options, &job).unwrap_err();
    assert!(matches!(error, Error::Cancelled), "{error}");
    let left: Vec<String> =
        std::fs::read_dir(&directory).unwrap().filter_map(|entry| entry.ok().map(|entry| entry.file_name().to_string_lossy().into_owned())).collect();
    assert!(left.is_empty(), "cancelling left {left:?} behind");
}

#[test]
fn cancelling_a_search_stops_it() {
    let path = temp_file("cancel-search.pdf", &text_document());
    let mut doc = Document::open(&path).unwrap();
    let stop = Canceller::default();
    let hit = stop.clone();
    let job = Job::stopping(
        move |update| {
            if update.stage == Stage::Search {
                hit.cancel();
            }
        },
        stop,
    );
    let error = doc.search_with(&Query::literal("Fattura"), &job).unwrap_err();
    assert!(matches!(error, Error::Cancelled), "{error}");
}

#[test]
fn a_split_says_which_phase_it_is_in() {
    let directory = temp_dir("phases");
    let path = temp_file("phases.pdf", &text_document());
    let mut doc = Document::open(&path).unwrap();
    let plan = SplitPlan::equal_parts(6, 3).unwrap();
    let seen: std::sync::Arc<Mutex<Vec<Update>>> = std::sync::Arc::default();
    let sink = std::sync::Arc::clone(&seen);
    let job = Job::new(move |update| sink.lock().map(|mut list| list.push(update)).unwrap_or(()));
    let options = SplitOptions { out_dir: Some(directory), ..SplitOptions::default() };
    let report = doc.split_with(&plan, &options, &job).unwrap();
    let updates: Vec<Update> = seen.lock().map(|list| list.clone()).unwrap_or_default();

    let stages: Vec<Stage> = updates.iter().map(|update| update.stage).collect();
    assert!(stages.windows(2).all(|pair| pair[0] <= pair[1]), "phases went backwards: {stages:?}");
    for stage in [Stage::Structure, Stage::Plan, Stage::Write, Stage::Verify] {
        assert!(stages.contains(&stage), "{stage:?} was never reported: {stages:?}");
    }
    // Planning counts the names settled, not the parts there are: it opens at nothing and only
    // closes at the last name, so a console never starts on `piano 100%`.
    let planning: Vec<Update> = updates.iter().filter(|update| update.stage == Stage::Plan).copied().collect();
    assert!(planning.first().is_some_and(|update| update.fraction() < 1.0), "the plan opened as finished: {planning:?}");
    assert!(planning.windows(2).all(|pair| pair[0].fraction() <= pair[1].fraction()), "the plan went backwards: {planning:?}");
    assert!(planning.last().is_some_and(|update| update.fraction() == 1.0), "the plan never finished: {planning:?}");
    // The last word is that every part was written and checked.
    let last = *updates.last().unwrap();
    assert_eq!(last.stage, Stage::Verify);
    assert_eq!(last.done, report.parts.len() as u64);
    assert_eq!(last.total, report.parts.len() as u64);
    // The writing is measured in bytes of the source, and it ends there.
    let written: Vec<u64> = updates.iter().filter(|update| update.stage == Stage::Write).map(|update| update.done).collect();
    assert!(written.windows(2).all(|pair| pair[0] <= pair[1]), "the byte count went backwards: {written:?}");
    assert!(written.last().is_some_and(|done| *done <= report.source_size));
}

#[test]
fn a_search_counts_the_pages_as_it_reads_them() {
    let path = temp_file("search-phases.pdf", &text_document());
    let mut doc = Document::open(&path).unwrap();
    let seen: std::sync::Arc<Mutex<Vec<Update>>> = std::sync::Arc::default();
    let sink = std::sync::Arc::clone(&seen);
    let job = Job::new(move |update| sink.lock().map(|mut list| list.push(update)).unwrap_or(()));
    let found = doc.search_with(&Query::literal("Fattura"), &job).unwrap();
    let updates: Vec<Update> = seen.lock().map(|list| list.clone()).unwrap_or_default();
    let search: Vec<&Update> = updates.iter().filter(|update| update.stage == Stage::Search).collect();
    assert_eq!(search.len(), found.scanned, "one report per page");
    assert!(search.iter().all(|update| update.total == found.scanned as u64));
    assert_eq!(search.last().map(|update| update.done), Some(found.scanned as u64));
}

#[test]
fn compressing_packs_the_part_and_writes_a_cross_reference_stream() {
    let directory = temp_dir("compress");
    let path = temp_file("compress.pdf", &text_document());
    let mut doc = Document::open(&path).unwrap();
    let plan = SplitPlan::equal_parts(6, 2).unwrap();
    let options = SplitOptions { out_dir: Some(directory.clone()), compression: Compression::Compress, ..SplitOptions::default() };
    let report = doc.split(&plan, &options).unwrap();
    // Nothing was asked: the part keeps the 1.7 of the file it came from, and packs its objects
    // anyway, since object streams are allowed from 1.5 on.
    assert_eq!(report.version, Version::PDF_1_7, "a compressed part keeps the source's version");
    assert_eq!(report.parts.len(), 2);
    for part in &report.parts {
        let bytes = std::fs::read(&part.path).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.starts_with("%PDF-1.7"), "{}", part.path.display());
        assert!(text.contains("/ObjStm"), "the part's own objects are packed");
        assert!(text.contains("/Type /XRef"), "the offsets are in a cross-reference stream");
        // Our own reader reopens what the writer wrote; the split already did that, and here it
        // is done once more on the finished name.
        let mut reader = Document::open(&part.path).unwrap();
        assert_eq!(reader.page_count().unwrap(), part.pages);
    }
}

#[test]
fn a_part_is_written_in_the_version_the_pages_need() {
    let directory = temp_dir("version");
    let path = temp_file("version.pdf", &text_document());
    let mut doc = Document::open(&path).unwrap();
    let plan = SplitPlan::equal_parts(6, 1).unwrap();
    // A rewrite asked for 1.4 with compression: 1.4 cannot say what a cross-reference stream
    // says, so the part is written as 1.5 and the report explains why.
    let options =
        SplitOptions { out_dir: Some(directory), compression: Compression::Compress, version: Some(Version::PDF_1_4), ..SplitOptions::default() };
    let report = doc.split(&plan, &options).unwrap();
    assert_eq!(report.version, Version::PDF_1_5);
    assert!(report.warnings.iter().any(|note| note.contains("1.5")), "{:?}", report.warnings);
    let bytes = std::fs::read(&report.parts[0].path).unwrap();
    assert!(String::from_utf8_lossy(&bytes).starts_with("%PDF-1.5"));
}

#[test]
fn a_neutral_split_keeps_the_streams_and_the_version_it_was_asked_for() {
    let directory = temp_dir("neutral");
    let path = temp_file("neutral.pdf", &text_document());
    let mut doc = Document::open(&path).unwrap();
    let plan = SplitPlan::equal_parts(6, 2).unwrap();
    let options =
        SplitOptions { out_dir: Some(directory), compression: Compression::Neutral, version: Some(Version::PDF_1_7), ..SplitOptions::default() };
    let report = doc.split(&plan, &options).unwrap();
    assert_eq!(report.version, Version::PDF_1_7);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    for part in &report.parts {
        let text = String::from_utf8_lossy(&std::fs::read(&part.path).unwrap()).into_owned();
        assert!(text.starts_with("%PDF-1.7"));
        assert!(!text.contains("/ObjStm"));
        assert!(text.contains("xref\n"), "a neutral part keeps the classic table");
    }
}

#[test]
fn the_name_template_names_every_part_the_way_it_was_asked() {
    let directory = temp_dir("template");
    let path = temp_file("template.pdf", &text_document());
    let mut doc = Document::open(&path).unwrap();
    let plan = SplitPlan::equal_parts(6, 3).unwrap();
    let template = NameTemplate {
        prefix: "Fascicolo ".to_string(),
        suffix: " fine".to_string(),
        sequence: SequencePosition::Start,
        width: 3,
        start: 7,
        always: true,
    };
    let options = SplitOptions { out_dir: Some(directory.clone()), template, compression: Compression::Neutral, ..SplitOptions::default() };
    let report = doc.split(&plan, &options).unwrap();
    let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
    let names: Vec<String> = report.parts.iter().map(|part| part.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
    // The prefix and the suffix surround the base once, the number stands in front of it with the
    // width that was asked for, and the count starts where it was told to.
    assert_eq!(
        names,
        vec![format!("Fascicolo 007-{stem} fine.pdf"), format!("Fascicolo 008-{stem} fine.pdf"), format!("Fascicolo 009-{stem} fine.pdf"),]
    );
}

#[test]
fn an_output_folder_that_is_not_there_yet_is_made() {
    let root = temp_dir("split-missing-out");
    let path = temp_file("missing-out.pdf", &text_document());
    let folder = root.join("nuova").join("sotto");
    assert!(!folder.exists());
    let mut doc = Document::open(&path).unwrap();
    let plan = SplitPlan::equal_parts(6, 2).unwrap();
    let options = SplitOptions { out_dir: Some(folder.clone()), ..SplitOptions::default() };
    let report = doc.split(&plan, &options).unwrap();
    assert_eq!(report.parts.len(), 2);
    assert!(report.parts.iter().all(|part| part.path.starts_with(&folder) && part.path.exists()));
}
/// Four pages, the first carrying two links that leave it: one `/Dest` into page 3 and one
/// `/GoTo` into page 4. Following either would bring the page, its `/Parent` — the page tree —
/// and with the tree every page of the document into the part.
fn linked_document() -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets: Vec<u32> = Vec::new();
    add(&mut out, &mut offsets, "<< /Type /Catalog /Pages 2 0 R >>");
    add(&mut out, &mut offsets, "<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R 6 0 R] /Count 4 >>");
    add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 7 0 R /Annots [11 0 R 12 0 R] >>");
    add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 8 0 R >>");
    add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 9 0 R >>");
    add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 10 0 R >>");
    add_stream(&mut out, &mut offsets, "", b"BT (Prima) Tj ET");
    add_stream(&mut out, &mut offsets, "", b"BT (Seconda) Tj ET");
    add_stream(&mut out, &mut offsets, "", b"BT (Terza) Tj ET");
    add_stream(&mut out, &mut offsets, "", b"BT (Quarta) Tj ET");
    add(&mut out, &mut offsets, "<< /Type /Annot /Subtype /Link /Rect [0 0 20 20] /Border [0 0 0] /P 3 0 R /Dest [5 0 R /Fit] >>");
    add(&mut out, &mut offsets, "<< /Type /Annot /Subtype /Link /Rect [0 0 20 20] /Border [0 0 0] /P 3 0 R /A << /S /GoTo /D [6 0 R /Fit] >> >>");
    classic(out, &offsets, 1)
}

#[test]
fn a_link_into_another_part_does_not_drag_its_page_in() {
    let directory = temp_dir("link-out");
    let path = temp_file("link-out.pdf", &linked_document());
    let mut doc = Document::open(&path).unwrap();
    let options = SplitOptions { out_dir: Some(directory), compression: Compression::Neutral, ..SplitOptions::default() };
    let report = doc.split(&SplitPlan::from_page_list("1-2").unwrap(), &options).unwrap();
    assert_eq!(report.parts.len(), 1);
    let part = &report.parts[0];
    let text = String::from_utf8_lossy(&std::fs::read(&part.path).unwrap()).into_owned();
    // The part is a part of two pages, not a copy of the file: the pages it does not hold are
    // nowhere in it, content and all.
    for gone in ["Terza", "Quarta"] {
        assert!(!text.contains(gone), "{gone} came along: {}", part.path.display());
    }
    for kept in ["Prima", "Seconda"] {
        assert!(text.contains(kept), "{kept} is missing");
    }
    // A destination into a page the part does not hold is dropped rather than left dangling.
    assert!(!text.contains("/Dest ["), "a dead destination was written");
    assert!(!text.contains("/GoTo"), "a dead action was written");
    // The annotation itself is still there: the page keeps its link object, minus the link.
    assert!(text.contains("/Subtype /Link"));
    // The part is smaller than the document it came from, which is the point.
    assert!(part.bytes < doc.file_size());
}

#[test]
fn a_source_page_tree_is_never_written_into_a_part() {
    // Exactly the part's own tree, with the pages it holds and nothing of the source's.
    let directory = temp_dir("stray-page");
    let path = temp_file("stray-page.pdf", &linked_document());
    let mut doc = Document::open(&path).unwrap();
    let options = SplitOptions { out_dir: Some(directory), compression: Compression::Neutral, ..SplitOptions::default() };
    let report = doc.split(&SplitPlan::from_page_list("2").unwrap(), &options).unwrap();
    let text = String::from_utf8_lossy(&std::fs::read(&report.parts[0].path).unwrap()).into_owned();
    assert!(!text.contains("Prima") && !text.contains("Terza"));
    assert_eq!(text.matches("/Type /Pages").count(), 1, "{text}");
    assert!(text.contains("/Count 1"));
}

/// A document of `pages` pages whose page tree declares `declared` as its `/Count`.
fn many_pages_document(pages: usize, declared: usize) -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets: Vec<u32> = Vec::new();
    add(&mut out, &mut offsets, "<< /Type /Catalog /Pages 2 0 R >>");
    let kids: Vec<String> = (0..pages).map(|page| format!("{} 0 R", page + 3)).collect();
    add(&mut out, &mut offsets, &format!("<< /Type /Pages /Kids [{}] /Count {declared} >>", kids.join(" ")));
    for _ in 0..pages {
        add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>");
    }
    classic(out, &offsets, 1)
}

#[test]
fn a_declared_page_count_that_is_too_low_does_not_shorten_the_cut() {
    // Above twenty thousand pages a walk is not done for the count, and a /Count written lower
    // than the truth would leave the last pages out of every plan without a word.
    let path = temp_file("low-count.pdf", &many_pages_document(20_100, 20_001));
    let mut doc = Document::open(&path).unwrap();
    assert_eq!(doc.page_count().unwrap(), 20_001, "the tree's own /Count is what a reader is given");
    assert_eq!(doc.page_count_exact().unwrap(), 20_100, "a cut plans on the walk, not on the declaration");
}

#[test]
fn the_parts_hold_their_dictionaries_in_several_object_streams() {
    // One stream for the whole part would make every reader decode all of it to reach one object.
    let directory = temp_dir("objstm-groups");
    let path = temp_file("objstm-groups.pdf", &many_pages_document(150, 150));
    let mut doc = Document::open(&path).unwrap();
    let options = SplitOptions { out_dir: Some(directory), compression: Compression::Compress, ..SplitOptions::default() };
    let report = doc.split(&SplitPlan::equal_parts(150, 1).unwrap(), &options).unwrap();
    let text = String::from_utf8_lossy(&std::fs::read(&report.parts[0].path).unwrap()).into_owned();
    let streams = text.matches("/Type /ObjStm").count();
    assert!(streams >= 2, "the part packed everything into {streams} stream(s)");
    let mut reopened = Document::open(&report.parts[0].path).unwrap();
    assert_eq!(reopened.page_count().unwrap(), 150);
}

#[test]
fn the_layers_and_the_lost_entries_are_told() {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets: Vec<u32> = Vec::new();
    add(
        &mut out,
        &mut offsets,
        "<< /Type /Catalog /Pages 2 0 R /PageLabels << /Nums [0 << /S /D >>] >> /OCProperties << /OCGs [4 0 R] /D << /ON [4 0 R] /OFF [] /Order [4 0 R] >> >> >>",
    );
    add(&mut out, &mut offsets, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>");
    add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>");
    add(&mut out, &mut offsets, "<< /Type /OCG /Name (Livello) >>");
    let bytes = classic(out, &offsets, 1);

    let directory = temp_dir("layers");
    let path = temp_file("layers.pdf", &bytes);
    let mut doc = Document::open(&path).unwrap();
    let options = SplitOptions { out_dir: Some(directory), ..SplitOptions::default() };
    let report = doc.split(&SplitPlan::equal_parts(1, 1).unwrap(), &options).unwrap();
    let text = String::from_utf8_lossy(&std::fs::read(&report.parts[0].path).unwrap()).into_owned();
    // The layer configuration comes along, so a layer the source hid does not come back visible.
    assert!(text.contains("/OCProperties") && text.contains("/OCG"), "{text}");
    assert!(text.contains("(Livello)"));
    // What a part cannot carry is named in the report instead of disappearing quietly.
    assert!(report.warnings.iter().any(|note| note.contains("/PageLabels")), "{:?}", report.warnings);
}

/// Two pages and a form: page 1 carries a `/FreeText` note that has no `/Type`, and the widget of
/// a radio group whose `/Parent` is the field holding `/FT`, `/V` and `/T`; page 2 carries a text
/// field whose widget has an appearance stream of its own, sixty kilobytes of it.
fn form_document() -> Vec<u8> {
    let mut out = b"%PDF-1.7\n".to_vec();
    let mut offsets: Vec<u32> = Vec::new();
    add(&mut out, &mut offsets, "<< /Type /Catalog /Pages 2 0 R /AcroForm 9 0 R >>");
    add(&mut out, &mut offsets, "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>");
    add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 5 0 R /Annots [6 0 R 7 0 R] >>");
    add(&mut out, &mut offsets, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 8 0 R /Annots [12 0 R] >>");
    add_stream(&mut out, &mut offsets, "", b"BT (Prima) Tj ET");
    // A note, not a page: no /Type, and a /Contents that says what it says.
    add(&mut out, &mut offsets, "<< /Subtype /FreeText /Rect [10 10 90 40] /Contents (Nota della pagina uno) >>");
    // A widget whose parent is the radio group's field.
    add(&mut out, &mut offsets, "<< /Type /Annot /Subtype /Widget /Rect [10 70 40 100] /Parent 10 0 R /P 3 0 R >>");
    add_stream(&mut out, &mut offsets, "", b"BT (Seconda) Tj ET");
    add(&mut out, &mut offsets, "<< /Fields [10 0 R 13 0 R] /CO [13 0 R] /DA (/Helv 0 Tf 0 g) >>");
    add(&mut out, &mut offsets, "<< /FT /Btn /T (Sesso) /Ff 32768 /Kids [7 0 R] >>");
    let mut appearance = b"ASPETTO-ESTERNO ".to_vec();
    appearance.extend_from_slice(&[b'x'; 60_000]);
    add_stream(&mut out, &mut offsets, "", &appearance);
    add(&mut out, &mut offsets, "<< /Type /Annot /Subtype /Widget /Rect [10 10 40 40] /Parent 13 0 R /P 4 0 R /AP << /N 11 0 R >> >>");
    add(&mut out, &mut offsets, "<< /FT /Tx /T (Nome) /Kids [12 0 R] >>");
    classic(out, &offsets, 1)
}

#[test]
fn a_note_without_a_type_is_written_as_the_note_it_is() {
    let directory = temp_dir("form-note");
    let path = temp_file("form-note.pdf", &form_document());
    let mut doc = Document::open(&path).unwrap();
    let options = SplitOptions { out_dir: Some(directory), compression: Compression::Neutral, ..SplitOptions::default() };
    let report = doc.split(&SplitPlan::from_page_list("1").unwrap(), &options).unwrap();
    let text = String::from_utf8_lossy(&std::fs::read(&report.parts[0].path).unwrap()).into_owned();
    // It has a /Contents of its own, which used to be taken for the sign of a page: the note was
    // written out as a null and vanished without a word.
    assert!(text.contains("/Subtype /FreeText"), "{text}");
    assert!(text.contains("(Nota della pagina uno)"), "la nota è sparita: {text}");
    assert_eq!(report.parts[0].pages, 1);
}

#[test]
fn a_radio_group_keeps_the_field_its_widget_belongs_to() {
    let directory = temp_dir("form-radio");
    let path = temp_file("form-radio.pdf", &form_document());
    let mut doc = Document::open(&path).unwrap();
    let options = SplitOptions { out_dir: Some(directory), compression: Compression::Neutral, ..SplitOptions::default() };
    let report = doc.split(&SplitPlan::from_page_list("1").unwrap(), &options).unwrap();
    let text = String::from_utf8_lossy(&std::fs::read(&report.parts[0].path).unwrap()).into_owned();
    // A widget whose /Parent was dropped inherits neither /FT nor /V nor /T: the group opens empty.
    let widget = text.split("/Subtype /Widget").nth(1).unwrap_or("");
    let body = widget.split(">>").next().unwrap_or("");
    assert!(body.contains("/Parent "), "il widget ha perso il campo: {body}");
    assert!(text.contains("/FT /Btn") && text.contains("/T (Sesso)"), "{text}");
    // /Kids holds the widget of this page, and the form is the part's own: one field, not two.
    let field = text.split("/FT /Btn").nth(1).unwrap_or("");
    let field_body = field.split(">>").next().unwrap_or("");
    assert!(field_body.contains("/Kids ["), "il campo ha perso il suo widget: {field_body}");
    assert!(!text.contains("/T (Nome)"), "il campo della pagina due è arrivato: {text}");
}

#[test]
fn a_field_of_another_page_does_not_bring_its_appearance_along() {
    // Copying /AcroForm whole carried every field, and with each field the widgets of the pages
    // this part does not hold, each with the appearance stream behind it.
    let directory = temp_dir("form-other");
    let path = temp_file("form-other.pdf", &form_document());
    let mut doc = Document::open(&path).unwrap();
    let options = SplitOptions { out_dir: Some(directory), compression: Compression::Neutral, ..SplitOptions::default() };
    let report = doc.split(&SplitPlan::from_page_list("1").unwrap(), &options).unwrap();
    let text = String::from_utf8_lossy(&std::fs::read(&report.parts[0].path).unwrap()).into_owned();
    assert!(!text.contains("ASPETTO-ESTERNO"), "l'aspetto della pagina due è arrivato: {text}");
    assert!(report.parts[0].bytes * 4 < doc.file_size(), "la parte pesa {} sul sorgente {}", report.parts[0].bytes, doc.file_size());
}

#[test]
fn the_space_a_split_needs_is_estimated_from_the_pages_it_covers() {
    // A plan that keeps a four-hundredth of the document needs a four-hundredth of its bytes, not
    // all of them: the whole source is the answer only when every page is in the plan.
    assert_eq!(crate::estimated_output(8_000, 4, 400), 80);
    assert_eq!(crate::estimated_output(8_000, 400, 400), 8_000);
    assert_eq!(crate::estimated_output(8_000, 0, 400), 0);
    // More pages planned than the document has (a duplicated page) still gives a sane figure.
    assert_eq!(crate::estimated_output(u64::MAX, 4, 2), u64::MAX);
}

#[test]
fn a_part_that_cannot_take_its_name_says_so() {
    let directory = temp_dir("unnamed");
    let path = temp_file("unnamed.pdf", &text_document());
    let mut doc = Document::open(&path).unwrap();
    let options = SplitOptions { out_dir: Some(directory), names: PartName::Text(LabelRule::new("Codice Fiscale")), ..SplitOptions::default() };
    let report = doc.split(&SplitPlan::equal_parts(6, 2).unwrap(), &options).unwrap();
    assert_eq!(report.parts.len(), 2);
    assert!(report.warnings.iter().any(|note| note.contains("trovato il nome")), "{:?}", report.warnings);
}
