//! Page-furniture stripping for short documents.
//!
//! The document-wide classifier in [`strip_repeated_lines`]
//! proves running headers and footers by cross-page repetition, which needs
//! three-plus pages. This module holds the short-document policy: per-page
//! positional and typographic evidence, plus the dispatcher that picks the
//! right policy for a document.

use std::collections::{HashMap, HashSet};

use crate::text_utils::{ends_with_identifier_label, MAX_UNNUMBERED_FRONT_PAGES};
use crate::types::TextLine;

/// Strip page-edge furniture on documents too short for repetition evidence.
///
/// [`strip_repeated_lines`] proves furniture by cross-page repetition and
/// needs three-plus pages; on shorter documents running headers, footer
/// credits, and page indicators survive untouched. This pass uses positional
/// and typographic evidence instead, and only strips where that evidence is
/// strong; anything ambiguous stays:
///
/// - A page needs enough body to judge (more than 8 lines). One block per edge is
///   considered: the run of up to 2 lines from the page edge at normal
///   leading. Runs of 3+ lines are body-sized content and stay whole.
/// - Isolation: the edge block must sit a clear gap (1.8x the page's median
///   leading) from the next line inward. A footnote block protects itself —
///   its outermost line is at normal leading from the rest of the block,
///   pulling the run past the size cap or the gap inside it.
/// - A block set larger than body text is a display title and stays, as
///   does anything past it.
/// - Blocks with no alphabetic content strip only on the full page-indicator
///   shape: a single short run set no larger than body text ("1 / 1", folio
///   ornaments). Multi-cell numeric rows are form or table data and stay.
/// - Marker-led blocks stay: digit or (digit) enumerations, symbol-marked
///   footnotes ("† ..."), and single-lowercase-letter markers ("a See ...")
///   are annotations that merely sit at the page edge.
/// - Rows of three-plus item clusters stay: figure label rows and table rows
///   share a baseline but are not one text run.
/// - The block's total text must be short (<= 90 chars) and non-structural.
fn strip_edge_furniture(lines: Vec<TextLine>) -> Vec<TextLine> {
    const MIN_PAGE_LINES: usize = 8;
    const MAX_EDGE_BLOCK_LINES: usize = 2;
    const ISOLATION_FACTOR: f32 = 1.8;
    const MAX_FURNITURE_CHARS: usize = 90;

    let mut pages: HashMap<u32, Vec<usize>> = HashMap::new();
    for (idx, line) in lines.iter().enumerate() {
        pages.entry(line.page).or_default().push(idx);
    }

    let mut removal: HashSet<usize> = HashSet::new();
    for indices in pages.values_mut() {
        indices.sort_by(|&a, &b| lines[b].y.total_cmp(&lines[a].y));
        // Strictly more than the floor: on a page at the minimum, the two
        // edge blocks could remove up to half the content.
        if indices.len() <= MIN_PAGE_LINES {
            continue;
        }

        // Char-weighted median font size = the body text size.
        let mut sizes: Vec<(f32, usize)> = Vec::new();
        for &i in indices.iter() {
            for item in &lines[i].items {
                sizes.push((item.font_size, item.text.chars().count()));
            }
        }
        sizes.sort_by(|a, b| a.0.total_cmp(&b.0));
        let total_chars: usize = sizes.iter().map(|&(_, c)| c).sum();
        let mut running = 0usize;
        let mut body_size = 12.0f32;
        for &(size, chars) in &sizes {
            running += chars;
            if running * 2 >= total_chars {
                body_size = size;
                break;
            }
        }

        let mut steps: Vec<f32> = indices
            .windows(2)
            .map(|w| lines[w[0]].y - lines[w[1]].y)
            .filter(|d| *d > 1.0)
            .collect();
        steps.sort_by(|a, b| a.total_cmp(b));
        let median_leading = steps.get(steps.len() / 2).copied().unwrap_or(12.0);
        let isolation = median_leading * ISOLATION_FACTOR;

        let line_font = |i: usize| -> f32 {
            lines[i]
                .items
                .iter()
                .map(|it| it.font_size)
                .fold(0.0, f32::max)
        };

        // Judge one edge. `ordered` lists the page's lines from that edge
        // inward; returns the edge block's indices when it is furniture.
        let judge_edge = |ordered: &[usize]| -> Option<Vec<usize>> {
            // The edge block: run of lines at normal leading from the edge.
            let mut block_len = 1usize;
            while block_len < ordered.len()
                && block_len <= MAX_EDGE_BLOCK_LINES
                && (lines[ordered[block_len - 1]].y - lines[ordered[block_len]].y).abs() < isolation
            {
                block_len += 1;
            }
            if block_len > MAX_EDGE_BLOCK_LINES || block_len >= ordered.len() {
                return None; // body-sized run, or no inward line to isolate from
            }
            let block = &ordered[..block_len];
            let inward = ordered[block_len];
            let gap = (lines[block[block_len - 1]].y - lines[inward].y).abs();
            if gap < isolation {
                return None;
            }
            let text: String = block
                .iter()
                .map(|&i| lines[i].text())
                .collect::<Vec<_>>()
                .join(" ");
            let trimmed = text.trim();
            if trimmed.is_empty() || trimmed.chars().count() > MAX_FURNITURE_CHARS {
                return None;
            }
            // A line of three-plus item clusters separated by box-sized
            // gaps is content sharing a baseline — a figure's label row,
            // or a numeric table row on an undetected borderless table —
            // never furniture. Real furniture is one text run, or two at
            // most (running title on one margin, folio on the other).
            let mut max_clusters = 1usize;
            for &i in block {
                let line = &lines[i];
                let mut xs: Vec<(f32, f32)> = line
                    .items
                    .iter()
                    .map(|it| (it.x, it.x + it.width))
                    .collect();
                xs.sort_by(|a, b| a.0.total_cmp(&b.0));
                let cluster_gap = line_font(i).max(6.0) * 2.0;
                let mut clusters = 1usize;
                let mut run_right = xs.first().map(|&(_, r)| r).unwrap_or(0.0);
                for &(left, right) in xs.iter().skip(1) {
                    if left - run_right > cluster_gap {
                        clusters += 1;
                    }
                    run_right = run_right.max(right);
                }
                if clusters >= 3 {
                    return None;
                }
                max_clusters = max_clusters.max(clusters);
            }
            let has_alpha = trimmed.chars().any(|c| c.is_alphabetic());
            if !has_alpha {
                // A number-only block strips only on the full page-indicator
                // shape: one text run, a handful of characters, set no
                // larger than body text. Multi-cell numeric rows are form or
                // table data, and number-only display content (a cover-page
                // year) is set well above body size.
                if max_clusters > 1
                    || trimmed.chars().count() > 20
                    || block.iter().any(|&i| line_font(i) > body_size * 1.2)
                {
                    return None;
                }
                return Some(block.to_vec());
            }
            // Textual furniture must be set strictly smaller than body text.
            // Same-size edge blocks are content that merely sits at the
            // margin: section headings above their heading gap, affiliation
            // blocks on title pages, continuation paragraphs.
            if block.iter().any(|&i| line_font(i) >= body_size * 0.98) {
                return None;
            }
            if is_structural_line(trimmed) {
                return None;
            }
            // Marker-led edge blocks are annotations, not furniture:
            // digit- or (digit)-led enumerations (footnotes at the bottom,
            // list/legal continuations at the top), symbol-marked footnotes
            // ("† Estimates assume ..."), and single-lowercase-letter
            // markers ("a See the appendix ..."). A capitalized one-letter
            // word ("A Publication of ...") is prose, not a marker.
            let first = trimmed.chars().next();
            let marker_led = first.is_some_and(|c| c.is_numeric())
                || starts_with_paren_marker(trimmed)
                || first.is_some_and(|c| matches!(c, '†' | '‡' | '§' | '¶' | '*'))
                || trimmed.split_whitespace().next().is_some_and(|tok| {
                    tok.chars().count() == 1 && tok.chars().all(|c| c.is_lowercase())
                });
            if marker_led {
                return None;
            }
            Some(block.to_vec())
        };

        if let Some(block) = judge_edge(indices) {
            removal.extend(block);
        }
        let from_bottom: Vec<usize> = indices.iter().rev().copied().collect();
        if let Some(block) = judge_edge(&from_bottom) {
            removal.extend(block);
        }
    }

    if removal.is_empty() {
        return lines;
    }
    lines
        .into_iter()
        .enumerate()
        .filter_map(|(idx, line)| (!removal.contains(&idx)).then_some(line))
        .collect()
}

/// Strip running headers and footers, choosing evidence by document length:
/// three-plus pages prove furniture by cross-page repetition
/// ([`strip_repeated_lines`]); shorter documents carry no repetition signal
/// and fall back to per-page positional and typographic evidence
/// ([`strip_edge_furniture`]).
#[cfg(test)]
pub(crate) fn strip_header_footer_lines(lines: Vec<TextLine>, page_count: u32) -> Vec<TextLine> {
    strip_header_footer_lines_in_frames(lines, page_count, &HashMap::new())
}

/// [`strip_header_footer_lines`] with each page's frame, its visible
/// `(bottom, top)` in the lines' coordinates where known (see
/// [`strip_repeated_lines_in_frames`]).
pub(crate) fn strip_header_footer_lines_in_frames(
    lines: Vec<TextLine>,
    page_count: u32,
    page_frames: &HashMap<u32, (f32, f32)>,
) -> Vec<TextLine> {
    if lines.is_empty() {
        return lines;
    }
    if page_count < 3 {
        strip_edge_furniture(lines)
    } else {
        strip_repeated_lines_in_frames(lines, page_count, page_frames)
    }
}

/// Normalize whitespace in a string for comparison: trim and collapse internal runs of whitespace.
fn normalize_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Stands in for a page number in a comparison key.
const FOLIO: char = '\u{E000}';

/// A decimal digit's value, in the scripts page numbers are set in: ASCII,
/// Arabic-Indic, Persian, Devanagari and the other Indic scripts, Thai, Lao,
/// Tibetan, Myanmar, Khmer, Mongolian, and full-width forms.
fn decimal_digit_value(c: char) -> Option<u32> {
    if let Some(digit) = c.to_digit(10) {
        return Some(digit);
    }
    const ZEROS: [u32; 19] = [
        0x0660, 0x06F0, 0x07C0, 0x0966, 0x09E6, 0x0A66, 0x0AE6, 0x0B66, 0x0BE6, 0x0C66, 0x0CE6,
        0x0D66, 0x0E50, 0x0ED0, 0x0F20, 0x1040, 0x17E0, 0x1810, 0xFF10,
    ];
    let code = c as u32;
    ZEROS
        .iter()
        .find(|&&zero| (zero..zero + 10).contains(&code))
        .map(|&zero| code - zero)
}

/// The row's page-number slot: its whitespace-collapsed text with the
/// standalone number of up to four digits that opens (`leading`) or closes
/// it replaced by [`FOLIO`], and that number. A number joined to other
/// characters ("EDU-401", "502A", "2-123/2026") is part of the text, and so
/// is a closing number labelled as an identifier ("Invoice No. 3", "Дело №
/// 12", "Приложение 4"): a document number is no page number, even when it
/// happens to keep step with the pages.
fn folio_slot(collapsed: &str, leading: bool) -> Option<(String, u32)> {
    let (rest, token) = if leading {
        let (token, rest) = collapsed.split_once(' ')?;
        (rest, token)
    } else {
        let (rest, token) = collapsed.rsplit_once(' ')?;
        (rest, token)
    };
    if token.chars().count() > 4 {
        return None;
    }
    let mut value = 0u32;
    for c in token.chars() {
        value = value * 10 + decimal_digit_value(c)?;
    }
    if !leading && ends_with_identifier_label(rest) {
        return None;
    }
    let template = if leading {
        format!("{FOLIO} {rest}")
    } else {
        format!("{rest} {FOLIO}")
    };
    Some((template, value))
}

/// Comparison keys for repetition. Two rows repeat only when their text is
/// the same, letters, numbers, punctuation and case included: "Case 2-123"
/// and "Case 2-124" are different rows. Whitespace is no part of the key,
/// since OCR splits and joins words inconsistently ("Confi dential"). The
/// one tolerance is a plausible page number: a standalone number of up to
/// four digits opening or closing the row, not labelled as an identifier
/// (see [`folio_slot`]), that equals the page index or trails it by the same
/// few pages of front matter on every page that shows the row (see
/// [`MAX_UNNUMBERED_FRONT_PAGES`]). So "Annual Report 5" on page 5 and
/// "Annual Report 6" on page 6 share a key, and so do "Report 3" and "Report
/// 4" on pages 5 and 6 after a cover and a contents page; "Invoice 4171" to
/// "Invoice 4174" on pages 1 to 4 do not.
struct ComparisonKeys {
    /// Learned page-number template -> its printed number minus the page.
    folio_templates: HashMap<String, i64>,
}

impl ComparisonKeys {
    /// Learns the page-number templates from the edge rows that could be
    /// furniture, as (text, page) pairs.
    fn learn<'a>(rows: impl IntoIterator<Item = (&'a str, u32)>) -> Self {
        let mut slots: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
        for (text, page) in rows {
            let collapsed = normalize_whitespace(text);
            for leading in [false, true] {
                if let Some((template, value)) = folio_slot(&collapsed, leading) {
                    slots
                        .entry(without_whitespace(&template))
                        .or_default()
                        .push((page, value));
                }
            }
        }
        let folio_templates = slots
            .into_iter()
            .filter(|(template, occurrences)| {
                let offset = |&(page, value): &(u32, u32)| i64::from(value) - i64::from(page);
                let first = offset(&occurrences[0]);
                let pages: HashSet<u32> = occurrences.iter().map(|&(page, _)| page).collect();
                let in_step = pages.len() >= 2 && occurrences.iter().all(|o| offset(o) == first);
                let plausible = (-MAX_UNNUMBERED_FRONT_PAGES..=0).contains(&first)
                    && occurrences.iter().all(|&(_, value)| value >= 1);
                if in_step && !plausible {
                    log::debug!(
                        "number in step with the pages but no page number (offset {first}): {}",
                        template.replace(FOLIO, "#")
                    );
                }
                in_step && plausible
            })
            .map(|(template, occurrences)| {
                let (page, value) = occurrences[0];
                (template, i64::from(value) - i64::from(page))
            })
            .collect();
        Self { folio_templates }
    }

    /// The key of the row on `page`, and its length for the length floor:
    /// the whitespace-collapsed text's characters, its page-number slot
    /// aside. The slot is a page number only when it keeps the learned step
    /// on this page too: a page that taught nothing (one too sparse to have
    /// edge rows) cannot pass "Annual Report 4174" off as page 4.
    fn key(&self, text: &str, page: u32) -> (String, usize) {
        let collapsed = normalize_whitespace(text);
        // Templates are learned and matched without whitespace, the key's
        // own form, so spellings that differ only in spacing share one
        // page-number check.
        for leading in [false, true] {
            if let Some((template, value)) = folio_slot(&collapsed, leading) {
                let key = without_whitespace(&template);
                let offset = i64::from(value) - i64::from(page);
                if self.folio_templates.get(&key) == Some(&offset) {
                    let chars = template.replace(FOLIO, "").trim().chars().count();
                    return (key, chars);
                }
            }
        }
        (without_whitespace(&collapsed), collapsed.chars().count())
    }
}

fn without_whitespace(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Whether `text` is nothing but a plausible page number for `page`: a
/// number of up to four digits that equals the page index or trails it by
/// up to [`MAX_UNNUMBERED_FRONT_PAGES`].
fn is_bare_page_number(text: &str, page: u32) -> bool {
    let token = text.trim();
    if token.is_empty() || token.chars().count() > 4 {
        return false;
    }
    let mut value = 0u32;
    for c in token.chars() {
        match decimal_digit_value(c) {
            Some(digit) => value = value * 10 + digit,
            None => return false,
        }
    }
    value >= 1 && (-MAX_UNNUMBERED_FRONT_PAGES..=0).contains(&(i64::from(value) - i64::from(page)))
}

/// A parenthesized annotation marker: a short (≤3 chars) token of digits or
/// lowercase letters closed by ')' — "(1)", "(12)", "(a)", "(iv)", "(١)".
/// Parenthetical prose ("(all amounts in thousands)") is not a marker.
fn starts_with_paren_marker(text: &str) -> bool {
    text.strip_prefix('(').is_some_and(|rest| {
        let marker: String = rest.chars().take_while(|c| c.is_alphanumeric()).collect();
        !marker.is_empty()
            && marker.chars().count() <= 3
            && marker.chars().all(|c| c.is_numeric() || c.is_lowercase())
            && rest[marker.len()..].starts_with(')')
    })
}

/// Returns true if the line looks like a list item, heading, or marker-led
/// annotation (should not be stripped).
fn is_structural_line(text: &str) -> bool {
    let t = text.trim_start();
    let numbered =
        t.chars().next().is_some_and(|c| c.is_numeric()) && (t.contains(". ") || t.contains(") "));
    t.starts_with('#')
        || t.starts_with("- ")
        || t.starts_with("* ")
        || t.starts_with("• ")
        || numbered
        || starts_with_paren_marker(t)
}

/// Returns true if a line consists entirely of a single repeated character
/// (e.g., "----------", "**************", "============").
fn is_decorative_separator(text: &str) -> bool {
    let mut chars = text.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => return false,
    };
    chars.all(|c| c == first)
}

/// A Y-band's members in document order plus the row's coalesced text.
/// Single source for band coalescing: the frequency build and both band
/// passes must key on identical text.
fn coalesced_band(lines: &[TextLine], indices: &[usize]) -> (Vec<usize>, String) {
    let mut sorted = indices.to_vec();
    sorted.sort();
    let coalesced: String = sorted
        .iter()
        .map(|&i| lines[i].text())
        .collect::<Vec<_>>()
        .join(" ");
    (sorted, coalesced)
}

/// Strip lines that repeat on many distinct pages (running headers/footers).
///
/// A line is considered a repeated header/footer if:
/// 1. Its comparison key appears on `>= max(3, ceil(page_count * 30%))` distinct pages
/// 2. It is at least 10 characters long
/// 3. It doesn't look like a structural element (heading, list item)
/// 4. It consistently appears in the top or bottom N distinct Y positions
/// 5. Its Y positions across pages have low variance (consistent placement),
///    distinguishing true headers/footers from table content that happens to
///    land near page margins
/// 6. It is not a decorative separator (repeated single character)
///
/// Additionally, TextLines at the same Y position on a page are grouped into
/// "Y-bands." When any member of a Y-band is stripped, all siblings in that
/// band are also stripped. This handles split column headers where individual
/// fragments may not independently meet the frequency threshold.
///
/// Rows compare as exact text, numbers included (see [`ComparisonKeys`]):
/// "Case No. 2-123" does not repeat "Case No. 2-124". A plausible page number
/// opening or closing the row is the one tolerance, so headers like "Chapter
/// 3 — Page 5" and "Chapter 3 — Page 6" are the same row. The
/// first page that shows a repeated row keeps it.
#[cfg(test)]
pub(crate) fn strip_repeated_lines(lines: Vec<TextLine>, page_count: u32) -> Vec<TextLine> {
    strip_repeated_lines_in_frames(lines, page_count, &HashMap::new())
}

/// [`strip_repeated_lines`], with each page's frame where known: its visible
/// `(bottom, top)` in the lines' coordinates. Only a sparse page's position
/// check reads it. A page with one or two rows has no margin of its own, so
/// a row proven on the full pages goes there only at the place the full
/// pages show it: as far from the top edge (a header) or the bottom edge (a
/// footer), on pages of any size, when every page involved has a frame, and
/// at the same height otherwise; and only once, the row closest to that
/// place, so the same sentence a little further in is body text that stays.
pub(crate) fn strip_repeated_lines_in_frames(
    lines: Vec<TextLine>,
    page_count: u32,
    page_frames: &HashMap<u32, (f32, f32)>,
) -> Vec<TextLine> {
    if lines.is_empty() || page_count < 3 {
        return lines;
    }

    // Compute Y range per page (min_y, max_y)
    let mut page_y_range: HashMap<u32, (f32, f32)> = HashMap::new();
    for line in &lines {
        let entry = page_y_range.entry(line.page).or_insert((line.y, line.y));
        if line.y < entry.0 {
            entry.0 = line.y;
        }
        if line.y > entry.1 {
            entry.1 = line.y;
        }
    }

    // Build sorted Y values per page, so we can check line rank (position from edge)
    let mut page_sorted_ys: HashMap<u32, Vec<f32>> = HashMap::new();
    for line in &lines {
        page_sorted_ys.entry(line.page).or_default().push(line.y);
    }
    for ys in page_sorted_ys.values_mut() {
        ys.sort_by(|a, b| a.total_cmp(b));
        ys.dedup();
    }

    // A line is in the page margin if it's among the first or last N distinct
    // Y positions on that page. This is more robust than a percentage-based zone
    // because it catches actual edge lines regardless of how much content fills
    // the page. N=5 accommodates multi-line headers/footers and repeated form
    // column headers (e.g., 5-row IRS form headers) that sit just inside the
    // page margin.
    const EDGE_LINE_COUNT: usize = 5;

    /// One representative baseline per row. Column mates within 0.1pt share
    /// a row; that is the same tolerance the edge match uses.
    fn cluster_rows(ys: &[f32]) -> Vec<f32> {
        let mut rows = Vec::new();
        for &y in ys {
            match rows.last() {
                Some(&last) if y - last < 0.1 => {}
                _ => rows.push(y),
            }
        }
        rows
    }

    let page_rows: HashMap<u32, Vec<f32>> = page_sorted_ys
        .iter()
        .map(|(&page, ys)| (page, cluster_rows(ys)))
        .collect();

    /// Returns true if the given Y position is among the first or last N
    /// clustered rows on the specified page. `page_rows` is computed once.
    fn is_y_at_edge(y: f32, page: u32, page_rows: &HashMap<u32, Vec<f32>>, n: usize) -> bool {
        let rows = match page_rows.get(&page) {
            Some(rows) => rows,
            None => return false,
        };
        let count = rows.len();
        // One or two rows have no interior, so they are not a margin.
        if count <= 2 {
            return false;
        }
        // On a short page the full N-row zones meet and would mark the body
        // as furniture. Keep only the outermost row, which is still enough
        // for a running header or footer, and grow the zone one row per row
        // past 2 * N so it never jumps to cover nearly the whole page.
        let edge = count.saturating_sub(n * 2).clamp(1, n);
        let pos = match rows.iter().position(|&py| (py - y).abs() < 0.1) {
            Some(p) => p,
            None => return false,
        };
        pos < edge || pos + edge >= count
    }

    // Average page span for normalizing Y variance
    let avg_span = {
        let total: f32 = page_y_range.values().map(|(lo, hi)| hi - lo).sum();
        if page_y_range.is_empty() {
            1.0
        } else {
            (total / page_y_range.len() as f32).max(1.0)
        }
    };

    // The span of the pages that have a margin, for a sparse page's position
    // check: a sparse page must not move its own tolerance.
    let full_page_span = {
        let spans: Vec<f32> = page_y_range
            .iter()
            .filter(|(page, _)| page_rows.get(page).is_some_and(|rows| rows.len() > 2))
            .map(|(_, (lo, hi))| hi - lo)
            .collect();
        if spans.is_empty() {
            avg_span
        } else {
            (spans.iter().sum::<f32>() / spans.len() as f32).max(1.0)
        }
    };

    // Build Y-bands: group line indices by (page, quantized_y).
    // Lines at the same Y position (within ~0.1pt) on the same page form a band.
    let mut y_bands: HashMap<(u32, i32), Vec<usize>> = HashMap::new();
    for (idx, line) in lines.iter().enumerate() {
        let y_bucket = (line.y * 10.0).round() as i32;
        y_bands.entry((line.page, y_bucket)).or_default().push(idx);
    }

    // A band with a structural member is one physical row of content: the
    // whole row is protected, because removing only the plain fragments
    // would leave a mangled half-row. Every removal path below honors this.
    // Protection covers the neighboring buckets too: same-row fragments can
    // straddle a rounding boundary (y 100.04 and 100.06 land in different
    // buckets), and a boundary must never split a row's protection.
    let mut protected_bands: HashSet<(u32, i32)> = HashSet::new();
    for (&(page, bucket), indices) in &y_bands {
        if indices
            .iter()
            .any(|&idx| is_structural_line(lines[idx].text().trim()))
        {
            protected_bands.insert((page, bucket - 1));
            protected_bands.insert((page, bucket));
            protected_bands.insert((page, bucket + 1));
        }
    }
    let band_key = |line: &TextLine| -> (u32, i32) { (line.page, (line.y * 10.0).round() as i32) };

    // Edge rows that could be furniture: single lines, and the coalesced
    // text of multi-member Y-bands. The latter catches split column headers
    // where individual fragments don't meet the frequency threshold but the
    // combined row does.
    let mut edge_lines: Vec<(String, u32, f32)> = Vec::new();
    for line in &lines {
        if !is_y_at_edge(line.y, line.page, &page_rows, EDGE_LINE_COUNT) {
            continue;
        }
        let text = line.text();
        // Structural checks see the original text, before any key is made.
        if is_structural_line(text.trim()) {
            continue;
        }
        edge_lines.push((text, line.page, line.y));
    }
    let mut edge_bands: Vec<(String, u32, f32)> = Vec::new();
    for (&(page, _), indices) in &y_bands {
        if indices.len() < 2 {
            continue; // single-line bands are already in the individual map
        }
        let band_y = lines[indices[0]].y;
        if !is_y_at_edge(band_y, page, &page_rows, EDGE_LINE_COUNT) {
            continue;
        }
        let (_, coalesced) = coalesced_band(&lines, indices);
        if is_structural_line(coalesced.trim()) {
            continue;
        }
        edge_bands.push((coalesced, page, band_y));
    }
    let keys = ComparisonKeys::learn(
        edge_lines
            .iter()
            .chain(&edge_bands)
            .map(|(text, page, _)| (text.as_str(), *page)),
    );

    // Frequency maps: comparison key -> distinct pages and Y positions.
    let frequencies = |rows: &[(String, u32, f32)]| {
        let mut freq: HashMap<String, HashSet<u32>> = HashMap::new();
        let mut y_positions: HashMap<String, Vec<f32>> = HashMap::new();
        let mut placements: HashMap<String, Vec<(u32, f32)>> = HashMap::new();
        for (text, page, y) in rows {
            let (key, chars) = keys.key(text, *page);
            if chars < 10 || is_decorative_separator(&key) {
                continue;
            }
            freq.entry(key.clone()).or_default().insert(*page);
            y_positions.entry(key.clone()).or_default().push(*y);
            placements.entry(key).or_default().push((*page, *y));
        }
        (freq, y_positions, placements)
    };
    let (freq, y_positions, placements) = frequencies(&edge_lines);
    let (band_freq, band_y_positions, _) = frequencies(&edge_bands);

    // At least 3 pages and at least 30% of them, rounded up: on 11 pages,
    // 3 pages are 27% and too few.
    let threshold = 3u32.max(page_count.saturating_mul(30).div_ceil(100));

    // Check Y-position consistency: headers/footers appear at the same position
    // on every page, table content varies. Require normalized stddev < 5% of
    // average page span.
    const POSITION_TOLERANCE: f32 = 0.05;
    let has_consistent_y = |text: &str, positions: &HashMap<String, Vec<f32>>| -> bool {
        let pos = match positions.get(text) {
            Some(p) if p.len() >= 2 => p,
            _ => return true, // single occurrence — allow
        };
        let n = pos.len() as f32;
        let mean = pos.iter().sum::<f32>() / n;
        let variance = pos.iter().map(|y| (y - mean).powi(2)).sum::<f32>() / n;
        let stddev = variance.sqrt();
        stddev / avg_span < POSITION_TOLERANCE
    };
    // The cutoff is compared in f32, as every other position check is, so a
    // row exactly at 5% stays as it always has.
    let within_tolerance =
        |distance: f64| (distance.abs() as f32) / full_page_span < POSITION_TOLERANCE;
    // The middle of the full pages' rows: a sparse page whose frame is
    // unknown has a header's place above it and a footer's below.
    let full_page_middle: f64 = {
        let middles: Vec<f64> = page_y_range
            .iter()
            .filter(|(page, _)| page_rows.get(page).is_some_and(|rows| rows.len() > 2))
            .map(|(_, &(lo, hi))| (f64::from(lo) + f64::from(hi)) / 2.0)
            .collect();
        if middles.is_empty() {
            0.0
        } else {
            middles.iter().sum::<f64>() / middles.len() as f64
        }
    };
    // Where the full pages show the repeated row `key`, projected onto
    // `page`: `None` when they show no place a sparse page can use. A sparse
    // page has no edge rows to judge by, so this is its only position check:
    // a row there goes only within the tolerance of this place. The full
    // pages' places agree when every one is within the same share of their
    // span of the places' mean; a sparse row then matches within that share
    // of the mean, so it is always that close to a place some full page
    // uses. When the places do not agree, a sparse page loses nothing.
    //
    // With frames, a place is the distance from the edge the full pages keep
    // the row at: the top or the bottom, whichever they agree on. When they
    // agree on both, the one with clearly the steadier distance (a range of
    // at most half the other's) wins, else the nearer one, the bottom when
    // both are as near (a row exactly halfway down, as before M17): Letter
    // pages and an A4 page with the row at one height agree on the top
    // loosely but on the bottom exactly. Full pages of mixed sizes
    // with the row at one height may agree on neither, or on the bottom
    // only: a distance from the top averaged over them is a place no page
    // uses. The full pages whose frames are known decide the edge; it takes
    // two of them, since one page agrees with itself on both edges, and with
    // only one a sparse page loses nothing. Without the sparse page's own
    // frame, or without any full page's, the height. The statistics are in
    // f64, and the ranges compared are exact, so rounding cannot make one
    // edge look steadier than the other. The learned place, projected onto
    // the sparse page (or the learned height), must lie within its frame
    // when that is known. A row in a protected band (beside a heading or a
    // numbered clause) is body text that is never removed, so its place
    // teaches nothing about the header's, and the other places must reach
    // the repeat threshold without it.
    let learned_place = |key: &str, page: u32| -> Option<f64> {
        let seen: Vec<(u32, f32)> = placements
            .get(key)
            .into_iter()
            .flatten()
            .filter(|&&(seen_page, seen_y)| {
                !protected_bands.contains(&(seen_page, (seen_y * 10.0).round() as i32))
            })
            .copied()
            .collect();
        // The unprotected places must still make a repeat on their own.
        let seen_pages: HashSet<u32> = seen.iter().map(|&(seen_page, _)| seen_page).collect();
        if (seen_pages.len() as u32) < threshold {
            return None;
        }
        // The places' mean, with the largest deviation from it, when every
        // place is within the tolerance of it.
        let agreed = |places: &[f64]| -> Option<(f64, f64)> {
            let first = *places.first()?;
            if places.iter().all(|&place| place == first) {
                return Some((first, 0.0));
            }
            let mean = places.iter().sum::<f64>() / places.len() as f64;
            let deviation = places
                .iter()
                .map(|place| (place - mean).abs())
                .fold(0.0, f64::max);
            within_tolerance(deviation).then_some((mean, deviation))
        };
        let framed: Vec<(u32, f64, (f64, f64))> = seen
            .iter()
            .filter_map(|&(seen_page, seen_y)| {
                let &(b, t) = page_frames.get(&seen_page)?;
                (t > b).then_some((seen_page, f64::from(seen_y), (f64::from(b), f64::from(t))))
            })
            .collect();
        let framed_pages: HashSet<u32> = framed.iter().map(|&(page, _, _)| page).collect();
        let edge = match framed_pages.len() {
            0 => None,
            1 => return None,
            _ => {
                let from_top: Vec<f64> = framed.iter().map(|&(_, y, (_, t))| t - y).collect();
                let from_bottom: Vec<f64> = framed.iter().map(|&(_, y, (b, _))| y - b).collect();
                match (agreed(&from_top), agreed(&from_bottom)) {
                    (None, None) => return None,
                    (Some((at, _)), None) => Some((true, at)),
                    (None, Some((at, _))) => Some((false, at)),
                    (Some((top_at, _)), Some((bottom_at, _))) => {
                        // Steadiness is the range (largest minus smallest
                        // distance): exact for distances made from f32
                        // coordinates, so no rounding can tip it, and equal
                        // ranges (same-size pages) tie.
                        let range = |distances: &[f64]| {
                            let (low, high) = distances
                                .iter()
                                .fold((f64::INFINITY, f64::NEG_INFINITY), |(low, high), &d| {
                                    (low.min(d), high.max(d))
                                });
                            high - low
                        };
                        let (top_range, bottom_range) = (range(&from_top), range(&from_bottom));
                        let from_the_top =
                            if top_range != bottom_range && top_range * 2.0 <= bottom_range {
                                true
                            } else if top_range != bottom_range && bottom_range * 2.0 <= top_range {
                                false
                            } else {
                                top_at < bottom_at
                            };
                        Some(if from_the_top {
                            (true, top_at)
                        } else {
                            (false, bottom_at)
                        })
                    }
                }
            }
        };
        match (edge, page_frames.get(&page)) {
            (Some((from_the_top, at)), Some(&(bottom, top))) if top > bottom => {
                let (bottom, top) = (f64::from(bottom), f64::from(top));
                // The learned place, projected onto this page, must be on
                // it: a page too short for it has no header there.
                let place = if from_the_top { top - at } else { bottom + at };
                (bottom..=top).contains(&place).then_some(place)
            }
            (_, frame) => {
                let heights: Vec<f64> = seen.iter().map(|&(_, seen_y)| f64::from(seen_y)).collect();
                // A known frame still bounds the learned height.
                let on_the_page = |at: f64| {
                    frame.is_none_or(|&(bottom, top)| {
                        top <= bottom || (f64::from(bottom)..=f64::from(top)).contains(&at)
                    })
                };
                agreed(&heights)
                    .map(|(at, _)| at)
                    .filter(|&at| on_the_page(at))
            }
        }
    };

    // Identify candidates from individual frequency map
    let candidates: HashSet<String> = freq
        .into_iter()
        .filter(|(text, pages)| {
            pages.len() as u32 >= threshold
                && !is_structural_line(text)
                && has_consistent_y(text, &y_positions)
        })
        .map(|(text, _)| text)
        .collect();

    // Identify candidates from coalesced band frequency map
    let band_candidates: HashSet<String> = band_freq
        .into_iter()
        .filter(|(text, pages)| {
            pages.len() as u32 >= threshold
                && !is_structural_line(text)
                && has_consistent_y(text, &band_y_positions)
        })
        .map(|(text, _)| text)
        .collect();

    if candidates.is_empty() && band_candidates.is_empty() {
        return lines;
    }

    // Build removal set.
    // A line is removed if it's at an edge position and:
    //   (a) its individual text matches a candidate, OR
    //   (b) its Y-band's coalesced text matches a band candidate, OR
    //   (c) any sibling in its Y-band was removed (propagation).
    //
    // The first occurrence (lowest page number) of each repeated header/footer
    // is kept so that document titles, column headers, etc. appear once.
    let mut removal_set: HashSet<usize> = HashSet::new();

    // A sparse page loses each repeated row at most once: the row closest
    // to the place the full pages show it, if within the tolerance. An
    // identical sentence also within the tolerance is the page's own text
    // and stays. Of two rows equally close, the one beyond the place (above
    // a header's place, below a footer's) is the header or footer. A row is
    // the page's clustered row, so a split or doubled header row goes whole.
    // Rows in protected bands are never removed and are no match. A row is
    // judged once, by its clustered baseline, so copies of a sentence in
    // one row cannot outscore the header.
    // (page, key) -> (distance from the place, row baseline, clustered row).
    let mut sparse_matches: HashMap<(u32, String), (f64, f32, usize)> = HashMap::new();
    // The sparse-page lines within the tolerance of their row's place.
    let mut sparse_eligible: HashSet<usize> = HashSet::new();
    for (idx, line) in lines.iter().enumerate() {
        let Some(rows) = page_rows.get(&line.page).filter(|rows| rows.len() <= 2) else {
            continue;
        };
        if protected_bands.contains(&band_key(line)) {
            continue;
        }
        let (key, _) = keys.key(&line.text(), line.page);
        if !candidates.contains(&key) {
            continue;
        }
        let Some(place) = learned_place(&key, line.page) else {
            continue;
        };
        // A header's place is in the page's upper half, a footer's in its
        // lower half.
        let middle = match page_frames.get(&line.page) {
            Some(&(bottom, top)) if top > bottom => (f64::from(bottom) + f64::from(top)) / 2.0,
            _ => full_page_middle,
        };
        // Exactly halfway down is a footer's half, as the edge it is
        // measured from is the bottom there.
        let top_side = place > middle;
        if !within_tolerance(f64::from(line.y) - place) {
            continue;
        }
        let Some(row) = rows.iter().position(|&py| (py - line.y).abs() < 0.1) else {
            continue;
        };
        sparse_eligible.insert(idx);
        let row_y = rows[row];
        let distance = (f64::from(row_y) - place).abs();
        let best = sparse_matches
            .entry((line.page, key))
            .or_insert((distance, row_y, row));
        let beyond = if top_side {
            row_y > best.1
        } else {
            row_y < best.1
        };
        if distance < best.0 || (distance == best.0 && beyond) {
            *best = (distance, row_y, row);
        }
    }

    // Track which page first shows each candidate (to preserve first occurrence)
    let mut first_page_individual: HashMap<String, u32> = HashMap::new();
    for (idx, line) in lines.iter().enumerate() {
        // A one- or two-row page adds no candidates, but a header already
        // confirmed on full pages is still furniture there, where the full
        // pages show it; the same sentence elsewhere on it is body text.
        let sparse_page = page_rows.get(&line.page).is_some_and(|r| r.len() <= 2);
        if !sparse_page && !is_y_at_edge(line.y, line.page, &page_rows, EDGE_LINE_COUNT) {
            continue;
        }
        let text = line.text();
        // Removal must apply the same original-text structural protection
        // as candidate collection: a structural line can share a comparison
        // key with a non-structural candidate. Protection extends to the line's whole Y-band —
        // one physical row — so a heading's row-mates are not half-removed.
        if protected_bands.contains(&band_key(line)) {
            continue;
        }
        let (key, _) = keys.key(&text, line.page);
        if sparse_page {
            let row = page_rows
                .get(&line.page)
                .and_then(|rows| rows.iter().position(|&py| (py - line.y).abs() < 0.1));
            let closest = sparse_eligible.contains(&idx)
                && sparse_matches
                    .get(&(line.page, key.clone()))
                    .is_some_and(|&(_, _, best_row)| row == Some(best_row));
            if !closest {
                continue;
            }
        }
        if candidates.contains(&key) {
            let first = first_page_individual.entry(key).or_insert(line.page);
            if line.page > *first {
                removal_set.insert(idx);
            } else if line.page == *first {
                // Keep this occurrence (first page)
            }
        }
    }

    // Track first page for band candidates
    let mut first_page_band: HashMap<String, u32> = HashMap::new();
    // First pass: find first page for each band candidate
    for (&(page, _), indices) in &y_bands {
        if indices.len() < 2 {
            continue;
        }
        let band_y = lines[indices[0]].y;
        if !is_y_at_edge(band_y, page, &page_rows, EDGE_LINE_COUNT) {
            continue;
        }
        let (_, coalesced) = coalesced_band(&lines, indices);
        let (key, _) = keys.key(&coalesced, page);
        if band_candidates.contains(&key) {
            let first = first_page_band.entry(key).or_insert(page);
            if page < *first {
                *first = page;
            }
        }
    }
    // Second pass: mark for removal (skip first page)
    for (&(page, y_bucket), indices) in &y_bands {
        if indices.len() < 2 || protected_bands.contains(&(page, y_bucket)) {
            continue;
        }
        let band_y = lines[indices[0]].y;
        if !is_y_at_edge(band_y, page, &page_rows, EDGE_LINE_COUNT) {
            continue;
        }
        let (sorted_indices, coalesced) = coalesced_band(&lines, indices);
        if is_structural_line(coalesced.trim()) {
            continue;
        }
        let (key, _) = keys.key(&coalesced, page);
        if band_candidates.contains(&key) {
            let first = first_page_band.get(&key).copied().unwrap_or(0);
            if page > first {
                for &idx in &sorted_indices {
                    removal_set.insert(idx);
                }
            }
        }
    }

    // (c) Y-band sibling propagation: if any member is removed, remove the
    //     members that repeat too (provided the band is at an edge position
    //     and the band is not structurally protected). A member repeats when
    //     its own text, short or not, shows at a page edge on as many pages
    //     as a repeat needs, or when it is a bare page number. One that
    //     changes from page to page ("Invoice No. 4172" beside a repeated
    //     "Acme Holdings") is the page's own text and stays.
    let mut fragment_pages: HashMap<String, HashSet<u32>> = HashMap::new();
    for (text, page, _) in &edge_lines {
        let (key, _) = keys.key(text, *page);
        fragment_pages.entry(key).or_default().insert(*page);
    }
    let repeats = |line: &TextLine| -> bool {
        let text = line.text();
        let (key, _) = keys.key(&text, line.page);
        fragment_pages
            .get(&key)
            .is_some_and(|pages| pages.len() as u32 >= threshold)
            || is_bare_page_number(&text, line.page)
    };
    for (&(page, y_bucket), indices) in &y_bands {
        if protected_bands.contains(&(page, y_bucket)) {
            continue;
        }
        let band_y = lines[indices[0]].y;
        if !is_y_at_edge(band_y, page, &page_rows, EDGE_LINE_COUNT) {
            continue;
        }
        if indices.iter().any(|idx| removal_set.contains(idx)) {
            for &idx in indices {
                if repeats(&lines[idx]) {
                    removal_set.insert(idx);
                }
            }
        }
    }

    if removal_set.is_empty() {
        return lines;
    }

    lines
        .into_iter()
        .enumerate()
        .filter(|(idx, _)| !removal_set.contains(idx))
        .map(|(_, line)| line)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ItemType, TextItem};

    fn make_item(text: &str, font_size: f32, mcid: Option<i64>) -> TextItem {
        TextItem {
            text: text.to_string(),
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: font_size,
            font: "TestFont".to_string(),
            font_tag: String::new(),
            legacy_symbol_rewrite: false,
            font_size,
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
            mcid,
            baseline_shift: 0.0,
        }
    }

    fn make_line(text: &str, font_size: f32, page: u32, y: f32, mcid: Option<i64>) -> TextLine {
        TextLine {
            items: vec![make_item(text, font_size, mcid)],
            y,
            page,
            adaptive_threshold: 0.10,
        }
    }

    /// A dense single page: 14 unique body lines at 15pt leading starting
    /// from y=650, all 10pt font.
    fn body_page(lines: &mut Vec<TextLine>) {
        for j in 0..14u32 {
            lines.push(make_line(
                &format!("body paragraph line number {j} with ordinary words"),
                10.0,
                1,
                650.0 - j as f32 * 15.0,
                None,
            ));
        }
    }

    #[test]
    fn edge_furniture_strips_isolated_running_header() {
        let mut lines = vec![make_line(
            "martinez et al respiratory research vol nine",
            8.0,
            1,
            760.0, // 110pt above the body: isolated
            None,
        )];
        body_page(&mut lines);
        let result = strip_header_footer_lines(lines, 1);
        assert!(
            !result.iter().any(|l| l.text().contains("martinez")),
            "isolated small running header must be stripped"
        );
        assert_eq!(result.len(), 14);
    }

    #[test]
    fn edge_furniture_keeps_display_title() {
        let mut lines = vec![make_line("Annual Report", 22.0, 1, 760.0, None)];
        body_page(&mut lines);
        let result = strip_header_footer_lines(lines, 1);
        assert!(
            result.iter().any(|l| l.text().contains("Annual Report")),
            "display title set larger than body must survive"
        );
    }

    #[test]
    fn edge_furniture_strips_numeric_page_indicator() {
        let mut lines = Vec::new();
        body_page(&mut lines);
        lines.push(make_line("1 / 1", 10.0, 1, 380.0, None)); // 65pt below body
        let result = strip_header_footer_lines(lines, 1);
        assert!(
            !result.iter().any(|l| l.text().contains("1 / 1")),
            "isolated non-alphabetic page indicator must be stripped"
        );
    }

    #[test]
    fn edge_furniture_keeps_single_line_footnote() {
        let mut lines = Vec::new();
        body_page(&mut lines);
        lines.push(make_line(
            "1 See the appendix for the full derivation",
            8.0,
            1,
            380.0,
            None,
        ));
        let result = strip_header_footer_lines(lines, 1);
        assert!(
            result.iter().any(|l| l.text().contains("appendix")),
            "digit-led footnote text at the bottom must survive"
        );
    }

    #[test]
    fn edge_furniture_keeps_footnote_block() {
        let mut lines = Vec::new();
        body_page(&mut lines);
        // Three-line footnote block at normal internal leading, isolated
        // from the body: the run is body-sized, so it stays whole.
        for j in 0..3u32 {
            lines.push(make_line(
                &format!("footnote continuation words line {j}"),
                8.0,
                1,
                390.0 - j as f32 * 12.0,
                None,
            ));
        }
        let result = strip_header_footer_lines(lines, 1);
        assert_eq!(
            result
                .iter()
                .filter(|l| l.text().contains("footnote continuation"))
                .count(),
            3,
            "multi-line footnote block must survive whole"
        );
    }

    #[test]
    fn edge_furniture_keeps_two_cell_numeric_row() {
        // Two numeric cells at the bottom edge: form or table data, not a
        // page indicator, which is a single short run.
        let mut lines = Vec::new();
        body_page(&mut lines);
        let mut items = Vec::new();
        for (k, cell) in ["42.50", "38.20"].iter().enumerate() {
            let mut item = make_item(cell, 10.0, None);
            item.x = 50.0 + k as f32 * 200.0;
            item.width = 40.0;
            items.push(item);
        }
        lines.push(TextLine {
            items,
            y: 380.0,
            page: 1,
            adaptive_threshold: 0.10,
        });
        let result = strip_header_footer_lines(lines, 1);
        assert!(
            result.iter().any(|l| l.text().contains("42.50")),
            "two-cell numeric row must survive as data"
        );
    }

    #[test]
    fn edge_furniture_keeps_symbol_and_letter_marked_footnotes() {
        let mut lines = Vec::new();
        body_page(&mut lines);
        lines.push(make_line(
            "† Estimates assume constant returns",
            8.0,
            1,
            380.0,
            None,
        ));
        let result = strip_header_footer_lines(lines, 1);
        assert!(
            result.iter().any(|l| l.text().contains("constant returns")),
            "symbol-marked footnote must survive"
        );

        let mut lines = Vec::new();
        body_page(&mut lines);
        lines.push(make_line(
            "a See the appendix for derivations",
            8.0,
            1,
            380.0,
            None,
        ));
        let result = strip_header_footer_lines(lines, 1);
        assert!(
            result.iter().any(|l| l.text().contains("appendix")),
            "lowercase-letter-marked footnote must survive"
        );
    }

    #[test]
    fn edge_furniture_keeps_paren_letter_marker() {
        let mut lines = Vec::new();
        body_page(&mut lines);
        lines.push(make_line(
            "(a) See the statutory definitions above",
            8.0,
            1,
            380.0,
            None,
        ));
        let result = strip_header_footer_lines(lines, 1);
        assert!(
            result.iter().any(|l| l.text().contains("statutory")),
            "(letter)-marked enumeration must survive"
        );
    }

    #[test]
    fn structural_line_survives_normalized_key_collision() {
        // A running head on pages 1-4 normalizes to the same key as a
        // numbered heading on page 5 (normalization strips the heading's
        // leading digit). The heading's structural protection must hold in
        // the removal pass, not just candidate collection.
        let mut lines = Vec::new();
        for page in 1..=4u32 {
            lines.push(make_line(
                ". Introduction extra words here",
                10.0,
                page,
                750.0,
                None,
            ));
            for j in 0..20u32 {
                lines.push(make_line(
                    &format!("unique body content words {page} {j} lorem ipsum"),
                    10.0,
                    page,
                    600.0 - j as f32 * 15.0,
                    None,
                ));
            }
        }
        lines.push(make_line(
            "1. Introduction extra words here",
            10.0,
            5,
            750.0,
            None,
        ));
        for j in 0..20u32 {
            lines.push(make_line(
                &format!("unique body content words five {j} lorem ipsum"),
                10.0,
                5,
                600.0 - j as f32 * 15.0,
                None,
            ));
        }
        let result = strip_repeated_lines(lines, 5);
        assert!(
            result
                .iter()
                .any(|l| l.text().contains("1. Introduction extra")),
            "structural line must survive a normalized-key collision"
        );
    }

    #[test]
    fn edge_furniture_keeps_unicode_paren_marker() {
        // Greek-letter footnote markers are lowercase too: "(α)" earns the
        // same protection as "(a)".
        let mut lines = Vec::new();
        body_page(&mut lines);
        lines.push(make_line(
            "(α) See the appendix for the derivation",
            8.0,
            1,
            380.0,
            None,
        ));
        let result = strip_header_footer_lines(lines, 1);
        assert!(
            result.iter().any(|l| l.text().contains("appendix")),
            "Unicode lowercase paren marker must survive"
        );
    }

    #[test]
    fn edge_furniture_keeps_unicode_digit_marker() {
        // Arabic-Indic digits are digits: "(١)" earns the same protection
        // as "(1)".
        let mut lines = Vec::new();
        body_page(&mut lines);
        lines.push(make_line(
            "(١) See the appendix for the derivation",
            8.0,
            1,
            380.0,
            None,
        ));
        let result = strip_header_footer_lines(lines, 1);
        assert!(
            result.iter().any(|l| l.text().contains("appendix")),
            "Unicode digit paren marker must survive"
        );
    }

    #[test]
    fn edge_furniture_strips_parenthetical_prose_footer() {
        // Parenthetical lowercase PROSE is not a footnote marker: only a
        // short "(a)"/"(12)"-style token closed by ')' earns protection.
        let mut lines = Vec::new();
        body_page(&mut lines);
        lines.push(make_line(
            "(all amounts in thousands of dollars)",
            8.0,
            1,
            380.0,
            None,
        ));
        let result = strip_header_footer_lines(lines, 1);
        assert!(
            !result.iter().any(|l| l.text().contains("thousands")),
            "parenthetical prose footer must still strip"
        );
    }

    #[test]
    fn band_removal_spares_structural_sibling() {
        // A repeated edge Y-band pairs a plain fragment with a structural
        // sibling; the coalesced text reads non-structural, but the sibling
        // itself must survive removal.
        let mut lines = Vec::new();
        for page in 1..=10u32 {
            lines.push(make_line(
                "Journal of Applied Testing",
                10.0,
                page,
                750.0,
                None,
            ));
            lines.push(make_line(
                "1. Introduction overview",
                10.0,
                page,
                750.0,
                None,
            ));
            for j in 0..20u32 {
                lines.push(make_line(
                    &format!("unique body content words {page} {j} lorem ipsum"),
                    10.0,
                    page,
                    600.0 - j as f32 * 15.0,
                    None,
                ));
            }
        }
        let result = strip_repeated_lines(lines, 10);
        assert_eq!(
            result
                .iter()
                .filter(|l| l.text().contains("1. Introduction overview"))
                .count(),
            10,
            "structural band sibling must survive on every page"
        );
        assert_eq!(
            result
                .iter()
                .filter(|l| l.text().contains("Journal of Applied Testing"))
                .count(),
            10,
            "the structural member protects its whole physical row"
        );
    }

    #[test]
    fn repetition_length_floor_counts_chars_not_bytes() {
        // A six-character CJK line is 18 UTF-8 bytes: byte counting would
        // let it past the ten-character minimum and classify the repeated
        // short heading as furniture.
        let mut lines = Vec::new();
        for page in 1..=10u32 {
            lines.push(make_line("第一章序論", 10.0, page, 750.0, None));
            for j in 0..20u32 {
                lines.push(make_line(
                    &format!("unique body content words {page} {j} lorem ipsum"),
                    10.0,
                    page,
                    600.0 - j as f32 * 15.0,
                    None,
                ));
            }
        }
        let result = strip_repeated_lines(lines, 10);
        assert_eq!(
            result
                .iter()
                .filter(|l| l.text().contains("第一章"))
                .count(),
            10,
            "short CJK line must stay under the character-count floor"
        );
    }

    #[test]
    fn repeated_paren_numbered_line_is_not_stripped() {
        let mut lines = Vec::new();
        for page in 1..=10u32 {
            lines.push(make_line(
                "(1) Sign and date the enclosed form",
                10.0,
                page,
                750.0,
                None,
            ));
            for j in 0..20u32 {
                lines.push(make_line(
                    &format!("unique body content words {page} {j} lorem ipsum"),
                    10.0,
                    page,
                    600.0 - j as f32 * 15.0,
                    None,
                ));
            }
        }
        let result = strip_repeated_lines(lines, 10);
        assert_eq!(
            result
                .iter()
                .filter(|l| l.text().contains("Sign and date"))
                .count(),
            10,
            "(digit) enumerations are structural and must never be stripped"
        );
    }

    #[test]
    fn repetition_normalizes_unicode_page_numbers() {
        // A running header whose page number uses Arabic-Indic digits must
        // normalize to the same key on every page, or repetition never
        // reaches threshold and the furniture survives.
        let digits = ["١", "٢", "٣", "٤", "٥", "٦", "٧", "٨", "٩", "١٠"];
        let mut lines = Vec::new();
        for page in 1..=10u32 {
            lines.push(make_line(
                &format!("Annual Review Overview {}", digits[(page - 1) as usize]),
                10.0,
                page,
                750.0,
                None,
            ));
            for j in 0..20u32 {
                lines.push(make_line(
                    &format!("unique body content words {page} {j} lorem ipsum"),
                    10.0,
                    page,
                    600.0 - j as f32 * 15.0,
                    None,
                ));
            }
        }
        let result = strip_repeated_lines(lines, 10);
        assert_eq!(
            result
                .iter()
                .filter(|l| l.text().contains("Annual Review Overview"))
                .count(),
            1,
            "Unicode-numbered running header must strip to its first occurrence"
        );
    }

    #[test]
    fn repeated_paren_letter_annotation_is_not_stripped() {
        let mut lines = Vec::new();
        for page in 1..=10u32 {
            lines.push(make_line(
                "(a) See the appendix for details",
                10.0,
                page,
                750.0,
                None,
            ));
            for j in 0..20u32 {
                lines.push(make_line(
                    &format!("unique body content words {page} {j} lorem ipsum"),
                    10.0,
                    page,
                    600.0 - j as f32 * 15.0,
                    None,
                ));
            }
        }
        let result = strip_repeated_lines(lines, 10);
        assert_eq!(
            result
                .iter()
                .filter(|l| l.text().contains("See the appendix"))
                .count(),
            10,
            "(letter) annotations are structural in the repetition path too"
        );
    }

    #[test]
    fn row_protection_survives_bucket_rounding_boundary() {
        // The structural fragment and its row-mate sit 0.02pt apart across
        // a rounding boundary (buckets 7501 and 7500); neighbor-bucket
        // protection must still shield the plain fragment.
        let mut lines = Vec::new();
        for page in 1..=10u32 {
            lines.push(make_line(
                "1. Methods overview line",
                10.0,
                page,
                750.06,
                None,
            ));
            lines.push(make_line(
                "plain sibling fragment text",
                10.0,
                page,
                750.04,
                None,
            ));
            for j in 0..20u32 {
                lines.push(make_line(
                    &format!("unique body content words {page} {j} lorem ipsum"),
                    10.0,
                    page,
                    600.0 - j as f32 * 15.0,
                    None,
                ));
            }
        }
        let result = strip_repeated_lines(lines, 10);
        assert_eq!(
            result
                .iter()
                .filter(|l| l.text().contains("plain sibling fragment"))
                .count(),
            10,
            "row-mates across a rounding boundary keep structural protection"
        );
    }

    #[test]
    fn repeated_numbered_heading_is_not_stripped() {
        // The same numbered heading recurring on many pages must keep its
        // structural protection: normalization strips the leading digit, so
        // the check must run against the original text.
        let mut lines = Vec::new();
        for page in 1..=10u32 {
            lines.push(make_line(
                "1. Introduction to the method",
                10.0,
                page,
                750.0,
                None,
            ));
            for j in 0..20u32 {
                lines.push(make_line(
                    &format!("unique body content words {page} {j} lorem ipsum dolor"),
                    10.0,
                    page,
                    600.0 - j as f32 * 15.0,
                    None,
                ));
            }
        }
        let result = strip_repeated_lines(lines, 10);
        let count = result
            .iter()
            .filter(|l| l.text().contains("Introduction to the method"))
            .count();
        assert_eq!(count, 10, "numbered headings must never be stripped");
    }

    #[test]
    fn edge_furniture_keeps_numeric_table_row() {
        // A bottom row of an undetected borderless table: four numeric
        // cells sharing a baseline. No alphabetic content, but three-plus
        // clusters mark it as content, not a page indicator.
        let mut lines = Vec::new();
        body_page(&mut lines);
        let mut items = Vec::new();
        for (k, cell) in ["1,234", "5,678", "9,012", "3,456"].iter().enumerate() {
            let mut item = make_item(cell, 10.0, None);
            item.x = 50.0 + k as f32 * 120.0;
            item.width = 40.0;
            items.push(item);
        }
        lines.push(TextLine {
            items,
            y: 380.0,
            page: 1,
            adaptive_threshold: 0.10,
        });
        let result = strip_header_footer_lines(lines, 1);
        assert!(
            result.iter().any(|l| l.text().contains("5,678")),
            "numeric table row must survive on cluster evidence"
        );
    }

    #[test]
    fn edge_furniture_keeps_diagram_label_row() {
        // Four small-font labels sharing one baseline with box-sized gaps —
        // a figure's caption row at the page edge, not a running header.
        let mut lines = Vec::new();
        let mut items = Vec::new();
        for (k, label) in ["SOURCE", "TRANSMITTER", "RECEIVER", "DESTINATION"]
            .iter()
            .enumerate()
        {
            let mut item = make_item(label, 7.0, None);
            item.x = k as f32 * 120.0;
            item.width = 60.0;
            items.push(item);
        }
        lines.push(TextLine {
            items,
            y: 760.0,
            page: 1,
            adaptive_threshold: 0.10,
        });
        body_page(&mut lines);
        let result = strip_header_footer_lines(lines, 1);
        assert!(
            result.iter().any(|l| l.text().contains("TRANSMITTER")),
            "scattered diagram label row must survive"
        );
    }

    #[test]
    fn edge_furniture_skips_sparse_pages() {
        // Six lines total: too sparse to judge, nothing may be stripped.
        let lines: Vec<TextLine> = (0..6u32)
            .map(|j| {
                make_line(
                    &format!("sparse content line {j}"),
                    10.0,
                    1,
                    700.0 - j as f32 * 80.0,
                    None,
                )
            })
            .collect();
        let result = strip_header_footer_lines(lines, 1);
        assert_eq!(result.len(), 6, "sparse pages must be untouched");
    }

    #[test]
    fn test_strip_repeated_keeps_first_occurrence() {
        // Simulate a repeated page header on 10 pages.
        // Each page has a running header at y=750 and many unique body lines.
        let mut lines = Vec::new();
        for page in 1..=10u32 {
            // Header at top
            lines.push(make_line(
                "VOICE OF SOUTH MARION May fifteen twenty twenty five",
                10.0,
                page,
                750.0,
                None,
            ));
            // Body content — unique text per line per page (no digits to strip)
            for j in 0..20u32 {
                lines.push(make_line(
                    &format!(
                        "parcel r-{:04}-{:03} owner smith address oak street",
                        page * 100 + j,
                        page
                    ),
                    10.0,
                    page,
                    600.0 - j as f32 * 15.0,
                    None,
                ));
            }
        }

        let result = strip_repeated_lines(lines, 10);

        // The header should appear exactly once (page 1)
        let header_count = result
            .iter()
            .filter(|l| l.text().contains("VOICE OF SOUTH MARION"))
            .count();
        assert_eq!(header_count, 1, "repeated header should be kept once");

        // First occurrence should be on page 1
        let first_header = result
            .iter()
            .find(|l| l.text().contains("VOICE OF SOUTH MARION"))
            .unwrap();
        assert_eq!(first_header.page, 1, "first occurrence should be on page 1");
    }

    #[test]
    fn repeated_body_on_a_short_page_is_kept() {
        // Eight distinct rows is inside the old "everything is an edge" zone
        // (2 * N). The same sentences on three pages are body text that a
        // large-print page repeats, not a running header.
        let story = [
            "left page story opens with a red book on the table",
            "the next sentence keeps the same characters in the room",
            "dialogue continues across the wide printed lines here",
            "a fourth sentence still belongs to the page body text",
            "fifth sentence of the repeated passage stays in place",
            "sixth sentence should not vanish once neighbors arrive",
            "seventh sentence of the short page is still the story",
            "eighth sentence closes the passage without being a footer",
        ];
        let mut lines = Vec::new();
        for page in [1u32, 2, 5] {
            for (row, text) in story.iter().enumerate() {
                lines.push(make_line(text, 10.0, page, 700.0 - row as f32 * 40.0, None));
            }
        }
        for page in [3u32, 4, 6] {
            for row in 0..8u32 {
                lines.push(make_line(
                    &format!("different page {page} body sentence number {row} here"),
                    10.0,
                    page,
                    700.0 - row as f32 * 40.0,
                    None,
                ));
            }
        }
        let result = strip_repeated_lines(lines, 6);
        // The outermost rows can still be a header and a footer. The
        // interior of the passage must stay on every page that carries it.
        assert_eq!(
            result.iter().filter(|l| l.page == 2).count(),
            story.len() - 2,
            "short-page body must keep its interior rows"
        );
        assert!(
            result
                .iter()
                .any(|l| l.page == 2 && l.text().contains("next sentence")),
            "an interior sentence must survive on the repeated page"
        );
    }

    #[test]
    fn short_page_still_strips_an_outer_running_header() {
        // Eight body rows plus one header is still inside the short-page
        // zone. Only the outermost row is furniture, so the header drops
        // and the body stays.
        let mut lines = Vec::new();
        for page in 1..=6u32 {
            lines.push(make_line(
                "Chapter notes running header text",
                10.0,
                page,
                760.0,
                None,
            ));
            for row in 0..8u32 {
                lines.push(make_line(
                    &format!("unique short page {page} body sentence number {row}"),
                    10.0,
                    page,
                    700.0 - row as f32 * 40.0,
                    None,
                ));
            }
        }
        let result = strip_repeated_lines(lines, 6);
        assert_eq!(
            result
                .iter()
                .filter(|l| l.text().contains("Chapter notes"))
                .count(),
            1,
            "the outer header on a short page must still strip"
        );
        assert_eq!(
            result.iter().filter(|l| l.page == 2).count(),
            8,
            "the short page body must stay"
        );
    }

    #[test]
    fn offset_column_baselines_stay_one_sparse_page() {
        // Six logical rows, each with a second column 0.05pt lower. Counting
        // those baselines separately yields twelve rows and the fixed zone
        // then covers the body.
        let mut lines = Vec::new();
        for page in [1u32, 2, 5] {
            for row in 0..6u32 {
                let y = 700.0 - row as f32 * 40.0;
                lines.push(make_line(
                    &format!("left column story sentence number {row} continues onward"),
                    10.0,
                    page,
                    y,
                    None,
                ));
                lines.push(make_line(
                    &format!("right column story sentence number {row} continues onward"),
                    10.0,
                    page,
                    y + 0.05,
                    None,
                ));
            }
        }
        for page in [3u32, 4, 6] {
            for row in 0..6u32 {
                let y = 700.0 - row as f32 * 40.0;
                lines.push(make_line(
                    &format!("other page {page} left sentence number {row} unique"),
                    10.0,
                    page,
                    y,
                    None,
                ));
                lines.push(make_line(
                    &format!("other page {page} right sentence number {row} unique"),
                    10.0,
                    page,
                    y + 0.05,
                    None,
                ));
            }
        }
        let result = strip_repeated_lines(lines, 6);
        assert_eq!(
            result
                .iter()
                .filter(|l| l.page == 2 && l.text().contains("column story"))
                .count(),
            8,
            "interior rows of a short two-column page must stay"
        );
        assert!(
            result.iter().any(|l| {
                l.page == 2 && l.text().contains("left column story sentence number 1")
            }),
            "a column mate 0.05pt off the row must stay with that row"
        );
    }

    #[test]
    fn repeated_header_on_a_tall_page_still_strips() {
        // Just past the sparse cutoff: eleven rows, so the top line is a
        // real margin and a running header still drops after its first page.
        let mut lines = Vec::new();
        for page in 1..=6u32 {
            lines.push(make_line(
                "Quarterly Bulletin running header text",
                10.0,
                page,
                760.0,
                None,
            ));
            for row in 0..10u32 {
                lines.push(make_line(
                    &format!("unique body words page {page} row {row} lorem ipsum"),
                    10.0,
                    page,
                    700.0 - row as f32 * 20.0,
                    None,
                ));
            }
        }
        let result = strip_repeated_lines(lines, 6);
        assert_eq!(
            result
                .iter()
                .filter(|l| l.text().contains("Quarterly Bulletin"))
                .count(),
            1,
            "a header above a real interior must still strip"
        );
    }

    #[test]
    fn repeated_body_just_past_the_short_page_cutoff_is_kept() {
        // Eleven and twelve rows used to switch straight to the five-row
        // zones, which left only the middle row or two of a repeated passage.
        for rows in [11u32, 12] {
            let mut lines = Vec::new();
            for page in [1u32, 2, 5] {
                for row in 0..rows {
                    lines.push(make_line(
                        &format!("repeated passage sentence number {row} of the story"),
                        10.0,
                        page,
                        700.0 - row as f32 * 20.0,
                        None,
                    ));
                }
            }
            for page in [3u32, 4, 6] {
                for row in 0..rows {
                    lines.push(make_line(
                        &format!("different page {page} body sentence number {row} here"),
                        10.0,
                        page,
                        700.0 - row as f32 * 20.0,
                        None,
                    ));
                }
            }
            let result = strip_repeated_lines(lines, 6);
            let edge = (rows - 10) as usize;
            assert_eq!(
                result.iter().filter(|l| l.page == 2).count(),
                rows as usize - 2 * edge,
                "a {rows}-row page must keep its repeated interior"
            );
        }
    }

    #[test]
    fn running_header_on_a_near_empty_page_still_strips() {
        // Five full pages confirm the header as furniture. Page 6 holds only
        // the header and one body line; two rows are not a margin, but a
        // header already confirmed on full pages must not leak back in.
        let mut lines = Vec::new();
        for page in 1..=6u32 {
            lines.push(make_line(
                "Quarterly Bulletin running header text",
                10.0,
                page,
                760.0,
                None,
            ));
            let rows = if page == 6 { 1 } else { 10 };
            for row in 0..rows {
                lines.push(make_line(
                    &format!("unique body words page {page} row {row} lorem ipsum"),
                    10.0,
                    page,
                    700.0 - row as f32 * 20.0,
                    None,
                ));
            }
        }
        let result = strip_repeated_lines(lines, 6);
        assert!(
            !result
                .iter()
                .any(|l| l.page == 6 && l.text().contains("Quarterly Bulletin")),
            "a confirmed running header must strip on a near-empty page"
        );
        assert!(
            result
                .iter()
                .any(|l| l.page == 6 && l.text().contains("page 6 row 0")),
            "the near-empty page body must stay"
        );
    }

    #[test]
    fn a_repeated_sentence_in_a_sparse_pages_body_stays() {
        // Codex on mdoc#957: the header of pages 1-3 is a body line on a
        // sparse page 4, away from the header position. It is the page's
        // own text and stays.
        let header = "Please sign and return this form";
        let mut lines = headed_pages(3, |_| header.into());
        lines.push(make_line(header, 10.0, 4, 400.0, None));
        lines.push(make_line("Thank you for your order", 10.0, 4, 380.0, None));
        let result = strip_repeated_lines(lines, 4);
        assert_eq!(header_pages(&result, header), [1, 4]);
        // Where the full pages show it, it is the header and goes.
        let mut lines = headed_pages(3, |_| header.into());
        lines.push(make_line(header, 10.0, 4, 750.0, None));
        lines.push(make_line("Thank you for your order", 10.0, 4, 380.0, None));
        let result = strip_repeated_lines(lines, 4);
        assert_eq!(header_pages(&result, header), [1]);
        assert!(result
            .iter()
            .any(|l| l.text() == "Thank you for your order"));
        // A sparse first page with the sentence in its body keeps it, and
        // the first full page keeps the header.
        let mut lines = vec![
            make_line(header, 10.0, 1, 400.0, None),
            make_line("Cover note for the order", 10.0, 1, 380.0, None),
        ];
        for line in headed_pages(4, |_| header.into()) {
            if line.page > 1 {
                lines.push(line);
            }
        }
        let result = strip_repeated_lines(lines, 4);
        assert_eq!(header_pages(&result, header), [1, 2]);
    }

    #[test]
    fn a_sparse_page_does_not_move_its_own_tolerance() {
        // Codex on #22: the full pages span 435 pt (header at 750, body from
        // 600 down to 315), so a sparse page's row must sit within 21.75 pt
        // of 750. A sparse page spanning 676 pt must not widen that to keep
        // its body line at 726 out of reach...
        let header = "Please sign and return this form";
        let mut lines = headed_pages(3, |_| header.into());
        lines.push(make_line(header, 10.0, 4, 726.0, None));
        lines.push(make_line("Thank you for your order", 10.0, 4, 50.0, None));
        let result = strip_repeated_lines(lines, 4);
        assert_eq!(header_pages(&result, header), [1, 4]);
        // ...nor a one-row page narrow it to keep a header 20 pt off.
        let mut lines = headed_pages(3, |_| header.into());
        lines.push(make_line(header, 10.0, 4, 730.0, None));
        let result = strip_repeated_lines(lines, 4);
        assert_eq!(header_pages(&result, header), [1]);
    }

    #[test]
    fn a_sparse_page_of_another_size_is_judged_from_its_edge() {
        // Codex on #22: pages 1-3 are Letter (792 pt) with the header 42 pt
        // below the top; page 4 is Legal (1008 pt) and sparse.
        let header = "Please sign and return this form";
        let frames: HashMap<u32, (f32, f32)> = (1..=4)
            .map(|page| (page, (0.0, if page == 4 { 1008.0 } else { 792.0 })))
            .collect();
        let with_page_4 = |y: f32| {
            let mut lines = headed_pages(3, |_| header.into());
            lines.push(make_line(header, 10.0, 4, y, None));
            lines.push(make_line("Thank you for your order", 10.0, 4, 380.0, None));
            strip_repeated_lines_in_frames(lines, 4, &frames)
        };
        // At y=750 on Legal the sentence is 258 pt below the top: body text.
        assert_eq!(header_pages(&with_page_4(750.0), header), [1, 4]);
        // 42 pt below Legal's top it is the header.
        assert_eq!(header_pages(&with_page_4(966.0), header), [1]);
    }

    #[test]
    fn a_sparse_page_follows_the_edge_the_full_pages_agree_on() {
        // Codex on mdoc#959: Letter pages 1-3 and a Legal page 4 carry the
        // header at y=750, the same height: 42 pt below the top on Letter,
        // 258 pt on Legal. Their distance from the top averages to 96 pt, a
        // place no page uses; from the bottom they agree (750 pt).
        let header = "Please sign and return this form";
        let letter_and_legal = |legal_bottom: f32| -> HashMap<u32, (f32, f32)> {
            (1..=5)
                .map(|page| {
                    let frame = if page == 4 {
                        (legal_bottom, legal_bottom + 1008.0)
                    } else {
                        (0.0, 792.0)
                    };
                    (page, frame)
                })
                .collect()
        };
        let with_page_5 = |y: f32, frames: &HashMap<u32, (f32, f32)>| {
            let mut lines = headed_pages(4, |_| header.into());
            lines.push(make_line(header, 10.0, 5, y, None));
            lines.push(make_line("Thank you for your order", 10.0, 5, 380.0, None));
            strip_repeated_lines_in_frames(lines, 5, frames)
        };
        let frames = letter_and_legal(0.0);
        // 96 pt below the top of sparse Letter page 5: body text, kept.
        assert_eq!(header_pages(&with_page_5(696.0, &frames), header), [1, 5]);
        // At the header's real place, 750 pt from the bottom: removed.
        assert_eq!(header_pages(&with_page_5(750.0, &frames), header), [1]);
        // A Legal box raised by 100 pt: the full pages agree on neither
        // edge, so a sparse page loses nothing.
        let frames = letter_and_legal(100.0);
        assert_eq!(header_pages(&with_page_5(750.0, &frames), header), [1, 5]);
        // Codex on #23: without page 5's own frame, the full pages still
        // agree on neither edge, so it still loses nothing.
        let mut frames = letter_and_legal(100.0);
        frames.remove(&5);
        assert_eq!(header_pages(&with_page_5(750.0, &frames), header), [1, 5]);
    }

    #[test]
    fn a_sparse_page_follows_the_steadier_edge_when_both_agree() {
        // Codex on #23: Letter pages 1-4 and an A4 page 5 carry the header at
        // y=750. From the top that is 42 pt four times and 92 pt once, a
        // spread of 20 pt, inside the 21.75 pt tolerance; from the bottom it
        // is 750 pt every time. The bottom wins: on sparse Legal page 6 the
        // sentence 52 pt below the top (the top's mean) is body text and
        // stays, and at y=750 it is the header and goes.
        let header = "Please sign and return this form";
        let frames: HashMap<u32, (f32, f32)> = (1..=6)
            .map(|page| {
                let top = match page {
                    5 => 842.0,
                    6 => 1008.0,
                    _ => 792.0,
                };
                (page, (0.0, top))
            })
            .collect();
        let with_page_6 = |y: f32| {
            let mut lines = headed_pages(5, |_| header.into());
            lines.push(make_line(header, 10.0, 6, y, None));
            lines.push(make_line("Thank you for your order", 10.0, 6, 380.0, None));
            strip_repeated_lines_in_frames(lines, 6, &frames)
        };
        assert_eq!(header_pages(&with_page_6(956.0), header), [1, 6]);
        assert_eq!(header_pages(&with_page_6(750.0), header), [1]);
    }

    #[test]
    fn a_sparse_page_follows_the_steadier_edge_at_exactly_half_the_deviation() {
        // Codex on #23: full pages 775, 790 and 805 pt tall with the header
        // at y=745, 750 and 755: 30, 40 and 50 pt below the top (deviation
        // 10 pt), 745 to 755 pt above the bottom (deviation 5 pt, exactly
        // half). The bottom wins: on sparse Legal page 4 the sentence 40 pt
        // below the top stays, and at y=750 it goes. Codex's sixth review:
        // tops 792, 792 and 793.5 with the header at 750, 750 and 750.5,
        // where rounding made twice the bottom's deviation (1/3 pt) a hair
        // more than the top's (2/3 pt).
        let header = "Please sign and return this form";
        let check = |tops: [f32; 3], places: [f32; 3], body: f32| {
            let frames: HashMap<u32, (f32, f32)> = (1..=4)
                .map(|page| {
                    (
                        page,
                        (
                            0.0,
                            if page == 4 {
                                1008.0
                            } else {
                                tops[page as usize - 1]
                            },
                        ),
                    )
                })
                .collect();
            let with_page_4 = |y: f32| {
                let mut lines = headed_pages(3, |_| header.into());
                for line in lines.iter_mut().filter(|line| line.y == 750.0) {
                    line.y = places[line.page as usize - 1];
                }
                lines.push(make_line(header, 10.0, 4, y, None));
                lines.push(make_line("Thank you for your order", 10.0, 4, 380.0, None));
                strip_repeated_lines_in_frames(lines, 4, &frames)
            };
            assert_eq!(header_pages(&with_page_4(body), header), [1, 4]);
            assert_eq!(header_pages(&with_page_4(750.0), header), [1]);
        };
        check([775.0, 790.0, 805.0], [745.0, 750.0, 755.0], 968.0);
        check([792.0, 792.0, 793.5], [750.0, 750.0, 750.5], 966.0);
    }

    #[test]
    fn a_sparse_page_keeps_a_row_no_full_page_puts_that_close() {
        // Codex on #23: Letter pages 1-4 and a Legal page 5 whose box starts
        // at y=50, all with the header at y=750: 750 pt above the bottom four
        // times and 700 pt once. Their spread is 20 pt, inside the 21.75 pt
        // tolerance, but their mean, 740 pt, is 40 pt from page 5's place:
        // they do not agree, and from the top (42 pt, 308 pt) neither. On
        // sparse Legal page 6 the sentence at y=725, 25 pt from every full
        // page's place, stays.
        let header = "Please sign and return this form";
        let frames: HashMap<u32, (f32, f32)> = (1..=6)
            .map(|page| {
                let frame = match page {
                    5 => (50.0, 1058.0),
                    6 => (0.0, 1008.0),
                    _ => (0.0, 792.0),
                };
                (page, frame)
            })
            .collect();
        let mut lines = headed_pages(5, |_| header.into());
        lines.push(make_line(header, 10.0, 6, 725.0, None));
        lines.push(make_line("Thank you for your order", 10.0, 6, 380.0, None));
        let result = strip_repeated_lines_in_frames(lines, 6, &frames);
        assert_eq!(header_pages(&result, header), [1, 6]);
    }

    #[test]
    fn a_full_page_without_a_frame_does_not_bypass_the_edges() {
        // Codex on #23: Letter pages 1-3 and a Legal page 4 raised by 100 pt
        // agree on neither edge (the header at y=750 on all of them). Full
        // page 5 has no known frame. The known frames still rule out both
        // edges, so the sentence at y=750 on sparse page 6 stays.
        let header = "Please sign and return this form";
        let frames: HashMap<u32, (f32, f32)> = [1, 2, 3, 4, 6]
            .into_iter()
            .map(|page| {
                let frame = if page == 4 {
                    (100.0, 1108.0)
                } else {
                    (0.0, 792.0)
                };
                (page, frame)
            })
            .collect();
        let mut lines = headed_pages(5, |_| header.into());
        lines.push(make_line(header, 10.0, 6, 750.0, None));
        lines.push(make_line("Thank you for your order", 10.0, 6, 380.0, None));
        let result = strip_repeated_lines_in_frames(lines, 6, &frames);
        assert_eq!(header_pages(&result, header), [1, 6]);
    }

    #[test]
    fn a_full_page_without_a_frame_does_not_discard_the_edge() {
        // Codex on #23: Letter pages 1-3 and a Legal page 4 agree on the
        // bottom only (the header at y=750 on all of them); full page 5 has
        // no known frame. Sparse page 6's box is raised by 100 pt, so the
        // header's place there is y=850: the sentence at y=750 is body text
        // and stays, and at y=850 it goes.
        let header = "Please sign and return this form";
        let frames: HashMap<u32, (f32, f32)> = [1, 2, 3, 4, 6]
            .into_iter()
            .map(|page| {
                let frame = match page {
                    4 => (0.0, 1008.0),
                    6 => (100.0, 892.0),
                    _ => (0.0, 792.0),
                };
                (page, frame)
            })
            .collect();
        let with_page_6 = |y: f32| {
            let mut lines = headed_pages(5, |_| header.into());
            lines.push(make_line(header, 10.0, 6, y, None));
            lines.push(make_line("Thank you for your order", 10.0, 6, 380.0, None));
            strip_repeated_lines_in_frames(lines, 6, &frames)
        };
        assert_eq!(header_pages(&with_page_6(750.0), header), [1, 6]);
        assert_eq!(header_pages(&with_page_6(850.0), header), [1]);
    }

    #[test]
    fn a_sparse_page_follows_an_exactly_steady_edge_over_a_nearly_steady_one() {
        // Codex on #23: tops at 792, 792 and 792.25 pt with the header at
        // y=750: the bottom distance is exactly steady, the top one deviates
        // by a sixth of a point. The bottom wins: on sparse Legal page 4 the
        // sentence 42 pt below the top stays, and at y=750 it goes.
        let header = "Please sign and return this form";
        let frames: HashMap<u32, (f32, f32)> = [
            (1, (0.0, 792.0)),
            (2, (0.0, 792.0)),
            (3, (0.0, 792.25)),
            (4, (0.0, 1008.0)),
        ]
        .into_iter()
        .collect();
        let with_page_4 = |y: f32| {
            let mut lines = headed_pages(3, |_| header.into());
            lines.push(make_line(header, 10.0, 4, y, None));
            lines.push(make_line("Thank you for your order", 10.0, 4, 380.0, None));
            strip_repeated_lines_in_frames(lines, 4, &frames)
        };
        assert_eq!(header_pages(&with_page_4(966.0), header), [1, 4]);
        assert_eq!(header_pages(&with_page_4(750.0), header), [1]);
    }

    #[test]
    fn one_known_full_page_frame_decides_no_edge() {
        // Codex on #23: three full pages with the header at y=750, only
        // page 1's Letter frame known. One page agrees with itself on both
        // edges, which says nothing about which one the header keeps to, so
        // sparse Legal page 4 loses nothing: not the sentence 42 pt below
        // its top, and not the one at y=750.
        let header = "Please sign and return this form";
        let frames: HashMap<u32, (f32, f32)> = [(1, (0.0, 792.0)), (4, (0.0, 1008.0))]
            .into_iter()
            .collect();
        let with_page_4 = |y: f32| {
            let mut lines = headed_pages(3, |_| header.into());
            lines.push(make_line(header, 10.0, 4, y, None));
            lines.push(make_line("Thank you for your order", 10.0, 4, 380.0, None));
            strip_repeated_lines_in_frames(lines, 4, &frames)
        };
        assert_eq!(header_pages(&with_page_4(966.0), header), [1, 4]);
        assert_eq!(header_pages(&with_page_4(750.0), header), [1, 4]);
    }

    #[test]
    fn rounding_does_not_make_one_edge_look_steadier() {
        // Codex on #23: 1,000 identical Letter pages with the header at
        // y=667.04376. Both edges are exactly steady; summed in f32 the top
        // distance seemed to deviate by about a thousandth of a point, so
        // the bottom won. A tie goes to the nearer edge, the top: on sparse
        // Legal page 1001 the sentence at y=667.04376 stays, and 124.95624
        // pt below the top (y=883.04376) it goes.
        let header = "Please sign and return this form";
        let place = 667.04376_f32;
        let frames: HashMap<u32, (f32, f32)> = (1..=1001)
            .map(|page| (page, (0.0, if page == 1001 { 1008.0 } else { 792.0 })))
            .collect();
        let with_page_1001 = |y: f32| {
            let mut lines = headed_pages(1000, |_| header.into());
            for line in lines.iter_mut().filter(|line| line.y == 750.0) {
                line.y = place;
            }
            lines.push(make_line(header, 10.0, 1001, y, None));
            lines.push(make_line(
                "Thank you for your order",
                10.0,
                1001,
                380.0,
                None,
            ));
            strip_repeated_lines_in_frames(lines, 1001, &frames)
        };
        assert_eq!(header_pages(&with_page_1001(place), header), [1, 1001]);
        assert_eq!(
            header_pages(&with_page_1001(1008.0 - (792.0 - place)), header),
            [1]
        );
    }

    #[test]
    fn an_exactly_steady_edge_beats_one_that_barely_moves() {
        // Codex on #23: frames from -792 to tops 0, 0 and 0.000001 with the
        // header at y=-42: exactly 750 pt above the bottom on every page,
        // and a top distance that moves by a millionth of a point. The
        // bottom wins: on sparse Legal page 4 the sentence 42 pt below the
        // top stays, and at y=750 it goes.
        let header = "Please sign and return this form";
        let frames: HashMap<u32, (f32, f32)> = [
            (1, (-792.0, 0.0)),
            (2, (-792.0, 0.0)),
            (3, (-792.0, 0.000001)),
            (4, (0.0, 1008.0)),
        ]
        .into_iter()
        .collect();
        let with_page_4 = |y: f32| {
            let mut lines = headed_pages(3, |_| header.into());
            for line in &mut lines {
                line.y -= 792.0;
            }
            lines.push(make_line(header, 10.0, 4, y, None));
            lines.push(make_line("Thank you for your order", 10.0, 4, 380.0, None));
            strip_repeated_lines_in_frames(lines, 4, &frames)
        };
        assert_eq!(header_pages(&with_page_4(966.0), header), [1, 4]);
        assert_eq!(header_pages(&with_page_4(750.0), header), [1]);
    }

    #[test]
    fn a_learned_place_off_the_sparse_page_removes_nothing() {
        // Codex on #23: Letter pages 1-3 and a Legal page 4 agree on the
        // header 750 pt above the bottom. Sparse page 5 is only 740 pt tall,
        // so that place is off the page: the sentence at y=730, within the
        // tolerance of it, is body text and stays.
        let header = "Please sign and return this form";
        let frames: HashMap<u32, (f32, f32)> = (1..=5)
            .map(|page| {
                let top = match page {
                    4 => 1008.0,
                    5 => 740.0,
                    _ => 792.0,
                };
                (page, (0.0, top))
            })
            .collect();
        let mut lines = headed_pages(4, |_| header.into());
        lines.push(make_line(header, 10.0, 5, 730.0, None));
        lines.push(make_line("Thank you for your order", 10.0, 5, 380.0, None));
        let result = strip_repeated_lines_in_frames(lines, 5, &frames);
        assert_eq!(header_pages(&result, header), [1, 5]);
    }

    #[test]
    fn a_learned_height_off_the_sparse_page_removes_nothing() {
        // Codex on #23: the full pages' frames are unknown, so the height is
        // learned (y=750); sparse page 4's frame is known and only 740 pt
        // tall, so that height is off the page: the sentence at y=730 stays.
        let header = "Please sign and return this form";
        let frames: HashMap<u32, (f32, f32)> = [(4, (0.0, 740.0))].into_iter().collect();
        let mut lines = headed_pages(3, |_| header.into());
        lines.push(make_line(header, 10.0, 4, 730.0, None));
        lines.push(make_line("Thank you for your order", 10.0, 4, 380.0, None));
        let result = strip_repeated_lines_in_frames(lines, 4, &frames);
        assert_eq!(header_pages(&result, header), [1, 4]);
    }

    #[test]
    fn the_steadier_edge_is_judged_exactly() {
        // Codex on #23: frames from -792 to tops 0, 0 and 2^-19 with the
        // header at y=-8, -8 and -8 + 2^-21. The bottom distances' range
        // (2^-21) is a third of the top's (3 * 2^-21), far below any fixed
        // noise threshold. The bottom wins: on sparse Legal page 4 the
        // sentence 8 pt below the top stays, and at y=784 it goes.
        let header = "Please sign and return this form";
        let tiny = 2f32.powi(-21);
        let frames: HashMap<u32, (f32, f32)> = [
            (1, (-792.0, 0.0)),
            (2, (-792.0, 0.0)),
            (3, (-792.0, 4.0 * tiny)),
            (4, (0.0, 1008.0)),
        ]
        .into_iter()
        .collect();
        let with_page_4 = |y: f32| {
            let mut lines = headed_pages(3, |_| header.into());
            for line in &mut lines {
                line.y -= 758.0;
                if line.page == 3 && line.y == -8.0 {
                    line.y += tiny;
                }
            }
            lines.push(make_line(header, 10.0, 4, y, None));
            lines.push(make_line("Thank you for your order", 10.0, 4, 380.0, None));
            strip_repeated_lines_in_frames(lines, 4, &frames)
        };
        assert_eq!(header_pages(&with_page_4(1000.0), header), [1, 4]);
        assert_eq!(header_pages(&with_page_4(784.0), header), [1]);
    }

    #[test]
    fn a_protected_row_teaches_nothing_about_the_edge() {
        // Codex on #23: Letter pages 1-3 carry the header at y=750, 42 pt
        // below the top. Full Legal page 4 has the same sentence at y=750
        // beside a numbered clause, so that row is protected body text. It
        // must not make the bottom the edge: on sparse Legal page 5 the
        // sentence at y=750 stays, and 42 pt below the top (y=966) it goes.
        let header = "Please sign and return this form";
        let frames: HashMap<u32, (f32, f32)> = (1..=5)
            .map(|page| (page, (0.0, if page >= 4 { 1008.0 } else { 792.0 })))
            .collect();
        let with_page_5 = |y: f32| {
            let mut lines = headed_pages(4, |_| header.into());
            lines.push(make_line("1. The parties agree.", 10.0, 4, 750.0, None));
            lines.push(make_line(header, 10.0, 5, y, None));
            lines.push(make_line("Thank you for your order", 10.0, 5, 380.0, None));
            strip_repeated_lines_in_frames(lines, 5, &frames)
        };
        assert_eq!(header_pages(&with_page_5(750.0), header), [1, 4, 5]);
        assert_eq!(header_pages(&with_page_5(966.0), header), [1, 4]);
    }

    #[test]
    fn a_protected_row_does_not_make_up_the_repeat() {
        // Codex on #23: the header on Letter pages 1-2 and the same sentence
        // beside a numbered clause on full Legal page 3. The protected row
        // counts towards the three-page repeat, but the two other pages
        // alone are no repeat, so sparse Legal page 4 loses nothing, not
        // even the sentence 42 pt below its top.
        let header = "Please sign and return this form";
        let frames: HashMap<u32, (f32, f32)> = (1..=4)
            .map(|page| (page, (0.0, if page >= 3 { 1008.0 } else { 792.0 })))
            .collect();
        let mut lines = headed_pages(3, |_| header.into());
        lines.push(make_line("1. The parties agree.", 10.0, 3, 750.0, None));
        lines.push(make_line(header, 10.0, 4, 966.0, None));
        lines.push(make_line("Thank you for your order", 10.0, 4, 380.0, None));
        let result = strip_repeated_lines_in_frames(lines, 4, &frames);
        assert!(header_pages(&result, header).contains(&4));
    }

    #[test]
    fn a_sparse_row_exactly_at_the_cutoff_stays() {
        // Codex on #23: with the header at y=715 over body text down to
        // y=315, the full pages span exactly 400 pt and the tolerance is
        // exactly 20 pt. A sparse row 20 pt from the learned place is not
        // within it, with or without frames; 19 pt is.
        let header = "Please sign and return this form";
        let lines = |y: f32| {
            let mut lines = headed_pages(3, |_| header.into());
            for line in lines.iter_mut().filter(|line| line.y == 750.0) {
                line.y = 715.0;
            }
            lines.push(make_line(header, 10.0, 4, y, None));
            lines.push(make_line("Thank you for your order", 10.0, 4, 380.0, None));
            lines
        };
        let frames: HashMap<u32, (f32, f32)> = (1..=4).map(|page| (page, (0.0, 792.0))).collect();
        for frames in [HashMap::new(), frames] {
            let result = strip_repeated_lines_in_frames(lines(695.0), 4, &frames);
            assert_eq!(header_pages(&result, header), [1, 4]);
            let result = strip_repeated_lines_in_frames(lines(696.0), 4, &frames);
            assert_eq!(header_pages(&result, header), [1]);
        }
    }

    #[test]
    fn a_row_exactly_halfway_down_is_measured_from_the_bottom() {
        // Codex on lawkitt/mdoc#967: Letter pages 1-3 end with a repeated
        // row at y=396, exactly halfway down (396 pt from either edge). The
        // pages agree on both edges equally, so neither is the nearer; as
        // before M17, the place is measured from the bottom. On sparse Legal
        // page 4 that is y=396, not 612 (396 pt below its top): the row at
        // 396 goes and the one at 612 stays.
        let footer = "Please sign and return this form";
        let mut lines = Vec::new();
        for page in 1..=3u32 {
            for j in 0..20u32 {
                lines.push(make_line(
                    &format!("уникальный текст страницы {page}, строка {j}, body"),
                    10.0,
                    page,
                    700.0 - j as f32 * 15.0,
                    None,
                ));
            }
            lines.push(make_line(footer, 10.0, page, 396.0, None));
        }
        lines.push(make_line(footer, 10.0, 4, 612.0, None));
        lines.push(make_line(footer, 10.0, 4, 396.0, None));
        let mut frames = letter_frames(3);
        frames.insert(4, (0.0, 1008.0));
        let result = strip_repeated_lines_in_frames(lines.clone(), 4, &frames);
        assert_eq!(kept_at(&result, 4, footer), [612.0]);
        assert_eq!(header_pages(&result, footer), [1, 4]);
        // Codex on #24: on a sparse Letter page the place is the midpoint
        // itself, a footer's half too, so of two rows 10 pt either side the
        // lower one goes, with or without the frames.
        let mut lines: Vec<TextLine> = lines.into_iter().filter(|l| l.page != 4).collect();
        lines.push(make_line(footer, 10.0, 4, 406.0, None));
        lines.push(make_line(footer, 10.0, 4, 386.0, None));
        for frames in [HashMap::new(), letter_frames(4)] {
            let result = strip_repeated_lines_in_frames(lines.clone(), 4, &frames);
            assert_eq!(kept_at(&result, 4, footer), [406.0]);
        }
    }

    /// `pages` pages of 20 unique body lines, each topped by `header` at
    /// y=750 and footed by `footer` at y=50.
    fn headed_and_footed_pages(pages: u32, header: &str, footer: &str) -> Vec<TextLine> {
        let mut lines = headed_pages(pages, |_| header.into());
        for page in 1..=pages {
            lines.push(make_line(footer, 10.0, page, 50.0, None));
        }
        lines.sort_by_key(|line| line.page);
        lines
    }

    /// The heights at which `needle` stays on `page`.
    fn kept_at(result: &[TextLine], page: u32, needle: &str) -> Vec<f32> {
        result
            .iter()
            .filter(|l| l.page == page && l.text().contains(needle))
            .map(|l| l.y)
            .collect()
    }

    /// Letter frames for pages 1..=`pages`.
    fn letter_frames(pages: u32) -> HashMap<u32, (f32, f32)> {
        (1..=pages).map(|page| (page, (0.0, 792.0))).collect()
    }

    #[test]
    fn a_sparse_page_loses_only_the_match_closest_to_the_header() {
        // Maintainer decision on mdoc#925 (M18): the header of pages 1-3
        // (y=750) stands on sparse page 4 at its place and again as a body
        // sentence 15 pt lower, both inside the 21.75 pt tolerance. Only the
        // match closest to the place goes; the sentence stays. Before, both
        // went. The same with and without the pages' frames, whichever line
        // comes first.
        let header = "Please sign and return this form";
        let sparse = |rows: &[f32], frames: &HashMap<u32, (f32, f32)>| {
            let mut lines = headed_pages(3, |_| header.into());
            for &y in rows {
                lines.push(make_line(header, 10.0, 4, y, None));
            }
            strip_repeated_lines_in_frames(lines, 4, frames)
        };
        for frames in [HashMap::new(), letter_frames(4)] {
            let result = sparse(&[750.0, 735.0], &frames);
            assert_eq!(kept_at(&result, 4, header), [735.0]);
            assert_eq!(header_pages(&result, header), [1, 4]);
            assert_eq!(
                kept_at(&sparse(&[735.0, 750.0], &frames), 4, header),
                [735.0]
            );
            // Above the place, the nearer one goes as well.
            assert_eq!(
                kept_at(&sparse(&[762.0, 745.0], &frames), 4, header),
                [762.0]
            );
            // A match alone within the tolerance still goes, as before.
            assert!(kept_at(&sparse(&[740.0], &frames), 4, header).is_empty());
            // Neither close enough: both stay, as before.
            assert_eq!(
                kept_at(&sparse(&[700.0, 380.0], &frames), 4, header),
                [700.0, 380.0]
            );
        }
    }

    #[test]
    fn a_sparse_page_of_another_size_loses_only_the_closest_match() {
        // M18 on mixed sizes. Letter pages 1-3 carry the header 42 pt below
        // the top; on sparse Legal page 4 that is y=966. The match there
        // goes, the sentence 16 pt lower stays.
        let header = "Please sign and return this form";
        let mut frames = letter_frames(3);
        frames.insert(4, (0.0, 1008.0));
        let sparse = |rows: &[f32]| {
            let mut lines = headed_pages(3, |_| header.into());
            for &y in rows {
                lines.push(make_line(header, 10.0, 4, y, None));
            }
            strip_repeated_lines_in_frames(lines, 4, &frames)
        };
        assert_eq!(kept_at(&sparse(&[966.0, 950.0]), 4, header), [950.0]);
        assert_eq!(kept_at(&sparse(&[950.0, 970.0]), 4, header), [950.0]);
        // Letter pages 1-3 and a full Legal page 4 agree from the bottom
        // (M17): on sparse Letter page 5 the place is y=750.
        frames.insert(5, (0.0, 792.0));
        let mut lines = headed_pages(4, |_| header.into());
        lines.push(make_line(header, 10.0, 5, 735.0, None));
        lines.push(make_line(header, 10.0, 5, 750.0, None));
        let result = strip_repeated_lines_in_frames(lines, 5, &frames);
        assert_eq!(kept_at(&result, 5, header), [735.0]);
    }

    #[test]
    fn of_two_equally_close_matches_the_outer_one_goes() {
        // M18 ties: two rows 10 pt either side of the place. A header's
        // place is in the page's upper half, so the row above it goes; a
        // footer's is in the lower half, so the row below it goes. Line
        // order does not matter, nor the frames.
        let header = "Please sign and return this form";
        let footer = "Confidential: Acme Holdings";
        let sparse = |rows: &[(&str, f32)], frames: &HashMap<u32, (f32, f32)>| {
            let mut lines = headed_and_footed_pages(3, header, footer);
            for &(text, y) in rows {
                lines.push(make_line(text, 10.0, 4, y, None));
            }
            strip_repeated_lines_in_frames(lines, 4, frames)
        };
        for frames in [HashMap::new(), letter_frames(4)] {
            for rows in [
                [(header, 760.0), (header, 740.0)],
                [(header, 740.0), (header, 760.0)],
            ] {
                assert_eq!(kept_at(&sparse(&rows, &frames), 4, header), [740.0]);
            }
            for rows in [
                [(footer, 60.0), (footer, 40.0)],
                [(footer, 40.0), (footer, 60.0)],
            ] {
                assert_eq!(kept_at(&sparse(&rows, &frames), 4, footer), [60.0]);
            }
        }
        // A header learned from the bottom edge (Letter pages and a full
        // Legal page, M17) is still a header: of two rows equally close to
        // its place on sparse Letter page 5, the upper one goes.
        let mut frames = letter_frames(5);
        frames.insert(4, (0.0, 1008.0));
        let mut lines = headed_pages(4, |_| header.into());
        lines.push(make_line(header, 10.0, 5, 740.0, None));
        lines.push(make_line(header, 10.0, 5, 760.0, None));
        let result = strip_repeated_lines_in_frames(lines, 5, &frames);
        assert_eq!(kept_at(&result, 5, header), [740.0]);
    }

    #[test]
    fn copies_in_one_row_do_not_outscore_the_header() {
        // Codex on #24: the header 10 pt above its place, and two copies of
        // the sentence in one row 10 pt below it (740 and 740.0625, one
        // clustered row at 740). The rows tie, so the upper row, the header,
        // goes; both copies stay.
        let header = "Please sign and return this form";
        for frames in [HashMap::new(), letter_frames(4)] {
            let mut lines = headed_pages(3, |_| header.into());
            lines.push(make_line(header, 10.0, 4, 760.0, None));
            lines.push(make_line(header, 10.0, 4, 740.0, None));
            lines.push(make_line(header, 10.0, 4, 740.0625, None));
            let result = strip_repeated_lines_in_frames(lines, 4, &frames);
            assert_eq!(kept_at(&result, 4, header), [740.0, 740.0625]);
        }
    }

    #[test]
    fn a_sparse_page_loses_its_header_and_its_footer() {
        // M18: one match per learned row. A sparse page whose two rows are
        // the header and the footer of the full pages loses both.
        let header = "Please sign and return this form";
        let footer = "Confidential: Acme Holdings";
        for frames in [HashMap::new(), letter_frames(4)] {
            let mut lines = headed_and_footed_pages(3, header, footer);
            lines.push(make_line(header, 10.0, 4, 750.0, None));
            lines.push(make_line(footer, 10.0, 4, 50.0, None));
            let result = strip_repeated_lines_in_frames(lines, 4, &frames);
            assert_eq!(header_pages(&result, header), [1]);
            assert_eq!(header_pages(&result, footer), [1]);
            assert!(!result.iter().any(|l| l.page == 4));
        }
    }

    #[test]
    fn the_closest_row_on_a_sparse_page_goes_whole() {
        // M18: the match is a row. The header set twice in its row (within
        // 0.1 pt, one physical row) goes whole; the sentence below stays.
        let header = "Please sign and return this form";
        let mut lines = headed_pages(3, |_| header.into());
        lines.push(make_line(header, 10.0, 4, 750.0, None));
        lines.push(make_line(header, 10.0, 4, 750.05, None));
        lines.push(make_line(header, 10.0, 4, 735.0, None));
        let result = strip_repeated_lines_in_frames(lines, 4, &letter_frames(4));
        assert_eq!(kept_at(&result, 4, header), [735.0]);
    }

    #[test]
    fn a_sparse_first_page_keeps_both_matches() {
        // M11's kept copy with M18: a sparse first page keeps every match,
        // and the first full page loses its header. A later sparse page
        // loses only the closest match.
        let header = "Please sign and return this form";
        let mut lines = vec![
            make_line(header, 10.0, 1, 750.0, None),
            make_line(header, 10.0, 1, 735.0, None),
        ];
        for line in headed_pages(4, |_| header.into()) {
            if line.page > 1 {
                lines.push(line);
            }
        }
        lines.push(make_line(header, 10.0, 5, 750.0, None));
        lines.push(make_line(header, 10.0, 5, 735.0, None));
        let result = strip_repeated_lines_in_frames(lines, 5, &letter_frames(5));
        assert_eq!(kept_at(&result, 1, header), [750.0, 735.0]);
        assert_eq!(header_pages(&result, header), [1, 1, 5]);
        assert_eq!(kept_at(&result, 5, header), [735.0]);
    }

    /// `pages` pages of 20 unique body lines, each topped by `header(page)`.
    fn headed_pages(pages: u32, header: impl Fn(u32) -> String) -> Vec<TextLine> {
        let mut lines = Vec::new();
        for page in 1..=pages {
            lines.push(make_line(&header(page), 10.0, page, 750.0, None));
            for j in 0..20u32 {
                lines.push(make_line(
                    &format!("уникальный текст страницы {page}, строка {j}, body"),
                    10.0,
                    page,
                    600.0 - j as f32 * 15.0,
                    None,
                ));
            }
        }
        lines
    }

    fn header_pages(result: &[TextLine], needle: &str) -> Vec<u32> {
        result
            .iter()
            .filter(|l| l.text().contains(needle))
            .map(|l| l.page)
            .collect()
    }

    #[test]
    fn exact_repeat_stays_on_its_first_page_only() {
        let result = strip_repeated_lines(headed_pages(5, |_| "Дело № 2-123/2026".into()), 5);
        assert_eq!(header_pages(&result, "Дело № 2-123/2026"), [1]);
        assert_eq!(result.len(), 5 * 20 + 1, "body lines all stay");
    }

    #[test]
    fn rows_that_differ_in_a_number_are_not_repeats() {
        // Numbers count (upstream #653): a case number that changes, inside
        // the row or at its end, makes a different row.
        let result = strip_repeated_lines(headed_pages(5, |p| format!("Дело № 2-12{p}/2026")), 5);
        assert_eq!(result.len(), 5 * 21);
        let cases = [0, 4171, 4382, 4593, 4604];
        let result = strip_repeated_lines(
            headed_pages(4, |p| format!("Case file number {}", cases[p as usize])),
            4,
        );
        assert_eq!(result.len(), 4 * 21, "a changing trailing number is kept");
        let result = strip_repeated_lines(
            headed_pages(4, |p| {
                format!("{} Exhibit list of the claimant", cases[p as usize])
            }),
            4,
        );
        assert_eq!(result.len(), 4 * 21, "a changing leading number is kept");
    }

    #[test]
    fn identifiers_joined_to_numbers_keep_their_digits() {
        // Upstream #653's reproduction: a running header with its page
        // number, over body codes that differ only in digits.
        let mut lines = Vec::new();
        for (page, code) in [(1u32, 401), (2, 502), (3, 603)] {
            let rows = [
                format!("Course workbook Page {page}"),
                format!("Textbook catalogue code EDU-{code}"),
                format!("{code}A identifies the worksheet"),
            ];
            for (row, text) in rows.iter().enumerate() {
                lines.push(make_line(text, 10.0, page, 750.0 - row as f32 * 26.0, None));
            }
            for j in 0..12u32 {
                lines.push(make_line(
                    &format!("Unique paragraph for page {page} with extra worksheet item {j}."),
                    10.0,
                    page,
                    600.0 - j as f32 * 25.0,
                    None,
                ));
            }
        }
        let result = strip_repeated_lines(lines, 3);
        for code in ["EDU-401", "EDU-502", "EDU-603", "401A", "502A", "603A"] {
            assert_eq!(header_pages(&result, code).len(), 1, "{code} must survive");
        }
        assert_eq!(header_pages(&result, "Course workbook"), [1]);
    }

    #[test]
    fn a_page_number_that_rises_with_the_page_still_repeats() {
        let result = strip_repeated_lines(
            headed_pages(5, |p| format!("Annual Report of the Board Page {p}")),
            5,
        );
        assert_eq!(header_pages(&result, "Annual Report of the Board"), [1]);
        let result = strip_repeated_lines(headed_pages(5, |p| format!("Annual Report {p}")), 5);
        assert_eq!(header_pages(&result, "Annual Report"), [1]);
        // Printed numbering may start after a cover and a contents page:
        // page 3 is printed 1, and the offset holds.
        let result = strip_repeated_lines(
            headed_pages(6, |p| match p {
                1 => "Договор поставки, титульный лист".into(),
                2 => "Содержание договора поставки".into(),
                _ => format!("{} Отчёт о работе за год", p - 2),
            }),
            6,
        );
        assert_eq!(header_pages(&result, "Отчёт о работе за год"), [3]);
    }

    #[test]
    fn identifiers_that_rise_with_the_page_are_no_page_numbers() {
        // Codex on mdoc#944: a document number one ahead per page keeps step
        // with the pages but is far ahead of them.
        let result = strip_repeated_lines(
            headed_pages(4, |p| format!("Acme Holdings Invoice {}", 4170 + p)),
            4,
        );
        assert_eq!(header_pages(&result, "Acme Holdings Invoice").len(), 4);
        let result = strip_repeated_lines(
            headed_pages(4, |p| format!("{} Acme Holdings statement", 4170 + p)),
            4,
        );
        assert_eq!(header_pages(&result, "Acme Holdings statement").len(), 4);
        // Numbering carried over from a longer document leads the page
        // index too, and is not taken for a page number either.
        let result =
            strip_repeated_lines(headed_pages(5, |p| format!("Annual Report {}", p + 2)), 5);
        assert_eq!(header_pages(&result, "Annual Report").len(), 5);
        // A labelled number is an identifier even when it equals the page
        // index: one invoice, case or annex per page.
        for label in [
            "Acme Holdings Invoice No.",
            "Acme Holdings invoice",
            "Арбитражный суд, дело №",
            "Арбитражный суд, Дело",
            "Счёт на оплату №",
            "Договор поставки, приложение",
            "Supply contract, Case #",
        ] {
            let result = strip_repeated_lines(headed_pages(4, |p| format!("{label} {p}")), 4);
            assert_eq!(header_pages(&result, label).len(), 4, "{label} N must stay");
        }
    }

    #[test]
    fn a_number_that_does_not_follow_the_page_is_no_page_number() {
        // Same row shape, numbers out of step with the pages: different rows.
        let numbers = [0, 7, 3, 9, 4];
        let result = strip_repeated_lines(
            headed_pages(4, |p| {
                format!("Annual Report of the Board {}", numbers[p as usize])
            }),
            4,
        );
        assert_eq!(header_pages(&result, "Annual Report of the Board").len(), 4);
    }

    #[test]
    fn a_learned_page_number_must_keep_step_on_every_page() {
        // Codex on #18: three full pages teach "Annual Report N"; a sparse
        // page 4, too short to teach anything, shows a different number.
        let mut lines = headed_pages(3, |p| format!("Annual Report {p}"));
        lines.push(make_line("Annual Report 4174", 10.0, 4, 750.0, None));
        lines.push(make_line(
            "уникальный текст страницы 4",
            10.0,
            4,
            600.0,
            None,
        ));
        let result = strip_repeated_lines(lines, 4);
        assert_eq!(header_pages(&result, "Annual Report"), [1, 4]);
        assert!(result.iter().any(|l| l.text() == "Annual Report 4174"));
    }

    #[test]
    fn a_changing_identifier_beside_a_repeated_fragment_stays() {
        // Codex on #18: one header row in two pieces on one baseline. The
        // left piece repeats; the right one changes and is the page's own.
        let mut lines = headed_pages(4, |_| "Acme Holdings Quarterly".into());
        for page in 1..=4u32 {
            let mut invoice = make_line(
                &format!("Invoice No. {}", 4170 + page),
                10.0,
                page,
                750.0,
                None,
            );
            invoice.items[0].x = 400.0;
            lines.push(invoice);
            // A bare page number on the same row goes with the header.
            let mut folio = make_line(&page.to_string(), 10.0, page, 750.0, None);
            folio.items[0].x = 540.0;
            lines.push(folio);
        }
        let result = strip_repeated_lines(lines, 4);
        assert_eq!(header_pages(&result, "Acme Holdings Quarterly"), [1]);
        assert_eq!(header_pages(&result, "Invoice No."), [1, 2, 3, 4]);
        let folios: Vec<u32> = result
            .iter()
            .filter(|l| l.y == 750.0 && l.text().chars().all(|c| c.is_ascii_digit()))
            .map(|l| l.page)
            .collect();
        assert_eq!(folios, [1]);
    }

    #[test]
    fn a_repeat_needs_at_least_30_percent_of_the_pages() {
        // Codex on mdoc#944: 3 of 11 pages are 27%, below the rule's 30%;
        // 4 of 11 are 36%.
        let header = |on: u32| {
            move |p: u32| {
                if p <= on {
                    "Confidential: Acme Holdings".to_string()
                } else {
                    format!("Section heading of page {p} differs")
                }
            }
        };
        let result = strip_repeated_lines(headed_pages(11, header(3)), 11);
        assert_eq!(header_pages(&result, "Acme Holdings"), [1, 2, 3]);
        let result = strip_repeated_lines(headed_pages(11, header(4)), 11);
        assert_eq!(header_pages(&result, "Acme Holdings"), [1]);
        // 3 of 10 pages are 30%, enough.
        let result = strip_repeated_lines(headed_pages(10, header(3)), 10);
        assert_eq!(header_pages(&result, "Acme Holdings"), [1]);
    }

    #[test]
    fn rows_that_differ_only_in_spacing_repeat() {
        // OCR splits words inconsistently: whitespace is no part of a row.
        let spellings = [
            "",
            "Confidential: Acme Holdings",
            "Confi dential: Acme Holdings",
        ];
        let result = strip_repeated_lines(
            headed_pages(4, |p| spellings[1 + (p as usize % 2)].into()),
            4,
        );
        assert_eq!(header_pages(&result, "Acme Holdings"), [1]);
    }

    #[test]
    fn spacing_variants_share_one_page_number_check() {
        // Two spellings of a row, each numbered in step with the page on its
        // own pages, but out of step together: different case numbers.
        let headers = [
            "",
            "Case file number 4171",
            "Case file number 4172",
            "Case file num ber 4382",
            "Case file num ber 4383",
        ];
        let result = strip_repeated_lines(headed_pages(4, |p| headers[p as usize].into()), 4);
        assert_eq!(header_pages(&result, "Case file num").len(), 4);
    }
}
