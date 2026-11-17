//! Table regressions from upstream issues (lawkitt/mdoc#925, W5), on
//! handmade PDFs. Each test reads the page the way mdoc does: per-page
//! Markdown, tables never merged across pages.

use pdf_inspector::extract_pages_markdown_mem;

/// A text run at (`x`, `y`) in `/F1` (Helvetica) or `/F2` (Helvetica-Bold).
fn text(font: &str, size: f32, x: f32, y: f32, s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('(', "\\(")
        .replace(')', "\\)");
    format!("BT /{font} {size} Tf 1 0 0 1 {x} {y} Tm ({escaped}) Tj ET\n")
}

/// A stroked horizontal rule.
fn hline(x1: f32, x2: f32, y: f32) -> String {
    format!("0.5 w {x1} {y} m {x2} {y} l S\n")
}

/// A stroked vertical rule.
fn vline(x: f32, y1: f32, y2: f32) -> String {
    format!("0.5 w {x} {y1} m {x} {y2} l S\n")
}

/// A Letter-sized PDF with one page per content stream.
fn make_pdf(pages: &[String]) -> Vec<u8> {
    let mut objects: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        String::new(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold >>".into(),
    ];
    let mut kids = Vec::new();
    for content in pages {
        objects.push(format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len()
        ));
        let contents = objects.len();
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
             /Resources << /Font << /F1 3 0 R /F2 4 0 R >> >> /Contents {contents} 0 R >>"
        ));
        kids.push(format!("{} 0 R", objects.len()));
    }
    objects[1] = format!(
        "<< /Type /Pages /Kids [{}] /Count {} >>",
        kids.join(" "),
        kids.len()
    );
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (index, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", index + 1).as_bytes());
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
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF",
            objects.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

fn page_markdown(pdf: &[u8]) -> Vec<String> {
    extract_pages_markdown_mem(pdf, None)
        .expect("handmade PDF extracts")
        .pages
        .into_iter()
        .map(|page| page.markdown)
        .collect()
}

fn table_rows(markdown: &str) -> Vec<&str> {
    markdown
        .lines()
        .filter(|line| line.starts_with('|') && !line.starts_with("|---"))
        .collect()
}

/// Upstream #612 (reported by closytalk-daniele-sarnari): a Word document
/// exported by LibreOffice draws its line-ruled table with strokes inside
/// one full-page clip, and the "Title" style's bottom border is one more
/// stroke above the headings. That rule became the table's top edge, so
/// both headings and the paragraph between it and the table turned into a
/// fabricated first row. The same holds for a heading's rule below a table.
#[test]
fn headings_between_a_title_rule_and_a_ruled_table_stay_outside_it() {
    let mut page = String::from("q 0 0.028 611.972 791.972 re W* n\n");
    page += &hline(84.6, 522.0, 688.0); // the title's bottom border
    page += &text("F1", 26.0, 90.1, 695.3, "Atelier Verdi-Brand Book 2027");
    page += &text("F2", 14.0, 90.1, 645.9, "1. Tono di voce");
    page += &text(
        "F1",
        11.0,
        90.1,
        629.7,
        "Elegante, caldo, sobrio. Frasi brevi. Parliamo con il noi e diamo del tu alla cliente. Mai urlato,",
    );
    page += &text("F1", 11.0, 90.1, 615.2, "mai gergale.");
    page += &text("F2", 14.0, 90.1, 573.3, "2. Palette");
    let rows = [
        ["Nome", "HEX", "Uso"],
        ["Verde Salvia", "#7A9E7E", "Colore principale"],
        ["Avorio Seta", "#F5EFE0", "Sfondo"],
        ["Terracotta", "#C4623A", "Accento"],
        ["Nero Inchiostro", "#1C1C1C", "Testo"],
    ];
    let baselines: Vec<f32> = (0..5).map(|row| 556.6 - row as f32 * 13.15).collect();
    let mut edges = vec![baselines[0] + 10.6];
    edges.extend(baselines.iter().map(|y| y - 2.55));
    for y in &edges {
        page += &hline(84.6, 522.0, *y);
    }
    for x in [84.6, 228.6, 372.6, 522.0] {
        page += &vline(x, edges[5], edges[0]);
    }
    for (y, row) in baselines.iter().zip(rows) {
        for (x, cell) in [90.1, 234.1, 378.1].into_iter().zip(row) {
            page += &text("F1", 11.0, x, *y, cell);
        }
    }
    // A heading and its paragraph below the table, closed by a rule.
    page += &text("F2", 14.0, 90.1, 470.0, "3. Tipografia");
    page += &text("F1", 11.0, 90.1, 452.0, "Titoli in serif, testo in sans.");
    page += &hline(84.6, 522.0, 440.0);
    page += "Q\n";

    let markdown = page_markdown(&make_pdf(&[page])).remove(0);
    assert_eq!(
        table_rows(&markdown),
        [
            "|Nome|HEX|Uso|",
            "|Verde Salvia|#7A9E7E|Colore principale|",
            "|Avorio Seta|#F5EFE0|Sfondo|",
            "|Terracotta|#C4623A|Accento|",
            "|Nero Inchiostro|#1C1C1C|Testo|",
        ],
        "{markdown}"
    );
    for outside in [
        "1. Tono di voce",
        "Elegante, caldo, sobrio.",
        "mai gergale.",
        "2. Palette",
        "3. Tipografia",
        "Titoli in serif",
    ] {
        let line = markdown
            .lines()
            .find(|line| line.contains(outside))
            .unwrap_or_else(|| panic!("{outside:?} missing:\n{markdown}"));
        assert!(
            !line.starts_with('|'),
            "{outside:?} in the table:\n{markdown}"
        );
    }
    let order: Vec<usize> = ["Tono di voce", "Palette", "|Nome|", "Tipografia"]
        .iter()
        .map(|needle| markdown.find(needle).unwrap())
        .collect();
    assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{markdown}");
}

/// A filled rectangle, as Word draws a 0.5 pt table border.
fn fill(x: f32, y: f32, width: f32, height: f32) -> String {
    format!("{x} {y} {width} {height} re f\n")
}

/// The 3 x 4 "Table Grid" of upstream #669: three body cells wrap onto a
/// second line, the last one in the last row. `borders` draws Word's 0.5 pt
/// filled border rectangles; without them the table is borderless.
fn wrapped_last_row_page(borders: bool) -> String {
    let columns = [72.0, 228.0, 384.0, 540.0];
    let edges = [368.1, 354.2, 327.9, 301.6, 275.3];
    let mut page = text("F2", 14.0, 72.0, 420.0, "Table section");
    page += &text(
        "F1",
        11.0,
        72.0,
        395.0,
        "The table below lists three rows of sample content for the report.",
    );
    if borders {
        for y in edges {
            page += &fill(71.75, y - 0.25, 468.5, 0.5);
        }
        for x in columns {
            page += &fill(x - 0.25, 275.05, 0.5, 93.3);
        }
    }
    let lines: [(f32, [&str; 3]); 7] = [
        (357.9, ["[10] Name", "[11] Description", "[12] Notes"]),
        (
            344.0,
            ["[13] One", "[14] A cell that wraps into", "[15] Short"],
        ),
        (330.9, ["", "the right column.", ""]),
        (
            317.7,
            ["[16] Two", "[17] Another wrapped cell", "[18] Also short"],
        ),
        (304.6, ["", "on the right.", ""]),
        (
            291.4,
            [
                "[19] Three",
                "[20] The text after the table",
                "[21] Across both columns",
            ],
        ),
        (278.3, ["", "comes next.", ""]),
    ];
    for (y, cells) in lines {
        for (x, cell) in columns.iter().zip(cells) {
            if !cell.is_empty() {
                page += &text("F1", 11.0, x + 5.4, y, cell);
            }
        }
    }
    page += &text(
        "F1",
        11.0,
        72.0,
        250.0,
        "After the table the document continues with an ordinary paragraph.",
    );
    page
}

/// Upstream #669 (reported by motchie): the heuristic detector bounds a
/// table by its rows with items in two or more columns, so the wrapped line
/// of the last row's cell fell below the table and came out as a paragraph.
#[test]
fn a_wrapped_last_row_stays_in_its_cell() {
    for borders in [true, false] {
        let markdown = page_markdown(&make_pdf(&[wrapped_last_row_page(borders)])).remove(0);
        assert_eq!(
            table_rows(&markdown),
            [
                "|[10] Name|[11] Description|[12] Notes|",
                "|[13] One|[14] A cell that wraps into the right column.|[15] Short|",
                "|[16] Two|[17] Another wrapped cell on the right.|[18] Also short|",
                "|[19] Three|[20] The text after the table comes next.|[21] Across both columns|",
            ],
            "borders: {borders}\n{markdown}"
        );
        assert_eq!(markdown.matches("comes next.").count(), 1, "{markdown}");
        let after = markdown
            .lines()
            .find(|line| line.contains("After the table the document continues"))
            .unwrap_or_else(|| panic!("paragraph after the table missing:\n{markdown}"));
        assert!(!after.starts_with('|'), "{markdown}");
    }
}
