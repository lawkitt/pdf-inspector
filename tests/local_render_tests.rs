#![cfg(all(feature = "render-pdfium", not(target_arch = "wasm32")))]

use pdf_inspector::vision::{PdfiumRenderer, RenderError, RenderOptions, RenderPixelFormat};

fn load_renderer() -> Option<PdfiumRenderer> {
    match PdfiumRenderer::load() {
        Ok(renderer) => Some(renderer),
        Err(RenderError::PdfiumLoad { .. }) => {
            eprintln!("skipping PDFium runtime test because no native library is installed");
            None
        }
        Err(error) => panic!("failed to load PDFium: {error}"),
    }
}

#[test]
fn renders_owned_rgb_page_and_round_trips_coordinates() {
    let Some(renderer) = load_renderer() else {
        return;
    };
    let bytes = std::fs::read("tests/fixtures/thermo-freon12.pdf").unwrap();
    let pages = renderer
        .render_pages(
            &bytes,
            &[1],
            None,
            &RenderOptions::new().dpi(150.0).form_fields(false),
        )
        .unwrap();

    assert_eq!(pages.len(), 1);
    let page = &pages[0];
    assert_eq!(page.page(), 1);
    assert_eq!(page.format(), RenderPixelFormat::Rgb8);
    assert_eq!(page.stride(), page.width() as usize * 3);
    assert_eq!(page.pixels().len(), page.stride() * page.height() as usize);
    assert!((page.width() as f32 - page.page_width()).abs() > 1.0);

    let pdf_rect = page.pixel_rect_to_pdf_rect(10.0, 10.0, 20.0, 12.0);
    let pixel_rect = page.pdf_rect_to_pixel(&pdf_rect);
    assert!((pixel_rect.0 - 10.0).abs() < 0.01);
    assert!((pixel_rect.1 - 10.0).abs() < 0.01);
    assert!((pixel_rect.2 - 20.0).abs() < 0.01);
    assert!((pixel_rect.3 - 12.0).abs() < 0.01);
}

/// A page drawn through a Form XObject with the given `/BBox`, whose
/// content is a 24 pt line at the top of the page.
fn page_drawn_through_form(form_bbox: &str) -> Vec<u8> {
    let page_content = "q Q q 0 0 612 792 re W n /Fm1 Do Q";
    let form_content = "BT /F1 24 Tf 72 700 Td (Drawn through the form) Tj ET";
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
         /Resources << /Font << /F1 5 0 R >> /XObject << /Fm1 6 0 R >> >> /Contents 4 0 R >>"
            .to_string(),
        format!(
            "<< /Length {} >>\nstream\n{page_content}\nendstream",
            page_content.len()
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        format!(
            "<< /Type /XObject /Subtype /Form /BBox [{form_bbox}] \
             /Resources << /Font << /F1 5 0 R >> >> /Length {} >>\nstream\n{form_content}\nendstream",
            form_content.len()
        ),
    ];
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (index, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", index + 1).as_bytes());
    }
    let xref_start = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in &offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_start}\n%%EOF",
            objects.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

/// Dark pixels in the rendered first page, and the rows they span.
fn rendered_ink(renderer: &PdfiumRenderer, bytes: &[u8]) -> (usize, Option<(u32, u32)>) {
    let pages = renderer
        .render_pages(
            bytes,
            &[1],
            None,
            &RenderOptions::new()
                .dpi(72.0)
                .pixel_format(RenderPixelFormat::Rgb8),
        )
        .unwrap();
    let page = &pages[0];
    let pixels = page.pixels();
    let mut dark = 0usize;
    let mut rows: Option<(u32, u32)> = None;
    for y in 0..page.height() {
        for x in 0..page.width() {
            let i = y as usize * page.stride() + x as usize * 3;
            if pixels[i] < 128 && pixels[i + 1] < 128 && pixels[i + 2] < 128 {
                dark += 1;
                rows = Some(match rows {
                    Some((top, bottom)) => (top.min(y), bottom.max(y)),
                    None => (y, y),
                });
            }
        }
    }
    (dark, rows)
}

/// A Form XObject whose `/BBox` has no area clips its content to nothing
/// when rendered as written; the document repaired by
/// `widen_degenerate_form_bboxes_mem` renders the form's text where it is
/// painted, and a form with a real box keeps that box.
#[test]
fn zero_area_form_bbox_renders_after_the_repair() {
    let Some(renderer) = load_renderer() else {
        return;
    };
    let original = page_drawn_through_form("0 0 0 0");
    let (dark, _) = rendered_ink(&renderer, &original);
    assert_eq!(dark, 0, "the zero-area box clips the form as written");

    let repaired = pdf_inspector::widen_degenerate_form_bboxes_mem(&original)
        .unwrap()
        .expect("the zero-area box is repaired");
    let (dark, rows) = rendered_ink(&renderer, &repaired);
    assert!(
        dark > 100,
        "the repaired form paints its text: {dark} dark pixels"
    );
    // 24 pt text on the 700 pt baseline of a 792 pt page, at 72 dpi.
    let (top, bottom) = rows.unwrap();
    assert!(
        (70..=95).contains(&top) && (85..=100).contains(&bottom),
        "rows {top}..{bottom}"
    );

    let proper = page_drawn_through_form("0 690 612 792");
    assert!(pdf_inspector::widen_degenerate_form_bboxes_mem(&proper)
        .unwrap()
        .is_none());
    let (dark, _) = rendered_ink(&renderer, &proper);
    assert!(dark > 100, "a form with a real box renders as written");
}

/// A form box written as ±(DBL_MAX / 2) in full — 308-digit numerals — is
/// saturated to a box that clips nothing, and the repaired bytes render
/// the form's text where it stands.
#[test]
fn overlong_form_bbox_numerals_render_after_the_repair() {
    let Some(renderer) = load_renderer() else {
        return;
    };
    let digits = format!("8988465674311578{}", "0".repeat(292));
    let original = page_drawn_through_form(&format!("-{digits} -{digits} {digits} {digits}"));
    let repaired = pdf_inspector::widen_degenerate_form_bboxes_mem(&original)
        .unwrap()
        .expect("the numerals are saturated");
    let (dark, rows) = rendered_ink(&renderer, &repaired);
    assert!(
        dark > 100,
        "the repaired form paints its text: {dark} dark pixels"
    );
    let (top, bottom) = rows.unwrap();
    assert!(
        (70..=95).contains(&top) && (85..=100).contains(&bottom),
        "rows {top}..{bottom}"
    );
}

#[test]
fn rejects_zero_and_out_of_range_page_numbers() {
    let Some(renderer) = load_renderer() else {
        return;
    };
    let bytes = std::fs::read("tests/fixtures/thermo-freon12.pdf").unwrap();

    assert!(matches!(
        renderer.render_pages(&bytes, &[0], None, &RenderOptions::new()),
        Err(RenderError::InvalidPageNumber)
    ));

    assert!(matches!(
        renderer.render_pages(&bytes, &[u32::MAX], None, &RenderOptions::new()),
        Err(RenderError::PageOutOfBounds { .. })
    ));
}

/// Regions of `tests/fixtures/scanned_comments.pdf` (see
/// `scripts/make_ocr_comment_fixture.py`), as PDF `(x0, y0, x1, y1)`.
mod scanned_comments {
    pub const FREE_TEXT: (f64, f64, f64, f64) = (72.0, 520.0, 372.0, 556.0);
    pub const NOTE: (f64, f64, f64, f64) = (392.0, 470.0, 592.0, 500.0);
    pub const HIGHLIGHT: (f64, f64, f64, f64) = (72.0, 680.0, 276.0, 698.0);
    pub const STAMP: (f64, f64, f64, f64) = (72.0, 280.0, 292.0, 340.0);
    pub const BODY: (f64, f64, f64, f64) = (60.0, 620.0, 560.0, 710.0);
}

/// Pixels inside the PDF-space `region` of `page` that satisfy `test`.
fn count_in(
    page: &pdf_inspector::vision::RenderedPage,
    region: (f64, f64, f64, f64),
    test: impl Fn(u8, u8, u8) -> bool,
) -> usize {
    let transform = page.transform();
    let (ax, ay) = transform.page_to_pixel(region.0, region.1);
    let (bx, by) = transform.page_to_pixel(region.2, region.3);
    let clamp_x = |v: f64| (v.max(0.0) as u32).min(page.width());
    let clamp_y = |v: f64| (v.max(0.0) as u32).min(page.height());
    let (x0, x1) = (clamp_x(ax.min(bx)), clamp_x(ax.max(bx)));
    let (y0, y1) = (clamp_y(ay.min(by)), clamp_y(ay.max(by)));
    let pixels = page.pixels();
    let mut count = 0;
    for y in y0..y1 {
        for x in x0..x1 {
            let i = y as usize * page.stride() + x as usize * 3;
            if test(pixels[i], pixels[i + 1], pixels[i + 2]) {
                count += 1;
            }
        }
    }
    count
}

fn dark(r: u8, g: u8, b: u8) -> bool {
    r < 128 && g < 128 && b < 128
}

fn yellow(r: u8, g: u8, b: u8) -> bool {
    r > 200 && g > 200 && b < 100
}

fn red(r: u8, g: u8, b: u8) -> bool {
    r > 150 && g < 80 && b < 80
}

fn render_first_page(
    renderer: &PdfiumRenderer,
    bytes: &[u8],
    password: Option<&str>,
    options: &RenderOptions,
) -> pdf_inspector::vision::RenderedPage {
    renderer
        .render_pages(bytes, &[1], password, &options.clone().dpi(72.0))
        .unwrap()
        .remove(0)
}

/// By default the render leaves out notes, FreeText and text markup while it
/// keeps the stamp and the page itself; `comment_annotations(true)` draws
/// them, and `annotations(false)` still leaves out every annotation.
#[test]
fn comment_annotations_are_left_out_of_the_render_by_default() {
    use scanned_comments::*;
    let Some(renderer) = load_renderer() else {
        return;
    };
    let bytes = std::fs::read("tests/fixtures/scanned_comments.pdf").unwrap();

    let hidden = render_first_page(&renderer, &bytes, None, &RenderOptions::new());
    assert_eq!(count_in(&hidden, FREE_TEXT, dark), 0, "FreeText drawn");
    assert_eq!(count_in(&hidden, NOTE, dark), 0, "note drawn");
    assert_eq!(count_in(&hidden, HIGHLIGHT, yellow), 0, "highlight drawn");
    assert!(count_in(&hidden, STAMP, red) > 500, "stamp missing");
    assert!(count_in(&hidden, BODY, dark) > 1000, "page image missing");

    let drawn = render_first_page(
        &renderer,
        &bytes,
        None,
        &RenderOptions::new().comment_annotations(true),
    );
    assert!(count_in(&drawn, FREE_TEXT, dark) > 200, "FreeText missing");
    assert!(count_in(&drawn, NOTE, dark) > 200, "note missing");
    assert!(
        count_in(&drawn, HIGHLIGHT, yellow) > 500,
        "highlight missing"
    );
    assert!(count_in(&drawn, STAMP, red) > 500, "stamp missing");

    let none = render_first_page(
        &renderer,
        &bytes,
        None,
        &RenderOptions::new()
            .annotations(false)
            .comment_annotations(true),
    );
    assert_eq!(count_in(&none, FREE_TEXT, dark), 0, "FreeText drawn");
    assert_eq!(count_in(&none, STAMP, red), 0, "stamp drawn");
    assert!(count_in(&none, BODY, dark) > 1000, "page image missing");
}

/// The fixture rewritten by lopdf: with object streams and a
/// cross-reference stream, and encrypted with RC4 (owner password only),
/// AES-128 (a user password) and AES-256. Comments stay hidden and the page
/// and stamp still render, so the hidden copy's update decrypts and chains
/// to the original in each.
#[test]
fn comment_annotations_are_left_out_of_rewritten_and_encrypted_files() {
    use lopdf::encryption::crypt_filters::{Aes128CryptFilter, Aes256CryptFilter, CryptFilter};
    use lopdf::{Document, EncryptionState, EncryptionVersion, Permissions, SaveOptions};
    use scanned_comments::*;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    let Some(renderer) = load_renderer() else {
        return;
    };
    let original = std::fs::read("tests/fixtures/scanned_comments.pdf").unwrap();
    let fixture = || Document::load_mem(&original).unwrap();
    let save = |mut doc: Document, options: SaveOptions| {
        let mut bytes = Vec::new();
        doc.save_with_options(&mut bytes, options).unwrap();
        bytes
    };
    let encrypted = |version: fn(&Document) -> EncryptionState| {
        let mut doc = fixture();
        // Encryption keys derive from the file identifier.
        let id = lopdf::Object::String(
            b"scanned-comments".to_vec(),
            lopdf::StringFormat::Hexadecimal,
        );
        doc.trailer.set("ID", vec![id.clone(), id]);
        let state = version(&doc);
        doc.encrypt(&state).unwrap();
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    };
    let streams = SaveOptions::builder()
        .use_object_streams(true)
        .use_xref_streams(true)
        .build();

    let variants: Vec<(&str, Vec<u8>, Option<&str>)> = vec![
        ("object streams", save(fixture(), streams), None),
        (
            "RC4, owner password only",
            encrypted(|doc| {
                EncryptionState::try_from(EncryptionVersion::V2 {
                    document: doc,
                    owner_password: "owner",
                    user_password: "",
                    key_length: 128,
                    permissions: Permissions::all(),
                })
                .unwrap()
            }),
            None,
        ),
        (
            "AES-128, user password",
            encrypted(|doc| {
                let filter: Arc<dyn CryptFilter> = Arc::new(Aes128CryptFilter);
                EncryptionState::try_from(EncryptionVersion::V4 {
                    document: doc,
                    encrypt_metadata: true,
                    crypt_filters: BTreeMap::from([(b"StdCF".to_vec(), filter)]),
                    stream_filter: b"StdCF".to_vec(),
                    string_filter: b"StdCF".to_vec(),
                    owner_password: "owner",
                    user_password: "reader",
                    permissions: Permissions::all(),
                })
                .unwrap()
            }),
            Some("reader"),
        ),
        (
            "AES-256, owner password only",
            encrypted(|_| {
                let filter: Arc<dyn CryptFilter> = Arc::new(Aes256CryptFilter);
                EncryptionState::try_from(EncryptionVersion::V5 {
                    encrypt_metadata: true,
                    crypt_filters: BTreeMap::from([(b"StdCF".to_vec(), filter)]),
                    file_encryption_key: &[7; 32],
                    stream_filter: b"StdCF".to_vec(),
                    string_filter: b"StdCF".to_vec(),
                    owner_password: "owner",
                    user_password: "",
                    permissions: Permissions::all(),
                })
                .unwrap()
            }),
            None,
        ),
    ];

    for (name, bytes, password) in variants {
        let hidden = render_first_page(&renderer, &bytes, password, &RenderOptions::new());
        assert_eq!(
            count_in(&hidden, FREE_TEXT, dark),
            0,
            "{name}: FreeText drawn"
        );
        assert_eq!(count_in(&hidden, NOTE, dark), 0, "{name}: note drawn");
        assert_eq!(
            count_in(&hidden, HIGHLIGHT, yellow),
            0,
            "{name}: highlight drawn"
        );
        assert!(count_in(&hidden, STAMP, red) > 500, "{name}: stamp missing");
        assert!(
            count_in(&hidden, BODY, dark) > 1000,
            "{name}: page image missing"
        );

        let drawn = render_first_page(
            &renderer,
            &bytes,
            password,
            &RenderOptions::new().comment_annotations(true),
        );
        assert!(
            count_in(&drawn, FREE_TEXT, dark) > 200,
            "{name}: FreeText missing"
        );
    }
}
