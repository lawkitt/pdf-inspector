//! Generates `tests/fixtures/bilingual_contract_en_ru.pdf`: one A4 page of a
//! bilingual services agreement laid out the way Russian–English contracts
//! usually are — a bordered two-column grid, English on the left and Russian
//! on the right, one row per numbered clause, each language's text wrapping
//! inside its own cell.
//!
//! Every word was written for this fixture. The page uses two non-embedded
//! TrueType fonts (Arial and Arial Bold) under a Windows-1251-style simple
//! encoding: WinAnsi for ASCII, `/Differences` naming the Cyrillic glyphs
//! (`afii10017`…) at 0xC0–0xFF, Ё/ё and №, plus a ToUnicode CMap. The
//! `/Widths` are approximate Arial advances; the layout below wraps and
//! positions with the same table, so the declared metrics and the drawn
//! lines agree. The grid is drawn as stroked rules: horizontal rules
//! between rows across the full width, vertical rules down the left edge,
//! the middle and the right edge.
//!
//! Text is justified and shown word by word, each word placed on its own
//! (`Tm` … `Tj`), as word processors export justified text. Cells keep
//! 10 pt of padding on each side, so the two text columns stand 20 pt
//! apart across the middle rule: wide enough for the side-by-side split to
//! propose a gutter there, which is the layout upstream #274's veto must
//! not turn into a scrambled table (lawkitt/mdoc#925 W4). With 5 pt of
//! padding no split is proposed and the question does not arise.
//!
//! Regenerate with `cargo run --example bilingual_contract_fixture`. The
//! output is deterministic (no timestamps, no compression, no document ID).

use std::fmt::Write as _;
use std::path::Path;

use lopdf::xref::XrefType;
use lopdf::{dictionary, Document, Object, Stream, StringFormat};

const PAGE_WIDTH: f32 = 595.0;
const PAGE_HEIGHT: f32 = 842.0;
const GRID_LEFT: f32 = 50.0;
const GRID_RIGHT: f32 = 545.0;
const GRID_MIDDLE: f32 = (GRID_LEFT + GRID_RIGHT) / 2.0;
const GRID_TOP: f32 = 800.0;
/// Space between a cell's side rules and its text.
const CELL_PADDING: f32 = 10.0;
const FONT_SIZE: f32 = 9.0;
const LEADING: f32 = 11.5;
/// Gap between a cell's top rule and its first baseline.
const FIRST_BASELINE_DROP: f32 = 4.0 + FONT_SIZE;
/// Gap between a cell's last baseline and its bottom rule.
const BOTTOM_PADDING: f32 = 5.0;

/// One run of a cell: bold or regular, starting a new line.
struct Run {
    bold: bool,
    text: &'static str,
}

const fn regular(text: &'static str) -> Run {
    Run { bold: false, text }
}

const fn bold(text: &'static str) -> Run {
    Run { bold: true, text }
}

/// The rows of the grid: English cell, Russian cell.
fn rows() -> Vec<(Vec<Run>, Vec<Run>)> {
    vec![
        (
            vec![bold("SERVICES AGREEMENT No. 17/2026")],
            vec![bold("ДОГОВОР ОКАЗАНИЯ УСЛУГ № 17/2026")],
        ),
        (
            vec![regular("Moscow, 1 December 2026")],
            vec![regular("г. Москва, 1 декабря 2026 г.")],
        ),
        (
            vec![regular(
                "Northwind Analytics LLC (the \"Customer\") and Severny Code LLC \
                 (the \"Contractor\") have entered into this Agreement as follows.",
            )],
            vec![regular(
                "ООО «Нортвинд Аналитика» (далее – «Заказчик») и ООО «Северный Код» \
                 (далее – «Исполнитель») заключили настоящий Договор о нижеследующем.",
            )],
        ),
        (
            vec![
                bold("1. Subject of the Agreement"),
                regular(
                    "1.1. The Contractor undertakes to provide software maintenance \
                     services, and the Customer undertakes to accept and pay for them.",
                ),
                regular("1.2. The scope and timing of the services are set out in Appendix No. 1."),
            ],
            vec![
                bold("1. Предмет договора"),
                regular(
                    "1.1. Исполнитель обязуется оказывать услуги по сопровождению \
                     программного обеспечения, а Заказчик обязуется принимать и \
                     оплачивать эти услуги.",
                ),
                regular("1.2. Объем и сроки оказания услуг определяются в Приложении № 1."),
            ],
        ),
        (
            vec![
                bold("2. Price and Payment"),
                regular("2.1. The monthly fee is 150,000 rubles, VAT included."),
                regular(
                    "2.2. The Customer pays within ten business days after signing \
                     the acceptance certificate.",
                ),
            ],
            vec![
                bold("2. Цена и порядок расчетов"),
                regular("2.1. Ежемесячная стоимость услуг составляет 150 000 рублей, включая НДС."),
                regular(
                    "2.2. Заказчик оплачивает услуги в течение десяти рабочих дней \
                     после подписания акта сдачи-приемки.",
                ),
            ],
        ),
        (
            vec![
                bold("3. Obligations of the Parties"),
                regular(
                    "3.1. The Contractor shall respond to a support request within \
                     four business hours.",
                ),
                regular(
                    "3.2. The Customer shall grant the Contractor access to the \
                     systems needed for the services.",
                ),
            ],
            vec![
                bold("3. Обязанности сторон"),
                regular(
                    "3.1. Исполнитель обязан отвечать на запрос о поддержке в течение \
                     четырех рабочих часов.",
                ),
                regular(
                    "3.2. Заказчик обязан предоставить Исполнителю доступ к системам, \
                     необходимым для оказания услуг.",
                ),
            ],
        ),
        (
            vec![
                bold("4. Liability"),
                regular(
                    "4.1. For late payment the Customer pays a penalty of 0.1 percent \
                     of the overdue amount per day.",
                ),
                regular("4.2. Neither Party is liable for failures caused by force majeure."),
            ],
            vec![
                bold("4. Ответственность сторон"),
                regular(
                    "4.1. За просрочку оплаты Заказчик уплачивает неустойку в размере \
                     0,1 процента от просроченной суммы за каждый день.",
                ),
                regular(
                    "4.2. Стороны не несут ответственности за неисполнение \
                     обязательств вследствие обстоятельств непреодолимой силы.",
                ),
            ],
        ),
        (
            vec![
                bold("5. Term and Termination"),
                regular(
                    "5.1. This Agreement takes effect on signing and remains in force \
                     until 31 December 2027.",
                ),
                regular(
                    "5.2. Either Party may terminate this Agreement with thirty days' \
                     written notice.",
                ),
            ],
            vec![
                bold("5. Срок действия и расторжение"),
                regular(
                    "5.1. Настоящий Договор вступает в силу с момента подписания и \
                     действует до 31 декабря 2027 г.",
                ),
                regular(
                    "5.2. Каждая из сторон вправе расторгнуть настоящий Договор, \
                     письменно уведомив другую сторону за тридцать дней.",
                ),
            ],
        ),
        (
            vec![
                bold("6. Governing Law"),
                regular("6.1. This Agreement is governed by the laws of the Russian Federation."),
            ],
            vec![
                bold("6. Применимое право"),
                regular(
                    "6.1. Настоящий Договор регулируется законодательством Российской Федерации.",
                ),
            ],
        ),
    ]
}

// ---------------------------------------------------------------------------
// Encoding and metrics
// ---------------------------------------------------------------------------

const UPPER: &str = "АБВГДЕЖЗИЙКЛМНОПРСТУФХЦЧШЩЪЫЬЭЮЯ";
const LOWER: &str = "абвгдежзийклмнопрстуфхцчшщъыьэюя";

/// Approximate Arial advances (1/1000 em) of А…Я and а…я.
const UPPER_WIDTHS: [u16; 32] = [
    667, 656, 667, 542, 677, 667, 923, 604, 719, 719, 583, 656, 833, 722, 778, 719, 667, 722, 611,
    635, 760, 667, 740, 667, 917, 938, 792, 885, 656, 719, 1010, 722,
];
const LOWER_WIDTHS: [u16; 32] = [
    556, 573, 531, 365, 583, 556, 669, 458, 559, 559, 438, 583, 688, 552, 556, 542, 556, 500, 458,
    500, 823, 500, 573, 521, 802, 823, 625, 719, 521, 510, 750, 542,
];

/// Approximate Arial/Helvetica advances of ASCII 0x20…0x7E.
const ASCII_WIDTHS: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278,
    278, // ' '…'/'
    556, 556, 556, 556, 556, 556, 556, 556, 556, 556, 278, 278, 584, 584, 584,
    556, // '0'…'?'
    1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833, 722,
    778, // '@'…'O'
    667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278, 469,
    556, // 'P'…'_'
    333, 556, 556, 500, 556, 556, 278, 556, 556, 222, 222, 500, 222, 833, 556,
    556, // '`'…'o'
    556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334, 260, 334, 584, // 'p'…'~'
];

/// The code a character is shown with, and its glyph name when the code
/// is not WinAnsi's.
fn encode(ch: char) -> (u8, Option<String>) {
    if ch.is_ascii() && !ch.is_ascii_control() {
        return (ch as u8, None);
    }
    if let Some(i) = UPPER.chars().position(|c| c == ch) {
        // afii10023 is Ё, which sits outside the run.
        let n = 10017 + i + usize::from(i >= 6);
        return (0xC0 + i as u8, Some(format!("afii{n}")));
    }
    if let Some(i) = LOWER.chars().position(|c| c == ch) {
        let n = 10065 + i + usize::from(i >= 6);
        return (0xE0 + i as u8, Some(format!("afii{n}")));
    }
    match ch {
        'Ё' => (0xA8, Some("afii10023".into())),
        'ё' => (0xB8, Some("afii10071".into())),
        '№' => (0xB9, Some("afii61352".into())),
        // WinAnsi and Windows-1251 agree on these.
        '«' => (0xAB, None),
        '»' => (0xBB, None),
        '–' => (0x96, None),
        '—' => (0x97, None),
        _ => panic!("no code for {ch:?}"),
    }
}

fn width(ch: char) -> u16 {
    if ch.is_ascii() && !ch.is_ascii_control() {
        return ASCII_WIDTHS[ch as usize - 0x20];
    }
    if let Some(i) = UPPER.chars().position(|c| c == ch) {
        return UPPER_WIDTHS[i];
    }
    if let Some(i) = LOWER.chars().position(|c| c == ch) {
        return LOWER_WIDTHS[i];
    }
    match ch {
        'Ё' => 667,
        'ё' => 556,
        '№' => 1073,
        '«' | '»' => 556,
        '–' => 556,
        '—' => 1000,
        _ => panic!("no width for {ch:?}"),
    }
}

fn text_width(text: &str) -> f32 {
    text.chars().map(|c| f32::from(width(c))).sum::<f32>() * FONT_SIZE / 1000.0
}

/// Every code the encoding uses, with its character.
fn code_points() -> Vec<(u8, char)> {
    let mut codes: Vec<(u8, char)> = (0x20u8..=0x7E).map(|b| (b, b as char)).collect();
    for ch in UPPER.chars().chain(LOWER.chars()).chain("Ёё№«»–—".chars()) {
        codes.push((encode(ch).0, ch));
    }
    codes.sort();
    codes
}

fn encoding_dictionary() -> lopdf::Dictionary {
    let mut differences = Vec::new();
    let mut previous: Option<u8> = None;
    for (code, ch) in code_points() {
        let (_, name) = encode(ch);
        let Some(name) = name else { continue };
        if previous.is_none_or(|p| p + 1 != code) {
            differences.push(Object::Integer(i64::from(code)));
        }
        differences.push(Object::Name(name.into_bytes()));
        previous = Some(code);
    }
    dictionary! {
        "Type" => "Encoding",
        "BaseEncoding" => "WinAnsiEncoding",
        "Differences" => Object::Array(differences),
    }
}

fn to_unicode_cmap() -> Vec<u8> {
    let entries = code_points();
    let mut cmap = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n\
         /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
         /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n\
         1 begincodespacerange\n<00> <FF>\nendcodespacerange\n",
    );
    for chunk in entries.chunks(100) {
        writeln!(cmap, "{} beginbfchar", chunk.len()).unwrap();
        for (code, ch) in chunk {
            writeln!(cmap, "<{code:02X}> <{:04X}>", *ch as u32).unwrap();
        }
        cmap.push_str("endbfchar\n");
    }
    cmap.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    cmap.into_bytes()
}

fn widths_array() -> Object {
    let mut widths = vec![Object::Integer(0); 256 - 32];
    for (code, ch) in code_points() {
        widths[usize::from(code) - 32] = Object::Integer(i64::from(width(ch)));
    }
    Object::Array(widths)
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// Greedy word wrap at spaces into lines no wider than `max`.
fn wrap(text: &str, max: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let candidate = if line.is_empty() {
            word.to_string()
        } else {
            format!("{line} {word}")
        };
        if text_width(&candidate) <= max || line.is_empty() {
            line = candidate;
        } else {
            lines.push(std::mem::take(&mut line));
            line = word.to_string();
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// The cell's lines, each with whether it is bold and whether it is
/// justified (every line of a paragraph but its last).
fn cell_lines(runs: &[Run], max: f32) -> Vec<(bool, String, bool)> {
    runs.iter()
        .flat_map(|run| {
            let lines = wrap(run.text, max);
            let last = lines.len() - 1;
            lines
                .into_iter()
                .enumerate()
                .map(move |(i, l)| (run.bold, l, i < last))
        })
        .collect()
}

fn escape(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        let (code, _) = encode(ch);
        match code {
            b'(' | b')' | b'\\' => {
                out.push('\\');
                out.push(code as char);
            }
            0x20..=0x7E => out.push(code as char),
            _ => write!(out, "\\{code:03o}").unwrap(),
        }
    }
    out
}

fn page_content() -> String {
    let max = GRID_MIDDLE - GRID_LEFT - 2.0 * CELL_PADDING;
    let mut text = String::new();
    let mut row_rules = vec![GRID_TOP];
    let mut top = GRID_TOP;
    for (english, russian) in rows() {
        let cells = [
            (GRID_LEFT + CELL_PADDING, cell_lines(&english, max)),
            (GRID_MIDDLE + CELL_PADDING, cell_lines(&russian, max)),
        ];
        let lines = cells.iter().map(|(_, l)| l.len()).max().unwrap_or(1);
        for (x, lines) in &cells {
            text.push_str("BT\n");
            for (i, (is_bold, line, justified)) in lines.iter().enumerate() {
                let y = top - FIRST_BASELINE_DROP - i as f32 * LEADING;
                let font = if *is_bold { "F2" } else { "F1" };
                writeln!(text, "/{font} {FONT_SIZE} Tf").unwrap();
                // Each word is placed on its own, as word processors export
                // justified text; every line of a paragraph but its last is
                // spread to the cell's width.
                let words: Vec<&str> = line.split(' ').collect();
                let spacing = if *justified && words.len() > 1 {
                    let ink: f32 = words.iter().map(|w| text_width(w)).sum();
                    (max - ink) / (words.len() - 1) as f32
                } else {
                    text_width(" ")
                };
                let mut word_x = *x;
                for word in words {
                    writeln!(text, "1 0 0 1 {word_x:.2} {y:.2} Tm ({}) Tj", escape(word)).unwrap();
                    word_x += text_width(word) + spacing;
                }
            }
            text.push_str("ET\n");
        }
        top -= FIRST_BASELINE_DROP + (lines - 1) as f32 * LEADING + BOTTOM_PADDING;
        row_rules.push(top);
    }
    assert!(top > 40.0, "the grid runs off the page (bottom at {top})");

    let mut rules = String::from("q 0.5 w 0 G\n");
    for y in &row_rules {
        writeln!(rules, "{GRID_LEFT:.2} {y:.2} m {GRID_RIGHT:.2} {y:.2} l S").unwrap();
    }
    for x in [GRID_LEFT, GRID_MIDDLE, GRID_RIGHT] {
        writeln!(rules, "{x:.2} {GRID_TOP:.2} m {x:.2} {top:.2} l S").unwrap();
    }
    rules.push_str("Q\n");
    rules + &text
}

// ---------------------------------------------------------------------------
// Document
// ---------------------------------------------------------------------------

fn plain_stream(dict: lopdf::Dictionary, content: Vec<u8>) -> Object {
    Object::Stream(Stream::new(dict, content).with_compression(false))
}

fn literal(bytes: &[u8]) -> Object {
    Object::String(bytes.to_vec(), StringFormat::Literal)
}

fn main() {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bilingual_contract_en_ru.pdf");
    let mut doc = Document::new();
    doc.reference_table.cross_reference_type = XrefType::CrossReferenceTable;
    let encoding = doc.add_object(encoding_dictionary());
    let to_unicode = doc.add_object(plain_stream(dictionary! {}, to_unicode_cmap()));
    let mut font = |base_font: &str, flags: i64, stem_v: i64| {
        let descriptor = doc.add_object(dictionary! {
            "Type" => "FontDescriptor",
            "FontName" => Object::Name(base_font.as_bytes().to_vec()),
            "Flags" => Object::Integer(flags),
            "FontBBox" => Object::Array(vec![(-665).into(), (-325).into(), 2000.into(), 1006.into()]),
            "ItalicAngle" => Object::Integer(0),
            "Ascent" => Object::Integer(905),
            "Descent" => Object::Integer(-212),
            "CapHeight" => Object::Integer(716),
            "StemV" => Object::Integer(stem_v),
        });
        doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "TrueType",
            "BaseFont" => Object::Name(base_font.as_bytes().to_vec()),
            "FirstChar" => Object::Integer(32),
            "LastChar" => Object::Integer(255),
            "Widths" => widths_array(),
            "Encoding" => Object::Reference(encoding),
            "ToUnicode" => Object::Reference(to_unicode),
            "FontDescriptor" => Object::Reference(descriptor),
        })
    };
    // Flags 32: nonsymbolic. The bold face also sets ForceBold (1 << 18).
    let regular_font = font("Arial", 32, 80);
    let bold_font = font("Arial,Bold", 32 | (1 << 18), 140);

    let content_id = doc.add_object(plain_stream(dictionary! {}, page_content().into_bytes()));
    let pages_id = doc.new_object_id();
    let page = doc.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => Object::Reference(pages_id),
        "MediaBox" => Object::Array(vec![0.into(), 0.into(), PAGE_WIDTH.into(), PAGE_HEIGHT.into()]),
        "Resources" => dictionary! {
            "Font" => dictionary! {
                "F1" => Object::Reference(regular_font),
                "F2" => Object::Reference(bold_font),
            },
        },
        "Contents" => Object::Reference(content_id),
    });
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => Object::Array(vec![Object::Reference(page)]),
            "Count" => Object::Integer(1),
        }),
    );
    let catalog = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    let info = doc.add_object(dictionary! {
        "Title" => literal(b"Bilingual EN-RU services agreement (synthetic)"),
        "Producer" => literal(b"pdf-inspector examples/bilingual_contract_fixture.rs"),
    });
    doc.trailer.set("Root", Object::Reference(catalog));
    doc.trailer.set("Info", Object::Reference(info));
    doc.save(&path)
        .unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
    println!("{}", path.display());
}
