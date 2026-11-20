//! Files attached to a PDF: the document's `/Names /EmbeddedFiles` tree
//! (which also holds a portfolio's files) and `/FileAttachment` annotations.
//! None of their content is ever extracted; they are listed so a caller can
//! tell its reader they exist.
//!
//! A file counts once however many places name it: the name tree and an
//! annotation pointing at the same file specification, or at the same
//! embedded stream, are one file. A file specification without an embedded
//! stream only refers to a file outside the PDF and is not listed. Every
//! value is read defensively: nothing here can fail an extraction.

use std::collections::{BTreeMap, HashSet};

use lopdf::{Dictionary, Document, Object, ObjectId};

use super::fonts::resolve_array;
use crate::text_utils::decode_pdf_text_string;
use crate::PdfAttachedFile;

/// Most attached files listed for one extraction; the rest are left out.
pub const MAX_ATTACHED_FILES: usize = 10_000;

/// Longest attached file name kept, in characters.
pub const MAX_ATTACHED_FILE_NAME_CHARS: usize = 512;

/// Most name-tree nodes and annotation entries examined for one extraction,
/// so a tree or a page listing millions of entries costs a bounded walk.
const MAX_ENTRIES_EXAMINED: usize = 100_000;

/// Deepest `/Kids` nesting followed in the name tree.
const MAX_NAME_TREE_DEPTH: usize = 64;

/// The keys of a file specification naming its file and, under `/EF`,
/// holding the embedded stream, in the order a reader prefers them.
const FILE_NAME_KEYS: [&[u8]; 5] = [b"UF", b"F", b"Unix", b"DOS", b"Mac"];

/// The files attached to `doc`: first the name tree's, in its order, then
/// those of the `/FileAttachment` annotations on `pages` (1-indexed page
/// number to page object), in page order and then `/Annots` order.
pub(crate) fn attached_files(
    doc: &Document,
    pages: &BTreeMap<u32, ObjectId>,
) -> Vec<PdfAttachedFile> {
    let mut walk = Walk::default();
    if let Some(tree) = embedded_files_tree(doc) {
        walk.name_tree(doc, tree, 0);
    }
    for (&page, &page_id) in pages {
        walk.page_annotations(doc, page, page_id);
    }
    walk.files
}

#[derive(Default)]
struct Walk {
    files: Vec<PdfAttachedFile>,
    /// File specifications and embedded streams already listed.
    seen: HashSet<ObjectId>,
    /// Name-tree nodes already entered, against reference cycles.
    nodes: HashSet<ObjectId>,
    examined: usize,
}

impl Walk {
    fn exhausted(&self) -> bool {
        self.examined >= MAX_ENTRIES_EXAMINED || self.files.len() >= MAX_ATTACHED_FILES
    }

    /// One name-tree node: its `/Names` pairs, then its `/Kids`.
    fn name_tree(&mut self, doc: &Document, node: &Object, depth: usize) {
        if depth > MAX_NAME_TREE_DEPTH || self.exhausted() {
            return;
        }
        if let Object::Reference(id) = node {
            if !self.nodes.insert(*id) {
                return;
            }
        }
        let Some(dict) = dictionary(doc, node) else {
            return;
        };
        self.examined += 1;
        if let Some(names) = dict.get(b"Names").ok().and_then(|n| resolve_array(doc, n)) {
            for [key, spec] in names.as_chunks::<2>().0 {
                if self.exhausted() {
                    return;
                }
                self.examined += 1;
                let key = match doc.dereference(key) {
                    Ok((_, Object::String(bytes, _))) => Some(decode_pdf_text_string(bytes)),
                    _ => None,
                };
                self.file_spec(doc, spec, None, key);
            }
        }
        if let Some(kids) = dict.get(b"Kids").ok().and_then(|k| resolve_array(doc, k)) {
            for kid in kids {
                self.name_tree(doc, kid, depth + 1);
            }
        }
    }

    /// One page's `/FileAttachment` annotations.
    fn page_annotations(&mut self, doc: &Document, page: u32, page_id: ObjectId) {
        let Some(annots) = doc
            .get_dictionary(page_id)
            .ok()
            .and_then(|dict| dict.get(b"Annots").ok())
            .and_then(|annots| resolve_array(doc, annots))
        else {
            return;
        };
        for entry in annots {
            if self.exhausted() {
                return;
            }
            self.examined += 1;
            let Some(annot) = dictionary(doc, entry) else {
                continue;
            };
            let is_attachment = annot
                .get(b"Subtype")
                .ok()
                .and_then(|subtype| doc.dereference(subtype).ok())
                .and_then(|(_, subtype)| subtype.as_name().ok())
                == Some(b"FileAttachment".as_slice());
            if !is_attachment {
                continue;
            }
            if let Ok(spec) = annot.get(b"FS") {
                self.file_spec(doc, spec, Some(page), None);
            }
        }
    }

    /// A file specification, listed when it embeds a file not listed yet.
    /// `key` is the name tree's name for it, the last resort for its name.
    fn file_spec(&mut self, doc: &Document, spec: &Object, page: Option<u32>, key: Option<String>) {
        let spec_id = match spec {
            Object::Reference(id) => Some(*id),
            _ => None,
        };
        let Some(spec) = dictionary(doc, spec) else {
            return;
        };
        let Some((stream_id, stream)) = embedded_stream(doc, spec) else {
            return;
        };
        let mut new = true;
        for id in [spec_id, stream_id].into_iter().flatten() {
            new &= !self.seen.contains(&id);
        }
        if !new {
            return;
        }
        self.seen.extend([spec_id, stream_id].into_iter().flatten());
        let name = FILE_NAME_KEYS
            .into_iter()
            .find_map(|key| text_value(doc, spec, key))
            .or(key)
            .map(|name| truncate_chars(name.trim(), MAX_ATTACHED_FILE_NAME_CHARS))
            .filter(|name| !name.is_empty());
        self.files.push(PdfAttachedFile {
            page,
            name,
            size: stream_size(doc, stream),
        });
    }
}

/// The catalog's `/Names /EmbeddedFiles` root node.
fn embedded_files_tree(doc: &Document) -> Option<&Object> {
    let catalog = doc.catalog().ok()?;
    let names = dictionary(doc, catalog.get(b"Names").ok()?)?;
    names.get(b"EmbeddedFiles").ok()
}

/// The embedded file stream of a file specification's `/EF`, and its
/// object when it is an indirect one.
fn embedded_stream<'a>(
    doc: &'a Document,
    spec: &'a Dictionary,
) -> Option<(Option<ObjectId>, &'a lopdf::Stream)> {
    let ef = dictionary(doc, spec.get(b"EF").ok()?)?;
    FILE_NAME_KEYS.into_iter().find_map(|key| {
        let object = ef.get(key).ok()?;
        let id = match object {
            Object::Reference(id) => Some(*id),
            _ => None,
        };
        match doc.dereference(object).ok()? {
            (_, Object::Stream(stream)) => Some((id, stream)),
            _ => None,
        }
    })
}

/// The embedded file's size: its `/Params /Size`, else the length of an
/// unfiltered stream's bytes; `None` when neither is known without
/// decoding.
fn stream_size(doc: &Document, stream: &lopdf::Stream) -> Option<u64> {
    let declared = stream
        .dict
        .get(b"Params")
        .ok()
        .and_then(|params| dictionary(doc, params))
        .and_then(|params| params.get(b"Size").ok())
        .and_then(|size| doc.dereference(size).ok())
        .and_then(|(_, size)| match size {
            Object::Integer(size) => u64::try_from(*size).ok(),
            _ => None,
        });
    declared.or_else(|| {
        stream
            .dict
            .get(b"Filter")
            .is_err()
            .then_some(stream.content.len() as u64)
    })
}

fn dictionary<'a>(doc: &'a Document, object: &'a Object) -> Option<&'a Dictionary> {
    match doc.dereference(object).ok()? {
        (_, Object::Dictionary(dict)) => Some(dict),
        _ => None,
    }
}

fn text_value(doc: &Document, dict: &Dictionary, key: &[u8]) -> Option<String> {
    match doc.dereference(dict.get(key).ok()?).ok()? {
        (_, Object::String(bytes, _)) => Some(decode_pdf_text_string(bytes)),
        _ => None,
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => text[..end].to_string(),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Stream, StringFormat};

    fn text(value: &str) -> Object {
        Object::String(value.as_bytes().to_vec(), StringFormat::Literal)
    }

    /// A document with one page; `annots` go on the page, `names` (if any)
    /// become the catalog's `/Names` dictionary.
    struct Builder {
        doc: Document,
        page: ObjectId,
    }

    impl Builder {
        fn new() -> Self {
            let mut doc = Document::with_version("1.7");
            let pages = doc.new_object_id();
            let page = doc.add_object(dictionary! {
                "Type" => "Page",
                "Parent" => pages,
                "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            });
            doc.objects.insert(
                pages,
                Object::Dictionary(dictionary! {
                    "Type" => "Pages",
                    "Kids" => vec![page.into()],
                    "Count" => 1,
                }),
            );
            let catalog = doc.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages});
            doc.trailer.set("Root", catalog);
            Self { doc, page }
        }

        fn embedded(&mut self, content: &[u8], size: Option<i64>) -> ObjectId {
            let mut dict = dictionary! {"Type" => "EmbeddedFile"};
            if let Some(size) = size {
                dict.set("Params", dictionary! {"Size" => size});
            }
            self.doc.add_object(Stream::new(dict, content.to_vec()))
        }

        fn spec(&mut self, name: &str, stream: Option<ObjectId>) -> ObjectId {
            let mut dict = dictionary! {"Type" => "Filespec", "F" => text(name)};
            if let Some(stream) = stream {
                dict.set("EF", dictionary! {"F" => stream});
            }
            self.doc.add_object(dict)
        }

        fn names(&mut self, tree: Object) {
            let catalog = self.doc.catalog_mut().unwrap();
            catalog.set("Names", dictionary! {"EmbeddedFiles" => tree});
        }

        fn annots(&mut self, annots: Vec<Object>) {
            let page = self.doc.get_dictionary_mut(self.page).unwrap();
            page.set("Annots", annots);
        }

        fn files(&self) -> Vec<PdfAttachedFile> {
            attached_files(&self.doc, &self.doc.get_pages())
        }
    }

    fn attachment(spec: impl Into<Object>) -> Object {
        Object::Dictionary(dictionary! {
            "Type" => "Annot",
            "Subtype" => "FileAttachment",
            "Rect" => vec![0.into(), 0.into(), 10.into(), 10.into()],
            "FS" => spec.into(),
        })
    }

    fn file(page: Option<u32>, name: &str, size: Option<u64>) -> PdfAttachedFile {
        PdfAttachedFile {
            page,
            name: Some(name.to_string()),
            size,
        }
    }

    #[test]
    fn lists_the_name_tree_and_annotations_once_each() {
        let mut pdf = Builder::new();
        let notes = pdf.embedded(b"Client notes", None);
        let notes_spec = pdf.spec("client-notes.txt", Some(notes));
        let draft = pdf.embedded(b"x", Some(2048));
        let draft_spec = pdf.spec("draft.docx", Some(draft));
        pdf.names(Object::Dictionary(dictionary! {
            "Names" => vec![text("client-notes.txt"), notes_spec.into()],
        }));
        // An annotation naming the tree's file, one with its own file, and
        // one whose specification shares the tree file's stream.
        let shared_stream = pdf.spec("copy.txt", Some(notes));
        pdf.annots(vec![
            attachment(notes_spec),
            attachment(draft_spec),
            attachment(shared_stream),
        ]);
        assert_eq!(
            pdf.files(),
            vec![
                file(None, "client-notes.txt", Some(12)),
                file(Some(1), "draft.docx", Some(2048)),
            ]
        );
    }

    #[test]
    fn follows_kids_and_survives_cycles() {
        let mut pdf = Builder::new();
        let a = pdf.embedded(b"a", None);
        let a_spec = pdf.spec("a.txt", Some(a));
        let b = pdf.embedded(b"bb", None);
        let b_spec = pdf.spec("b.txt", Some(b));
        let root = pdf.doc.new_object_id();
        let leaf = pdf.doc.add_object(dictionary! {
            "Names" => vec![text("a"), a_spec.into(), text("b"), b_spec.into()],
            // A cycle back to the root is ignored.
            "Kids" => vec![root.into()],
        });
        pdf.doc.objects.insert(
            root,
            Object::Dictionary(dictionary! {"Kids" => vec![leaf.into()]}),
        );
        pdf.names(root.into());
        assert_eq!(
            pdf.files(),
            vec![file(None, "a.txt", Some(1)), file(None, "b.txt", Some(2))]
        );
    }

    #[test]
    fn a_reference_without_an_embedded_file_is_not_listed() {
        let mut pdf = Builder::new();
        let external = pdf.spec("outside.pdf", None);
        pdf.names(Object::Dictionary(dictionary! {
            "Names" => vec![text("outside.pdf"), external.into()],
        }));
        pdf.annots(vec![attachment(external), attachment(text("plain.txt"))]);
        assert!(pdf.files().is_empty());
    }

    #[test]
    fn the_tree_key_names_a_file_without_a_name_of_its_own() {
        let mut pdf = Builder::new();
        let stream = pdf.embedded(b"x", None);
        let spec = pdf
            .doc
            .add_object(dictionary! {"Type" => "Filespec", "EF" => dictionary! {"F" => stream}});
        pdf.names(Object::Dictionary(dictionary! {
            "Names" => vec![text("from-the-key.txt"), spec.into()],
        }));
        assert_eq!(pdf.files(), vec![file(None, "from-the-key.txt", Some(1))]);
    }

    #[test]
    fn a_filtered_stream_without_a_declared_size_has_no_size() {
        let mut pdf = Builder::new();
        let stream = pdf.doc.add_object(Stream::new(
            dictionary! {"Type" => "EmbeddedFile", "Filter" => "FlateDecode"},
            vec![0; 8],
        ));
        let spec = pdf.spec("packed.bin", Some(stream));
        pdf.annots(vec![attachment(spec)]);
        assert_eq!(pdf.files(), vec![file(Some(1), "packed.bin", None)]);
    }
}
