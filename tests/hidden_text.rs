//! Text drawn in the colour of what lies under it, or covered by a later
//! opaque paint, is left out of the Markdown (lawkitt/mdoc#925 M4,
//! firecrawl/pdf-inspector#456). Each test page carries a visible control
//! line, so a test also shows that ordinary text survives next to the case.

use pdf_inspector::{
    extract_pages_markdown_mem, extract_text_with_positions_mem, process_pdf_mem, PageHiddenText,
    HIDDEN_TEXT_COLOUR_TOLERANCE,
};

/// Helvetica under its standard encoding: base-14 widths, so every run has
/// a known advance.
const HELVETICA_FONT: &str = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>";

/// The visible text every test page starts with: a line the tests look
/// for, then a ten-line paragraph, so a page that draws a small image still
/// reads as a text page.
const CONTROL: &str = "BT /F1 12 Tf 0 g 72 760 Td (Control line stays) Tj ET\n\
BT /F1 10 Tf 0 g 72 400 Td 12 TL \
(This paragraph of ordinary visible text keeps every test page a text page.) Tj T* \
(It carries enough characters for the detector to read the page as text) Tj T* \
(even when the page also draws a small picture somewhere above it.) Tj T* \
(None of these lines is ever hidden, covered or drawn in a pale colour.) Tj T* \
(They sit well below the band where each test draws its own case.) Tj T* \
(A page that draws an image needs ten text operators to count as text,) Tj T* \
(so the paragraph runs on for a few more lines than it otherwise would.) Tj T* \
(Each line is a separate show operator in plain black Helvetica.) Tj T* \
(The words themselves carry no meaning beyond filling the page.) Tj T* \
(This is the last line of the control paragraph.) Tj ET\n";

/// A one-page US Letter PDF showing `content` with `/F1` = Helvetica.
/// `resources` is spliced into the page's resource dictionary after the
/// font entry (for `/ExtGState`, `/XObject`, `/Properties`, `/ColorSpace`),
/// and `objects` are written as objects 6, 7, ... so resources can refer
/// to them.
fn make_pdf(content: &str, resources: &str, objects: &[&[u8]]) -> Vec<u8> {
    let content = format!("{CONTROL}{content}");
    let mut bodies: Vec<Vec<u8>> = vec![
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
             /Resources << /Font << /F1 5 0 R >> {resources} >> /Contents 4 0 R >>"
        )
        .into_bytes(),
        format!(
            "<< /Length {} >>\nstream\n{}\nendstream",
            content.len(),
            content
        )
        .into_bytes(),
        HELVETICA_FONT.as_bytes().to_vec(),
    ];
    bodies.extend(objects.iter().map(|o| o.to_vec()));
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in bodies.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let xref = pdf.len();
    pdf.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", bodies.len() + 1).as_bytes(),
    );
    for offset in offsets {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            bodies.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

/// A 2x2 image XObject of one gray `level` (hex byte), with `extra`
/// dictionary entries (a `/SMask`, `/ImageMask`).
fn gray_image(level: &str, extra: &str) -> Vec<u8> {
    let data = format!("{level}{level}{level}{level}>");
    format!(
        "<< /Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceGray \
         /BitsPerComponent 8 /Filter /ASCIIHexDecode {extra} /Length {} >>\nstream\n{data}\nendstream",
        data.len()
    )
    .into_bytes()
}

/// The hidden text `extract_pages_markdown_mem` reports.
fn hidden_text(pdf: &[u8]) -> Vec<PageHiddenText> {
    extract_pages_markdown_mem(pdf, None)
        .expect("the test PDF parses")
        .hidden_text_by_page
}

fn markdown(pdf: &[u8]) -> String {
    process_pdf_mem(pdf)
        .expect("the test PDF parses")
        .markdown
        .unwrap_or_default()
}

/// Asserts the Markdown keeps the control line and `phrase`.
#[track_caller]
fn assert_kept(pdf: &[u8], phrase: &str) {
    let md = markdown(pdf);
    assert!(md.contains("Control line stays"), "control lost:\n{md}");
    assert!(md.contains(phrase), "{phrase:?} was left out:\n{md}");
}

/// Asserts the Markdown keeps the control line and leaves out `phrase`.
#[track_caller]
fn assert_left_out(pdf: &[u8], phrase: &str) {
    let md = markdown(pdf);
    assert!(md.contains("Control line stays"), "control lost:\n{md}");
    assert!(!md.contains(phrase), "{phrase:?} leaked:\n{md}");
}

// ---------------------------------------------------------------------
// Same colour as what is underneath
// ---------------------------------------------------------------------

#[test]
fn white_text_on_the_unpainted_page_is_left_out() {
    let pdf = make_pdf(
        "BT /F1 12 Tf 1 g 72 700 Td (Hidden white words) Tj ET",
        "",
        &[],
    );
    assert_left_out(&pdf, "Hidden white words");
}

#[test]
fn white_text_in_every_device_space_is_left_out() {
    for colour in ["1 g", "1 1 1 rg", "0 0 0 0 k", "/DeviceRGB cs 1 1 1 sc"] {
        let pdf = make_pdf(
            &format!("BT /F1 12 Tf {colour} 72 700 Td (Hidden white words) Tj ET"),
            "",
            &[],
        );
        assert_left_out(&pdf, "Hidden white words");
    }
}

#[test]
fn white_text_on_a_white_fill_is_left_out() {
    let pdf = make_pdf(
        "1 g 60 690 300 30 re f\nBT /F1 12 Tf 1 g 72 700 Td (Hidden white words) Tj ET",
        "",
        &[],
    );
    assert_left_out(&pdf, "Hidden white words");
}

#[test]
fn text_in_the_colour_of_its_fill_is_left_out() {
    let pdf = make_pdf(
        "1 0 0 rg 60 690 300 30 re f\nBT /F1 12 Tf 1 0 0 rg 72 700 Td (Red on red) Tj ET",
        "",
        &[],
    );
    assert_left_out(&pdf, "Red on red");
}

#[test]
fn white_text_on_a_dark_fill_is_kept() {
    for fill in ["0 g", "0.2 0.2 0.5 rg", "0 0 0 1 k", "0.6 g"] {
        let pdf = make_pdf(
            &format!("{fill} 60 690 300 30 re f\nBT /F1 12 Tf 1 g 72 700 Td (White on dark) Tj ET"),
            "",
            &[],
        );
        assert_kept(&pdf, "White on dark");
    }
}

#[test]
fn white_text_on_a_dark_fill_drawn_as_a_path_is_kept() {
    let pdf = make_pdf(
        "0 g 60 690 m 360 690 l 360 720 l 60 720 l h f\n\
         BT /F1 12 Tf 1 g 72 700 Td (White on dark path) Tj ET",
        "",
        &[],
    );
    assert_kept(&pdf, "White on dark path");
}

#[test]
fn white_text_on_a_dark_fill_under_a_transformed_matrix_is_kept() {
    // The fill is placed through `cm` scaling and the text through its own
    // `cm`: both land in the same device box.
    let pdf = make_pdf(
        "q 10 0 0 10 0 0 cm 0 g 6 69 30 3 re f Q\n\
         q 1 0 0 1 72 700 cm BT /F1 12 Tf 1 g 0 0 Td (Scaled dark band) Tj ET Q",
        "",
        &[],
    );
    assert_kept(&pdf, "Scaled dark band");
}

#[test]
fn white_text_on_an_image_is_kept() {
    // An image's colours are not read: what it shows under the text is
    // unknown, so the text stays.
    let pdf = make_pdf(
        "q 300 0 0 30 60 690 cm /Im1 Do Q\nBT /F1 12 Tf 1 g 72 700 Td (White on a photo) Tj ET",
        "/XObject << /Im1 6 0 R >>",
        &[&gray_image("20", "")],
    );
    assert_kept(&pdf, "White on a photo");
}

#[test]
fn white_text_on_a_white_image_is_kept() {
    // Even a white image: its pixels are not read.
    let pdf = make_pdf(
        "q 300 0 0 30 60 690 cm /Im1 Do Q\nBT /F1 12 Tf 1 g 72 700 Td (White on white image) Tj ET",
        "/XObject << /Im1 6 0 R >>",
        &[&gray_image("FF", "")],
    );
    assert_kept(&pdf, "White on white image");
}

#[test]
fn white_text_on_a_fill_that_covers_only_part_of_it_is_kept() {
    // The dark band runs under the first few letters only.
    let pdf = make_pdf(
        "0 g 60 690 40 30 re f\nBT /F1 12 Tf 1 g 72 700 Td (Partly on dark) Tj ET",
        "",
        &[],
    );
    assert_kept(&pdf, "Partly on dark");
}

#[test]
fn white_text_on_a_shading_is_kept() {
    let pdf = make_pdf(
        "q 60 690 300 30 re W n /Sh1 sh Q\nBT /F1 12 Tf 1 g 72 700 Td (White on gradient) Tj ET",
        "/Shading << /Sh1 << /ShadingType 2 /ColorSpace /DeviceGray /Coords [60 0 360 0] \
         /Function << /FunctionType 2 /Domain [0 1] /C0 [0] /C1 [0.5] /N 1 >> >> >>",
        &[],
    );
    assert_kept(&pdf, "White on gradient");
}

#[test]
fn white_text_on_a_dark_form_is_kept() {
    // A form XObject paints the dark band: what forms paint is not read
    // into the text's background, so the text stays.
    let form = b"<< /Type /XObject /Subtype /Form /BBox [0 0 612 792] /Length 22 >>\nstream\n0 g 60 690 300 30 re f\nendstream";
    let pdf = make_pdf(
        "/Fm1 Do\nBT /F1 12 Tf 1 g 72 700 Td (White on a form band) Tj ET",
        "/XObject << /Fm1 6 0 R >>",
        &[form],
    );
    assert_kept(&pdf, "White on a form band");
}

#[test]
fn white_stroked_text_with_a_dark_outline_is_kept() {
    // Render mode 2 fills white and strokes black: the outline shows.
    let pdf = make_pdf(
        "BT /F1 12 Tf 1 g 0 G 2 Tr 72 700 Td (Outlined white) Tj ET",
        "",
        &[],
    );
    assert_kept(&pdf, "Outlined white");
}

#[test]
fn white_text_in_an_unread_colour_space_is_kept() {
    // A Separation colour is not converted: unknown, so kept.
    let pdf = make_pdf(
        "BT /F1 12 Tf /CS1 cs 0 sc 72 700 Td (Spot colour words) Tj ET",
        "/ColorSpace << /CS1 [/Separation /White /DeviceGray << /FunctionType 2 \
         /Domain [0 1] /C0 [1] /C1 [1] /N 1 >>] >>",
        &[],
    );
    assert_kept(&pdf, "Spot colour words");
}

#[test]
fn white_text_inside_optional_content_is_kept() {
    let pdf = make_pdf(
        "/OC /L1 BDC BT /F1 12 Tf 1 g 72 700 Td (Layered white words) Tj ET EMC",
        "/Properties << /L1 << /Type /OCG /Name (Layer) >> >>",
        &[],
    );
    assert_kept(&pdf, "Layered white words");
}

#[test]
fn white_text_on_a_dark_fill_inside_optional_content_is_kept() {
    // The band sits in a layer that may be on or off: the background is
    // unknown.
    let pdf = make_pdf(
        "/OC /L1 BDC 0 g 60 690 300 30 re f EMC\n\
         BT /F1 12 Tf 1 g 72 700 Td (White over a layer) Tj ET",
        "/Properties << /L1 << /Type /OCG /Name (Layer) >> >>",
        &[],
    );
    assert_kept(&pdf, "White over a layer");
}

#[test]
fn a_dark_fill_with_an_unread_blend_is_an_unknown_background() {
    // A difference blend turns the white band dark wherever it lands.
    let pdf = make_pdf(
        "q /GS1 gs 1 g 60 690 300 30 re f Q\nBT /F1 12 Tf 1 g 72 700 Td (White over a blend) Tj ET",
        "/ExtGState << /GS1 << /Type /ExtGState /BM /Difference >> >>",
        &[],
    );
    assert_kept(&pdf, "White over a blend");
}

// ---------------------------------------------------------------------
// Tolerance: near-equal colours
// ---------------------------------------------------------------------

#[test]
fn near_white_text_on_white_is_left_out() {
    // 0.98 gray is 250 of 255: within the tolerance of white.
    let pdf = make_pdf(
        "BT /F1 12 Tf 0.98 g 72 700 Td (Almost white words) Tj ET",
        "",
        &[],
    );
    assert_left_out(&pdf, "Almost white words");
}

#[test]
fn light_gray_text_on_white_is_kept() {
    // 0.9 gray (230 of 255) is pale but readable.
    for colour in ["0.9 g", "0.96 g", "1 1 0.9 rg"] {
        let pdf = make_pdf(
            &format!("BT /F1 12 Tf {colour} 72 700 Td (Pale but readable) Tj ET"),
            "",
            &[],
        );
        assert_kept(&pdf, "Pale but readable");
    }
}

#[test]
fn text_near_the_colour_of_its_fill_is_left_out() {
    let pdf = make_pdf(
        "0.2 0.4 0.6 rg 60 690 300 30 re f\nBT /F1 12 Tf 0.21 0.41 0.61 rg 72 700 Td (Blue on blue) Tj ET",
        "",
        &[],
    );
    assert_left_out(&pdf, "Blue on blue");
}

// ---------------------------------------------------------------------
// Covered by a later paint
// ---------------------------------------------------------------------

#[test]
fn text_under_a_later_opaque_rectangle_is_left_out() {
    let pdf = make_pdf(
        "BT /F1 12 Tf 0 g 72 700 Td (Covered black words) Tj ET\n1 g 60 690 300 30 re f",
        "",
        &[],
    );
    assert_left_out(&pdf, "Covered black words");
}

#[test]
fn text_under_a_later_black_redaction_box_is_left_out() {
    let pdf = make_pdf(
        "BT /F1 12 Tf 0 g 72 700 Td (Redacted name) Tj ET\n0 g 60 690 300 30 re f",
        "",
        &[],
    );
    assert_left_out(&pdf, "Redacted name");
}

#[test]
fn text_under_a_later_opaque_image_is_left_out() {
    let pdf = make_pdf(
        "BT /F1 12 Tf 0 g 72 700 Td (Under a picture) Tj ET\nq 300 0 0 30 60 690 cm /Im1 Do Q",
        "/XObject << /Im1 6 0 R >>",
        &[&gray_image("80", "")],
    );
    assert_left_out(&pdf, "Under a picture");
}

#[test]
fn text_under_a_later_semi_transparent_fill_is_kept() {
    let pdf = make_pdf(
        "BT /F1 12 Tf 0 g 72 700 Td (Under a tint) Tj ET\n\
         q /GS1 gs 1 1 0 rg 60 690 300 30 re f Q",
        "/ExtGState << /GS1 << /Type /ExtGState /ca 0.5 >> >>",
        &[],
    );
    assert_kept(&pdf, "Under a tint");
}

#[test]
fn text_under_a_later_multiply_highlight_is_kept() {
    let pdf = make_pdf(
        "BT /F1 12 Tf 0 g 72 700 Td (Highlighted words) Tj ET\n\
         q /GS1 gs 1 1 0 rg 60 690 300 30 re f Q",
        "/ExtGState << /GS1 << /Type /ExtGState /BM /Multiply >> >>",
        &[],
    );
    assert_kept(&pdf, "Highlighted words");
}

#[test]
fn an_opaque_graphics_state_still_covers() {
    // Word writes `/ca 1 /CA 1 /BM /Normal` states before its fills.
    let pdf = make_pdf(
        "BT /F1 12 Tf 0 g 72 700 Td (Covered under a state) Tj ET\n\
         q /GS1 gs 1 g 60 690 300 30 re f Q",
        "/ExtGState << /GS1 << /Type /ExtGState /ca 1 /CA 1 /BM /Normal /SMask /None >> >>",
        &[],
    );
    assert_left_out(&pdf, "Covered under a state");
}

#[test]
fn text_under_a_later_soft_masked_image_is_kept() {
    let pdf = make_pdf(
        "BT /F1 12 Tf 0 g 72 700 Td (Under a masked picture) Tj ET\nq 300 0 0 30 60 690 cm /Im1 Do Q",
        "/XObject << /Im1 6 0 R >>",
        &[&gray_image("80", "/SMask 7 0 R"), &gray_image("40", "")],
    );
    assert_kept(&pdf, "Under a masked picture");
}

#[test]
fn text_under_a_later_stencil_mask_is_kept() {
    let stencil = b"<< /Type /XObject /Subtype /Image /Width 8 /Height 1 /ImageMask true \
         /BitsPerComponent 1 /Filter /ASCIIHexDecode /Length 3 >>\nstream\n55>\nendstream";
    let pdf = make_pdf(
        "BT /F1 12 Tf 0 g 72 700 Td (Under a stencil) Tj ET\nq 300 0 0 30 60 690 cm /Im1 Do Q",
        "/XObject << /Im1 6 0 R >>",
        &[stencil],
    );
    assert_kept(&pdf, "Under a stencil");
}

#[test]
fn text_partly_under_a_later_rectangle_is_kept() {
    let pdf = make_pdf(
        "BT /F1 12 Tf 0 g 72 700 Td (Half covered words) Tj ET\n1 g 60 690 60 30 re f",
        "",
        &[],
    );
    assert_kept(&pdf, "Half covered words");
}

#[test]
fn text_under_a_rectangle_painted_before_it_is_kept() {
    let pdf = make_pdf(
        "1 1 0 rg 60 690 300 30 re f\nBT /F1 12 Tf 0 g 72 700 Td (Black on yellow) Tj ET",
        "",
        &[],
    );
    assert_kept(&pdf, "Black on yellow");
}

#[test]
fn text_under_a_later_rectangle_clipped_away_from_it_is_kept() {
    // The cover is clipped to a strip beside the text.
    let pdf = make_pdf(
        "BT /F1 12 Tf 0 g 72 700 Td (Beside a clipped box) Tj ET\n\
         q 400 600 100 100 re W n 1 g 60 690 300 30 re f Q",
        "",
        &[],
    );
    assert_kept(&pdf, "Beside a clipped box");
}

#[test]
fn text_under_a_later_rectangle_in_a_curved_clip_is_kept() {
    let pdf = make_pdf(
        "BT /F1 12 Tf 0 g 72 700 Td (Inside a round clip) Tj ET\n\
         q 60 705 m 60 740 360 740 360 705 c h W n 1 g 60 690 300 30 re f Q",
        "",
        &[],
    );
    assert_kept(&pdf, "Inside a round clip");
}

#[test]
fn text_under_a_later_frame_with_a_hole_is_kept() {
    // Even-odd fill of two nested rectangles leaves the text in the hole.
    let pdf = make_pdf(
        "BT /F1 12 Tf 0 g 72 700 Td (Framed words) Tj ET\n\
         1 0 0 rg 40 670 340 70 re 60 690 300 30 re f*",
        "",
        &[],
    );
    assert_kept(&pdf, "Framed words");
}

#[test]
fn text_under_a_later_stroke_is_kept() {
    let pdf = make_pdf(
        "BT /F1 12 Tf 0 g 72 700 Td (Struck through) Tj ET\n0 G 1 w 60 704 m 360 704 l S",
        "",
        &[],
    );
    assert_kept(&pdf, "Struck through");
}

#[test]
fn text_under_a_later_fill_in_optional_content_is_kept() {
    let pdf = make_pdf(
        "BT /F1 12 Tf 0 g 72 700 Td (Under a layer) Tj ET\n/OC /L1 BDC 1 g 60 690 300 30 re f EMC",
        "/Properties << /L1 << /Type /OCG /Name (Layer) >> >>",
        &[],
    );
    assert_kept(&pdf, "Under a layer");
}

// ---------------------------------------------------------------------
// The upstream probe (firecrawl/pdf-inspector#456)
// ---------------------------------------------------------------------

#[test]
fn upstream_probe_cases() {
    // Cases 2-4 are the three this item adds; case 5 (render mode 3) was
    // already left out.
    let pdf = make_pdf(
        "BT /F1 14 Tf 0 g 60 740 Td (CASE 1 visible black text on white) Tj ET\n\
         BT /F1 14 Tf 1 g 60 700 Td (CASE 2 white fill on unpainted page) Tj ET\n\
         1 0 0 rg 50 650 400 26 re f\n\
         BT /F1 14 Tf 1 0 0 rg 60 658 Td (CASE 3 red text on a red rectangle) Tj ET\n\
         BT /F1 14 Tf 0 g 60 610 Td (CASE 4 black text occluded by a later box) Tj ET\n\
         1 g 50 602 400 26 re f\n\
         BT /F1 14 Tf 0 g 3 Tr 60 560 Td (CASE 5 black text at render mode 3) Tj ET\n\
         BT /F1 14 Tf 0 g 0 Tr 60 520 Td (CASE 6 visible black text, second control) Tj ET",
        "",
        &[],
    );
    let texts: Vec<String> = extract_text_with_positions_mem(&pdf)
        .unwrap()
        .into_iter()
        .map(|item| item.text)
        .collect();
    let joined = texts.join("\n");
    for kept in ["CASE 1", "CASE 6"] {
        assert!(joined.contains(kept), "{kept} lost: {texts:?}");
    }
    for hidden in ["CASE 2", "CASE 3", "CASE 4", "CASE 5"] {
        assert!(!joined.contains(hidden), "{hidden} leaked: {texts:?}");
    }
}

// ---------------------------------------------------------------------
// More shapes of the same cases
// ---------------------------------------------------------------------

#[test]
fn white_text_on_a_dark_inline_image_is_kept() {
    let pdf = make_pdf(
        "q 300 0 0 30 60 690 cm BI /W 2 /H 2 /CS /G /BPC 8 /F /AHx ID 20202020> EI Q\n\
         BT /F1 12 Tf 1 g 72 700 Td (White on an inline image) Tj ET",
        "",
        &[],
    );
    assert_kept(&pdf, "White on an inline image");
}

#[test]
fn white_text_on_a_dark_fill_under_a_quarter_turn_is_kept() {
    // The band is drawn through a 90-degree CTM: still a rectangle.
    let pdf = make_pdf(
        "q 0 1 -1 0 360 690 cm 0 g 0 0 30 300 re f Q\n\
         BT /F1 12 Tf 1 g 72 700 Td (White on a turned band) Tj ET",
        "",
        &[],
    );
    assert_kept(&pdf, "White on a turned band");
}

#[test]
fn text_under_a_later_fill_under_a_quarter_turn_is_left_out() {
    let pdf = make_pdf(
        "BT /F1 12 Tf 0 g 72 700 Td (Under a turned box) Tj ET\n\
         q 0 1 -1 0 360 690 cm 1 g 0 0 30 300 re f Q",
        "",
        &[],
    );
    assert_left_out(&pdf, "Under a turned box");
}

#[test]
fn rotated_white_text_is_kept() {
    // Only upright runs are judged.
    let pdf = make_pdf(
        "BT /F1 12 Tf 1 g 0 1 -1 0 300 600 Tm (Turned white words) Tj ET",
        "",
        &[],
    );
    assert_kept(&pdf, "Turned white words");
}

/// A form XObject over the whole page whose content is `content`, with
/// `/F1` among its resources.
fn page_form(content: &str) -> Vec<u8> {
    format!(
        "<< /Type /XObject /Subtype /Form /BBox [0 0 612 792] \
         /Resources << /Font << /F1 5 0 R >> >> /Length {} >>\nstream\n{content}\nendstream",
        content.len()
    )
    .into_bytes()
}

#[test]
fn white_text_inside_a_form_on_the_bare_page_is_left_out() {
    let form = page_form("BT /F1 12 Tf 1 g 72 700 Td (White form words) Tj ET");
    let pdf = make_pdf("/Fm1 Do", "/XObject << /Fm1 6 0 R >>", &[&form]);
    assert_left_out(&pdf, "White form words");
    assert_eq!(hidden_text(&pdf)[0].stretches, 1);
}

#[test]
fn white_text_inside_a_form_on_its_own_dark_band_is_kept() {
    // Forms used to drop every white run, whatever lay under it.
    let form = page_form(
        "0 g 60 690 300 30 re f BT /F1 12 Tf 1 g 72 700 Td (White on the form band) Tj ET",
    );
    let pdf = make_pdf("/Fm1 Do", "/XObject << /Fm1 6 0 R >>", &[&form]);
    assert_kept(&pdf, "White on the form band");
}

#[test]
fn white_text_inside_a_nested_form_on_the_outer_forms_band_is_kept() {
    let inner = page_form("BT /F1 12 Tf 1 g 72 700 Td (White nested words) Tj ET");
    let outer = b"<< /Type /XObject /Subtype /Form /BBox [0 0 612 792] \
         /Resources << /XObject << /Fm2 7 0 R >> >> /Length 30 >>\nstream\n\
         0 g 60 690 300 30 re f /Fm2 Do\nendstream";
    let pdf = make_pdf("/Fm1 Do", "/XObject << /Fm1 6 0 R >>", &[outer, &inner]);
    assert_kept(&pdf, "White nested words");
}

#[test]
fn white_text_inside_a_form_under_the_pages_white_fill_is_left_out() {
    // The form inherits the page's `1 g`.
    let form = page_form("BT /F1 12 Tf 72 700 Td (Inherited white) Tj ET");
    let pdf = make_pdf("1 g /Fm1 Do", "/XObject << /Fm1 6 0 R >>", &[&form]);
    assert_left_out(&pdf, "Inherited white");
}

#[test]
fn page_text_on_a_band_a_form_clipped_away_is_left_out() {
    // The form's box ends below the text: its dark band never reaches it.
    let form = b"<< /Type /XObject /Subtype /Form /BBox [0 0 612 600] /Length 22 >>\nstream\n0 g 60 690 300 30 re f\nendstream";
    let pdf = make_pdf(
        "/Fm1 Do\nBT /F1 12 Tf 1 g 72 700 Td (White past the form box) Tj ET",
        "/XObject << /Fm1 6 0 R >>",
        &[form],
    );
    assert_left_out(&pdf, "White past the form box");
}

#[test]
fn form_text_under_a_later_opaque_rectangle_is_left_out() {
    let form = page_form("BT /F1 12 Tf 0 g 72 700 Td (Covered form words) Tj ET");
    let pdf = make_pdf(
        "/Fm1 Do\n1 g 60 690 300 30 re f",
        "/XObject << /Fm1 6 0 R >>",
        &[&form],
    );
    assert_left_out(&pdf, "Covered form words");
}

#[test]
fn white_text_on_several_separate_white_cells_is_left_out() {
    // Two cell fills in one path, side by side: each fills its own box.
    let pdf = make_pdf(
        "1 g 60 690 150 30 re 210 690 150 30 re f\n\
         BT /F1 12 Tf 1 g 72 700 Td (In the first cell) Tj ET",
        "",
        &[],
    );
    assert_left_out(&pdf, "In the first cell");
}

#[test]
fn white_text_on_white_after_a_dark_fill_elsewhere_is_left_out() {
    // A dark band away from the text does not count under it.
    let pdf = make_pdf(
        "0 g 60 500 300 30 re f\nBT /F1 12 Tf 1 g 72 700 Td (Hidden away words) Tj ET",
        "",
        &[],
    );
    assert_left_out(&pdf, "Hidden away words");
}

#[test]
fn white_text_on_a_white_fill_over_a_dark_band_is_left_out() {
    // The topmost fill under the text is white: the band below is hidden.
    let pdf = make_pdf(
        "0 g 50 680 320 50 re f\n1 g 60 690 300 30 re f\n\
         BT /F1 12 Tf 1 g 72 700 Td (White over white over dark) Tj ET",
        "",
        &[],
    );
    assert_left_out(&pdf, "White over white over dark");
}

#[test]
fn white_text_on_a_white_tint_over_a_dark_band_is_kept() {
    // A translucent white fill over the dark band leaves a gray.
    let pdf = make_pdf(
        "0 g 50 680 320 50 re f\nq /GS1 gs 1 g 60 690 300 30 re f Q\n\
         BT /F1 12 Tf 1 g 72 700 Td (White over a tint) Tj ET",
        "/ExtGState << /GS1 << /Type /ExtGState /ca 0.5 >> >>",
        &[],
    );
    assert_kept(&pdf, "White over a tint");
}

// ---------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------

#[test]
fn the_report_counts_stretches_of_hidden_text_per_page() {
    // Two hidden runs with nothing kept between them are one stretch.
    let pdf = make_pdf(
        "BT /F1 12 Tf 1 g 72 700 Td (Hidden white words) Tj ET\n\
         BT /F1 12 Tf 0 g 72 650 Td (Covered black) Tj ET\n1 g 60 640 300 30 re f",
        "",
        &[],
    );
    assert_eq!(
        hidden_text(&pdf),
        vec![PageHiddenText {
            page: 1,
            stretches: 1,
        }]
    );
}

#[test]
fn visible_text_between_hidden_runs_starts_a_new_stretch() {
    let pdf = make_pdf(
        "BT /F1 12 Tf 1 g 72 700 Td (First hidden) Tj ET\n\
         BT /F1 12 Tf 0 g 72 680 Td (Seen between) Tj ET\n\
         BT /F1 12 Tf 1 g 72 660 Td (Second hidden) Tj ( ) Tj (still hidden) Tj ET",
        "",
        &[],
    );
    assert_kept(&pdf, "Seen between");
    // The blank run between the last two hidden runs splits nothing.
    assert_eq!(hidden_text(&pdf)[0].stretches, 2);
}

#[test]
fn the_report_is_empty_when_nothing_is_hidden() {
    let pdf = make_pdf(
        "0 g 60 690 300 30 re f\nBT /F1 12 Tf 1 g 72 700 Td (White on dark) Tj ET",
        "",
        &[],
    );
    assert!(hidden_text(&pdf).is_empty());
}

#[test]
fn the_report_does_not_count_render_mode_3_text() {
    let pdf = make_pdf(
        "BT /F1 12 Tf 0 g 3 Tr 72 700 Td (Invisible words) Tj ET",
        "",
        &[],
    );
    assert!(hidden_text(&pdf).is_empty());
}

#[test]
fn the_tolerance_is_six_of_255() {
    assert_eq!(HIDDEN_TEXT_COLOUR_TOLERANCE, 6);
}

// ---------------------------------------------------------------------
// Review round 1 (lawkitt/pdf-inspector#8)
// ---------------------------------------------------------------------

#[test]
fn white_text_over_a_thick_dark_stroke_is_kept() {
    let pdf = make_pdf(
        "0 G 30 w 60 705 m 360 705 l S\nBT /F1 12 Tf 1 g 72 700 Td (White on a stroke) Tj ET",
        "",
        &[],
    );
    assert_kept(&pdf, "White on a stroke");
}

#[test]
fn white_text_over_a_dark_fill_and_stroke_outline_is_kept() {
    // `B` fills white and strokes a wide black border through the text.
    let pdf = make_pdf(
        "1 g 0 G 20 w 60 700 300 10 re B\nBT /F1 12 Tf 1 g 72 700 Td (White on a border) Tj ET",
        "",
        &[],
    );
    assert_kept(&pdf, "White on a border");
}

#[test]
fn a_cover_inside_a_translucent_group_is_no_cover() {
    // The group is invoked at half alpha; inside it a state sets full
    // alpha before the white box. The group's alpha still applies: the
    // text shows through gray.
    let form = b"<< /Type /XObject /Subtype /Form /BBox [0 0 612 792] \
         /Group << /S /Transparency >> \
         /Resources << /ExtGState << /Opaque << /ca 1 >> >> >> /Length 33 >>\nstream\n\
         /Opaque gs 1 g 60 690 300 30 re f\nendstream";
    let pdf = make_pdf(
        "BT /F1 12 Tf 0 g 72 700 Td (Under a translucent group) Tj ET\nq /Half gs /Fm1 Do Q",
        "/ExtGState << /Half << /ca 0.5 >> >> /XObject << /Fm1 6 0 R >>",
        &[form],
    );
    assert_kept(&pdf, "Under a translucent group");
}

#[test]
fn an_image_in_optional_content_is_no_cover() {
    let pdf = make_pdf(
        "BT /F1 12 Tf 0 g 72 700 Td (Under a layered picture) Tj ET\nq 300 0 0 30 60 690 cm /Im1 Do Q",
        "/XObject << /Im1 6 0 R >>",
        &[
            &gray_image("80", "/OC 7 0 R"),
            b"<< /Type /OCG /Name (Pictures) >>",
        ],
    );
    assert_kept(&pdf, "Under a layered picture");
}

#[test]
fn a_form_in_optional_content_is_no_cover() {
    let form = b"<< /Type /XObject /Subtype /Form /BBox [0 0 612 792] /OC 7 0 R /Length 22 >>\nstream\n1 g 60 690 300 30 re f\nendstream";
    let pdf = make_pdf(
        "BT /F1 12 Tf 0 g 72 700 Td (Under a layered form) Tj ET\n/Fm1 Do",
        "/XObject << /Fm1 6 0 R >>",
        &[form, b"<< /Type /OCG /Name (Boxes) >>"],
    );
    assert_kept(&pdf, "Under a layered form");
}

#[test]
fn an_actual_text_span_of_white_glyphs_on_the_bare_page_is_left_out() {
    let pdf = make_pdf(
        "/Span << /ActualText (Secret) >> BDC BT /F1 12 Tf 1 g 72 700 Td (S e c r e t) Tj ET EMC",
        "",
        &[],
    );
    assert_left_out(&pdf, "Secret");
    assert_eq!(hidden_text(&pdf)[0].stretches, 1);
}

#[test]
fn an_actual_text_span_of_mixed_glyphs_is_kept() {
    // One glyph run is white, the other black: the span is visible.
    let pdf = make_pdf(
        "/Span << /ActualText (Halfseen) >> BDC BT /F1 12 Tf 1 g 72 700 Td (Half) Tj \
         0 g (seen) Tj ET EMC",
        "",
        &[],
    );
    assert_kept(&pdf, "Halfseen");
}
