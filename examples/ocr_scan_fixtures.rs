//! Generates the image-only OCR fixtures under `tests/fixtures/`.
//!
//! - `ocr_rotated_90.pdf`, `ocr_rotated_180.pdf`, `ocr_rotated_270.pdf`:
//!   `ocr_qualification_english.pdf` turned 90°, 180° and 270° clockwise, as
//!   a page fed into a scanner sideways or upside down comes out. The source
//!   is mdoc's synthetic English OCR qualification page (generated for that
//!   project and dedicated to the public domain under CC0-1.0); its JPEG is
//!   copied byte for byte and only the placement matrix and the page size
//!   change, so the page carries no `/Rotate`.
//! - `ocr_clean_scan_paragraphs.pdf`: a clean scan of a page with a title,
//!   headings, paragraphs and a numbered list. The page is set in Helvetica
//!   (`ocr_clean_scan_paragraphs_native.pdf`, the same page as text),
//!   rendered by PDFium at 150 dpi in grey and stored as the page's only
//!   content, a Flate-compressed image. The text was written for these tests.
//!
//! Regenerate with
//! `PDFIUM_LIB_PATH=/path/to/pdfium cargo run --features ocr --example ocr_scan_fixtures`.
//! The output is deterministic for a given PDFium build (no timestamps, no
//! document ID); the committed files were rendered by PDFium native-v7988.

#[cfg(all(feature = "render-pdfium", not(target_arch = "wasm32")))]
fn main() {
    use std::path::Path;

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let source = lopdf::Document::load(root.join("ocr_qualification_english.pdf"))
        .expect("qualification page loads");
    let page_id = *source.get_pages().get(&1).expect("one page");
    let page = source.get_dictionary(page_id).expect("page dictionary");
    let (_, image) = page
        .get(b"Resources")
        .and_then(|resources| source.dereference(resources))
        .and_then(|(_, resources)| resources.as_dict())
        .and_then(|resources| resources.get(b"XObject"))
        .and_then(|xobjects| source.dereference(xobjects))
        .and_then(|(_, xobjects)| xobjects.as_dict())
        .and_then(|xobjects| xobjects.get(b"image"))
        .and_then(|image| source.dereference(image))
        .expect("qualification page image");
    let image = image.as_stream().expect("image stream").clone();
    let (width, height) = (595.44_f32, 842.04_f32);
    for degrees in [90u32, 180, 270] {
        // Placement of the image's unit square for the page turned
        // `degrees` clockwise.
        let (page_width, page_height, matrix) = match degrees {
            90 => (height, width, [0.0, -width, height, 0.0, 0.0, width]),
            180 => (width, height, [-width, 0.0, 0.0, -height, width, height]),
            _ => (height, width, [0.0, width, -height, 0.0, height, 0.0]),
        };
        let content = format!(
            "q {} cm /image Do Q\n",
            matrix.map(|value| format!("{value:.2}")).join(" ")
        );
        write_image_page(
            &root.join(format!("ocr_rotated_{degrees}.pdf")),
            image.clone(),
            page_width,
            page_height,
            &content,
            &format!("Synthetic OCR qualification page turned {degrees} degrees clockwise"),
        );
    }

    write_clean_scan(&root.join("ocr_clean_scan_paragraphs.pdf"));
}

/// The clean scan's lines: font size, left edge and text, top to bottom.
/// The vertical step between lines is the leading of the line's own size;
/// an empty line is a paragraph gap of [`CLEAN_SCAN_PARAGRAPH_GAP`].
#[cfg(all(feature = "render-pdfium", not(target_arch = "wasm32")))]
const CLEAN_SCAN_LINES: &[(f32, f32, &str)] = &[
    (20.0, 72.0, "Service Agreement Summary"),
    (0.0, 0.0, ""),
    (
        11.0,
        72.0,
        "This summary describes the main terms of the agreement between the two",
    ),
    (
        11.0,
        72.0,
        "parties. It is a synthetic page written for testing, and it holds no real",
    ),
    (
        11.0,
        72.0,
        "names, amounts or dates. Each section below covers one part of the",
    ),
    (11.0, 72.0, "agreement in a few plain sentences."),
    (0.0, 0.0, ""),
    (16.0, 72.0, "Payment terms"),
    (0.0, 0.0, ""),
    (
        11.0,
        72.0,
        "The customer pays each invoice within thirty days of receiving it. Late",
    ),
    (
        11.0,
        72.0,
        "payments carry interest at the rate the agreement states, counted from",
    ),
    (
        11.0,
        72.0,
        "the day after the payment was due until the day it arrives.",
    ),
    (0.0, 0.0, ""),
    (
        11.0,
        72.0,
        "1. Invoices are sent on the first working day of each month.",
    ),
    (
        11.0,
        72.0,
        "2. Payments are made by bank transfer to the account on the invoice.",
    ),
    (
        11.0,
        72.0,
        "3. A disputed amount is paid once the dispute is settled.",
    ),
    (0.0, 0.0, ""),
    (16.0, 72.0, "Ending the agreement"),
    (0.0, 0.0, ""),
    (
        11.0,
        72.0,
        "Either party may end the agreement with sixty days of written notice.",
    ),
    (
        11.0,
        72.0,
        "Obligations that arose before the agreement ended remain in force, and",
    ),
    (
        11.0,
        72.0,
        "each party returns the documents it holds that belong to the other.",
    ),
    (0.0, 0.0, ""),
    (
        11.0,
        72.0,
        "Notices are sent to the addresses the parties gave when they signed. A",
    ),
    (
        11.0,
        72.0,
        "party that moves tells the other party its new address in writing within",
    ),
    (11.0, 72.0, "ten days of the move."),
];

/// Extra space between paragraphs, in points: about one line of the body.
#[cfg(all(feature = "render-pdfium", not(target_arch = "wasm32")))]
const CLEAN_SCAN_PARAGRAPH_GAP: f32 = 11.0;

#[cfg(all(feature = "render-pdfium", not(target_arch = "wasm32")))]
fn write_clean_scan(path: &std::path::Path) {
    use lopdf::{dictionary, Document, Object, Stream};
    use pdf_inspector::vision::{PdfiumRenderer, RenderOptions, RenderPixelFormat};

    let (page_width, page_height) = (612.0_f32, 792.0_f32);
    let mut content = String::from("BT\n");
    let mut baseline = page_height - 90.0;
    for &(size, left, text) in CLEAN_SCAN_LINES {
        if text.is_empty() {
            baseline -= CLEAN_SCAN_PARAGRAPH_GAP;
            continue;
        }
        let escaped = text
            .replace('\\', "\\\\")
            .replace('(', "\\(")
            .replace(')', "\\)");
        content.push_str(&format!(
            "/F1 {size} Tf 1 0 0 1 {left} {baseline:.2} Tm ({escaped}) Tj\n"
        ));
        baseline -= size * 1.4;
    }
    content.push_str("ET\n");

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let contents = doc.add_object(Stream::new(dictionary! {}, content.into_bytes()));
    let page = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => Object::Reference(pages_id),
        "MediaBox" => vec![0.into(), 0.into(), page_width.into(), page_height.into()],
        "Resources" => dictionary! { "Font" => dictionary! { "F1" => Object::Reference(font) } },
        "Contents" => Object::Reference(contents),
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page)],
            "Count" => 1,
        }),
    );
    let catalog = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    doc.trailer.set("Root", Object::Reference(catalog));
    let mut native = Vec::new();
    doc.save_to(&mut native).expect("text page serializes");
    let native_path = path.with_file_name("ocr_clean_scan_paragraphs_native.pdf");
    std::fs::write(&native_path, &native)
        .unwrap_or_else(|e| panic!("writing {}: {e}", native_path.display()));
    println!("{}", native_path.display());

    let renderer = PdfiumRenderer::load().expect("PDFium loads (set PDFIUM_LIB_PATH)");
    let rendered = renderer
        .render_pages(
            &native,
            &[1],
            None,
            &RenderOptions::new()
                .dpi(150.0)
                .pixel_format(RenderPixelFormat::Gray8),
        )
        .expect("text page renders")
        .remove(0);
    let (width, height) = (rendered.width(), rendered.height());
    let mut pixels = Vec::with_capacity(width as usize * height as usize);
    for row in rendered.pixels().chunks(rendered.stride()) {
        pixels.extend_from_slice(&row[..width as usize]);
    }
    let mut image = Stream::new(
        dictionary! {
            "Type" => "XObject",
            "Subtype" => "Image",
            "Width" => i64::from(width),
            "Height" => i64::from(height),
            "ColorSpace" => "DeviceGray",
            "BitsPerComponent" => 8,
        },
        pixels,
    );
    image.compress().expect("image compresses");
    write_image_page(
        path,
        image,
        page_width,
        page_height,
        &format!("q {page_width:.2} 0 0 {page_height:.2} 0 0 cm /image Do Q\n"),
        "Synthetic clean scan: title, headings, paragraphs and a numbered list",
    );
}

/// Writes a one-page PDF whose only content is `image`, placed by `content`.
#[cfg(all(feature = "render-pdfium", not(target_arch = "wasm32")))]
fn write_image_page(
    path: &std::path::Path,
    image: lopdf::Stream,
    page_width: f32,
    page_height: f32,
    content: &str,
    title: &str,
) {
    use lopdf::{dictionary, Document, Object, Stream, StringFormat};

    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let image = doc.add_object(image);
    let contents = doc.add_object(Stream::new(dictionary! {}, content.as_bytes().to_vec()));
    let page = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => Object::Reference(pages_id),
        "MediaBox" => vec![0.into(), 0.into(), page_width.into(), page_height.into()],
        "Resources" => dictionary! { "XObject" => dictionary! { "image" => Object::Reference(image) } },
        "Contents" => Object::Reference(contents),
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page)],
            "Count" => 1,
        }),
    );
    let catalog = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    let info = doc.add_object(dictionary! {
        "Title" => Object::String(title.as_bytes().to_vec(), StringFormat::Literal),
        "Producer" => Object::String(
            b"pdf-inspector examples/ocr_scan_fixtures.rs".to_vec(),
            StringFormat::Literal,
        ),
    });
    doc.trailer.set("Root", Object::Reference(catalog));
    doc.trailer.set("Info", Object::Reference(info));
    doc.save(path)
        .unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
    println!("{}", path.display());
}

#[cfg(not(all(feature = "render-pdfium", not(target_arch = "wasm32"))))]
fn main() {
    eprintln!("ocr_scan_fixtures requires the native `render-pdfium` feature");
    std::process::exit(1);
}
