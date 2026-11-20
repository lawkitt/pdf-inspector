//! Content in an optional-content layer that the document's default
//! configuration turns off is left out of the Markdown and counted as
//! hidden text (lawkitt/mdoc#1004): marked content, form XObjects, `/OCMD`
//! membership dictionaries, annotations and form field widgets. The
//! default configuration alone decides; usage (`/AS`) is not read.

use pdf_inspector::types::ItemType;
use pdf_inspector::{
    extract_pages_markdown_mem, extract_text_with_positions_mem, process_pdf_mem, PageHiddenText,
};

/// Object 6 is layer L1, object 7 layer L2, in every test PDF.
const L1: &str = "<< /Type /OCG /Name (Hidden notes) >>";
const L2: &str = "<< /Type /OCG /Name (Visible layer) >>";

/// The default configuration of lawkitt/mdoc#1004: L1 off, L2 on.
const L1_OFF: &str = "/D << /Order [6 0 R 7 0 R] /ON [7 0 R] /OFF [6 0 R] >>";

/// The page resources naming the two layers for marked content.
const LAYER_PROPERTIES: &str = "/Properties << /L1 6 0 R /L2 7 0 R >>";

/// A one-page US Letter PDF: `content` drawn with `/F1` = Helvetica,
/// `resources` spliced into the page's resources, `page` into the page
/// dictionary (`/Annots`) and `catalog` into the catalog (`/AcroForm`).
/// Objects 6 and 7 are L1 and L2, `objects` follow from object 8, and
/// `config` is the `/OCProperties` default configuration entry (`/D ...`),
/// or none at all when empty.
fn make_pdf(
    config: &str,
    catalog: &str,
    page: &str,
    content: &str,
    resources: &str,
    objects: &[&str],
) -> Vec<u8> {
    let oc_properties = if config.is_empty() {
        String::new()
    } else {
        format!("/OCProperties << /OCGs [6 0 R 7 0 R] {config} >>")
    };
    let mut bodies: Vec<String> = vec![
        format!("<< /Type /Catalog /Pages 2 0 R {oc_properties} {catalog} >>"),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
             /Resources << /Font << /F1 5 0 R >> {resources} >> /Contents 4 0 R {page} >>"
        ),
        format!(
            "<< /Length {} >>\nstream\n{}\nendstream",
            content.len(),
            content
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        L1.to_string(),
        L2.to_string(),
    ];
    bodies.extend(objects.iter().map(|o| o.to_string()));
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in bodies.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
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

/// A form XObject drawing `content`, with `extra` dictionary entries.
fn form(content: &str, extra: &str) -> String {
    format!(
        "<< /Type /XObject /Subtype /Form /BBox [0 0 612 792] \
         /Resources << /Font << /F1 5 0 R >> {LAYER_PROPERTIES} >> {extra} /Length {} >>\n\
         stream\n{content}\nendstream",
        content.len()
    )
}

/// A line of text at `y`.
fn line(y: u32, text: &str) -> String {
    format!("BT /F1 14 Tf 72 {y} Td ({text}) Tj ET\n")
}

const VISIBLE: &str = "Visible paragraph on the page.";
const OFF: &str = "Off layer: Ivan Petrov, 12 Baker Street";
const ON: &str = "On layer: second visible line.";

fn markdown(pdf: &[u8]) -> String {
    process_pdf_mem(pdf)
        .expect("the test PDF parses")
        .markdown
        .unwrap_or_default()
}

fn hidden_text(pdf: &[u8]) -> Vec<PageHiddenText> {
    extract_pages_markdown_mem(pdf, None)
        .expect("the test PDF parses")
        .hidden_text_by_page
}

fn hidden_on_page_one(stretches: u32) -> Vec<PageHiddenText> {
    vec![PageHiddenText { page: 1, stretches }]
}

/// Asserts the Markdown keeps every phrase of `kept` and none of `left_out`.
#[track_caller]
fn assert_markdown(pdf: &[u8], kept: &[&str], left_out: &[&str]) {
    let md = markdown(pdf);
    for phrase in kept {
        assert!(md.contains(phrase), "{phrase:?} was left out:\n{md}");
    }
    for phrase in left_out {
        assert!(!md.contains(phrase), "{phrase:?} leaked:\n{md}");
    }
}

/// The page of lawkitt/mdoc#1004: a line outside any layer, one in L1 and
/// one in L2, under the default configuration `config`.
fn three_lines(config: &str) -> Vec<u8> {
    let content = format!(
        "{}/OC /L1 BDC\n{}EMC\n/OC /L2 BDC\n{}EMC\n",
        line(720, VISIBLE),
        line(690, OFF),
        line(660, ON)
    );
    make_pdf(config, "", "", &content, LAYER_PROPERTIES, &[])
}

// ---------------------------------------------------------------------
// The default configuration
// ---------------------------------------------------------------------

#[test]
fn a_line_in_a_layer_listed_off_is_left_out_and_counted() {
    let pdf = three_lines(L1_OFF);
    assert_markdown(&pdf, &[VISIBLE, ON], &[OFF, "Ivan Petrov"]);
    assert_eq!(hidden_text(&pdf), hidden_on_page_one(1));
}

#[test]
fn base_state_off_turns_off_every_layer_not_listed_on() {
    let pdf = three_lines("/D << /BaseState /OFF /ON [7 0 R] >>");
    assert_markdown(&pdf, &[VISIBLE, ON], &[OFF]);
    assert_eq!(hidden_text(&pdf), hidden_on_page_one(1));

    let pdf = three_lines("/D << /BaseState /OFF >>");
    assert_markdown(&pdf, &[VISIBLE], &[OFF, ON]);
    // Two layers, with nothing kept between them: one stretch.
    assert_eq!(hidden_text(&pdf), hidden_on_page_one(1));
}

#[test]
fn layers_the_default_configuration_leaves_on_are_kept() {
    for config in [
        "",
        "/D << >>",
        "/D << /ON [6 0 R 7 0 R] >>",
        "/D << /BaseState /ON /ON [6 0 R] >>",
    ] {
        let pdf = three_lines(config);
        assert_markdown(&pdf, &[VISIBLE, OFF, ON], &[]);
        assert_eq!(hidden_text(&pdf), Vec::new(), "config {config:?}");
    }
}

#[test]
fn an_off_layer_between_kept_lines_counts_each_stretch() {
    let content = format!(
        "/OC /L1 BDC\n{}EMC\n{}/OC /L1 BDC\n{}EMC\n",
        line(720, "First off passage"),
        line(690, VISIBLE),
        line(660, "Second off passage"),
    );
    let pdf = make_pdf(L1_OFF, "", "", &content, LAYER_PROPERTIES, &[]);
    assert_markdown(&pdf, &[VISIBLE], &["off passage"]);
    assert_eq!(hidden_text(&pdf), hidden_on_page_one(2));
}

#[test]
fn marked_content_with_inline_membership_properties_is_read() {
    let content = format!(
        "{}/OC << /Type /OCMD /OCGs [6 0 R] >> BDC\n{}EMC\n",
        line(720, VISIBLE),
        line(690, OFF)
    );
    let pdf = make_pdf(L1_OFF, "", "", &content, "", &[]);
    assert_markdown(&pdf, &[VISIBLE], &[OFF]);
}

#[test]
fn an_actual_text_span_in_an_off_layer_is_left_out() {
    let content = format!(
        "{}/OC /L1 BDC\n/Span << /ActualText (Replacement words) >> BDC\n{}EMC\nEMC\n",
        line(720, VISIBLE),
        line(690, "Painted glyphs")
    );
    let pdf = make_pdf(L1_OFF, "", "", &content, LAYER_PROPERTIES, &[]);
    assert_markdown(&pdf, &[VISIBLE], &["Replacement words", "Painted glyphs"]);
    assert_eq!(hidden_text(&pdf), hidden_on_page_one(1));
}

// ---------------------------------------------------------------------
// Form XObjects
// ---------------------------------------------------------------------

#[test]
fn a_form_carrying_an_off_layer_is_left_out() {
    let content = format!("{}/Fm1 Do\n/Fm2 Do\n", line(720, VISIBLE));
    let resources = format!("{LAYER_PROPERTIES} /XObject << /Fm1 8 0 R /Fm2 9 0 R >>");
    let off_form = form(&line(690, OFF), "/OC 6 0 R");
    let on_form = form(&line(660, ON), "/OC 7 0 R");
    let pdf = make_pdf(L1_OFF, "", "", &content, &resources, &[&off_form, &on_form]);
    assert_markdown(&pdf, &[VISIBLE, ON], &[OFF]);
    assert_eq!(hidden_text(&pdf), hidden_on_page_one(1));
}

#[test]
fn an_off_layer_inside_a_form_is_left_out() {
    let inner = format!("{}/OC /L1 BDC\n{}EMC\n", line(660, ON), line(690, OFF));
    let content = format!("{}/Fm1 Do\n", line(720, VISIBLE));
    let resources = format!("{LAYER_PROPERTIES} /XObject << /Fm1 8 0 R >>");
    let pdf = make_pdf(L1_OFF, "", "", &content, &resources, &[&form(&inner, "")]);
    assert_markdown(&pdf, &[VISIBLE, ON], &[OFF]);
    assert_eq!(hidden_text(&pdf), hidden_on_page_one(1));
}

#[test]
fn a_form_invoked_in_an_off_layer_is_left_out_with_its_nested_forms() {
    let nested = form(&line(690, OFF), "");
    let outer = format!(
        "<< /Type /XObject /Subtype /Form /BBox [0 0 612 792] \
         /Resources << /Font << /F1 5 0 R >> /XObject << /Fm2 9 0 R >> >> /Length 7 >>\n\
         stream\n/Fm2 Do\nendstream"
    );
    let content = format!(
        "{}/OC /L1 BDC /Fm1 Do EMC\n{}",
        line(720, VISIBLE),
        line(660, ON)
    );
    let resources = format!("{LAYER_PROPERTIES} /XObject << /Fm1 8 0 R >>");
    let pdf = make_pdf(L1_OFF, "", "", &content, &resources, &[&outer, &nested]);
    assert_markdown(&pdf, &[VISIBLE, ON], &[OFF]);
    assert_eq!(hidden_text(&pdf), hidden_on_page_one(1));
}

// ---------------------------------------------------------------------
// Membership dictionaries
// ---------------------------------------------------------------------

/// The page of a form carrying the membership dictionary object 8, which
/// draws the off line.
fn form_with_membership(membership: &str) -> Vec<u8> {
    let content = format!("{}/Fm1 Do\n", line(720, VISIBLE));
    let resources = format!("{LAYER_PROPERTIES} /XObject << /Fm1 9 0 R >>");
    let off_form = form(&line(690, OFF), "/OC 8 0 R");
    make_pdf(
        L1_OFF,
        "",
        "",
        &content,
        &resources,
        &[membership, &off_form],
    )
}

#[test]
fn a_membership_dictionary_follows_its_visibility_policy() {
    // L1 is off and L2 on.
    for (policy, off) in [
        ("", false),
        ("/P /AnyOn", false),
        ("/P /AllOn", true),
        ("/P /AnyOff", false),
        ("/P /AllOff", true),
    ] {
        let membership = format!("<< /Type /OCMD /OCGs [6 0 R 7 0 R] {policy} >>");
        let pdf = form_with_membership(&membership);
        let md = markdown(&pdf);
        assert_eq!(!md.contains(OFF), off, "policy {policy:?}:\n{md}");
    }
    let only_off = form_with_membership("<< /Type /OCMD /OCGs 6 0 R >>");
    assert_markdown(&only_off, &[VISIBLE], &[OFF]);
    assert_eq!(hidden_text(&only_off), hidden_on_page_one(1));
}

#[test]
fn a_visibility_expression_decides_over_the_policy() {
    // `/OCGs` alone would leave the form on (L2 is on).
    for (expression, off) in [
        ("[/Not 7 0 R]", true),
        ("[/And 6 0 R 7 0 R]", true),
        ("[/Or 6 0 R 7 0 R]", false),
        ("[/And 7 0 R [/Not 6 0 R]]", false),
    ] {
        let membership = format!("<< /Type /OCMD /OCGs [7 0 R] /VE {expression} >>");
        let pdf = form_with_membership(&membership);
        let md = markdown(&pdf);
        assert_eq!(!md.contains(OFF), off, "expression {expression}:\n{md}");
    }
}

// ---------------------------------------------------------------------
// Annotations and form fields
// ---------------------------------------------------------------------

/// A FreeText annotation saying `text`, with `extra` entries.
fn free_text(text: &str, extra: &str) -> String {
    format!(
        "<< /Type /Annot /Subtype /FreeText /Rect [72 500 300 520] /Contents ({text}) \
         /DA (/Helv 10 Tf 0 g) {extra} >>"
    )
}

#[test]
fn an_annotation_in_an_off_layer_is_left_out_and_counted() {
    let off_note = free_text("Off note: Ivan Petrov", "/OC 6 0 R");
    let on_note = free_text("On note stays", "/OC 7 0 R");
    let pdf = make_pdf(
        L1_OFF,
        "",
        "/Annots [8 0 R 9 0 R]",
        &line(720, VISIBLE),
        "",
        &[&off_note, &on_note],
    );
    let result = extract_pages_markdown_mem(&pdf, None).expect("the test PDF parses");
    let texts: Vec<&str> = result.annotations.iter().map(|a| a.text.as_str()).collect();
    assert_eq!(texts, vec!["On note stays"]);
    assert_eq!(result.hidden_text_by_page, hidden_on_page_one(1));
}

#[test]
fn a_form_field_whose_widget_is_in_an_off_layer_is_left_out_and_counted() {
    let field = |name: &str, value: &str, y: u32, oc: &str| {
        format!(
            "<< /Type /Annot /Subtype /Widget /FT /Tx /T ({name}) /V ({value}) \
             /Rect [72 {y} 300 {}] /P 3 0 R {oc} >>",
            y + 20
        )
    };
    let off_field = field("client", "Ivan Petrov", 600, "/OC 6 0 R");
    let on_field = field("city", "Springfield", 560, "/OC 7 0 R");
    let pdf = make_pdf(
        L1_OFF,
        "/AcroForm << /Fields [8 0 R 9 0 R] >>",
        "/Annots [8 0 R 9 0 R]",
        &line(720, VISIBLE),
        "",
        &[&off_field, &on_field],
    );
    assert_markdown(&pdf, &[VISIBLE, "Springfield"], &["Ivan Petrov"]);
    let result = extract_pages_markdown_mem(&pdf, None).expect("the test PDF parses");
    assert_eq!(result.hidden_text_by_page, hidden_on_page_one(1));
    // Only the kept field's name is counted as left out.
    let names: Vec<u32> = result
        .form_field_names_by_page
        .iter()
        .map(|page| page.names)
        .collect();
    assert_eq!(names, vec![1]);
}

#[test]
fn a_link_in_an_off_layer_is_left_out() {
    let link = |uri: &str, oc: &str| {
        format!(
            "<< /Type /Annot /Subtype /Link /Rect [72 715 300 735] \
             /A << /S /URI /URI ({uri}) >> {oc} >>"
        )
    };
    let off_link = link("https://example.com/off", "/OC 6 0 R");
    let on_link = link("https://example.com/on", "/OC 7 0 R");
    let pdf = make_pdf(
        L1_OFF,
        "",
        "/Annots [8 0 R 9 0 R]",
        &line(720, VISIBLE),
        "",
        &[&off_link, &on_link],
    );
    let links: Vec<String> = extract_text_with_positions_mem(&pdf)
        .expect("the test PDF parses")
        .into_iter()
        .filter_map(|item| match item.item_type {
            ItemType::Link(url) => Some(url),
            _ => None,
        })
        .collect();
    assert_eq!(links, vec!["https://example.com/on".to_string()]);
}
