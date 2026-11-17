//! Annotations that carry text: sticky notes, FreeText boxes and text markup
//! with a comment, reported beside the Markdown and never inside it.
//!
//! A [`Collector`] reads each requested page's `/Annots` while the per-page
//! extraction runs, then [`Collector::finish`] links replies to their
//! parents, folds review states into a resolved flag, reads the text under
//! markup quads from the page's own text, and moves the geometry into the
//! visible-page-box frame. Every value is read defensively: an annotation
//! that cannot be read is skipped, and nothing here can fail an extraction.

use std::collections::{HashMap, HashSet};

use lopdf::{Dictionary, Document, Object, ObjectId};

use super::fonts::resolve_array;
use super::geometry::PageRotation;
use super::page_box::{visible_page_box, PageBox};
use crate::text_utils::decode_pdf_text_string;
use crate::types::{ItemType, TextItem};
use crate::{AnnotationKind, AnnotationRect, PdfAnnotation};

/// Most annotations reported for one extraction; the rest are left out.
pub const MAX_ANNOTATIONS: usize = 10_000;

/// Most `/Annots` entries examined for one extraction, reported or not, so
/// a page listing millions of entries costs a bounded walk.
pub(crate) const MAX_ANNOTS_EXAMINED: usize = 100_000;

/// Longest annotation text, in characters, kept for
/// [`PdfAnnotation::text`] and [`PdfAnnotation::marked_text`]; longer text
/// is cut at a character boundary.
pub const MAX_ANNOTATION_TEXT_CHARS: usize = 32_768;

/// Longest author name kept, in characters.
pub const MAX_ANNOTATION_AUTHOR_CHARS: usize = 512;

/// Longest date kept, in characters.
pub const MAX_ANNOTATION_DATE_CHARS: usize = 64;

/// Most quads kept for one markup annotation.
pub const MAX_ANNOTATION_QUADS: usize = 1_000;

/// Most quad-against-run comparisons spent reading marked text for one
/// extraction; markup past the budget reports no marked text.
pub(crate) const MAX_MARKED_TEXT_CHECKS: usize = 5_000_000;

/// Most bytes of rich text (`/RC`) read for one annotation: a string is
/// cut there, a stream inflating past it is not read.
pub(crate) const MAX_RICH_TEXT_BYTES: usize = 1024 * 1024;

/// One annotation as read from the page, in raw PDF user space.
struct Found {
    page: u32,
    object: Option<ObjectId>,
    kind: AnnotationKind,
    author: Option<String>,
    date: Option<String>,
    text: String,
    rect: [f32; 4],
    quads: Vec<[f32; 4]>,
    reply_to: Option<ObjectId>,
    page_box: PageBox,
}

/// A review state an annotation records for its parent.
struct ReviewState {
    parent: ObjectId,
    resolved: bool,
    date: Option<i64>,
    order: usize,
}

/// Gathers the text-bearing annotations of the requested pages.
#[derive(Default)]
pub(crate) struct Collector {
    found: Vec<Found>,
    states: Vec<ReviewState>,
    examined: usize,
    /// Pages whose text is not reliable enough to read marked text from.
    unreliable_pages: HashSet<u32>,
}

impl Collector {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Read one page's annotations. Malformed entries are skipped.
    pub(crate) fn add_page(&mut self, doc: &Document, page: u32, page_id: ObjectId) {
        let Some(annots) = doc
            .get_dictionary(page_id)
            .ok()
            .and_then(|dict| dict.get(b"Annots").ok())
            .and_then(|annots| resolve_array(doc, annots))
        else {
            return;
        };
        let page_box = visible_page_box(doc, page_id).unwrap_or(PageBox::LETTER);
        for entry in annots {
            if self.examined >= MAX_ANNOTS_EXAMINED || self.found.len() >= MAX_ANNOTATIONS {
                return;
            }
            self.examined += 1;
            let (object, dict) = match entry {
                Object::Reference(id) => match doc.get_dictionary(*id) {
                    Ok(dict) => (Some(*id), dict),
                    Err(_) => continue,
                },
                Object::Dictionary(dict) => (None, dict),
                _ => continue,
            };
            self.add_annotation(doc, page, object, dict, page_box);
        }
    }

    fn add_annotation(
        &mut self,
        doc: &Document,
        page: u32,
        object: Option<ObjectId>,
        dict: &Dictionary,
        page_box: PageBox,
    ) {
        let Some(subtype) = name(doc, dict, b"Subtype") else {
            return;
        };
        let kind = match subtype {
            b"Text" => AnnotationKind::Text,
            b"FreeText" => AnnotationKind::FreeText,
            b"Highlight" => AnnotationKind::Highlight,
            b"Underline" => AnnotationKind::Underline,
            b"StrikeOut" => AnnotationKind::StrikeOut,
            b"Squiggly" => AnnotationKind::Squiggly,
            _ => return,
        };
        let parent = match dict.get(b"IRT") {
            Ok(Object::Reference(id)) => Some(*id),
            _ => None,
        };
        // `/RT /Group` makes the annotation part of its parent, not a reply.
        let reply_to = parent.filter(|_| name(doc, dict, b"RT") != Some(b"Group"));

        // A Text annotation with a `/State` in reply to another records that
        // annotation's state; it is not a comment of its own.
        if kind == AnnotationKind::Text {
            if let (Some(parent), Some(state)) = (parent, name(doc, dict, b"State")) {
                let model = name(doc, dict, b"StateModel");
                let is_review = match model {
                    Some(model) => model == b"Review",
                    // Without a model, the state's value implies it (PDF
                    // 32000-1, table 172).
                    None => !matches!(state, b"Marked" | b"Unmarked"),
                };
                if is_review {
                    self.states.push(ReviewState {
                        parent,
                        resolved: matches!(state, b"Accepted" | b"Completed"),
                        date: date_of(doc, dict).as_deref().and_then(pdf_date_key),
                        order: self.states.len(),
                    });
                }
                return;
            }
        }

        let Some(text) = annotation_text(doc, dict) else {
            return;
        };
        let Some(rect) = rect(doc, dict) else {
            return;
        };
        let quads = if kind.is_markup() {
            quads(doc, dict)
        } else {
            Vec::new()
        };
        self.found.push(Found {
            page,
            object,
            kind,
            author: text_value(doc, dict, b"T")
                .map(|author| truncate_chars(author.trim(), MAX_ANNOTATION_AUTHOR_CHARS))
                .filter(|author| !author.is_empty()),
            date: date_of(doc, dict),
            text,
            rect,
            quads,
            reply_to,
            page_box,
        });
    }

    /// Leave out the marked text of a page whose text is unreliable (it
    /// needs OCR), rather than report garbled text.
    pub(crate) fn mark_text_unreliable(&mut self, page: u32) {
        self.unreliable_pages.insert(page);
    }

    /// The annotations found, in page order and then `/Annots` order.
    ///
    /// `items` are the document's text items in the frame the extractor
    /// laid them out in (raw user space, turned on pages listed in
    /// `rotations`); they supply the marked text.
    pub(crate) fn finish(
        self,
        items: &[TextItem],
        rotations: &HashMap<u32, PageRotation>,
    ) -> Vec<PdfAnnotation> {
        let Collector {
            found,
            states,
            unreliable_pages,
            ..
        } = self;

        // The latest review state per parent decides whether it is resolved.
        let mut latest: HashMap<ObjectId, &ReviewState> = HashMap::new();
        for state in &states {
            match latest.get(&state.parent) {
                Some(current) if !is_later(state, current) => {}
                _ => {
                    latest.insert(state.parent, state);
                }
            }
        }

        let mut index_of: HashMap<ObjectId, usize> = HashMap::new();
        for (index, annotation) in found.iter().enumerate() {
            if let Some(object) = annotation.object {
                index_of.entry(object).or_insert(index);
            }
        }

        // The text items of the pages whose markup marks text, in one pass.
        let wanted: HashSet<u32> = found
            .iter()
            .filter(|a| !a.quads.is_empty() && !unreliable_pages.contains(&a.page))
            .map(|a| a.page)
            .collect();
        let mut page_items: HashMap<u32, Vec<&TextItem>> = HashMap::new();
        if !wanted.is_empty() {
            for item in items {
                if matches!(item.item_type, ItemType::Text) && wanted.contains(&item.page) {
                    page_items.entry(item.page).or_default().push(item);
                }
            }
        }

        let mut checks_left = MAX_MARKED_TEXT_CHECKS;
        let mut annotations: Vec<PdfAnnotation> = found
            .into_iter()
            .map(|a| {
                let marked_text = if a.kind.is_markup() && wanted.contains(&a.page) {
                    let rotation = rotations
                        .get(&a.page)
                        .copied()
                        .unwrap_or(PageRotation::Upright);
                    let items = page_items.get(&a.page).map(Vec::as_slice).unwrap_or(&[]);
                    let cost = a.quads.len().saturating_mul(items.len());
                    if cost <= checks_left {
                        checks_left -= cost;
                        marked_text(&a.quads, items, rotation)
                    } else {
                        None
                    }
                } else {
                    None
                };
                PdfAnnotation {
                    page: a.page,
                    kind: a.kind,
                    author: a.author,
                    date: a.date,
                    text: a.text,
                    rect: frame_rect(a.rect, a.page_box),
                    quads: a
                        .quads
                        .iter()
                        .map(|quad| frame_rect(*quad, a.page_box))
                        .collect(),
                    in_reply_to: a.reply_to.and_then(|parent| index_of.get(&parent).copied()),
                    resolved: a
                        .object
                        .and_then(|object| latest.get(&object))
                        .is_some_and(|state| state.resolved),
                    marked_text,
                }
            })
            .collect();
        break_reply_cycles(&mut annotations);
        annotations
    }
}

/// Cut the reply link that closes a cycle (an annotation replying to
/// itself, or two replying to each other), so following `in_reply_to`
/// always ends.
fn break_reply_cycles(annotations: &mut [PdfAnnotation]) {
    // 0: not seen, 1: on the current chain, 2: known to end.
    let mut state = vec![0u8; annotations.len()];
    for start in 0..annotations.len() {
        let mut chain = Vec::new();
        let mut current = start;
        while state[current] == 0 {
            state[current] = 1;
            chain.push(current);
            match annotations[current].in_reply_to {
                Some(parent) if state[parent] == 1 => {
                    annotations[current].in_reply_to = None;
                    break;
                }
                Some(parent) => current = parent,
                None => break,
            }
        }
        for index in chain {
            state[index] = 2;
        }
    }
}

impl AnnotationKind {
    fn is_markup(self) -> bool {
        matches!(
            self,
            AnnotationKind::Highlight
                | AnnotationKind::Underline
                | AnnotationKind::StrikeOut
                | AnnotationKind::Squiggly
        )
    }
}

/// Whether `state` was recorded after `current`: by date when both carry
/// one, else by `/Annots` order.
fn is_later(state: &ReviewState, current: &ReviewState) -> bool {
    match (state.date, current.date) {
        (Some(a), Some(b)) if a != b => a > b,
        _ => state.order > current.order,
    }
}

/// A raw `[x0, y0, x1, y1]` box as an [`AnnotationRect`] relative to the
/// visible page box's lower-left corner.
fn frame_rect(raw: [f32; 4], page_box: PageBox) -> AnnotationRect {
    AnnotationRect {
        x: raw[0] - page_box.x0,
        y: raw[1] - page_box.y0,
        width: raw[2] - raw[0],
        height: raw[3] - raw[1],
    }
}

fn name<'a>(doc: &'a Document, dict: &'a Dictionary, key: &[u8]) -> Option<&'a [u8]> {
    let object = dict.get(key).ok()?;
    let (_, object) = doc.dereference(object).ok()?;
    object.as_name().ok()
}

fn number(doc: &Document, object: &Object) -> Option<f32> {
    let (_, object) = doc.dereference(object).ok()?;
    let value = match object {
        Object::Integer(i) => *i as f32,
        Object::Real(r) => *r,
        _ => return None,
    };
    value.is_finite().then_some(value)
}

/// The bytes of a string value, following references.
fn string_bytes<'a>(doc: &'a Document, dict: &'a Dictionary, key: &[u8]) -> Option<&'a [u8]> {
    let object = dict.get(key).ok()?;
    let (_, object) = doc.dereference(object).ok()?;
    match object {
        Object::String(bytes, _) => Some(bytes),
        _ => None,
    }
}

/// A text string value, decoded.
fn text_value(doc: &Document, dict: &Dictionary, key: &[u8]) -> Option<String> {
    string_bytes(doc, dict, key).map(decode_pdf_text_string)
}

/// `/M`, else `/CreationDate`, as written.
fn date_of(doc: &Document, dict: &Dictionary) -> Option<String> {
    [b"M".as_slice(), b"CreationDate".as_slice()]
        .into_iter()
        .filter_map(|key| text_value(doc, dict, key))
        .map(|date| truncate_chars(date.trim(), MAX_ANNOTATION_DATE_CHARS))
        .find(|date| !date.is_empty())
}

/// `/Contents`, else `/RC` reduced to plain text; `None` when neither
/// carries any.
fn annotation_text(doc: &Document, dict: &Dictionary) -> Option<String> {
    let contents = string_bytes(doc, dict, b"Contents")
        .map(decode_pdf_text_string)
        .map(|text| normalize_text(&text))
        .filter(|text| !text.is_empty());
    let text = contents.or_else(|| {
        rich_text_bytes(doc, dict)
            .map(|bytes| rich_text_to_plain(&decode_pdf_text_string(&bytes)))
            .filter(|text| !text.is_empty())
    })?;
    Some(truncate_chars(&text, MAX_ANNOTATION_TEXT_CHARS))
}

/// The bytes of `/RC`, a text string or a text stream, within
/// [`MAX_RICH_TEXT_BYTES`].
fn rich_text_bytes(doc: &Document, dict: &Dictionary) -> Option<Vec<u8>> {
    let object = dict.get(b"RC").ok()?;
    let (_, object) = doc.dereference(object).ok()?;
    match object {
        Object::String(bytes, _) => Some(bytes[..bytes.len().min(MAX_RICH_TEXT_BYTES)].to_vec()),
        Object::Stream(stream) => stream
            .decompressed_content_with_limit(MAX_RICH_TEXT_BYTES)
            .ok(),
        _ => None,
    }
}

/// Line breaks as `\n` (PDF text often ends lines with `\r`), outer
/// whitespace trimmed.
fn normalize_text(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .trim()
        .to_string()
}

fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => text[..end].to_string(),
        None => text.to_string(),
    }
}

/// `/Rect` as `[x0, y0, x1, y1]` with `x0 <= x1`, `y0 <= y1`; `None` when it
/// is not four finite numbers.
fn rect(doc: &Document, dict: &Dictionary) -> Option<[f32; 4]> {
    let array = resolve_array(doc, dict.get(b"Rect").ok()?)?;
    if array.len() != 4 {
        return None;
    }
    let mut values = [0.0f32; 4];
    for (value, object) in values.iter_mut().zip(array) {
        *value = number(doc, object)?;
    }
    let [ax, ay, bx, by] = values;
    Some([ax.min(bx), ay.min(by), ax.max(bx), ay.max(by)])
}

/// `/QuadPoints` as the box around each quad's four points; a quad with a
/// value that is not a finite number is left out, as is an incomplete
/// trailing one.
fn quads(doc: &Document, dict: &Dictionary) -> Vec<[f32; 4]> {
    let Some(array) = dict
        .get(b"QuadPoints")
        .ok()
        .and_then(|object| resolve_array(doc, object))
    else {
        return Vec::new();
    };
    let (quads, _incomplete) = array.as_chunks::<8>();
    quads
        .iter()
        .filter_map(|chunk| {
            let mut points = [0.0f32; 8];
            for (value, object) in points.iter_mut().zip(chunk) {
                *value = number(doc, object)?;
            }
            let xs = [points[0], points[2], points[4], points[6]];
            let ys = [points[1], points[3], points[5], points[7]];
            Some([
                xs.iter().copied().fold(f32::INFINITY, f32::min),
                ys.iter().copied().fold(f32::INFINITY, f32::min),
                xs.iter().copied().fold(f32::NEG_INFINITY, f32::max),
                ys.iter().copied().fold(f32::NEG_INFINITY, f32::max),
            ])
        })
        .take(MAX_ANNOTATION_QUADS)
        .collect()
}

/// A sortable key for a PDF date (`D:YYYYMMDDHHmmSSOHH'mm'`, every part
/// after the year optional): seconds since 0000-03-01 UTC, roughly. `None`
/// when the date does not start with a four-digit year.
fn pdf_date_key(date: &str) -> Option<i64> {
    let text = date.trim();
    let text = text.strip_prefix("D:").unwrap_or(text);
    let bytes = text.as_bytes();
    let mut position = 0;
    let mut field = |width: usize, default: i64| -> i64 {
        let digits = bytes.get(position..position + width);
        match digits {
            Some(digits) if digits.iter().all(u8::is_ascii_digit) => {
                position += width;
                digits
                    .iter()
                    .fold(0i64, |value, digit| value * 10 + i64::from(digit - b'0'))
            }
            _ => {
                // A missing or broken part ends the date.
                position = bytes.len();
                default
            }
        }
    };
    if !bytes.get(..4)?.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let year = field(4, 0);
    let month = field(2, 1).clamp(1, 12);
    let day = field(2, 1).clamp(1, 31);
    let hour = field(2, 0).min(23);
    let minute = field(2, 0).min(59);
    let second = field(2, 0).min(59);
    let mut offset = 0i64;
    if let Some(&sign) = bytes.get(position) {
        if sign == b'+' || sign == b'-' {
            let rest = &text[position + 1..];
            let digits: Vec<i64> = rest
                .split(|c: char| !c.is_ascii_digit())
                .filter(|part| !part.is_empty())
                .take(2)
                .filter_map(|part| part.parse().ok())
                .collect();
            let minutes = digits.first().copied().unwrap_or(0).min(23) * 60
                + digits.get(1).copied().unwrap_or(0).min(59);
            offset = 60 * if sign == b'+' { minutes } else { -minutes };
        }
    }
    // Days from the civil date (Howard Hinnant's algorithm), shifted so the
    // year starts in March.
    let (y, m) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };
    let days = y * 365 + y / 4 - y / 100 + y / 400 + (153 * m + 2) / 5 + day - 1;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second - offset)
}

/// Reduce XHTML rich text (`/RC`) to plain text: tags dropped, entities
/// decoded, paragraphs and `<br>` as line breaks, other whitespace runs as
/// one space.
fn rich_text_to_plain(markup: &str) -> String {
    let mut out = String::new();
    let mut rest = markup;
    while !rest.is_empty() {
        if let Some(tail) = rest.strip_prefix('<') {
            // Comments, processing instructions and declarations.
            let (end_marker, skip) = if tail.starts_with("!--") {
                ("-->", 3)
            } else {
                (">", 1)
            };
            let Some(end) = tail.find(end_marker) else {
                break;
            };
            let tag = &tail[..end];
            rest = &tail[end + skip..];
            let closing = tag.starts_with('/');
            let tag_name: String = tag
                .trim_start_matches('/')
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == ':')
                .collect::<String>()
                .to_ascii_lowercase();
            let local = tag_name.rsplit(':').next().unwrap_or("");
            let block = matches!(
                local,
                "p" | "div" | "li" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
            );
            if local == "br" || (block && closing) {
                push_break(&mut out);
            } else if block {
                // A block opening after text starts on a new line.
                if !out.is_empty() {
                    push_break(&mut out);
                }
            }
            continue;
        }
        let end = rest.find('<').unwrap_or(rest.len());
        let text = decode_entities(&rest[..end]);
        for c in text.chars() {
            if c.is_whitespace() && c != '\u{a0}' {
                if !out.is_empty() && !out.ends_with([' ', '\n']) {
                    out.push(' ');
                }
            } else {
                out.push(if c == '\u{a0}' { ' ' } else { c });
            }
        }
        rest = &rest[end..];
    }
    let lines: Vec<&str> = out.lines().map(str::trim).collect();
    lines.join("\n").trim().to_string()
}

fn push_break(out: &mut String) {
    while out.ends_with(' ') {
        out.pop();
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 1..];
        let decoded = tail.find(';').filter(|&end| end <= 10).and_then(|end| {
            let entity = &tail[..end];
            let c = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                _ => entity
                    .strip_prefix("#x")
                    .or_else(|| entity.strip_prefix("#X"))
                    .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                    .or_else(|| entity.strip_prefix('#').and_then(|dec| dec.parse().ok()))
                    .and_then(char::from_u32),
            }?;
            Some((c, end))
        });
        match decoded {
            Some((c, end)) => {
                out.push(c);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Rough advance of a character in ems, to place words within a run whose
/// glyph positions were not kept.
fn char_weight(c: char) -> f32 {
    match c {
        'i' | 'j' | 'l' | 'I' | '.' | ',' | ':' | ';' | '\'' | '!' | '|' => 0.25,
        'f' | 't' | 'r' | ' ' | '(' | ')' | '[' | ']' | '-' | '/' => 0.33,
        'm' | 'w' | 'M' | 'W' | 'ш' | 'щ' | 'ж' | 'ю' | 'ы' | 'Ш' | 'Щ' | 'Ж' | 'Ю' | 'Ы' => {
            0.85
        }
        '\u{1100}'..='\u{115F}'
        | '\u{2E80}'..='\u{A4CF}'
        | '\u{AC00}'..='\u{D7A3}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{FF00}'..='\u{FF60}' => 1.0,
        c if c.is_ascii_digit() => 0.556,
        c if c.is_uppercase() => 0.68,
        c if c.is_lowercase() => 0.53,
        _ => 0.6,
    }
}

/// The page text under markup quads, from the text items (raw user space,
/// turned by `rotation`). Words of a run count when their estimated middle
/// lies within a quad's horizontal span and the run's glyph band overlaps
/// the quad's height by half; punctuation at either end that falls outside
/// is trimmed. Runs that are not horizontal in the page's frame are left
/// out. `None` when nothing is under the quads.
fn marked_text(quads: &[[f32; 4]], items: &[&TextItem], rotation: PageRotation) -> Option<String> {
    let mut pieces: Vec<String> = Vec::new();
    for quad in quads {
        let (mut x, mut y, mut width, mut height) =
            (quad[0], quad[1], quad[2] - quad[0], quad[3] - quad[1]);
        rotation.rotate_box(&mut x, &mut y, &mut width, &mut height);
        let (qx0, qy0, qx1, qy1) = (x, y, x + width, y + height);
        // (start x, end x, font size, text) of each run's marked part.
        let mut spans: Vec<(f32, f32, f32, String)> = Vec::new();
        for item in items {
            let angle = item.rotation.rem_euclid(360.0);
            if item.width <= 0.0 || !(angle < 2.0 || angle > 358.0) {
                continue;
            }
            let size = item.height.abs().max(item.font_size.abs());
            if size <= 0.0 {
                continue;
            }
            let (band0, band1) = (item.y - 0.2 * size, item.y + 0.8 * size);
            let overlap = band1.min(qy1) - band0.max(qy0);
            if overlap < 0.5 * (band1 - band0).min(qy1 - qy0).max(0.1) {
                continue;
            }
            if item.x > qx1 || item.x + item.width < qx0 {
                continue;
            }
            if let Some(span) = marked_span(item, qx0, qx1, size) {
                spans.push(span);
            }
        }
        spans.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut line = String::new();
        let mut previous_end: Option<f32> = None;
        for (start, end, size, text) in spans {
            if let Some(previous_end) = previous_end {
                if start - previous_end > 0.2 * size {
                    line.push(' ');
                }
            }
            line.push_str(&text);
            previous_end = Some(end);
        }
        if !line.trim().is_empty() {
            pieces.push(line);
        }
    }
    let joined = pieces.join(" ");
    let collapsed = joined.split_whitespace().collect::<Vec<_>>().join(" ");
    (!collapsed.is_empty()).then(|| truncate_chars(&collapsed, MAX_ANNOTATION_TEXT_CHARS))
}

/// The part of one run under `[qx0, qx1]`: `(start x, end x, size, text)`.
fn marked_span(item: &TextItem, qx0: f32, qx1: f32, size: f32) -> Option<(f32, f32, f32, String)> {
    let chars: Vec<char> = item.text.chars().collect();
    let total: f32 = chars.iter().map(|&c| char_weight(c)).sum();
    if chars.is_empty() || total <= 0.0 {
        return None;
    }
    let scale = item.width / total;
    // Each character's estimated left edge and middle.
    let mut lefts = Vec::with_capacity(chars.len() + 1);
    let mut x = item.x;
    for &c in &chars {
        lefts.push(x);
        x += char_weight(c) * scale;
    }
    lefts.push(x);
    let middle = |i: usize| (lefts[i] + lefts[i + 1]) / 2.0;

    // Words whose middle lies within the quad.
    let mut first: Option<usize> = None;
    let mut last: Option<usize> = None;
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        let word_middle = (lefts[start] + lefts[i]) / 2.0;
        if word_middle >= qx0 && word_middle <= qx1 {
            first.get_or_insert(start);
            last = Some(i);
        }
    }
    let (mut start, mut end) = (first?, last?);
    // Punctuation at the ends that lies outside the quad is not marked.
    while start < end && !chars[start].is_alphanumeric() && middle(start) < qx0 {
        start += 1;
    }
    while end > start && !chars[end - 1].is_alphanumeric() && middle(end - 1) > qx1 {
        end -= 1;
    }
    let text: String = chars[start..end].iter().collect();
    (!text.trim().is_empty()).then(|| (lefts[start], lefts[end], size, text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{dictionary, Stream};

    #[test]
    fn rich_text_keeps_paragraphs_and_decodes_entities() {
        let markup = "<?xml version=\"1.0\"?><body xmlns=\"http://www.w3.org/1999/xhtml\">\
            <p dir=\"ltr\">Due date &amp; <span style=\"x\">penalty</span></p>\
            <p>Срок   оплаты<br/>&#1044;&#x430;</p><!-- <p>no</p> --></body>";
        assert_eq!(
            rich_text_to_plain(markup),
            "Due date & penalty\nСрок оплаты\nДа"
        );
    }

    #[test]
    fn rich_text_survives_broken_markup() {
        assert_eq!(
            rich_text_to_plain("a &bogus; b <p unterminated"),
            "a &bogus; b"
        );
        assert_eq!(rich_text_to_plain("&#xFFFFFFFF; x"), "&#xFFFFFFFF; x");
        assert_eq!(rich_text_to_plain(""), "");
    }

    #[test]
    fn pdf_dates_sort_by_their_instant() {
        let utc = pdf_date_key("D:20261118030000Z").unwrap();
        let plus_seven = pdf_date_key("D:20261118100000+07'00'").unwrap();
        assert_eq!(utc, plus_seven);
        assert!(pdf_date_key("D:2026").unwrap() < pdf_date_key("D:202602").unwrap());
        assert!(pdf_date_key("D:20261231").unwrap() < pdf_date_key("D:20270101").unwrap());
        assert_eq!(pdf_date_key("yesterday"), None);
        assert_eq!(pdf_date_key(""), None);
    }

    #[test]
    fn truncation_cuts_at_a_character_boundary() {
        assert_eq!(truncate_chars("абв", 2), "аб");
        assert_eq!(truncate_chars("ab", 5), "ab");
    }

    fn text_item(text: &str, x: f32, y: f32, width: f32) -> TextItem {
        TextItem {
            text: text.to_string(),
            x,
            y,
            width,
            height: 12.0,
            font: String::new(),
            font_tag: String::new(),
            legacy_symbol_rewrite: false,
            font_size: 12.0,
            page: 1,
            is_bold: false,
            is_italic: false,
            font_weight: None,
            bold_source: None,
            fixed_pitch: None,
            fill_color: None,
            stroke_color: None,
            render_mode: None,
            is_underline: false,
            is_strikeout: false,
            layout_strikeout: false,
            rotation: 0.0,
            advance_known: true,
            item_type: ItemType::Text,
            mcid: None,
            baseline_shift: 0.0,
        }
    }

    #[test]
    fn marked_text_takes_the_words_under_the_quad() {
        // "The goods within thirty days." set at 6 pt per weight unit.
        let text = "within thirty days.";
        let total: f32 = text.chars().map(char_weight).sum();
        let item = text_item(text, 100.0, 700.0, total * 6.0);
        let start = 100.0 + "within ".chars().map(char_weight).sum::<f32>() * 6.0;
        let end = start + "thirty days".chars().map(char_weight).sum::<f32>() * 6.0;
        let quad = [start, 697.0, end, 710.0];
        assert_eq!(
            marked_text(&[quad], &[&item], PageRotation::Upright).as_deref(),
            Some("thirty days")
        );
        // A quad on another line marks nothing.
        let elsewhere = [start, 650.0, end, 663.0];
        assert_eq!(
            marked_text(&[elsewhere], &[&item], PageRotation::Upright),
            None
        );
    }

    #[test]
    fn marked_text_joins_runs_and_quads() {
        let first = text_item("con", 100.0, 700.0, 18.0);
        let second = text_item("tract", 118.2, 700.0, 25.0);
        let next_line = text_item("terms apply", 100.0, 680.0, 60.0);
        let quads = [[99.0, 697.0, 144.0, 710.0], [99.0, 677.0, 161.0, 690.0]];
        assert_eq!(
            marked_text(
                &quads,
                &[&first, &second, &next_line],
                PageRotation::Upright
            )
            .as_deref(),
            Some("contract terms apply")
        );
    }

    fn doc_with_annotations(annots: Vec<Object>) -> (Document, ObjectId) {
        let mut doc = Document::with_version("1.7");
        let pages_id = doc.new_object_id();
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
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
        (doc, page_id)
    }

    fn note(contents: &str) -> Dictionary {
        dictionary! {
            "Type" => "Annot",
            "Subtype" => "Text",
            "Rect" => vec![10.into(), 20.into(), 30.into(), 40.into()],
            "Contents" => Object::string_literal(contents),
        }
    }

    fn collect(doc: &Document, page_id: ObjectId) -> Vec<PdfAnnotation> {
        let mut collector = Collector::new();
        collector.add_page(doc, 1, page_id);
        collector.finish(&[], &HashMap::new())
    }

    #[test]
    fn malformed_annotations_are_skipped() {
        let mut bad_rect = note("bad rect");
        bad_rect.set(
            "Rect",
            vec![Object::Real(f32::NAN), 0.into(), 1.into(), 1.into()],
        );
        let mut short_rect = note("short rect");
        short_rect.set("Rect", vec![0.into(), 0.into()]);
        let mut no_subtype = note("no subtype");
        no_subtype.remove(b"Subtype");
        let mut numeric_contents = note("");
        numeric_contents.set("Contents", 7);
        let annots = vec![
            Object::Dictionary(bad_rect),
            Object::Dictionary(short_rect),
            Object::Dictionary(no_subtype),
            Object::Dictionary(numeric_contents),
            Object::Reference((999, 0)),
            Object::Integer(3),
            Object::Dictionary(note("kept")),
        ];
        let (doc, page_id) = doc_with_annotations(annots);
        let found = collect(&doc, page_id);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].text, "kept");
        assert_eq!(
            found[0].rect,
            AnnotationRect {
                x: 10.0,
                y: 20.0,
                width: 20.0,
                height: 20.0
            }
        );
    }

    #[test]
    fn reply_cycles_are_cut() {
        let (mut doc, page_id) = doc_with_annotations(Vec::new());
        let ids: Vec<ObjectId> = (0..3).map(|_| doc.new_object_id()).collect();
        // 0 replies to itself; 1 and 2 reply to each other.
        for (index, parent) in [(0, 0), (1, 2), (2, 1)] {
            let mut annotation = note(&format!("n{index}"));
            annotation.set("IRT", ids[parent]);
            doc.objects
                .insert(ids[index], Object::Dictionary(annotation));
        }
        let annots: Vec<Object> = ids.iter().map(|&id| id.into()).collect();
        doc.get_dictionary_mut(page_id)
            .unwrap()
            .set("Annots", annots);
        let found = collect(&doc, page_id);
        let links: Vec<Option<usize>> = found.iter().map(|a| a.in_reply_to).collect();
        assert_eq!(links, vec![None, Some(2), None]);
    }

    #[test]
    fn long_text_is_cut_and_the_count_is_bounded() {
        let long = "x".repeat(MAX_ANNOTATION_TEXT_CHARS + 10);
        let mut annots = vec![Object::Dictionary(note(&long))];
        annots.extend((0..MAX_ANNOTATIONS + 5).map(|_| Object::Dictionary(note("n"))));
        let (doc, page_id) = doc_with_annotations(annots);
        let found = collect(&doc, page_id);
        assert_eq!(found.len(), MAX_ANNOTATIONS);
        assert_eq!(found[0].text.chars().count(), MAX_ANNOTATION_TEXT_CHARS);
    }

    #[test]
    fn a_rich_text_stream_over_the_bound_is_not_read() {
        let mut doc_annots = Vec::new();
        let (mut doc, page_id) = doc_with_annotations(Vec::new());
        let small = doc.add_object(Stream::new(dictionary! {}, b"<p>small</p>".to_vec()));
        let big = doc.add_object(Stream::new(
            dictionary! {},
            vec![b'a'; MAX_RICH_TEXT_BYTES + 1],
        ));
        for (rc, label) in [(small, "small"), (big, "big")] {
            let mut annotation = note("");
            annotation.remove(b"Contents");
            annotation.set("RC", rc);
            annotation.set("T", Object::string_literal(label));
            doc_annots.push(Object::Dictionary(annotation));
        }
        doc.get_dictionary_mut(page_id)
            .unwrap()
            .set("Annots", doc_annots);
        let found = collect(&doc, page_id);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].text, "small");
    }
}
