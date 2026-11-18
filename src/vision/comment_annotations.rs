//! A render copy of a PDF with its comment-type annotations hidden.
//!
//! Comments are reported beside the Markdown (`PdfAnnotation`) and never in
//! it. Native extraction never reads annotation appearances, but a page
//! routed to OCR is read from its rendering, and PDFium draws annotation
//! appearances by default: a FreeText box or an open note would be
//! recognized into the page's Markdown. [`hide_comment_annotations`] returns
//! a copy of the file in which those annotations carry the Hidden flag, so
//! the renderer leaves them out while stamps, ink, links, form widgets and
//! every other annotation are still drawn.
//!
//! The copy is the original file followed by one incremental update (PDF
//! 32000-1, 7.5.6) that redefines the affected objects. Nothing before the
//! update changes, so the rest of the document renders from the original
//! bytes, and lopdf re-encrypts the redefined objects for an encrypted file.

use std::collections::HashSet;

use lopdf::xref::XrefEntry;
use lopdf::{Dictionary, Document, IncrementalDocument, Object, ObjectId};

/// Annotation subtypes hidden from OCR renders: notes, FreeText, popups and
/// text markup. Their text is reported as `PdfAnnotation`s instead.
pub(crate) const COMMENT_SUBTYPES: [&[u8]; 7] = [
    b"Text",
    b"FreeText",
    b"Popup",
    b"Highlight",
    b"Underline",
    b"StrikeOut",
    b"Squiggly",
];

/// Annotation flag bit 2 (PDF 32000-1, table 165): do not display or print.
const HIDDEN_FLAG: i64 = 2;

/// `/Annots` entries examined per document. A document with more cannot be
/// verified, and is rendered without annotations.
const MAX_ANNOTS_EXAMINED: usize = 100_000;

/// Same bound native extraction puts on one decompressed object stream.
const MAX_DECOMPRESSED_BYTES: usize = crate::MAX_STREAM_DECOMPRESSED_BYTES;

/// Why no verified render copy could be made. The caller renders the
/// original without any annotations, so no comment can reach OCR.
#[derive(Debug)]
pub(crate) enum HideError {
    /// lopdf could not read the file (or decrypt it with the password).
    Load(lopdf::Error),
    /// Some page or `/Annots` entry could not be read, for example an object
    /// stream over the decompression bound, so a comment may be missed.
    Unverified(String),
    /// lopdf could not write the update, e.g. an encrypted file whose
    /// `/Encrypt` dictionary is not an indirect object.
    Save(std::io::Error),
}

impl std::fmt::Display for HideError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Load(error) => write!(formatter, "could not read the PDF: {error}"),
            Self::Unverified(reason) => formatter.write_str(reason),
            Self::Save(error) => write!(formatter, "could not write the render copy: {error}"),
        }
    }
}

/// The result of [`hide_comment_annotations`].
#[derive(Debug)]
pub(crate) struct HiddenCopy {
    /// The copy to render, or `None` when no page has a comment annotation
    /// and the original can be rendered as it is.
    pub(crate) bytes: Option<Vec<u8>>,
    /// Pages lopdf found. The renderer checks PDFium sees as many, since a
    /// page lopdf missed was not searched for comments.
    pub(crate) page_count: usize,
}

/// A copy of `pdf` whose comment-type annotations ([`COMMENT_SUBTYPES`]) are
/// hidden. Fails when that cannot be made or verified: every page's
/// `/Annots` entries must resolve.
///
/// Every page is covered, not only the ones about to be rendered, so the
/// result does not depend on lopdf and PDFium agreeing on page order. Bytes
/// before the `%PDF-` header are dropped, as lopdf and PDFium both read the
/// file from its header.
pub(crate) fn hide_comment_annotations(
    pdf: &[u8],
    password: Option<&str>,
) -> Result<HiddenCopy, HideError> {
    let header = pdf.windows(5).position(|w| w == b"%PDF-").unwrap_or(0);
    let pdf = &pdf[header..];
    let doc = load(pdf, password).map_err(HideError::Load)?;
    let page_count = doc.get_pages().len();

    let changes = hidden_objects(&doc).map_err(HideError::Unverified)?;
    let bytes = if changes.is_empty() {
        None
    } else {
        Some(append_update(pdf, doc, changes).map_err(HideError::Save)?)
    };
    Ok(HiddenCopy { bytes, page_count })
}

/// `pdf` (from its header) followed by an incremental update that redefines
/// `changes`. `doc` is `pdf` as lopdf loaded it.
fn append_update(
    pdf: &[u8],
    doc: Document,
    changes: Vec<(ObjectId, Object)>,
) -> std::io::Result<Vec<u8>> {
    let had_xref_start = doc.xref_start != 0;
    let mut update = IncrementalDocument::create_from(pdf.to_vec(), doc);
    // `new_from_prev` copies the last trailer, which for a cross-reference
    // stream is that stream's dictionary. Keep only the document-level
    // entries; lopdf adds `/Size`, the stream keys and `/Encrypt` itself.
    // `/XRefStm` and `/DecodeParms` must not carry over: they describe the
    // previous section, not this one.
    let previous = std::mem::take(&mut update.new_document.trailer);
    let trailer = &mut update.new_document.trailer;
    for key in [b"Root".as_slice(), b"Info", b"ID"] {
        if let Ok(value) = previous.get(key) {
            trailer.set(key, value.clone());
        }
    }
    if had_xref_start {
        if let Ok(prev) = previous.get(b"Prev") {
            trailer.set("Prev", prev.clone());
        }
    }
    for (id, object) in changes {
        update.new_document.set_object(id, object);
    }

    let mut out = Vec::with_capacity(pdf.len() + 4096);
    update.save_to(&mut out)?;
    Ok(out)
}

fn load(pdf: &[u8], password: Option<&str>) -> Result<Document, lopdf::Error> {
    let options = |password: Option<&str>| lopdf::LoadOptions {
        password: password.map(str::to_string),
        max_decompressed_size: Some(MAX_DECOMPRESSED_BYTES),
        ..Default::default()
    };
    // Without a password lopdf tries the empty one, which opens files
    // encrypted with only an owner password.
    match Document::load_mem_with_options(pdf, options(password)) {
        Err(_) if password.is_some() => Document::load_mem_with_options(pdf, options(None)),
        result => result,
    }
}

/// The objects to redefine: each comment annotation that is an indirect
/// object, with the Hidden flag set, and the `/Annots` array (or the page,
/// when the array is direct) for annotations written directly in it.
///
/// Fails when a page, its `/Annots` array or one of its entries is missing
/// from what lopdf loaded (an object stream over the decompression bound, a
/// damaged object), or there are more entries than [`MAX_ANNOTS_EXAMINED`]:
/// PDFium may still draw what was not seen. An entry that is present but not
/// a dictionary is no annotation to PDFium either.
fn hidden_objects(doc: &Document) -> Result<Vec<(ObjectId, Object)>, String> {
    let mut changes = Vec::new();
    let mut seen = HashSet::new();
    let mut examined = 0usize;
    for page_id in doc.get_pages().into_values() {
        let page = doc
            .get_dictionary(page_id)
            .map_err(|_| format!("page object {page_id:?} could not be read"))?;
        let (array_id, annots) = match page.get(b"Annots") {
            Ok(Object::Reference(id)) => match doc.get_object(*id) {
                Ok(Object::Array(annots)) => (Some(*id), annots),
                Ok(_) => continue,
                Err(_) if never_written(doc, *id) => continue,
                Err(_) => return Err(format!("/Annots array {id:?} could not be read")),
            },
            Ok(Object::Array(annots)) => (None, annots),
            _ => continue,
        };
        if array_id.is_some_and(|id| !seen.insert(id)) {
            continue;
        }

        let mut direct: Option<Vec<Object>> = None;
        for (index, entry) in annots.iter().enumerate() {
            if examined >= MAX_ANNOTS_EXAMINED {
                return Err(format!("more than {MAX_ANNOTS_EXAMINED} /Annots entries"));
            }
            examined += 1;
            match entry {
                Object::Reference(id) => {
                    if !seen.insert(*id) {
                        continue;
                    }
                    match doc.get_object(*id) {
                        Ok(Object::Dictionary(dict)) if is_comment(doc, dict) => {
                            changes.push((*id, Object::Dictionary(hidden(doc, dict))));
                        }
                        Ok(_) => {}
                        Err(_) if never_written(doc, *id) => {}
                        Err(_) => return Err(format!("annotation {id:?} could not be read")),
                    }
                }
                Object::Dictionary(dict) if is_comment(doc, dict) => {
                    direct.get_or_insert_with(|| annots.clone())[index] =
                        Object::Dictionary(hidden(doc, dict));
                }
                _ => {}
            }
        }

        if let Some(annots) = direct {
            match array_id {
                Some(id) => changes.push((id, Object::Array(annots))),
                None => {
                    let mut page = page.clone();
                    page.set("Annots", Object::Array(annots));
                    changes.push((page_id, Object::Dictionary(page)));
                }
            }
        }
    }
    Ok(changes)
}

/// Whether `id` has no entry in the file's cross-reference table, or a free
/// one: a reference to it is null to every reader. An object the table
/// lists that lopdf did not load (an object stream over the decompression
/// bound, a damaged object) may still be drawn by PDFium.
fn never_written(doc: &Document, id: ObjectId) -> bool {
    matches!(
        doc.reference_table.get(id.0),
        None | Some(XrefEntry::Free | XrefEntry::UnusableFree)
    )
}

fn is_comment(doc: &Document, dict: &Dictionary) -> bool {
    dict.get(b"Subtype")
        .and_then(|subtype| doc.dereference(subtype))
        .and_then(|(_, subtype)| subtype.as_name())
        .is_ok_and(|subtype| COMMENT_SUBTYPES.contains(&subtype))
}

/// `dict` with the Hidden flag added to its `/F` flags.
fn hidden(doc: &Document, dict: &Dictionary) -> Dictionary {
    let flags = dict
        .get(b"F")
        .and_then(|flags| doc.dereference(flags))
        .ok()
        .and_then(|(_, flags)| match flags {
            Object::Integer(value) => Some(*value),
            Object::Real(value) if value.is_finite() => Some(*value as i64),
            _ => None,
        })
        .unwrap_or(0);
    let mut dict = dict.clone();
    dict.set("F", Object::Integer(flags | HIDDEN_FLAG));
    dict
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::dictionary;

    /// A one-page document with the given annotations, saved by lopdf with
    /// the requested cross-reference format.
    fn document(annots: Vec<Object>, direct: bool, xref_stream: bool) -> Vec<u8> {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let annots = if direct {
            Object::Array(annots)
        } else {
            let ids: Vec<Object> = annots
                .into_iter()
                .map(|annot| Object::Reference(doc.add_object(annot)))
                .collect();
            Object::Array(ids)
        };
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 200.into(), 200.into()],
            "Annots" => annots,
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        if xref_stream {
            doc.reference_table.cross_reference_type = lopdf::xref::XrefType::CrossReferenceStream;
        }
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    fn annot(subtype: &str, flags: Option<i64>) -> Object {
        let mut dict = dictionary! {
            "Type" => "Annot",
            "Subtype" => Object::Name(subtype.as_bytes().to_vec()),
            "Rect" => vec![10.into(), 10.into(), 50.into(), 50.into()],
            "Contents" => Object::string_literal(format!("{subtype} text")),
        };
        if let Some(flags) = flags {
            dict.set("F", flags);
        }
        Object::Dictionary(dict)
    }

    /// `(subtype, flags)` of every annotation on the copy's first page.
    fn flags_of(bytes: &[u8]) -> Vec<(String, i64)> {
        let doc = Document::load_mem(bytes).unwrap();
        let page_id = *doc.get_pages().get(&1).unwrap();
        let page = doc.get_dictionary(page_id).unwrap();
        let annots = match page.get(b"Annots").unwrap() {
            Object::Reference(id) => doc.get_object(*id).unwrap().as_array().unwrap().clone(),
            Object::Array(annots) => annots.clone(),
            other => panic!("unexpected /Annots {other:?}"),
        };
        annots
            .iter()
            .map(|entry| {
                let dict = match entry {
                    Object::Reference(id) => doc.get_dictionary(*id).unwrap(),
                    Object::Dictionary(dict) => dict,
                    other => panic!("unexpected entry {other:?}"),
                };
                let subtype =
                    String::from_utf8(dict.get(b"Subtype").unwrap().as_name().unwrap().to_vec())
                        .unwrap();
                let flags = dict.get(b"F").and_then(Object::as_i64).unwrap_or(0);
                (subtype, flags)
            })
            .collect()
    }

    fn all_kinds() -> Vec<Object> {
        let mut annots: Vec<Object> = COMMENT_SUBTYPES
            .iter()
            .map(|subtype| annot(std::str::from_utf8(subtype).unwrap(), None))
            .collect();
        annots.push(annot("Stamp", None));
        annots.push(annot("Ink", Some(4)));
        annots.push(annot("FileAttachment", None));
        annots.push(annot("Link", None));
        annots.push(annot("Widget", Some(4)));
        annots.push(annot("Square", None));
        annots
    }

    fn expected_flags() -> Vec<(String, i64)> {
        let mut expected: Vec<(String, i64)> = COMMENT_SUBTYPES
            .iter()
            .map(|subtype| (String::from_utf8(subtype.to_vec()).unwrap(), 2))
            .collect();
        expected.extend([
            ("Stamp".to_string(), 0),
            ("Ink".to_string(), 4),
            ("FileAttachment".to_string(), 0),
            ("Link".to_string(), 0),
            ("Widget".to_string(), 4),
            ("Square".to_string(), 0),
        ]);
        expected
    }

    #[test]
    fn hides_exactly_the_comment_subtypes() {
        for xref_stream in [false, true] {
            for direct in [false, true] {
                let original = document(all_kinds(), direct, xref_stream);
                let copy = hide_comment_annotations(&original, None)
                    .unwrap()
                    .bytes
                    .expect("comments to hide");
                assert!(
                    copy.starts_with(&original),
                    "the copy appends to the original"
                );
                assert_eq!(
                    flags_of(&copy),
                    expected_flags(),
                    "direct={direct} xref_stream={xref_stream}"
                );
            }
        }
    }

    #[test]
    fn keeps_existing_flags_beside_hidden() {
        let original = document(vec![annot("FreeText", Some(4 | 32))], false, false);
        let copy = hide_comment_annotations(&original, None)
            .unwrap()
            .bytes
            .unwrap();
        assert_eq!(flags_of(&copy), [("FreeText".to_string(), 4 | 32 | 2)]);
    }

    #[test]
    fn no_copy_without_comment_annotations() {
        let original = document(
            vec![annot("Stamp", None), annot("Link", None)],
            false,
            false,
        );
        assert!(hide_comment_annotations(&original, None)
            .unwrap()
            .bytes
            .is_none());
        let original = document(Vec::new(), true, false);
        assert!(hide_comment_annotations(&original, None)
            .unwrap()
            .bytes
            .is_none());
    }

    #[test]
    fn bytes_before_the_header_are_dropped() {
        let original = document(vec![annot("Text", None)], false, false);
        let mut prefixed = b"--boundary\r\n\r\n".to_vec();
        prefixed.extend_from_slice(&original);
        let copy = hide_comment_annotations(&prefixed, None)
            .unwrap()
            .bytes
            .unwrap();
        assert!(copy.starts_with(&original));
        assert_eq!(flags_of(&copy), [("Text".to_string(), 2)]);
    }

    #[test]
    fn reports_the_pages_lopdf_found() {
        let original = document(vec![annot("Stamp", None)], false, false);
        let hidden = hide_comment_annotations(&original, None).unwrap();
        assert_eq!(hidden.page_count, 1);
    }

    #[test]
    fn too_many_annots_entries_cannot_be_verified() {
        let mut annots = vec![Object::Null; MAX_ANNOTS_EXAMINED];
        annots.push(annot("FreeText", None));
        let original = document(annots, true, false);
        assert!(matches!(
            hide_comment_annotations(&original, None),
            Err(HideError::Unverified(_))
        ));
    }

    /// A one-page file whose `/Annots` refers to objects 4 and 5. Object 4
    /// is listed in the cross-reference table at an offset holding no
    /// object; object 5 is not listed at all.
    fn file_with_unreadable_annotation(list_object_4: bool) -> Vec<u8> {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Annots [4 0 R 5 0 R] >>",
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, body) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", index + 1).as_bytes());
        }
        let garbage = pdf.len();
        pdf.extend_from_slice(b"% not an object\n");
        let xref = pdf.len();
        let count = if list_object_4 { 5 } else { 4 };
        pdf.extend_from_slice(format!("xref\n0 {count}\n0000000000 65535 f \n").as_bytes());
        for offset in &offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        if list_object_4 {
            pdf.extend_from_slice(format!("{garbage:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!("trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF")
                .as_bytes(),
        );
        pdf
    }

    #[test]
    fn an_annotation_lopdf_could_not_load_cannot_be_verified() {
        let listed = file_with_unreadable_annotation(true);
        assert!(matches!(
            hide_comment_annotations(&listed, None),
            Err(HideError::Unverified(_))
        ));
        // References to objects the file never wrote are null to every
        // reader, so they leave nothing to hide.
        let unlisted = file_with_unreadable_annotation(false);
        let hidden = hide_comment_annotations(&unlisted, None).unwrap();
        assert!(hidden.bytes.is_none());
    }

    #[test]
    fn unreadable_input_is_an_error() {
        assert!(matches!(
            hide_comment_annotations(b"not a pdf", None),
            Err(HideError::Load(_))
        ));
    }

    /// Every fixture's comments can be verified (lopdf reads every page and
    /// `/Annots` entry, and finds the pages PDFium does), so none falls back
    /// to rendering without annotations; and every fixture renders the same
    /// through an update that redefines its first page unchanged: the update
    /// itself (its trailer, cross-reference format, `/Prev` and encryption)
    /// leaves the document as it was.
    #[test]
    fn a_no_op_update_renders_every_fixture_unchanged() {
        use crate::vision::{PdfiumRenderer, RenderOptions};

        let Ok(renderer) = PdfiumRenderer::load() else {
            eprintln!("skipping PDFium runtime test because no native library is installed");
            return;
        };
        let options = RenderOptions::new().dpi(36.0).comment_annotations(true);
        let mut paths: Vec<_> = std::fs::read_dir("tests/fixtures")
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "pdf"))
            .collect();
        paths.sort();

        let mut checked = 0;
        let mut skipped = Vec::new();
        let mut page_count_differs = Vec::new();
        for path in paths {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let bytes = std::fs::read(&path).unwrap();
            let header = bytes.windows(5).position(|w| w == b"%PDF-").unwrap_or(0);
            let pdf = &bytes[header..];
            // The one fixture with a user password is named after it.
            let password = name.contains("secret123").then_some("secret123");
            let Ok(original) = renderer.render_pages(&bytes, &[1], password, &options) else {
                skipped.push(format!("{name}: PDFium cannot render it"));
                continue;
            };
            let hidden = hide_comment_annotations(&bytes, password)
                .unwrap_or_else(|error| panic!("{name}: comments not verified: {error}"));
            if hidden.page_count != original_page_count(&renderer, &bytes, password) {
                page_count_differs.push(name.clone());
            }
            let Ok(doc) = load(pdf, password) else {
                skipped.push(format!("{name}: lopdf cannot load it"));
                continue;
            };
            let Some(page) = doc
                .get_pages()
                .get(&1)
                .and_then(|&id| Some((id, doc.get_dictionary(id).ok()?.clone())))
            else {
                skipped.push(format!("{name}: no first page"));
                continue;
            };
            let updated = match append_update(pdf, doc, vec![(page.0, Object::Dictionary(page.1))])
            {
                Ok(updated) => updated,
                Err(error) => {
                    skipped.push(format!("{name}: {error}"));
                    continue;
                }
            };
            let rendered = renderer
                .render_pages(&updated, &[1], password, &options)
                .unwrap_or_else(|error| panic!("{name}: {error}"));
            assert!(
                original[0].pixels() == rendered[0].pixels(),
                "{name} renders differently after a no-op update"
            );
            checked += 1;
        }
        eprintln!("checked {checked} fixtures; skipped {skipped:#?}");
        // PDFium counts a ninth page in this file that lopdf does not find
        // (and PDFium cannot load), so its render leaves out annotations.
        assert_eq!(page_count_differs, ["2013-app2.pdf"]);
        assert!(
            checked >= 40,
            "checked only {checked}; skipped {skipped:#?}"
        );
    }

    fn original_page_count(
        renderer: &crate::vision::PdfiumRenderer,
        bytes: &[u8],
        password: Option<&str>,
    ) -> usize {
        renderer.page_count_for_tests(bytes, password)
    }

    #[test]
    fn update_trailer_keeps_only_document_entries() {
        let original = document(all_kinds(), false, true);
        let copy = hide_comment_annotations(&original, None)
            .unwrap()
            .bytes
            .unwrap();
        let tail = String::from_utf8_lossy(&copy[original.len()..]).into_owned();
        assert!(tail.contains("/Root"), "{tail}");
        assert!(tail.contains("/Prev"), "{tail}");
        assert!(!tail.contains("/DecodeParms"), "{tail}");
        assert!(!tail.contains("/XRefStm"), "{tail}");
    }
}
