//! Files attached to a PDF are listed beside the Markdown and never read
//! into it (lawkitt/mdoc#1005): the `/Names /EmbeddedFiles` tree, a
//! portfolio's files, and `/FileAttachment` annotations, each file once.

use pdf_inspector::{extract_pages_markdown_mem, process_pdf_mem, PdfAttachedFile};

/// A PDF of `objects`, written as objects 1, 2, ... with a cross-reference
/// table; object 1 is the catalog.
fn write_pdf(objects: &[String]) -> Vec<u8> {
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

fn stream(dict: &str, content: &str) -> String {
    format!(
        "<< {dict} /Length {} >>\nstream\n{content}\nendstream",
        content.len()
    )
}

const PAGE_TEXT: &str = "BT /F1 14 Tf 72 720 Td (Visible paragraph on the page.) Tj ET";
const NOTE: &str = "Client notes: Ivan Petrov phoned on Monday.";

/// Objects 2-5: the page tree with one page (3) whose `/Annots` are
/// `annots`, its content and Helvetica.
fn one_page(annots: &str) -> Vec<String> {
    vec![
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
             /Resources << /Font << /F1 5 0 R >> >> /Annots [{annots}] >>"
        ),
        stream("", PAGE_TEXT),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
    ]
}

fn file(page: Option<u32>, name: &str, size: Option<u64>) -> PdfAttachedFile {
    PdfAttachedFile {
        page,
        name: Some(name.into()),
        size,
    }
}

fn assert_not_read(markdown: &str) {
    assert!(markdown.contains("Visible paragraph"), "{markdown}");
    for leaked in ["Ivan Petrov", "client-notes", "witness", "exhibit"] {
        assert!(!markdown.contains(leaked), "{leaked} in {markdown}");
    }
}

/// The synthetic PDF of lawkitt/mdoc#1004, cut to its attachment: the name
/// tree carries `client-notes.txt`. A `/FileAttachment` annotation on the
/// page points at the same file specification, and counts nothing more.
#[test]
fn embedded_files_tree_lists_its_file_once() {
    let mut objects = vec!["<< /Type /Catalog /Pages 2 0 R \
         /Names << /EmbeddedFiles << /Names [(client-notes.txt) 6 0 R] >> >> >>"
        .to_string()];
    objects.extend(one_page("8 0 R"));
    objects.push(
        "<< /Type /Filespec /F (client-notes.txt) /UF (client-notes.txt) /EF << /F 7 0 R >> >>"
            .into(),
    );
    objects.push(stream("/Type /EmbeddedFile", NOTE));
    objects.push(
        "<< /Type /Annot /Subtype /FileAttachment /Rect [72 600 90 618] /FS 6 0 R \
         /Contents (client-notes.txt) >>"
            .into(),
    );
    let pdf = write_pdf(&objects);

    let pages = extract_pages_markdown_mem(&pdf, None).unwrap();
    assert_eq!(
        pages.attached_files,
        vec![file(None, "client-notes.txt", Some(NOTE.len() as u64))]
    );
    assert!(
        pages.annotations.is_empty(),
        "a file attachment is not a comment: {:?}",
        pages.annotations
    );
    assert_not_read(&pages.pages[0].markdown);
    assert_not_read(&process_pdf_mem(&pdf).unwrap().markdown.unwrap_or_default());
}

/// A file carried only by a `/FileAttachment` annotation, the paperclip a
/// reader shows on the page.
#[test]
fn file_attachment_annotation_lists_its_file() {
    let mut objects = vec!["<< /Type /Catalog /Pages 2 0 R >>".to_string()];
    objects.extend(one_page("6 0 R"));
    objects.push(
        "<< /Type /Annot /Subtype /FileAttachment /Rect [72 600 90 618] /Name /PushPin \
         /FS << /Type /Filespec /F (witness-statement.txt) /EF << /F 7 0 R >> >> >>"
            .into(),
    );
    objects.push(stream(
        "/Type /EmbeddedFile /Params << /Size 4096 >> /Filter /FlateDecode",
        "not really deflated",
    ));
    let pdf = write_pdf(&objects);

    let pages = extract_pages_markdown_mem(&pdf, None).unwrap();
    assert_eq!(
        pages.attached_files,
        vec![file(Some(1), "witness-statement.txt", Some(4096))]
    );
    assert_not_read(&pages.pages[0].markdown);

    // Leaving the page out of the request leaves its annotation's file out.
    let none = extract_pages_markdown_mem(&pdf, Some(&[])).unwrap();
    assert!(none.attached_files.is_empty());
}

/// A portfolio (`/Collection`) keeps its files in the name tree, here split
/// across `/Kids`; every one is listed.
#[test]
fn portfolio_lists_every_file() {
    let mut objects = vec!["<< /Type /Catalog /Pages 2 0 R /PageMode /UseAttachments \
         /Collection << /Type /Collection /View /D >> \
         /Names << /EmbeddedFiles 6 0 R >> >>"
        .to_string()];
    objects.extend(one_page(""));
    objects.push("<< /Kids [7 0 R 8 0 R] >>".into());
    objects.push("<< /Limits [(a) (a)] /Names [(a) 9 0 R] >>".into());
    objects.push("<< /Limits [(b) (c)] /Names [(b) 10 0 R (c) 11 0 R] >>".into());
    objects.push("<< /Type /Filespec /UF (exhibit-a.pdf) /EF << /F 12 0 R >> >>".into());
    objects.push("<< /Type /Filespec /UF (exhibit-b.xlsx) /EF << /F 13 0 R >> >>".into());
    // No name of its own: the tree's key names it.
    objects.push("<< /Type /Filespec /EF << /UF 14 0 R >> >>".into());
    objects.push(stream("/Type /EmbeddedFile", "a"));
    objects.push(stream("/Type /EmbeddedFile", "bb"));
    objects.push(stream("/Type /EmbeddedFile", "ccc"));
    let pdf = write_pdf(&objects);

    let pages = extract_pages_markdown_mem(&pdf, None).unwrap();
    assert_eq!(
        pages.attached_files,
        vec![
            file(None, "exhibit-a.pdf", Some(1)),
            file(None, "exhibit-b.xlsx", Some(2)),
            file(None, "c", Some(3)),
        ]
    );
    assert_not_read(&pages.pages[0].markdown);
}

#[test]
fn a_pdf_without_attachments_lists_none() {
    let mut objects = vec!["<< /Type /Catalog /Pages 2 0 R >>".to_string()];
    objects.extend(one_page(""));
    let pdf = write_pdf(&objects);
    assert!(extract_pages_markdown_mem(&pdf, None)
        .unwrap()
        .attached_files
        .is_empty());
}

/// The OCR pipeline reports the native pass's attached files, with OCR off
/// as mdoc's import runs it first.
#[cfg(feature = "ocr")]
#[test]
fn ocr_pipeline_reports_attached_files() {
    use pdf_inspector::vision::{process_pdf_with_ocr_mem, OcrPdfOptions};
    let mut objects = vec!["<< /Type /Catalog /Pages 2 0 R \
         /Names << /EmbeddedFiles << /Names [(client-notes.txt) 6 0 R] >> >> >>"
        .to_string()];
    objects.extend(one_page(""));
    objects.push("<< /Type /Filespec /F (client-notes.txt) /EF << /F 7 0 R >> >>".into());
    objects.push(stream("/Type /EmbeddedFile", NOTE));
    let pdf = write_pdf(&objects);

    let result = process_pdf_with_ocr_mem(&pdf, OcrPdfOptions::new()).unwrap();
    assert_eq!(
        result.attached_files,
        vec![file(None, "client-notes.txt", Some(NOTE.len() as u64))]
    );
    assert_not_read(&result.markdown);
}
