//! Geometry-aware OCR Markdown assembly and native-text fusion.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::Instant;

use thiserror::Error;

use crate::markdown::{
    complete_table_markdown_from_items, to_markdown_from_items_with_rects_and_page_count,
    MarkdownOptions,
};
use crate::text_quality::{detect_encoding_issues, is_cid_garbage, is_garbage_text};
use crate::types::{ItemType, PdfRect, TextItem};
use crate::PageMarkdown;

use super::{
    OcrRun, PageContentSource, PageProvenance, RoutedOcrPage, VisionTimings, DEFAULT_RENDER_DPI,
};

/// OCR assembly and hosted-fallback policy.
#[derive(Debug, Clone)]
pub struct OcrFusionOptions {
    /// Markdown conversion options applied to positioned OCR spans.
    pub markdown: MarkdownOptions,
    /// Render resolution recorded in page provenance.
    pub render_dpi: f32,
    /// Recommend the hosted pipeline below this mean confidence when native
    /// extraction already marked the page as requiring OCR.
    pub hosted_recommendation_confidence: f32,
}

impl Default for OcrFusionOptions {
    fn default() -> Self {
        Self {
            markdown: MarkdownOptions {
                include_page_numbers: false,
                strip_headers_footers: false,
                ..MarkdownOptions::default()
            },
            render_dpi: DEFAULT_RENDER_DPI,
            hosted_recommendation_confidence: 0.5,
        }
    }
}

impl OcrFusionOptions {
    /// Creates OCR fusion options with local defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces Markdown conversion options.
    pub fn markdown(mut self, markdown: MarkdownOptions) -> Self {
        self.markdown = markdown;
        self
    }

    /// Records the renderer resolution in provenance.
    pub fn render_dpi(mut self, render_dpi: f32) -> Self {
        self.render_dpi = render_dpi;
        self
    }

    /// Sets the weak-OCR confidence threshold for recommending hosted parsing.
    pub fn hosted_recommendation_confidence(mut self, confidence: f32) -> Self {
        self.hosted_recommendation_confidence = confidence;
        self
    }

    /// Validates rendering and hosted-fallback thresholds without doing work.
    pub fn validate(&self) -> Result<(), OcrFusionError> {
        validate_options(self)
    }
}

/// Final Markdown and provenance for one page.
#[derive(Debug, Clone, PartialEq)]
pub struct FusedPageMarkdown {
    /// 1-indexed document page number, matching OCR and provenance fields.
    pub page_number: u32,
    /// Final page Markdown.
    pub markdown: String,
    /// Native/OCR source, model, timing, and fallback metadata.
    pub provenance: PageProvenance,
}

/// Output of fusing a selective OCR run into native page extraction.
#[derive(Debug, Clone, PartialEq)]
pub struct FusedPages {
    /// Pages in the same order as the native input.
    pub pages: Vec<FusedPageMarkdown>,
    /// Batch rendering wall time from the OCR run.
    pub render_time_ms: u64,
    /// Batch OCR wall time from the OCR run.
    pub ocr_time_ms: u64,
}

/// Origin of a trustworthy native-text candidate retained for adaptive OCR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeCandidateOrigin {
    /// Text produced by pdf-inspector's normal native extractor.
    Extractor,
    /// Positioned text independently recovered through PDFium.
    Pdfium,
}

impl NativeCandidateOrigin {
    fn description(self) -> &'static str {
        match self {
            Self::Extractor => "native extraction",
            Self::Pdfium => "PDFium native recovery",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct TextCandidateQuality {
    alphanumeric_chars: usize,
    score: f32,
}

/// Clean native text retained while an ambiguous page is compared with OCR.
#[derive(Debug, Clone)]
pub(crate) struct NativeFallbackCandidate {
    markdown: String,
    quality: TextCandidateQuality,
    origin: NativeCandidateOrigin,
}

/// How OCR output for a routed page may participate in final fusion.
#[derive(Debug, Clone)]
pub(crate) enum OcrFusionRoute {
    /// The page requires ordinary full-page OCR replacement or adaptive fusion.
    FullPage,
    /// The native page is clean; only tables detected inside these image
    /// regions may be appended.
    SupplementalRegions(Vec<PdfRect>),
}

impl NativeFallbackCandidate {
    /// True when an independent native recovery is substantial enough to
    /// cancel OCR for recoverable font/vector routing reasons.
    pub(crate) fn is_complete_recovery(&self) -> bool {
        self.quality.alphanumeric_chars >= 40 && self.quality.score >= 0.68
    }

    pub(crate) fn markdown(&self) -> &str {
        &self.markdown
    }

    pub(crate) fn is_stronger_than(&self, other: &Self) -> bool {
        self.quality.alphanumeric_chars > other.quality.alphanumeric_chars
            || (self.quality.alphanumeric_chars == other.quality.alphanumeric_chars
                && self.quality.score > other.quality.score)
    }
}

/// Validates and scores native Markdown for possible post-OCR comparison.
///
/// This intentionally uses script-agnostic evidence. A native candidate only
/// needs to be trustworthy, not necessarily complete: a clean native header
/// can still be fused with an image-backed OCR body.
pub(crate) fn assess_native_candidate(
    markdown: String,
    origin: NativeCandidateOrigin,
) -> Option<NativeFallbackCandidate> {
    let quality = assess_text_candidate(&markdown)?;
    (quality.alphanumeric_chars >= 8).then_some(NativeFallbackCandidate {
        markdown,
        quality,
        origin,
    })
}

/// Converts positioned OCR spans to Markdown through pdf-inspector's existing
/// deterministic geometry, reading-order, table, and Markdown pipeline.
///
/// The page number is 1-indexed. `document_page_count` prevents a selected
/// page from being mistaken for a one-page document during page-number logic.
pub fn ocr_page_to_markdown(
    page: &RoutedOcrPage,
    document_page_count: u32,
    options: &MarkdownOptions,
) -> String {
    let (items, _) = ocr_text_items(page, &HashSet::new(), true);
    let markdown = to_markdown_from_items_with_rects_and_page_count(
        items,
        options.clone(),
        &[],
        document_page_count,
    );
    preserve_ocr_line_breaks(&markdown, page)
}

/// Fuses a selective OCR run into per-page native Markdown.
///
/// OCR replaces pages whose native extraction was already rejected. On clean
/// native pages (for example in `Force` mode), normalized duplicate OCR blocks
/// are removed and only genuinely additional blocks are appended. Pages that
/// needed OCR but still have no credible OCR result, or whose confident OCR
/// only repeats an incomplete native fragment, recommend the hosted document
/// pipeline instead of silently presenting partial content as final.
pub fn fuse_ocr_pages(
    native_pages: &[PageMarkdown],
    ocr_run: &OcrRun,
    document_page_count: u32,
    options: &OcrFusionOptions,
) -> Result<FusedPages, OcrFusionError> {
    let routes = full_page_routes(ocr_run);
    fuse_ocr_pages_impl(
        native_pages,
        ocr_run,
        document_page_count,
        options,
        &BTreeMap::new(),
        &routes,
        &BTreeMap::new(),
    )
}

/// OCR-pipeline fusion with trustworthy partial native candidates.
#[cfg(test)]
pub(crate) fn fuse_ocr_pages_adaptive(
    native_pages: &[PageMarkdown],
    ocr_run: &OcrRun,
    document_page_count: u32,
    options: &OcrFusionOptions,
    native_candidates: &BTreeMap<u32, NativeFallbackCandidate>,
) -> Result<FusedPages, OcrFusionError> {
    let routes = full_page_routes(ocr_run);
    fuse_ocr_pages_impl(
        native_pages,
        ocr_run,
        document_page_count,
        options,
        native_candidates,
        &routes,
        &BTreeMap::new(),
    )
}

fn full_page_routes(ocr_run: &OcrRun) -> BTreeMap<u32, OcrFusionRoute> {
    ocr_run
        .pages
        .iter()
        .map(|page| (page.rendered.page(), OcrFusionRoute::FullPage))
        .collect()
}

/// OCR-pipeline fusion with an explicit route mode for every processed page.
/// `native_repeated` holds the rows native extraction left out of each
/// page as repeats (their text), so each page counts what its final content
/// left out.
pub(crate) fn fuse_ocr_pages_adaptive_with_routes(
    native_pages: &[PageMarkdown],
    ocr_run: &OcrRun,
    document_page_count: u32,
    options: &OcrFusionOptions,
    native_candidates: &BTreeMap<u32, NativeFallbackCandidate>,
    routes: &BTreeMap<u32, OcrFusionRoute>,
    native_repeated: &BTreeMap<u32, Vec<String>>,
) -> Result<FusedPages, OcrFusionError> {
    fuse_ocr_pages_impl(
        native_pages,
        ocr_run,
        document_page_count,
        options,
        native_candidates,
        routes,
        native_repeated,
    )
}

fn fuse_ocr_pages_impl(
    native_pages: &[PageMarkdown],
    ocr_run: &OcrRun,
    document_page_count: u32,
    options: &OcrFusionOptions,
    native_candidates: &BTreeMap<u32, NativeFallbackCandidate>,
    routes: &BTreeMap<u32, OcrFusionRoute>,
    native_repeated: &BTreeMap<u32, Vec<String>>,
) -> Result<FusedPages, OcrFusionError> {
    validate_options(options)?;

    let mut native_numbers = BTreeSet::new();
    for page in native_pages {
        let page_number = page
            .page
            .checked_add(1)
            .ok_or(OcrFusionError::PageOverflow)?;
        if !native_numbers.insert(page_number) {
            return Err(OcrFusionError::DuplicateNativePage { page: page_number });
        }
    }

    let mut ocr_by_page = BTreeMap::new();
    for page in &ocr_run.pages {
        let page_number = page.rendered.page();
        if !native_numbers.contains(&page_number) {
            return Err(OcrFusionError::UnexpectedOcrPage { page: page_number });
        }
        if ocr_by_page.insert(page_number, page).is_some() {
            return Err(OcrFusionError::DuplicateOcrPage { page: page_number });
        }
    }

    let repeated = if options.markdown.strip_headers_footers {
        repeated_ocr_header_footer_spans(native_pages, ocr_run, routes, native_candidates)
    } else {
        RepeatedOcrSpans::default()
    };
    // Page furniture on OCR text is judged across pages only, by the pass
    // above: a box height is no font size, so the per-page typographic
    // fallback for short documents would guess.
    let mut ocr_markdown_options = options.markdown.clone();
    ocr_markdown_options.strip_headers_footers = false;
    let no_spans = HashSet::new();

    let render_shares = equal_time_shares(ocr_run.render_time_ms, ocr_run.pages.len());
    let render_by_page: BTreeMap<u32, u64> = ocr_run
        .pages
        .iter()
        .zip(render_shares)
        .map(|(page, share)| (page.rendered.page(), share))
        .collect();

    let mut pages = Vec::with_capacity(native_pages.len());
    for native in native_pages {
        let page_number = native.page + 1;
        let assembly_started = Instant::now();
        let mut warnings = Vec::new();

        let (markdown, source, ocr_model, ocr_confidence, ocr_ms, hosted_recommended) =
            if let Some(local) = ocr_by_page.get(&page_number) {
                let route = routes
                    .get(&page_number)
                    .ok_or(OcrFusionError::MissingOcrRoute { page: page_number })?;
                let repeated_spans = repeated.spans.get(&page_number).unwrap_or(&no_spans);
                // Image regions are matched against native PDF-space
                // rectangles, so their spans keep the rendered page's own
                // geometry; only full-page OCR is laid out level and upright.
                let (ocr_items, discarded_spans) = ocr_text_items(
                    local,
                    repeated_spans,
                    matches!(route, OcrFusionRoute::FullPage),
                );
                if discarded_spans > 0 {
                    warnings.push(format!(
                        "discarded {discarded_spans} OCR spans with unusable text or geometry"
                    ));
                }
                warnings.extend(local.ocr.warnings.iter().cloned());
                let ocr_markdown = match route {
                    OcrFusionRoute::SupplementalRegions(regions) => {
                        let region_items = items_inside_regions(ocr_items, regions);
                        let table_markdown = complete_table_markdown_from_items(
                            region_items,
                            options.markdown.clone(),
                            document_page_count,
                        );
                        if table_markdown.is_empty() {
                            warnings.push(
                                "image-region OCR found no valid table; kept native content"
                                    .to_string(),
                            );
                        }
                        table_markdown
                    }
                    OcrFusionRoute::FullPage => {
                        let markdown = to_markdown_from_items_with_rects_and_page_count(
                            ocr_items,
                            ocr_markdown_options.clone(),
                            &[],
                            document_page_count,
                        );
                        preserve_ocr_line_breaks_without(&markdown, local, repeated_spans)
                    }
                };
                let (markdown, source, adaptive_recommends_hosted) = if let Some(candidate) =
                    native_candidates
                        .get(&page_number)
                        .filter(|_| native.needs_ocr)
                {
                    let choice = choose_adaptive_content(
                        candidate,
                        &ocr_markdown,
                        local.ocr.mean_confidence,
                        options.hosted_recommendation_confidence,
                    );
                    warnings.push(choice.warning);
                    (choice.markdown, choice.source, choice.recommend_hosted)
                } else if native.markdown.trim().is_empty() || native.needs_ocr {
                    (ocr_markdown, PageContentSource::Ocr, false)
                } else {
                    let (markdown, source) = merge_native_and_ocr(&native.markdown, &ocr_markdown);
                    (markdown, source, false)
                };
                let weak_ocr = local
                    .ocr
                    .mean_confidence
                    .is_none_or(|confidence| confidence < options.hosted_recommendation_confidence);
                let recommend_hosted = native.needs_ocr
                    && (markdown.trim().is_empty() || weak_ocr || adaptive_recommends_hosted);
                if native.needs_ocr && markdown.trim().is_empty() {
                    warnings.push("OCR produced no usable text".to_string());
                }
                (
                    markdown,
                    source,
                    Some(local.ocr.model.clone()),
                    local.ocr.mean_confidence,
                    local.ocr.processing_time_ms,
                    recommend_hosted,
                )
            } else {
                if native.needs_ocr {
                    warnings.push("page was recommended for OCR but was not processed".to_string());
                }
                (
                    native.markdown.clone(),
                    PageContentSource::Native,
                    None,
                    None,
                    0,
                    native.needs_ocr,
                )
            };

        let repeated_lines = repeated_lines_left_out(
            source,
            &markdown,
            native_repeated
                .get(&page_number)
                .map_or(&[][..], Vec::as_slice),
            repeated
                .rows
                .get(&page_number)
                .map_or(&[][..], Vec::as_slice),
        );
        pages.push(FusedPageMarkdown {
            page_number,
            markdown,
            provenance: PageProvenance {
                page_number,
                source,
                ocr_model,
                render_dpi: ocr_by_page
                    .contains_key(&page_number)
                    .then_some(options.render_dpi),
                ocr_confidence,
                timings: VisionTimings {
                    render_ms: render_by_page.get(&page_number).copied().unwrap_or(0),
                    ocr_ms,
                    assembly_ms: elapsed_ms(assembly_started),
                },
                warnings,
                repeated_lines,
                hosted_recommended,
            },
        });
    }

    Ok(FusedPages {
        pages,
        render_time_ms: ocr_run.render_time_ms,
        ocr_time_ms: ocr_run.ocr_time_ms,
    })
}

struct AdaptiveContentChoice {
    markdown: String,
    source: PageContentSource,
    warning: String,
    recommend_hosted: bool,
}

fn choose_adaptive_content(
    native: &NativeFallbackCandidate,
    ocr: &str,
    ocr_confidence: Option<f32>,
    weak_ocr_threshold: f32,
) -> AdaptiveContentChoice {
    let ocr_quality = assess_text_candidate(ocr);
    let weak_ocr = ocr_confidence.is_none_or(|confidence| confidence < weak_ocr_threshold);
    if weak_ocr || ocr_quality.is_none() {
        return AdaptiveContentChoice {
            markdown: ensure_trailing_newline(native.markdown()),
            source: PageContentSource::Native,
            warning: format!(
                "kept trustworthy {} because OCR was weak or unusable",
                native.origin.description()
            ),
            recommend_hosted: true,
        };
    }

    let ocr_quality = ocr_quality.expect("checked above");
    let overlap = content_overlap(native.markdown(), ocr);
    let ocr_novel_chars = ocr_quality
        .alphanumeric_chars
        .saturating_sub(overlap.shared_chars);
    let material_novelty =
        ocr_novel_chars >= 2 && ocr_novel_chars * 8 >= ocr_quality.alphanumeric_chars.max(1);
    let native_substantially_covered =
        overlap.shared_chars * 4 >= native.quality.alphanumeric_chars.max(1) * 3;

    if native_substantially_covered && !material_novelty {
        return AdaptiveContentChoice {
            markdown: ensure_trailing_newline(native.markdown()),
            source: PageContentSource::Native,
            warning: format!(
                "kept trustworthy {} because OCR added no material coverage",
                native.origin.description()
            ),
            // The candidate exists only because this page was routed with
            // incomplete native coverage. Agreement between two partial
            // hypotheses preserves trustworthy text, but does not prove that
            // the rest of the page was recovered.
            recommend_hosted: true,
        };
    }

    // A shorter, materially lower-quality OCR hypothesis should not displace
    // exact native text even when its mean confidence happens to be high.
    if ocr_quality.score + 0.12 < native.quality.score
        && ocr_quality.alphanumeric_chars * 10
            <= native.quality.alphanumeric_chars.saturating_mul(11)
    {
        return AdaptiveContentChoice {
            markdown: ensure_trailing_newline(native.markdown()),
            source: PageContentSource::Native,
            warning: format!(
                "kept higher-quality {} after comparing OCR",
                native.origin.description()
            ),
            recommend_hosted: false,
        };
    }

    let (markdown, source) = merge_native_and_ocr(native.markdown(), ocr);
    let warning = match source {
        PageContentSource::Native => format!(
            "kept trustworthy {} because OCR duplicated its content",
            native.origin.description()
        ),
        PageContentSource::Fused => format!(
            "fused trustworthy {} with complementary OCR",
            native.origin.description()
        ),
        PageContentSource::Ocr => unreachable!("merge never returns OCR-only content"),
    };
    AdaptiveContentChoice {
        markdown,
        source,
        warning,
        recommend_hosted: false,
    }
}

#[derive(Debug, Clone, Copy)]
struct ContentOverlap {
    shared_chars: usize,
}

fn content_overlap(first: &str, second: &str) -> ContentOverlap {
    let mut first_counts = BTreeMap::<char, usize>::new();
    for character in normalized_content_chars(first) {
        *first_counts.entry(character).or_insert(0) += 1;
    }
    let mut second_counts = BTreeMap::<char, usize>::new();
    for character in normalized_content_chars(second) {
        *second_counts.entry(character).or_insert(0) += 1;
    }
    let shared_chars = first_counts
        .iter()
        .map(|(character, count)| (*count).min(second_counts.get(character).copied().unwrap_or(0)))
        .sum();
    ContentOverlap { shared_chars }
}

fn normalized_content_chars(text: &str) -> impl Iterator<Item = char> + '_ {
    text.chars()
        .flat_map(char::to_lowercase)
        .filter(|character| character.is_alphanumeric())
}

fn assess_text_candidate(markdown: &str) -> Option<TextCandidateQuality> {
    if markdown.trim().is_empty()
        || is_garbage_text(markdown)
        || is_cid_garbage(markdown)
        || detect_encoding_issues(markdown)
    {
        return None;
    }

    let alphanumeric_chars = markdown
        .chars()
        .filter(|character| character.is_alphanumeric())
        .count();
    if alphanumeric_chars == 0 {
        return None;
    }
    let visible_chars = markdown
        .chars()
        .filter(|character| !character.is_whitespace())
        .count()
        .max(1);
    let density = alphanumeric_chars as f32 / visible_chars as f32;
    let length_score = (alphanumeric_chars as f32 / 160.0).min(1.0);
    let nonempty_lines = markdown
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
        .max(1);
    let line_score = (alphanumeric_chars as f32 / nonempty_lines as f32 / 12.0).min(1.0);
    let score = (0.45 + length_score * 0.25 + density * 0.20 + line_score * 0.10).min(1.0);
    Some(TextCandidateQuality {
        alphanumeric_chars,
        score,
    })
}

/// Converts recognized line polygons to ordinary PDF-space text items.
/// The page's OCR spans as positioned text items, leaving out `skip` (span
/// indices), and how many spans had unusable text or geometry.
///
/// With `layout`, lines are laid out as [`ocr_span_boxes`] describes and
/// their sizes normalized ([`snap_ocr_body_sizes`]); without it, each span
/// keeps the axis-aligned bounds of its polygon on the rendered page, so
/// the items stay in that page's PDF space.
fn ocr_text_items(
    page: &RoutedOcrPage,
    skip: &HashSet<usize>,
    layout: bool,
) -> (Vec<TextItem>, usize) {
    let (items, discarded) = indexed_ocr_text_items(page, skip, layout);
    (items.into_iter().map(|(_, item)| item).collect(), discarded)
}

fn indexed_ocr_text_items(
    page: &RoutedOcrPage,
    skip: &HashSet<usize>,
    layout: bool,
) -> (Vec<(usize, TextItem)>, usize) {
    let mut discarded = 0usize;
    let mut items = Vec::with_capacity(page.ocr.spans.len());
    let frame = if layout {
        ocr_span_boxes(page)
    } else {
        page_space_span_boxes(page)
    };
    for (index, (span, bounds)) in page.ocr.spans.iter().zip(&frame.boxes).enumerate() {
        if skip.contains(&index) {
            continue;
        }
        if span.text.trim().is_empty() {
            discarded += 1;
            continue;
        }
        let Some((left, top, right, bottom)) = *bounds else {
            discarded += 1;
            continue;
        };
        let rect = if frame.turns == 0 {
            page.rendered.pixel_rect_to_pdf_rect(
                f64::from(left),
                f64::from(top),
                f64::from(right - left),
                f64::from(bottom - top),
            )
        } else {
            // A page read turned upright is laid out as an upright page of
            // the same scale: its text runs left to right there, not on
            // the page as it is stored.
            let scale = page.rendered.page_width() / page.rendered.width() as f32;
            PdfRect {
                x: left * scale,
                y: (frame.height as f32 - bottom) * scale,
                width: (right - left) * scale,
                height: (bottom - top) * scale,
                page: page.rendered.page(),
            }
        };
        items.push((
            index,
            TextItem {
                text: span.text.trim().to_string(),
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
                font: "OCR".to_string(),
                font_tag: String::new(),
                legacy_symbol_rewrite: false,
                font_size: rect.height.max(1.0),
                page: page.rendered.page(),
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
            },
        ));
    }
    if layout {
        let (indices, mut plain): (Vec<usize>, Vec<TextItem>) = items.into_iter().unzip();
        snap_ocr_body_sizes(&mut plain);
        items = indices.into_iter().zip(plain).collect();
    }
    // OCR engines do not share an ordering contract. Geometry gives the
    // deterministic top-to-bottom seed expected by the existing layout
    // pipeline, which can still replace it with column-aware reading order.
    items.sort_by(|(_, first), (_, second)| {
        first
            .page
            .cmp(&second.page)
            .then(second.y.total_cmp(&first.y))
            .then(first.x.total_cmp(&second.x))
    });
    (items, discarded)
}

/// Repeated header/footer evidence across a run's full-page OCR.
#[derive(Debug, Default)]
struct RepeatedOcrSpans {
    /// 1-indexed page -> indices of the spans to leave out.
    spans: BTreeMap<u32, HashSet<usize>>,
    /// 1-indexed page -> the text of each row those spans formed.
    rows: BTreeMap<u32, Vec<String>>,
}

/// Rows the page's final content left out as repeats: those removed from
/// the source it kept. Fused content holds both sources, so a row removed
/// from one counts only when the other did not bring it back (whitespace
/// and Markdown escapes aside), and a row removed from both counts once.
fn repeated_lines_left_out(
    source: PageContentSource,
    markdown: &str,
    native_rows: &[String],
    ocr_rows: &[String],
) -> u32 {
    let count = |rows: usize| u32::try_from(rows).unwrap_or(u32::MAX);
    match source {
        PageContentSource::Native => count(native_rows.len()),
        PageContentSource::Ocr => count(ocr_rows.len()),
        PageContentSource::Fused => {
            let squeeze = |text: &str| -> String {
                text.chars()
                    .filter(|c| !c.is_whitespace() && *c != '\\')
                    .collect()
            };
            let shown = squeeze(markdown);
            let native: Vec<String> = native_rows.iter().map(|row| squeeze(row)).collect();
            let mut rows = native.clone();
            rows.extend(
                ocr_rows
                    .iter()
                    .map(|row| squeeze(row))
                    .filter(|row| !native.contains(row)),
            );
            count(
                rows.iter()
                    .filter(|row| !row.is_empty() && !shown.contains(row.as_str()))
                    .count(),
            )
        }
    }
}

/// Finds running headers and footers in full-page OCR by the native
/// document-wide repetition pass, over the recognized pages whose content
/// will be OCR: a row stays on the first such page that shows it, and its
/// exact repeats in the same place go, on three or more of those pages.
///
/// Left out of the pass, so they neither lose a row nor anchor one:
/// image-region OCR, which only adds to a native page; a page that may keep
/// a native candidate instead of its OCR, whose first occurrence would never
/// reach the output; and table rows (heuristic detection on the page's
/// spans), so a table's column headers repeated on each page stay with
/// their table, as whole-document conversion claims tables before it strips.
fn repeated_ocr_header_footer_spans(
    native_pages: &[PageMarkdown],
    ocr_run: &OcrRun,
    routes: &BTreeMap<u32, OcrFusionRoute>,
    native_candidates: &BTreeMap<u32, NativeFallbackCandidate>,
) -> RepeatedOcrSpans {
    let may_keep_native: BTreeSet<u32> = native_pages
        .iter()
        .filter(|page| page.needs_ocr && native_candidates.contains_key(&(page.page + 1)))
        .map(|page| page.page + 1)
        .collect();
    let full_pages: Vec<&RoutedOcrPage> = ocr_run
        .pages
        .iter()
        .filter(|page| {
            let number = page.rendered.page();
            matches!(routes.get(&number), Some(OcrFusionRoute::FullPage))
                && !may_keep_native.contains(&number)
        })
        .collect();
    let Ok(page_count) = u32::try_from(full_pages.len()) else {
        return RepeatedOcrSpans::default();
    };
    let mut spans_by_item: HashMap<crate::HeaderFooterItemKey, Vec<(u32, usize)>> = HashMap::new();
    let mut items = Vec::new();
    for page in full_pages {
        let (page_items, _) = indexed_ocr_text_items(page, &HashSet::new(), false);
        let plain: Vec<TextItem> = page_items.iter().map(|(_, item)| item.clone()).collect();
        let base_size =
            crate::markdown::analysis::calculate_font_stats_from_items(&plain).most_common_size;
        let in_tables: HashSet<usize> = crate::tables::detect_tables(&plain, base_size, false)
            .into_iter()
            .flat_map(|table| table.item_indices)
            .collect();
        for (position, (index, item)) in page_items.into_iter().enumerate() {
            if in_tables.contains(&position) {
                continue;
            }
            spans_by_item
                .entry(crate::HeaderFooterItemKey::from(&item))
                .or_default()
                .push((item.page, index));
            items.push(item);
        }
    }
    let repeated = crate::repeated_header_footer_items(
        &items,
        &HashMap::new(),
        &HashMap::new(),
        page_count,
        // As OCR Markdown conversion does: the calibrated folio bands.
        &HashMap::new(),
    );
    let mut result = RepeatedOcrSpans {
        spans: BTreeMap::new(),
        rows: repeated.rows_by_page,
    };
    for key in &repeated.items {
        for &(page, index) in spans_by_item.get(key).into_iter().flatten() {
            result.spans.entry(page).or_default().insert(index);
        }
    }
    result
}

/// Recognized lines from this share of the page's body height up to (not
/// including) [`OCR_BODY_SIZE_HIGH`] are body text. Detected boxes of one
/// type size differ with the line's capitals, ascenders and descenders, a
/// curl of the paper and a pixel or two of rounding. Native text has one
/// exact size.
const OCR_BODY_SIZE_LOW: f32 = 0.8;

/// Up to here a line of any length is body text: the Markdown converter's
/// size gate for headings is 1.2, and below it only bold text becomes a
/// heading, which OCR does not report.
const OCR_BODY_SIZE_HIGH: f32 = 1.2;

/// Up to here a line at least [`OCR_LONG_LINE_SHARE`] as long as the page's
/// median line, and set close above or below another line, is body text
/// too: a line inside a paragraph.
/// Long body lines of the scanned and photographed pages in mdoc's OCR
/// corpus measure up to 1.31 times their page's median height, while a
/// heading is rarely as long as the lines of the text under it, and a short
/// heading measured against long body lines comes out low (its box spans
/// only the capitals and ascenders it has).
const OCR_LONG_LINE_BODY_SIZE_HIGH: f32 = 1.35;

/// Share of the page's median line width from which a line counts as long.
const OCR_LONG_LINE_SHARE: f32 = 0.75;

/// Lines whose boxes are less than this share of the body height apart
/// belong to one block of text. A heading has the space of a paragraph
/// break around it.
const OCR_BLOCK_GAP_SHARE: f32 = 0.5;

/// Gives every line set at about the page's body height (the character-
/// weighted median) exactly that size, as native text of one font has.
/// Without it, a body line measured a few per cent short reads as smaller
/// type to the size-based rules (edge furniture, captions) and one measured
/// tall becomes a heading. Clearly larger or smaller type keeps its size.
fn snap_ocr_body_sizes(items: &mut [TextItem]) {
    let mut sizes: Vec<(f32, usize)> = items
        .iter()
        .map(|item| (item.font_size, item.text.chars().count()))
        .collect();
    sizes.sort_by(|a, b| a.0.total_cmp(&b.0));
    let total: usize = sizes.iter().map(|&(_, chars)| chars).sum();
    let mut running = 0usize;
    let Some(body) = sizes.iter().find_map(|&(size, chars)| {
        running += chars;
        (running * 2 >= total).then_some(size)
    }) else {
        return;
    };
    let mut widths: Vec<f32> = items.iter().map(|item| item.width).collect();
    widths.sort_by(f32::total_cmp);
    let typical_width = widths[widths.len() / 2];
    let in_block = |index: usize| {
        let item = &items[index];
        items.iter().enumerate().any(|(other_index, other)| {
            let overlap = (item.x + item.width).min(other.x + other.width) - item.x.max(other.x);
            let gap = (other.y - (item.y + item.height)).max(item.y - (other.y + other.height));
            other_index != index && overlap > 0.0 && gap < body * OCR_BLOCK_GAP_SHARE
        })
    };
    let snapped: Vec<bool> = (0..items.len())
        .map(|index| {
            let item = &items[index];
            let ratio = item.font_size / body;
            if (OCR_BODY_SIZE_LOW..OCR_BODY_SIZE_HIGH).contains(&ratio) {
                return true;
            }
            (OCR_BODY_SIZE_HIGH..OCR_LONG_LINE_BODY_SIZE_HIGH).contains(&ratio)
                && item.width >= typical_width * OCR_LONG_LINE_SHARE
                && in_block(index)
        })
        .collect();
    for (item, snapped) in items.iter_mut().zip(snapped) {
        if snapped {
            item.font_size = body;
        }
    }
}

type Edge = (f32, f32);

/// A quad's two pairs of opposite edges, `(along, across)`: the pair nearer
/// horizontal runs along the text line, the other across it. Along edges
/// point rightward.
fn quad_edges(points: &[super::ImagePoint; 4]) -> ((Edge, Edge), (Edge, Edge)) {
    let [p0, p1, p2, p3] = *points;
    let edge = |a: super::ImagePoint, b: super::ImagePoint| (b.x - a.x, b.y - a.y);
    let first = (edge(p0, p1), edge(p3, p2));
    let second = (edge(p1, p2), edge(p0, p3));
    let (along, across) = if horizontality(first.0) + horizontality(first.1)
        >= horizontality(second.0) + horizontality(second.1)
    {
        (first, second)
    } else {
        (second, first)
    };
    let rightward = |(dx, dy): Edge| if dx < 0.0 { (-dx, -dy) } else { (dx, dy) };
    ((rightward(along.0), rightward(along.1)), across)
}

fn edge_length((dx, dy): Edge) -> f32 {
    dx.hypot(dy)
}

fn horizontality(edge: Edge) -> f32 {
    let length = edge_length(edge);
    if length > 0.0 {
        edge.0.abs() / length
    } else {
        0.0
    }
}

/// Largest tilt treated as skew of a scanned or photographed page. Beyond
/// it the lines are not a tilted page but rotated text.
const MAX_PAGE_SKEW_RADIANS: f32 = 10.0 * std::f32::consts::PI / 180.0;

/// The page's skew in radians (image space, positive clockwise): the median
/// baseline angle of its text lines, those at least three times as long as
/// they are tall. Zero for fewer than three such lines or a tilt beyond
/// [`MAX_PAGE_SKEW_RADIANS`].
fn page_skew(quads: &[Option<[super::ImagePoint; 4]>]) -> f32 {
    let mut angles: Vec<f32> = quads
        .iter()
        .flatten()
        .filter(|points| points.iter().all(|p| p.x.is_finite() && p.y.is_finite()))
        .filter_map(|points| {
            let (along, across) = quad_edges(points);
            let length = (edge_length(along.0) + edge_length(along.1)) / 2.0;
            let thickness = (edge_length(across.0) + edge_length(across.1)) / 2.0;
            (thickness > 0.0 && length >= thickness * 3.0).then(|| {
                let (dx, dy) = (along.0 .0 + along.1 .0, along.0 .1 + along.1 .1);
                dy.atan2(dx)
            })
        })
        .collect();
    if angles.len() < 3 {
        return 0.0;
    }
    angles.sort_by(f32::total_cmp);
    let median = angles[angles.len() / 2];
    if median.abs() > MAX_PAGE_SKEW_RADIANS {
        0.0
    } else {
        median
    }
}

/// A page's recognized lines laid out upright.
struct OcrLineFrame {
    /// Quarter turns clockwise from the rendered page to the upright frame.
    turns: u8,
    /// Upright frame height in pixels.
    height: u32,
    /// Upright boxes `(left, top, right, bottom)` in frame pixels, in span
    /// order; `None` for blank text or unusable geometry.
    boxes: Vec<Option<(f32, f32, f32, f32)>>,
}

/// Quarter turns clockwise that bring the page's text upright: the turn
/// most of its spans report through `orientation_degrees` (the clockwise
/// angle the text is turned on the rendered page), none by default.
fn page_upright_turns(page: &RoutedOcrPage) -> u8 {
    let mut counts = [0usize; 4];
    for span in &page.ocr.spans {
        let turns = span
            .orientation_degrees
            .filter(|degrees| degrees.is_finite())
            .map_or(0, |degrees| {
                let quarters = (degrees / 90.0).round() as i64;
                (4 - quarters.rem_euclid(4)) % 4
            });
        counts[turns as usize] += 1;
    }
    (0..4u8)
        .max_by_key(|&turns| (counts[turns as usize], 3 - turns))
        .unwrap_or(0)
}

/// A rendered-page point on the page turned `turns` quarter turns clockwise.
fn turn_point(point: super::ImagePoint, turns: u8, width: f32, height: f32) -> super::ImagePoint {
    match turns % 4 {
        1 => super::ImagePoint::new(height - point.y, point.x),
        2 => super::ImagePoint::new(width - point.x, height - point.y),
        3 => super::ImagePoint::new(point.y, width - point.x),
        _ => point,
    }
}

/// The axis-aligned bounds of each span's polygon on the rendered page.
fn page_space_span_boxes(page: &RoutedOcrPage) -> OcrLineFrame {
    let (width, height) = (page.rendered.width() as f32, page.rendered.height() as f32);
    let boxes = page
        .ocr
        .spans
        .iter()
        .map(|span| {
            let points = &span.polygon.points;
            if span.text.trim().is_empty()
                || points.iter().any(|p| !p.x.is_finite() || !p.y.is_finite())
            {
                return None;
            }
            let xs = points.iter().map(|p| p.x);
            let ys = points.iter().map(|p| p.y);
            let left = xs.clone().fold(f32::INFINITY, f32::min).clamp(0.0, width);
            let right = xs.fold(f32::NEG_INFINITY, f32::max).clamp(0.0, width);
            let top = ys.clone().fold(f32::INFINITY, f32::min).clamp(0.0, height);
            let bottom = ys.fold(f32::NEG_INFINITY, f32::max).clamp(0.0, height);
            (right > left && bottom > top).then_some((left, top, right, bottom))
        })
        .collect();
    OcrLineFrame {
        turns: 0,
        height: page.rendered.height(),
        boxes,
    }
}

/// Upright boxes of a page's recognized lines, in span order.
fn ocr_span_boxes(page: &RoutedOcrPage) -> OcrLineFrame {
    let turns = page_upright_turns(page);
    let (width, height) = (page.rendered.width(), page.rendered.height());
    let (frame_width, frame_height) = if turns % 2 == 1 {
        (height, width)
    } else {
        (width, height)
    };
    let quads: Vec<Option<[super::ImagePoint; 4]>> = page
        .ocr
        .spans
        .iter()
        .map(|span| {
            (!span.text.trim().is_empty()).then(|| {
                span.polygon
                    .points
                    .map(|point| turn_point(point, turns, width as f32, height as f32))
            })
        })
        .collect();
    let skew = page_skew(&quads);
    let mut boxes: Vec<_> = quads
        .iter()
        .map(|quad| {
            quad.as_ref()
                .and_then(|points| image_quad_bounds(points, frame_width, frame_height, skew))
        })
        .collect();
    align_row_baselines(&mut boxes);
    OcrLineFrame {
        turns,
        height: frame_height,
        boxes,
    }
}

/// Puts the pieces of one printed line on one baseline.
///
/// The detector often splits a line at a wide gap (a blank to fill in, a
/// redaction, a tab stop), and each piece's box sits a pixel or two higher
/// or lower than the next. Native text of one line shares an exact
/// baseline; without one, the layout pipeline can read the right-hand piece
/// as the line above and put it first. Pieces whose centres lie within a
/// third of their height of each other, side by side without overlapping,
/// move to the bottom of the row's widest piece.
fn align_row_baselines(boxes: &mut [Option<(f32, f32, f32, f32)>]) {
    let center = |b: &(f32, f32, f32, f32)| (b.1 + b.3) / 2.0;
    let mut order: Vec<usize> = (0..boxes.len()).filter(|&i| boxes[i].is_some()).collect();
    order.sort_by(|&a, &b| {
        let (a, b) = (boxes[a].unwrap(), boxes[b].unwrap());
        center(&a).total_cmp(&center(&b))
    });
    let mut rows: Vec<Vec<usize>> = Vec::new();
    for index in order {
        let candidate = boxes[index].unwrap();
        let joins = rows.last().is_some_and(|row| {
            let first = boxes[row[0]].unwrap();
            let height = (candidate.3 - candidate.1).min(first.3 - first.1);
            (center(&candidate) - center(&first)).abs() <= height / 3.0
                && row.iter().all(|&member| {
                    let member = boxes[member].unwrap();
                    candidate.2 <= member.0 || member.2 <= candidate.0
                })
        });
        if joins {
            rows.last_mut().unwrap().push(index);
        } else {
            rows.push(vec![index]);
        }
    }
    for row in rows.iter().filter(|row| row.len() > 1) {
        let widest = row
            .iter()
            .map(|&i| boxes[i].unwrap())
            .max_by(|a, b| (a.2 - a.0).total_cmp(&(b.2 - b.0)))
            .unwrap();
        for &i in row {
            if let Some(b) = boxes[i].as_mut() {
                let shift = widest.3 - b.3;
                b.1 += shift;
                b.3 += shift;
            }
        }
    }
}

/// The upright box `(left, top, right, bottom)` of one recognized line.
///
/// An axis-aligned quad on an unskewed page is its own box. On a skewed page
/// (a photograph, or a scan fed in at a slant) the quad is first turned
/// about the page centre by the page's skew, so the lines of a slanted page
/// are level again and pieces of one line share a baseline. A quad still
/// tilted after that is replaced by a box of the line's own length and
/// height centred where it is: its axis-aligned bounds would be taller than
/// the type by the tilt, overlap the neighbouring lines, close the gaps that
/// separate paragraphs and promote long lines to headings by size.
fn image_quad_bounds(
    points: &[super::ImagePoint; 4],
    width: u32,
    height: u32,
    skew: f32,
) -> Option<(f32, f32, f32, f32)> {
    if points
        .iter()
        .any(|point| !point.x.is_finite() || !point.y.is_finite())
    {
        return None;
    }
    // Clip the quad to the page before it is turned: turned out of its
    // slant, a line near a corner of the page may lie past the frame's edge
    // and must stay whole.
    let points = points.map(|p| {
        super::ImagePoint::new(p.x.clamp(0.0, width as f32), p.y.clamp(0.0, height as f32))
    });
    let points = if skew == 0.0 {
        points
    } else {
        let (sin, cos) = skew.sin_cos();
        let (center_x, center_y) = (width as f32 / 2.0, height as f32 / 2.0);
        points.map(|p| {
            let (dx, dy) = (p.x - center_x, p.y - center_y);
            super::ImagePoint::new(
                center_x + dx * cos + dy * sin,
                center_y - dx * sin + dy * cos,
            )
        })
    };
    let min_x = points.iter().map(|p| p.x).fold(f32::INFINITY, f32::min);
    let max_x = points.iter().map(|p| p.x).fold(f32::NEG_INFINITY, f32::max);
    let min_y = points.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
    let max_y = points.iter().map(|p| p.y).fold(f32::NEG_INFINITY, f32::max);
    let axis_aligned = points
        .iter()
        .all(|p| (p.x == min_x || p.x == max_x) && (p.y == min_y || p.y == max_y));
    let (left, top, right, bottom) = if axis_aligned {
        (min_x, min_y, max_x, max_y)
    } else {
        let (along, across) = quad_edges(&points);
        let half_width = (edge_length(along.0) + edge_length(along.1)) / 4.0;
        let half_height = (edge_length(across.0) + edge_length(across.1)) / 4.0;
        let center_x = points.iter().map(|p| p.x).sum::<f32>() / 4.0;
        let center_y = points.iter().map(|p| p.y).sum::<f32>() / 4.0;
        (
            center_x - half_width,
            center_y - half_height,
            center_x + half_width,
            center_y + half_height,
        )
    };
    let (left, top, right, bottom) = if skew == 0.0 {
        (
            left.clamp(0.0, width as f32),
            top.clamp(0.0, height as f32),
            right.clamp(0.0, width as f32),
            bottom.clamp(0.0, height as f32),
        )
    } else {
        (left, top, right, bottom)
    };
    (right > left && bottom > top).then_some((left, top, right, bottom))
}

fn preserve_ocr_line_breaks(markdown: &str, page: &RoutedOcrPage) -> String {
    preserve_ocr_line_breaks_without(markdown, page, &HashSet::new())
}

/// [`preserve_ocr_line_breaks`] for a page whose `skip` spans (indices)
/// were left out of the Markdown.
fn preserve_ocr_line_breaks_without(
    markdown: &str,
    page: &RoutedOcrPage,
    skip: &HashSet<usize>,
) -> String {
    let spans: Vec<(&str, f32, f32, f32, f32)> = page
        .ocr
        .spans
        .iter()
        .zip(ocr_span_boxes(page).boxes)
        .enumerate()
        .filter(|(index, _)| !skip.contains(index))
        .filter_map(|(_, (span, bounds))| {
            let (left, top, right, bottom) = bounds?;
            (!span.text.trim().is_empty()).then_some((span.text.trim(), left, top, right, bottom))
        })
        .collect();
    if spans.len() < 2 {
        return markdown.to_string();
    }

    let mut line_heights: Vec<f32> = spans
        .iter()
        .map(|(_, _, top, _, bottom)| bottom - top)
        .filter(|height| height.is_finite() && *height > 0.0)
        .collect();
    if line_heights.is_empty() {
        return markdown.to_string();
    }
    line_heights.sort_by(f32::total_cmp);
    let median_height = line_heights[line_heights.len() / 2];

    // The Markdown converter owns reading order and may normalize syntax such
    // as list markers. Match every span back to its unique output occurrence,
    // then use Markdown order rather than imposing a second geometry sort.
    // If the mapping is incomplete or ambiguous, leave the converter output
    // untouched instead of risking a break at the wrong duplicate text.
    let mut mapped = Vec::with_capacity(spans.len());
    for (text, left, top, right, bottom) in spans {
        let Some((start, end)) = unique_markdown_span(markdown, text) else {
            return markdown.to_string();
        };
        mapped.push((start, end, left, top, right, bottom));
    }
    mapped.sort_by_key(|span| span.0);
    if mapped
        .windows(2)
        .any(|pair| pair[0].1 > pair[1].0 || pair[0].0 == pair[1].0)
    {
        return markdown.to_string();
    }

    // The left edge of the span that opens the Markdown line at `offset`.
    let line_left = |offset: usize| {
        let line_start = markdown[..offset].rfind('\n').map_or(0, |i| i + 1);
        mapped
            .iter()
            .find(|span| span.0 >= line_start)
            .map(|span| span.2)
    };
    // For each consecutive pair: the gap between them when the second span
    // sits at a hanging indent under a list item's line, a candidate
    // continuation of that item.
    let hanging: Vec<Option<f32>> = mapped
        .windows(2)
        .map(|pair| {
            let (_, current_end, _, _, _, current_bottom) = pair[0];
            let (_, _, next_left, next_top, _, _) = pair[1];
            (markdown_line_at(markdown, current_end).is_some_and(is_markdown_list_line)
                && line_left(current_end)
                    .is_some_and(|left| next_left > left + median_height * 0.5))
            .then_some(next_top - current_bottom)
        })
        .collect();
    // A candidate continues the item when it is closer than a line height,
    // or when it belongs to a run of at least two hanging lines spaced
    // alike (a double-spaced item): one indented line after a wide gap is
    // as likely a paragraph of its own.
    let mut continues = vec![false; hanging.len()];
    let mut i = 0;
    while i < hanging.len() {
        let run_end = (i..hanging.len())
            .find(|&j| hanging[j].is_none())
            .unwrap_or(hanging.len());
        let gaps: Vec<f32> = hanging[i..run_end].iter().flatten().copied().collect();
        let (low, high) = gaps.iter().fold((f32::MAX, f32::MIN), |(low, high), &g| {
            (low.min(g), high.max(g))
        });
        let regular = gaps.len() >= 2 && low > 0.0 && high <= low * 1.25;
        for (j, gap) in (i..run_end).zip(gaps) {
            continues[j] = regular || gap < median_height;
        }
        i = run_end + 1;
    }
    let mut replacements = Vec::new();
    for (index, pair) in mapped.windows(2).enumerate() {
        let (_, current_end, current_left, _, current_right, current_bottom) = pair[0];
        let (next_start, _, next_left, next_top, next_right, _) = pair[1];
        let overlap = (current_right.min(next_right) - current_left.max(next_left)).max(0.0);
        let narrowest_width = (current_right - current_left).min(next_right - next_left);
        let same_text_flow = narrowest_width > 0.0 && overlap >= narrowest_width * 0.2;
        let separated = next_top - current_bottom >= median_height * 0.65;
        let between = &markdown[current_end..next_start];
        // A word joined across the spans ("contrac" + "tion") has nothing
        // between them, and a list item's continuation at a hanging indent
        // stays in the item.
        let continues_item = continues[index];
        if same_text_flow
            && separated
            && !between.is_empty()
            && between.chars().all(char::is_whitespace)
            && !markdown_line_at(markdown, current_end).is_some_and(is_markdown_table_line)
            && !continues_item
            && !markdown_line_at(markdown, next_start).is_some_and(is_markdown_table_line)
        {
            replacements.push((current_end, next_start));
        }
    }

    let mut output = markdown.to_string();
    for (start, end) in replacements.into_iter().rev() {
        output.replace_range(start..end, "\n\n");
    }
    output
}

fn unique_markdown_span(markdown: &str, span_text: &str) -> Option<(usize, usize)> {
    let exact: Vec<_> = markdown.match_indices(span_text).collect();
    match exact.as_slice() {
        [(start, matched)] => return Some((*start, *start + matched.len())),
        [] => {}
        _ => return None,
    }

    // The converter normalizes a list marker away, and a line-end hyphen
    // may be joined away ("contrac-" in "contraction"); either or both.
    let unmarked = strip_list_marker(span_text);
    fn unhyphenated(text: &str) -> Option<&str> {
        text.strip_suffix('-')
            .filter(|word| word.ends_with(char::is_alphabetic))
    }
    [
        unmarked,
        unhyphenated(span_text),
        unmarked.and_then(unhyphenated),
    ]
    .into_iter()
    .flatten()
    .find_map(|normalized| {
        let matches: Vec<_> = markdown.match_indices(normalized).collect();
        match matches.as_slice() {
            [(start, matched)] => Some((*start, *start + matched.len())),
            _ => None,
        }
    })
}

fn strip_list_marker(text: &str) -> Option<&str> {
    const BULLETS: &[char] = &['•', '●', '○', '◦', '▪', '–', '—'];
    let trimmed = text.trim_start();
    let remainder = trimmed
        .strip_prefix(BULLETS)?
        .trim_start_matches(char::is_whitespace);
    (!remainder.is_empty()).then_some(remainder)
}

fn markdown_line_at(markdown: &str, offset: usize) -> Option<&str> {
    if offset > markdown.len() || !markdown.is_char_boundary(offset) {
        return None;
    }
    let start = markdown[..offset].rfind('\n').map_or(0, |index| index + 1);
    let end = markdown[offset..]
        .find('\n')
        .map_or(markdown.len(), |index| offset + index);
    markdown.get(start..end)
}

/// A line that opens a Markdown list item ("- ", "1. ", "1) ").
fn is_markdown_list_line(line: &str) -> bool {
    let line = line.trim_start();
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    line.starts_with("- ")
        || line.starts_with("* ")
        || ((1..=9).contains(&digits)
            && (line[digits..].starts_with(". ") || line[digits..].starts_with(") ")))
}

fn is_markdown_table_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.matches('|').count() >= 2
}

fn items_inside_regions(items: Vec<TextItem>, regions: &[PdfRect]) -> Vec<TextItem> {
    items
        .into_iter()
        .filter(|item| {
            let center_x = item.x + item.width / 2.0;
            let center_y = item.y + item.height / 2.0;
            regions.iter().any(|region| {
                const EDGE_TOLERANCE_PT: f32 = 2.0;
                item.page == region.page
                    && center_x >= region.x - EDGE_TOLERANCE_PT
                    && center_x <= region.x + region.width + EDGE_TOLERANCE_PT
                    && center_y >= region.y - EDGE_TOLERANCE_PT
                    && center_y <= region.y + region.height + EDGE_TOLERANCE_PT
            })
        })
        .collect()
}

fn merge_native_and_ocr(native: &str, ocr: &str) -> (String, PageContentSource) {
    let native_keys = comparison_units(native);
    let mut addition_keys = Vec::new();
    let mut additions: Vec<String> = Vec::new();
    for block in markdown_blocks(ocr) {
        let key = ComparisonKey::new(block);
        if key.is_empty()
            || native_keys.iter().any(|native| native.same_content(&key))
            || addition_keys
                .iter()
                .any(|existing: &ComparisonKey| existing.same_content(&key))
        {
            continue;
        }

        let addition = if let Some(native) = best_partial_overlap(&key, &native_keys) {
            let Some(novel) = novel_fragments(block, native) else {
                continue;
            };
            novel
        } else {
            block.trim_end().to_string()
        };
        let addition_key = ComparisonKey::new(&addition);
        if addition_key.is_empty()
            || native_keys
                .iter()
                .any(|native| native.same_content(&addition_key))
            || addition_keys
                .iter()
                .any(|existing: &ComparisonKey| existing.same_content(&addition_key))
        {
            continue;
        }
        addition_keys.push(addition_key);
        additions.push(addition);
    }

    if additions.is_empty() {
        (ensure_trailing_newline(native), PageContentSource::Native)
    } else {
        let mut result = native.trim_end_matches('\n').to_string();
        if !result.is_empty() {
            result.push_str("\n\n");
        }
        result.push_str(&additions.join("\n\n"));
        result.push('\n');
        (result, PageContentSource::Fused)
    }
}

fn markdown_blocks(markdown: &str) -> impl Iterator<Item = &str> {
    markdown
        .split("\n\n")
        .map(str::trim_end)
        .filter(|block| !block.trim().is_empty())
}

#[derive(Debug)]
struct ComparisonKey {
    tokens: Vec<String>,
    punctuation: String,
}

impl ComparisonKey {
    fn new(text: &str) -> Self {
        let mut tokens = Vec::new();
        let mut token = String::new();
        let mut punctuation = String::new();
        for character in text.chars().flat_map(char::to_lowercase) {
            if character.is_alphanumeric() {
                token.push(character);
            } else {
                if !token.is_empty() {
                    tokens.push(std::mem::take(&mut token));
                }
                if !character.is_whitespace() {
                    punctuation.push(character);
                }
            }
        }
        if !token.is_empty() {
            tokens.push(token);
        }
        Self {
            tokens,
            punctuation,
        }
    }

    fn is_empty(&self) -> bool {
        self.tokens.is_empty() && self.punctuation.is_empty()
    }

    fn same_content(&self, other: &Self) -> bool {
        if self.tokens.is_empty() || other.tokens.is_empty() {
            self.tokens.is_empty()
                && other.tokens.is_empty()
                && self.punctuation == other.punctuation
        } else {
            token_counts(&self.tokens) == token_counts(&other.tokens)
        }
    }
}

fn comparison_units(markdown: &str) -> Vec<ComparisonKey> {
    let mut units: Vec<_> = markdown_blocks(markdown).map(ComparisonKey::new).collect();
    units.extend(
        markdown
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(ComparisonKey::new),
    );
    units
}

fn token_counts(tokens: &[String]) -> BTreeMap<&str, usize> {
    let mut counts = BTreeMap::new();
    for token in tokens {
        *counts.entry(token.as_str()).or_insert(0) += 1;
    }
    counts
}

fn best_partial_overlap<'a>(
    ocr: &ComparisonKey,
    native: &'a [ComparisonKey],
) -> Option<&'a ComparisonKey> {
    if ocr.tokens.len() < 2 {
        return None;
    }
    native
        .iter()
        .filter(|key| !key.tokens.is_empty())
        .filter_map(|key| {
            let overlap = token_overlap(&ocr.tokens, &key.tokens);
            let substantial =
                overlap >= 2 && overlap * 2 >= ocr.tokens.len() && overlap * 2 >= key.tokens.len();
            substantial.then_some((overlap, key))
        })
        .max_by_key(|(overlap, _)| *overlap)
        .map(|(_, key)| key)
}

fn token_overlap(first: &[String], second: &[String]) -> usize {
    let first = token_counts(first);
    let second = token_counts(second);
    first
        .iter()
        .map(|(token, count)| (*count).min(second.get(token).copied().unwrap_or(0)))
        .sum()
}

fn novel_fragments(block: &str, native: &ComparisonKey) -> Option<String> {
    let mut available = token_counts(&native.tokens);
    let mut novel = Vec::new();
    for fragment in block.split_whitespace() {
        let key = ComparisonKey::new(fragment);
        let mut adds_content = false;
        for token in &key.tokens {
            match available.get_mut(token.as_str()) {
                Some(count) if *count > 0 => *count -= 1,
                _ => adds_content = true,
            }
        }
        if adds_content {
            novel.push(fragment);
        }
    }
    (!novel.is_empty()).then(|| novel.join(" "))
}

fn ensure_trailing_newline(markdown: &str) -> String {
    let mut result = markdown.trim_end_matches('\n').to_string();
    result.push('\n');
    result
}

fn equal_time_shares(total: u64, count: usize) -> Vec<u64> {
    if count == 0 {
        return Vec::new();
    }
    let count = count as u64;
    let base = total / count;
    let remainder = total % count;
    (0..count)
        .map(|index| base + u64::from(index < remainder))
        .collect()
}

fn validate_options(options: &OcrFusionOptions) -> Result<(), OcrFusionError> {
    if !options.render_dpi.is_finite() || options.render_dpi <= 0.0 {
        return Err(OcrFusionError::InvalidRenderDpi {
            value: options.render_dpi,
        });
    }
    let confidence = options.hosted_recommendation_confidence;
    if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) {
        return Err(OcrFusionError::InvalidHostedConfidence { value: confidence });
    }
    Ok(())
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Invalid page sets or fusion options.
#[derive(Debug, Error, PartialEq)]
#[non_exhaustive]
pub enum OcrFusionError {
    /// A 0-indexed native page could not be converted to 1-indexed form.
    #[error("native page number cannot be converted to a 1-indexed page")]
    PageOverflow,
    /// Native input repeated a page number.
    #[error("native page {page} appears more than once")]
    DuplicateNativePage {
        /// Repeated 1-indexed page number.
        page: u32,
    },
    /// OCR input repeated a page number.
    #[error("OCR page {page} appears more than once")]
    DuplicateOcrPage {
        /// Repeated 1-indexed page number.
        page: u32,
    },
    /// OCR returned a page that was not part of native extraction.
    #[error("OCR page {page} is not present in native page extraction")]
    UnexpectedOcrPage {
        /// Unexpected 1-indexed page number.
        page: u32,
    },
    /// OCR input did not have an explicit full-page or supplemental route.
    #[error("OCR page {page} has no fusion route")]
    MissingOcrRoute {
        /// Unrouted 1-indexed page number.
        page: u32,
    },
    /// Render resolution is non-finite or non-positive.
    #[error("render DPI must be positive and finite, got {value}")]
    InvalidRenderDpi {
        /// Invalid value.
        value: f32,
    },
    /// Hosted recommendation confidence is outside 0–1.
    #[error("hosted recommendation confidence must be between 0 and 1, got {value}")]
    InvalidHostedConfidence {
        /// Invalid value.
        value: f32,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vision::{
        ImagePoint, ImageQuad, ModelIdentity, OcrPage, OcrSpan, PageTransform, RenderPixelFormat,
        RenderedPage,
    };

    fn native(page: u32, markdown: &str, needs_ocr: bool) -> PageMarkdown {
        PageMarkdown {
            page,
            markdown: markdown.to_string(),
            needs_ocr,
            ocr_reason: needs_ocr.then(|| "scanned".to_string()),
        }
    }

    fn rendered_page(page: u32) -> RenderedPage {
        let transform =
            PageTransform::from_corners(200, 100, (0.0, 100.0), (200.0, 100.0), (0.0, 0.0))
                .unwrap();
        RenderedPage::new(
            page,
            200.0,
            100.0,
            200,
            100,
            600,
            RenderPixelFormat::Rgb8,
            vec![255; 60_000],
            transform,
        )
        .unwrap()
    }

    fn span(text: &str, top: f32, confidence: f32) -> OcrSpan {
        OcrSpan {
            text: text.to_string(),
            polygon: ImageQuad::new([
                ImagePoint::new(10.0, top),
                ImagePoint::new(190.0, top),
                ImagePoint::new(190.0, top + 10.0),
                ImagePoint::new(10.0, top + 10.0),
            ]),
            confidence,
            orientation_degrees: None,
        }
    }

    fn routed_page(page: u32, spans: Vec<OcrSpan>, confidence: Option<f32>) -> RoutedOcrPage {
        RoutedOcrPage {
            rendered: rendered_page(page),
            ocr: OcrPage {
                page_number: page,
                spans,
                mean_confidence: confidence,
                model: ModelIdentity::new("test-ocr", "v1"),
                processing_time_ms: 7,
                warnings: Vec::new(),
            },
        }
    }

    fn positioned_span(text: &str, left: f32, top: f32, right: f32, bottom: f32) -> OcrSpan {
        OcrSpan {
            text: text.to_string(),
            polygon: ImageQuad::new([
                ImagePoint::new(left, top),
                ImagePoint::new(right, top),
                ImagePoint::new(right, bottom),
                ImagePoint::new(left, bottom),
            ]),
            confidence: 0.9,
            orientation_degrees: None,
        }
    }

    fn run(pages: Vec<RoutedOcrPage>) -> OcrRun {
        OcrRun {
            pages,
            render_time_ms: 5,
            ocr_time_ms: 7,
        }
    }

    fn native_candidate(markdown: &str) -> NativeFallbackCandidate {
        assess_native_candidate(markdown.to_string(), NativeCandidateOrigin::Extractor).unwrap()
    }

    #[test]
    fn supplemental_image_ocr_keeps_native_text_without_a_valid_table() {
        let native = [native(0, "Native article text\n", false)];
        let run = run(vec![routed_page(
            1,
            vec![positioned_span(
                "Figure-only OCR label",
                20.0,
                20.0,
                180.0,
                35.0,
            )],
            Some(0.95),
        )]);
        let routes = BTreeMap::from([(
            1,
            OcrFusionRoute::SupplementalRegions(vec![PdfRect {
                x: 0.0,
                y: 0.0,
                width: 200.0,
                height: 100.0,
                page: 1,
            }]),
        )]);

        let result = fuse_ocr_pages_adaptive_with_routes(
            &native,
            &run,
            1,
            &OcrFusionOptions::new(),
            &BTreeMap::new(),
            &routes,
            &BTreeMap::new(),
        )
        .unwrap();

        assert_eq!(result.pages[0].markdown, "Native article text\n");
        assert_eq!(result.pages[0].provenance.source, PageContentSource::Native);
        assert!(result.pages[0].provenance.warnings[0].contains("no valid table"));
    }

    #[test]
    fn supplemental_image_ocr_fuses_a_table_inside_the_region() {
        let native = [native(0, "Native article text\n", false)];
        let mut spans = Vec::new();
        for (row, (left, right)) in [
            ("Metric", "Value"),
            ("Accuracy", "95%"),
            ("Recall", "93%"),
            ("Precision", "96%"),
        ]
        .into_iter()
        .enumerate()
        {
            let top = 10.0 + row as f32 * 20.0;
            spans.push(positioned_span(left, 10.0, top, 80.0, top + 10.0));
            spans.push(positioned_span(right, 110.0, top, 180.0, top + 10.0));
        }
        let run = run(vec![routed_page(1, spans, Some(0.95))]);
        let routes = BTreeMap::from([(
            1,
            OcrFusionRoute::SupplementalRegions(vec![PdfRect {
                x: 0.0,
                y: 0.0,
                width: 200.0,
                height: 100.0,
                page: 1,
            }]),
        )]);

        let result = fuse_ocr_pages_adaptive_with_routes(
            &native,
            &run,
            1,
            &OcrFusionOptions::new(),
            &BTreeMap::new(),
            &routes,
            &BTreeMap::new(),
        )
        .unwrap();

        assert_eq!(result.pages[0].provenance.source, PageContentSource::Fused);
        assert!(result.pages[0].markdown.contains("|Metric|Value|"));
        assert!(result.pages[0].markdown.contains("|Accuracy|95%|"));
        assert_eq!(
            result.pages[0].markdown.matches("Native article").count(),
            1
        );
    }

    #[test]
    fn supplemental_regions_match_spans_in_page_space() {
        // Image regions are native PDF-space rectangles: a span's reported
        // turn must not move it out of (or into) one.
        let fuse = |orientation_degrees: Option<f32>| {
            let native = [native(0, "Native article text\n", false)];
            let mut spans = Vec::new();
            for (row, (left, right)) in
                [("Metric", "Value"), ("Accuracy", "95%"), ("Recall", "93%")]
                    .into_iter()
                    .enumerate()
            {
                let top = 10.0 + row as f32 * 20.0;
                for (text, x) in [(left, 10.0), (right, 110.0)] {
                    let mut span = positioned_span(text, x, top, x + 70.0, top + 10.0);
                    span.orientation_degrees = orientation_degrees;
                    spans.push(span);
                }
            }
            let routes = BTreeMap::from([(
                1,
                OcrFusionRoute::SupplementalRegions(vec![PdfRect {
                    x: 0.0,
                    y: 30.0,
                    width: 200.0,
                    height: 70.0,
                    page: 1,
                }]),
            )]);
            fuse_ocr_pages_adaptive_with_routes(
                &native,
                &run(vec![routed_page(1, spans, Some(0.95))]),
                1,
                &OcrFusionOptions::new(),
                &BTreeMap::new(),
                &routes,
                &BTreeMap::new(),
            )
            .unwrap()
            .pages
            .remove(0)
            .markdown
        };

        let upright = fuse(None);
        assert!(upright.contains("|Metric|Value|"), "{upright:?}");
        assert_eq!(fuse(Some(90.0)), upright);
    }

    #[test]
    fn a_line_near_the_corner_of_a_slanted_page_is_kept() {
        let slope = 0.035;
        let mut spans: Vec<OcrSpan> = (0..12)
            .map(|i| {
                let top = 200.0 + 40.0 * i as f32;
                tilted_span(
                    &format!("Body line number {i} of the slanted page runs across"),
                    100.0,
                    top,
                    900.0,
                    20.0,
                    slope,
                )
            })
            .collect();
        // A folio in the top-left corner: turned out of the page's slant
        // about the page centre, it lies partly past the frame's edge.
        spans.push(positioned_span("A1", 5.0, 15.0, 25.0, 30.0));
        let page = sized_routed_page(1000, 1400, spans);

        let (items, discarded) = ocr_text_items(&page, &HashSet::new(), true);

        assert_eq!(discarded, 0);
        assert_eq!(items.len(), 13);
        let folio = items.iter().find(|item| item.text == "A1").unwrap();
        assert!(folio.width > 15.0 && folio.height > 10.0, "{folio:?}");
    }

    #[test]
    fn a_wide_heading_set_apart_keeps_its_size() {
        let page = sized_routed_page(
            400,
            300,
            vec![
                positioned_span(
                    "Body text opens the page and runs on across",
                    20.0,
                    20.0,
                    380.0,
                    30.0,
                ),
                positioned_span(
                    "the line to the end of a short paragraph.",
                    20.0,
                    33.0,
                    380.0,
                    43.0,
                ),
                // 1.3 times the body height, as wide as most of a body line,
                // with a paragraph's space above and below.
                positioned_span("Obligations of the parties", 20.0, 65.0, 320.0, 78.0),
                positioned_span(
                    "Each party keeps the other informed of any",
                    20.0,
                    100.0,
                    380.0,
                    110.0,
                ),
                positioned_span(
                    "change that affects the delivery of goods.",
                    20.0,
                    113.0,
                    380.0,
                    123.0,
                ),
            ],
        );

        let markdown = ocr_page_to_markdown(&page, 1, &MarkdownOptions::default());

        let headings: Vec<&str> = markdown.lines().filter(|l| l.starts_with('#')).collect();
        assert_eq!(headings.len(), 1, "{markdown:?}");
        assert_eq!(
            headings[0].trim_start_matches('#').trim(),
            "Obligations of the parties"
        );
    }

    #[test]
    fn supplemental_table_detection_requires_multiple_rows() {
        let items = vec![
            TextItem {
                text: "Metric".to_string(),
                x: 10.0,
                y: 40.0,
                width: 50.0,
                height: 10.0,
                font: "OCR".to_string(),
                font_tag: String::new(),
                legacy_symbol_rewrite: false,
                font_size: 10.0,
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
            },
            TextItem {
                text: "Value".to_string(),
                x: 110.0,
                y: 40.0,
                width: 50.0,
                height: 10.0,
                font: "OCR".to_string(),
                font_tag: String::new(),
                legacy_symbol_rewrite: false,
                font_size: 10.0,
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
            },
        ];

        let mut options = MarkdownOptions::default();
        options.base_font_size = Some(10.0);
        assert!(complete_table_markdown_from_items(items, options, 1).is_empty());
    }

    #[test]
    fn supplemental_image_ocr_filters_spans_by_region_center() {
        let items = vec![
            TextItem {
                text: "inside".to_string(),
                x: 10.0,
                y: 10.0,
                width: 20.0,
                height: 10.0,
                font: "OCR".to_string(),
                font_tag: String::new(),
                legacy_symbol_rewrite: false,
                font_size: 10.0,
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
            },
            TextItem {
                text: "outside".to_string(),
                x: 120.0,
                y: 10.0,
                width: 20.0,
                height: 10.0,
                font: "OCR".to_string(),
                font_tag: String::new(),
                legacy_symbol_rewrite: false,
                font_size: 10.0,
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
            },
        ];
        let regions = [PdfRect {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
            page: 1,
        }];

        let filtered = items_inside_regions(items, &regions);

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].text, "inside");
    }

    #[test]
    fn scanned_page_uses_geometry_ordered_ocr_and_provenance() {
        let native = [native(0, "", true)];
        let run = run(vec![routed_page(
            1,
            vec![
                span("Second line", 30.0, 0.9),
                span("First line", 10.0, 0.9),
            ],
            Some(0.9),
        )]);

        let result = fuse_ocr_pages(&native, &run, 1, &OcrFusionOptions::new()).unwrap();

        assert!(
            result.pages[0].markdown.find("First").unwrap()
                < result.pages[0].markdown.find("Second").unwrap()
        );
        assert_eq!(result.pages[0].provenance.source, PageContentSource::Ocr);
        assert_eq!(result.pages[0].page_number, 1);
        assert_eq!(
            result.pages[0].page_number,
            result.pages[0].provenance.page_number
        );
        assert_eq!(
            result.pages[0].provenance.ocr_model.as_ref().unwrap().name,
            "test-ocr"
        );
        assert!(!result.pages[0].provenance.hosted_recommended);
    }

    #[test]
    fn ocr_assembly_preserves_well_separated_detected_rows() {
        let page = routed_page(
            1,
            vec![
                positioned_span("Column A Column B", 10.0, 10.0, 190.0, 20.0),
                positioned_span("First row value", 10.0, 35.0, 190.0, 45.0),
                positioned_span("Second row value", 10.0, 60.0, 190.0, 70.0),
            ],
            Some(0.9),
        );

        let markdown = ocr_page_to_markdown(&page, 1, &MarkdownOptions::default());

        assert!(markdown.contains("Column A Column B\n\n"), "{markdown:?}");
        assert!(markdown.contains("First row value\n\n"), "{markdown:?}");
    }

    /// A page of `width` × `height` pixels, one PDF point per pixel.
    fn sized_routed_page(width: u32, height: u32, spans: Vec<OcrSpan>) -> RoutedOcrPage {
        let transform = PageTransform::from_corners(
            width,
            height,
            (0.0, f64::from(height)),
            (f64::from(width), f64::from(height)),
            (0.0, 0.0),
        )
        .unwrap();
        let rendered = RenderedPage::new(
            1,
            width as f32,
            height as f32,
            width,
            height,
            width as usize,
            RenderPixelFormat::Gray8,
            vec![255; width as usize * height as usize],
            transform,
        )
        .unwrap();
        RoutedOcrPage {
            rendered,
            ocr: OcrPage {
                page_number: 1,
                spans,
                mean_confidence: Some(0.95),
                model: ModelIdentity::new("test-ocr", "v1"),
                processing_time_ms: 7,
                warnings: Vec::new(),
            },
        }
    }

    /// A line `height` pixels tall from `left` to `right` whose top-left
    /// corner is at `top`, rising or falling `slope` pixels per pixel.
    fn tilted_span(
        text: &str,
        left: f32,
        top: f32,
        right: f32,
        height: f32,
        slope: f32,
    ) -> OcrSpan {
        let rise = (right - left) * slope;
        OcrSpan {
            text: text.to_string(),
            polygon: ImageQuad::new([
                ImagePoint::new(left, top),
                ImagePoint::new(right, top + rise),
                ImagePoint::new(right, top + rise + height),
                ImagePoint::new(left, top + height),
            ]),
            confidence: 0.95,
            orientation_degrees: None,
        }
    }

    #[test]
    fn a_slanted_page_keeps_its_paragraphs_and_gains_no_headings() {
        // Lines falling 2 degrees to the right: each line's axis-aligned
        // bounds are twice the type's height and overlap the next line.
        let slope = 0.035;
        let lines = [
            ("The first paragraph opens on this line and runs", 20.0),
            ("on to a second line of the same paragraph.", 34.0),
            ("A second paragraph starts after the gap and goes", 62.0),
            ("on to its own second line before it ends.", 76.0),
            ("A third paragraph is one line long here.", 104.0),
        ];
        let spans = lines
            .iter()
            .map(|&(text, top)| tilted_span(text, 20.0, top, 380.0, 10.0, slope))
            .collect();
        let page = sized_routed_page(400, 200, spans);

        let markdown = ocr_page_to_markdown(&page, 1, &MarkdownOptions::default());

        assert!(!markdown.contains('#'), "{markdown:?}");
        assert_eq!(
            markdown.trim(),
            "The first paragraph opens on this line and runs on to a second line of the same \
             paragraph.\n\nA second paragraph starts after the gap and goes on to its own second \
             line before it ends.\n\nA third paragraph is one line long here.",
        );
    }

    #[test]
    fn pieces_of_one_line_keep_their_order_across_a_blank() {
        // A form line split at a blank: the right-hand piece's box sits
        // three pixels higher than the left-hand one.
        let page = sized_routed_page(
            400,
            200,
            vec![
                positioned_span(
                    "The parties signed the contract below.",
                    20.0,
                    20.0,
                    380.0,
                    30.0,
                ),
                positioned_span("Contract No", 20.0, 40.0, 110.0, 50.0),
                positioned_span("dated the first of March", 200.0, 37.0, 380.0, 47.0),
                positioned_span("Both parties keep one copy of it.", 20.0, 60.0, 380.0, 70.0),
            ],
        );

        let markdown = ocr_page_to_markdown(&page, 1, &MarkdownOptions::default());

        let left = markdown.find("Contract No").unwrap();
        let right = markdown.find("dated the first of March").unwrap();
        let above = markdown.find("contract below.").unwrap();
        assert!(above < left && left < right, "{markdown:?}");
    }

    #[test]
    fn a_tall_body_line_is_body_text_and_a_short_large_line_a_heading() {
        let page = sized_routed_page(
            400,
            300,
            vec![
                positioned_span("Delivery terms", 20.0, 20.0, 150.0, 36.0),
                positioned_span(
                    "The supplier delivers the goods to the buyer's",
                    20.0,
                    50.0,
                    380.0,
                    60.0,
                ),
                // Measured 1.3 times the body height, as photographed body
                // lines can be.
                positioned_span(
                    "warehouse within ten working days of each order",
                    20.0,
                    63.0,
                    380.0,
                    76.0,
                ),
                positioned_span(
                    "and bears the cost of transport and insurance.",
                    20.0,
                    79.0,
                    380.0,
                    89.0,
                ),
            ],
        );

        let markdown = ocr_page_to_markdown(&page, 1, &MarkdownOptions::default());

        let headings: Vec<&str> = markdown.lines().filter(|l| l.starts_with('#')).collect();
        assert_eq!(headings.len(), 1, "{markdown:?}");
        assert_eq!(headings[0].trim_start_matches('#').trim(), "Delivery terms");
        assert!(
            markdown.contains("buyer's warehouse within ten working days of each order and bears"),
            "{markdown:?}"
        );
    }

    #[test]
    fn a_page_read_turned_upright_is_laid_out_upright() {
        // The same three lines, once on an upright 200 × 400 page and once
        // on that page stored turned 90 degrees clockwise (400 × 200), as
        // the engine reports a sideways scan it read turned upright.
        let upright = [
            positioned_span("Notice of delivery", 20.0, 20.0, 120.0, 32.0),
            positioned_span(
                "The goods arrive on Monday morning.",
                20.0,
                60.0,
                180.0,
                70.0,
            ),
            positioned_span("Please sign the delivery note.", 20.0, 100.0, 180.0, 110.0),
        ];
        let expected = ocr_page_to_markdown(
            &sized_routed_page(200, 400, upright.to_vec()),
            1,
            &MarkdownOptions::default(),
        );
        // Upright (u, v) on the 200 × 400 page is (400 - v, u) on the page
        // turned 90 degrees clockwise.
        let turned = upright
            .iter()
            .map(|span| OcrSpan {
                polygon: ImageQuad::new(
                    span.polygon
                        .points
                        .map(|point| ImagePoint::new(400.0 - point.y, point.x)),
                ),
                orientation_degrees: Some(90.0),
                ..span.clone()
            })
            .collect();

        let markdown = ocr_page_to_markdown(
            &sized_routed_page(400, 200, turned),
            1,
            &MarkdownOptions::default(),
        );

        assert_eq!(markdown, expected);
        assert!(markdown.contains("Notice of delivery"), "{markdown:?}");
        assert!(
            markdown.find("Monday").unwrap() < markdown.find("Please sign").unwrap(),
            "{markdown:?}"
        );
    }

    #[test]
    fn upright_turns_follow_the_spans_reported_orientation() {
        let with = |degrees: Option<f32>| {
            let mut span = positioned_span("Line", 10.0, 10.0, 90.0, 20.0);
            span.orientation_degrees = degrees;
            sized_routed_page(100, 100, vec![span])
        };
        assert_eq!(page_upright_turns(&with(None)), 0);
        assert_eq!(page_upright_turns(&with(Some(0.0))), 0);
        assert_eq!(page_upright_turns(&with(Some(90.0))), 3);
        assert_eq!(page_upright_turns(&with(Some(180.0))), 2);
        assert_eq!(page_upright_turns(&with(Some(270.0))), 1);
        assert_eq!(page_upright_turns(&with(Some(-90.0))), 1);
        assert_eq!(page_upright_turns(&with(Some(f32::NAN))), 0);
    }

    #[test]
    fn axis_aligned_quads_on_an_unskewed_page_keep_their_bounds() {
        let points = [
            ImagePoint::new(10.5, 20.25),
            ImagePoint::new(90.75, 20.25),
            ImagePoint::new(90.75, 31.0),
            ImagePoint::new(10.5, 31.0),
        ];
        assert_eq!(
            image_quad_bounds(&points, 100, 100, 0.0),
            Some((10.5, 20.25, 90.75, 31.0))
        );
    }

    #[test]
    fn line_break_recovery_uses_markdown_reading_order_for_columns() {
        let page = routed_page(
            1,
            vec![
                positioned_span("Left top", 10.0, 10.0, 90.0, 20.0),
                positioned_span("Right top", 110.0, 10.0, 190.0, 20.0),
                positioned_span("Left bottom", 10.0, 40.0, 90.0, 50.0),
                positioned_span("Right bottom", 110.0, 40.0, 190.0, 50.0),
            ],
            Some(0.9),
        );
        let markdown = "Left top Left bottom\n\nRight top Right bottom";

        let recovered = preserve_ocr_line_breaks(markdown, &page);

        assert_eq!(
            recovered,
            "Left top\n\nLeft bottom\n\nRight top\n\nRight bottom"
        );
    }

    #[test]
    fn line_break_recovery_preserves_tables_but_handles_page_prose() {
        let page = routed_page(
            1,
            vec![
                positioned_span("A", 10.0, 10.0, 90.0, 20.0),
                positioned_span("B", 110.0, 10.0, 190.0, 20.0),
                positioned_span("x", 10.0, 30.0, 90.0, 40.0),
                positioned_span("y", 110.0, 30.0, 190.0, 40.0),
                positioned_span("First prose", 10.0, 60.0, 190.0, 70.0),
                positioned_span("Second prose", 10.0, 90.0, 190.0, 100.0),
            ],
            Some(0.9),
        );
        let markdown = "| A | B |\n|---|---|\n| x | y |\n\nFirst prose Second prose";

        let recovered = preserve_ocr_line_breaks(markdown, &page);

        assert!(recovered.starts_with("| A | B |\n|---|---|\n| x | y |"));
        assert!(recovered.ends_with("First prose\n\nSecond prose"));
    }

    #[test]
    fn line_break_recovery_accepts_normalized_list_markers() {
        let page = routed_page(
            1,
            vec![
                positioned_span("• First item", 10.0, 10.0, 190.0, 20.0),
                positioned_span("Next paragraph", 10.0, 40.0, 190.0, 50.0),
            ],
            Some(0.9),
        );

        let recovered = preserve_ocr_line_breaks("- First item Next paragraph", &page);

        assert_eq!(recovered, "- First item\n\nNext paragraph");
    }

    #[test]
    fn line_break_recovery_survives_a_joined_line_end_hyphen() {
        // EN and RU: "contrac-" / "дого-" were joined into one word, and
        // the paragraph break after the next span is still recovered.
        for (first, second, third, joined) in [
            (
                "This is a contrac-",
                "tion clause.",
                "The next paragraph.",
                "This is a contraction clause.",
            ),
            (
                "Стороны заключили дого-",
                "вор аренды.",
                "Следующий абзац.",
                "Стороны заключили договор аренды.",
            ),
        ] {
            let page = routed_page(
                1,
                vec![
                    positioned_span(first, 10.0, 10.0, 190.0, 20.0),
                    positioned_span(second, 10.0, 24.0, 190.0, 34.0),
                    positioned_span(third, 10.0, 41.0, 190.0, 51.0),
                ],
                Some(0.9),
            );
            let recovered = preserve_ocr_line_breaks(&format!("{joined} {third}"), &page);
            assert_eq!(recovered, format!("{joined}\n\n{third}"));
        }
    }

    #[test]
    fn line_break_recovery_survives_a_bullet_and_a_joined_hyphen_together() {
        let page = routed_page(
            1,
            vec![
                positioned_span("• This is a contrac-", 10.0, 10.0, 190.0, 20.0),
                positioned_span("tion clause.", 20.0, 24.0, 190.0, 34.0),
                positioned_span("The next paragraph.", 10.0, 41.0, 190.0, 51.0),
            ],
            Some(0.9),
        );
        let recovered =
            preserve_ocr_line_breaks("- This is a contraction clause. The next paragraph.", &page);
        assert_eq!(
            recovered,
            "- This is a contraction clause.\n\nThe next paragraph."
        );
    }

    #[test]
    fn line_break_recovery_splits_an_indented_paragraph_after_a_list_item() {
        // A paragraph-sized gap: not a hanging continuation.
        let page = routed_page(
            1,
            vec![
                positioned_span("• First item.", 10.0, 10.0, 190.0, 20.0),
                positioned_span("An indented paragraph.", 20.0, 41.0, 190.0, 51.0),
            ],
            Some(0.9),
        );
        let recovered = preserve_ocr_line_breaks("- First item. An indented paragraph.", &page);
        assert_eq!(recovered, "- First item.\n\nAn indented paragraph.");
    }

    #[test]
    fn line_break_recovery_keeps_a_list_items_hanging_continuation() {
        let page = routed_page(
            1,
            vec![
                positioned_span("• First item starts", 10.0, 10.0, 190.0, 20.0),
                positioned_span("continued on another line.", 20.0, 27.0, 190.0, 37.0),
            ],
            Some(0.9),
        );
        let markdown = "- First item starts continued on another line.";
        assert_eq!(preserve_ocr_line_breaks(markdown, &page), markdown);
    }

    #[test]
    fn line_break_recovery_keeps_a_double_spaced_items_continuation() {
        let lines = [
            "• The tenant agrees to pay all charges",
            "arising under this lease and all related",
            "costs assessed during the contractual",
            "period unless otherwise agreed by both",
            "parties in writing before the relevant",
            "payment becomes due under the agreement.",
        ];
        let spans = lines
            .iter()
            .enumerate()
            .map(|(i, text)| {
                let top = 10.0 + 23.0 * i as f32;
                let left = if i == 0 { 10.0 } else { 20.0 };
                positioned_span(text, left, top, 190.0, top + 10.0)
            })
            .collect();
        let mut page = routed_page(1, spans, Some(0.9));
        let transform =
            PageTransform::from_corners(200, 200, (0., 200.), (200., 200.), (0., 0.)).unwrap();
        page.rendered = RenderedPage::new(
            1,
            200.,
            200.,
            200,
            200,
            600,
            RenderPixelFormat::Rgb8,
            vec![255; 120_000],
            transform,
        )
        .unwrap();
        let markdown = format!("- {}", lines.join(" ").trim_start_matches("• "));
        assert_eq!(preserve_ocr_line_breaks(&markdown, &page), markdown);
        assert_eq!(
            ocr_page_to_markdown(&page, 1, &MarkdownOptions::default()).trim_end(),
            markdown
        );
    }

    #[test]
    fn line_break_recovery_splits_one_indented_line_on_a_double_spaced_page() {
        // Paragraph gaps everywhere do not make the indented line after the
        // item a continuation: it is not part of a regular hanging run.
        let texts = [
            "• First item.",
            "An indented paragraph.",
            "Третий абзац.",
            "Fourth paragraph.",
        ];
        let spans = texts
            .iter()
            .enumerate()
            .map(|(i, text)| {
                let top = 10.0 + 25.0 * i as f32;
                let left = if i == 1 { 20.0 } else { 10.0 };
                positioned_span(text, left, top, 190.0, top + 10.0)
            })
            .collect();
        let page = routed_page(1, spans, Some(0.9));
        let recovered = preserve_ocr_line_breaks(
            "- First item. An indented paragraph. Третий абзац. Fourth paragraph.",
            &page,
        );
        assert_eq!(
            recovered,
            "- First item.\n\nAn indented paragraph.\n\nТретий абзац.\n\nFourth paragraph."
        );
    }

    #[test]
    fn line_break_recovery_accepts_white_bullet_list_markers() {
        let page = routed_page(
            1,
            vec![
                positioned_span("◦ First item", 10.0, 10.0, 190.0, 20.0),
                positioned_span("Next paragraph", 10.0, 40.0, 190.0, 50.0),
            ],
            Some(0.9),
        );

        let recovered = preserve_ocr_line_breaks("- First item Next paragraph", &page);

        assert_eq!(recovered, "- First item\n\nNext paragraph");
    }

    #[test]
    fn line_break_recovery_leaves_ambiguous_duplicates_unchanged() {
        let page = routed_page(
            1,
            vec![
                positioned_span("Repeated", 10.0, 10.0, 190.0, 20.0),
                positioned_span("Repeated", 10.0, 40.0, 190.0, 50.0),
            ],
            Some(0.9),
        );
        let markdown = "Repeated Repeated";

        assert_eq!(preserve_ocr_line_breaks(markdown, &page), markdown);
    }

    #[test]
    fn force_mode_deduplicates_native_content() {
        let native = [native(0, "Hello, world!\n", false)];
        let run = run(vec![routed_page(
            1,
            vec![span("Hello world", 10.0, 0.9)],
            Some(0.9),
        )]);

        let result = fuse_ocr_pages(&native, &run, 1, &OcrFusionOptions::new()).unwrap();

        assert_eq!(result.pages[0].markdown, "Hello, world!\n");
        assert_eq!(result.pages[0].provenance.source, PageContentSource::Native);
    }

    #[test]
    fn adaptive_fallback_keeps_native_text_when_ocr_is_weak() {
        let native = [native(0, "", true)];
        let run = run(vec![routed_page(
            1,
            vec![span("Inv0ice total uncertain", 10.0, 0.3)],
            Some(0.3),
        )]);
        let candidates = BTreeMap::from([(
            1,
            native_candidate("Invoice total: $420.00\nPayment received\n"),
        )]);

        let result =
            fuse_ocr_pages_adaptive(&native, &run, 1, &OcrFusionOptions::new(), &candidates)
                .unwrap();

        assert_eq!(result.pages[0].provenance.source, PageContentSource::Native);
        assert_eq!(
            result.pages[0].markdown,
            "Invoice total: $420.00\nPayment received\n"
        );
        assert!(result.pages[0].provenance.hosted_recommended);
        assert!(result.pages[0].provenance.warnings[0].contains("OCR was weak"));
    }

    #[test]
    fn adaptive_fallback_prefers_exact_native_text_over_duplicate_ocr() {
        let native = [native(0, "", true)];
        let run = run(vec![routed_page(
            1,
            vec![span("Invoice total 420.00 Payment received", 10.0, 0.98)],
            Some(0.98),
        )]);
        let candidates = BTreeMap::from([(
            1,
            native_candidate("Invoice total: $420.00\nPayment received\n"),
        )]);

        let result =
            fuse_ocr_pages_adaptive(&native, &run, 1, &OcrFusionOptions::new(), &candidates)
                .unwrap();

        assert_eq!(result.pages[0].provenance.source, PageContentSource::Native);
        assert!(result.pages[0].provenance.hosted_recommended);
        assert_eq!(result.pages[0].markdown.matches("Invoice").count(), 1);
        assert!(result.pages[0].provenance.warnings[0].contains("no material coverage"));
    }

    #[test]
    fn adaptive_fallback_fuses_short_novel_ocr_content() {
        let native = [native(0, "", true)];
        let run = run(vec![routed_page(
            1,
            vec![span("Status ready 42", 10.0, 0.98)],
            Some(0.98),
        )]);
        let candidates = BTreeMap::from([(1, native_candidate("Status ready\n"))]);

        let result =
            fuse_ocr_pages_adaptive(&native, &run, 1, &OcrFusionOptions::new(), &candidates)
                .unwrap();

        assert_eq!(result.pages[0].provenance.source, PageContentSource::Fused);
        assert!(result.pages[0].markdown.contains("42"));
        assert!(!result.pages[0].provenance.hosted_recommended);
    }

    #[test]
    fn adaptive_fallback_recommends_hosted_for_high_confidence_garbage_ocr() {
        let native = [native(0, "", true)];
        let garbage = "a@@b%%c&&d==e~~".repeat(12);
        let run = run(vec![routed_page(
            1,
            vec![span(&garbage, 10.0, 0.99)],
            Some(0.99),
        )]);
        let candidates = BTreeMap::from([(1, native_candidate("Invoice total 420\n"))]);

        let result =
            fuse_ocr_pages_adaptive(&native, &run, 1, &OcrFusionOptions::new(), &candidates)
                .unwrap();

        assert_eq!(result.pages[0].provenance.source, PageContentSource::Native);
        assert!(result.pages[0].provenance.hosted_recommended);
        assert!(!result.pages[0].markdown.contains("@@"));
    }

    #[test]
    fn adaptive_fallback_fuses_native_header_with_scanned_body() {
        let native = [native(0, "", true)];
        let run = run(vec![routed_page(
            1,
            vec![
                span("Quarterly account report", 10.0, 0.96),
                span("March revenue 420 units", 30.0, 0.96),
                span("April revenue 510 units", 50.0, 0.96),
            ],
            Some(0.96),
        )]);
        let candidates = BTreeMap::from([(1, native_candidate("# Quarterly account report\n"))]);

        let result =
            fuse_ocr_pages_adaptive(&native, &run, 1, &OcrFusionOptions::new(), &candidates)
                .unwrap();

        assert_eq!(result.pages[0].provenance.source, PageContentSource::Fused);
        assert_eq!(result.pages[0].markdown.matches("Quarterly").count(), 1);
        assert!(result.pages[0].markdown.contains("March revenue 420 units"));
        assert!(result.pages[0].markdown.contains("April revenue 510 units"));
        assert!(result.pages[0].provenance.warnings[0].contains("complementary"));
    }

    #[test]
    fn native_candidate_scoring_is_multilingual_and_rejects_garbage() {
        assert!(assess_native_candidate(
            "請求書 合計金額 4200円\n支払済み\n".to_string(),
            NativeCandidateOrigin::Extractor,
        )
        .is_some());
        assert!(assess_native_candidate(
            "a@@b%%c&&d==e~~".repeat(12),
            NativeCandidateOrigin::Extractor,
        )
        .is_none());
    }

    #[test]
    fn force_mode_appends_only_additional_ocr_blocks() {
        let native = [native(0, "Native title\n", false)];
        let run = run(vec![routed_page(
            1,
            vec![
                span("Native title", 10.0, 0.9),
                span("Image-only label", 30.0, 0.9),
            ],
            Some(0.9),
        )]);

        let result = fuse_ocr_pages(&native, &run, 1, &OcrFusionOptions::new()).unwrap();

        assert_eq!(result.pages[0].provenance.source, PageContentSource::Fused);
        assert_eq!(result.pages[0].markdown.matches("Native title").count(), 1);
        assert!(result.pages[0].markdown.contains("Image-only label"));
    }

    #[test]
    fn force_mode_preserves_punctuation_and_short_distinct_blocks() {
        let (markdown, source) = merge_native_and_ocr("Figure 1\n", "1\n\n***");
        assert_eq!(source, PageContentSource::Fused);
        assert_eq!(markdown, "Figure 1\n\n1\n\n***\n");
    }

    #[test]
    fn force_mode_extracts_only_novel_tokens_from_overlapping_blocks() {
        let (markdown, source) = merge_native_and_ocr("A C\n", "## A B C");
        assert_eq!(source, PageContentSource::Fused);
        assert_eq!(markdown, "A C\n\nB\n");
        assert_eq!(markdown.matches('A').count(), 1);
        assert_eq!(markdown.matches('C').count(), 1);
    }

    #[test]
    fn force_mode_deduplicates_reordered_tokens() {
        let native = [native(0, "Alpha Beta Gamma\n", false)];
        let run = run(vec![routed_page(
            1,
            vec![span("Gamma Alpha Beta", 10.0, 0.9)],
            Some(0.9),
        )]);

        let result = fuse_ocr_pages(&native, &run, 1, &OcrFusionOptions::new()).unwrap();

        assert_eq!(result.pages[0].provenance.source, PageContentSource::Native);
        assert_eq!(result.pages[0].markdown, "Alpha Beta Gamma\n");
    }

    #[test]
    fn fusion_preserves_native_leading_indentation() {
        let native = [native(0, "    indented code\n", false)];
        let duplicate = run(vec![routed_page(
            1,
            vec![span("indented code", 10.0, 0.9)],
            Some(0.9),
        )]);
        let result = fuse_ocr_pages(&native, &duplicate, 1, &OcrFusionOptions::new()).unwrap();
        assert_eq!(result.pages[0].markdown, "    indented code\n");

        let addition = run(vec![routed_page(
            1,
            vec![span("image label", 10.0, 0.9)],
            Some(0.9),
        )]);
        let result = fuse_ocr_pages(&native, &addition, 1, &OcrFusionOptions::new()).unwrap();
        assert_eq!(
            result.pages[0].markdown,
            "    indented code\n\nimage label\n"
        );
    }

    #[test]
    fn missing_or_weak_required_ocr_recommends_hosted() {
        let native = [native(0, "", true), native(1, "", true)];
        let run = run(vec![routed_page(
            2,
            vec![span("uncertain", 10.0, 0.3)],
            Some(0.3),
        )]);

        let result = fuse_ocr_pages(&native, &run, 2, &OcrFusionOptions::new()).unwrap();

        assert!(result.pages[0].provenance.hosted_recommended);
        assert!(result.pages[1].provenance.hosted_recommended);
    }

    #[test]
    fn rejects_ocr_pages_outside_native_selection() {
        let error = fuse_ocr_pages(
            &[native(0, "text", false)],
            &run(vec![routed_page(2, Vec::new(), None)]),
            2,
            &OcrFusionOptions::new(),
        )
        .unwrap_err();
        assert_eq!(error, OcrFusionError::UnexpectedOcrPage { page: 2 });
    }

    #[test]
    fn time_shares_preserve_batch_total() {
        assert_eq!(equal_time_shares(8, 3), vec![3, 3, 2]);
        assert_eq!(equal_time_shares(8, 0), Vec::<u64>::new());
    }

    /// A scanned A4-like page (600 x 800 pt, drawn at 300 x 400 px): an
    /// optional `header` at the top edge, then 16 body lines unique to the
    /// page. `header_height` is the header box in pixels (body boxes are 9).
    fn scanned_page(page: u32, header: Option<&str>, header_height: f32) -> RoutedOcrPage {
        let transform =
            PageTransform::from_corners(300, 400, (0.0, 800.0), (600.0, 800.0), (0.0, 0.0))
                .unwrap();
        let rendered = RenderedPage::new(
            page,
            600.0,
            800.0,
            300,
            400,
            300,
            RenderPixelFormat::Gray8,
            vec![255; 120_000],
            transform,
        )
        .unwrap();
        let mut spans = Vec::new();
        if let Some(header) = header {
            spans.push(positioned_span(
                header,
                30.0,
                12.0,
                200.0,
                12.0 + header_height,
            ));
        }
        for line in 0..16u32 {
            let top = 60.0 + line as f32 * 18.0;
            spans.push(positioned_span(
                &format!("Страница {page}, строка {line}: условия договора поставки."),
                30.0,
                top,
                270.0,
                top + 9.0,
            ));
        }
        RoutedOcrPage {
            rendered,
            ocr: OcrPage {
                page_number: page,
                spans,
                mean_confidence: Some(0.9),
                model: ModelIdentity::new("test-ocr", "v1"),
                processing_time_ms: 7,
                warnings: Vec::new(),
            },
        }
    }

    fn fuse_scans(pages: Vec<RoutedOcrPage>, options: &OcrFusionOptions) -> FusedPages {
        let native: Vec<PageMarkdown> = pages
            .iter()
            .map(|page| native(page.rendered.page() - 1, "", true))
            .collect();
        let count = pages.len() as u32;
        fuse_ocr_pages(&native, &run(pages), count, options).unwrap()
    }

    fn strip_options() -> OcrFusionOptions {
        OcrFusionOptions::new().markdown(MarkdownOptions::default())
    }

    fn pages_showing(fused: &FusedPages, text: &str) -> Vec<u32> {
        fused
            .pages
            .iter()
            .filter(|page| page.markdown.contains(text))
            .map(|page| page.page_number)
            .collect()
    }

    fn repeated_lines(fused: &FusedPages) -> Vec<u32> {
        fused
            .pages
            .iter()
            .map(|page| page.provenance.repeated_lines)
            .collect()
    }

    #[test]
    fn scan_header_repeated_exactly_stays_on_its_first_page_only() {
        let fused = fuse_scans(
            (1..=4)
                .map(|page| scanned_page(page, Some("Дело № 2-123/2026"), 9.0))
                .collect(),
            &strip_options(),
        );
        assert_eq!(pages_showing(&fused, "Дело № 2-123/2026"), [1]);
        assert_eq!(repeated_lines(&fused), [0, 1, 1, 1]);
        for page in 1..=4u32 {
            for line in 0..16u32 {
                assert_eq!(
                    pages_showing(&fused, &format!("Страница {page}, строка {line}:")),
                    [page],
                    "body text must survive"
                );
            }
        }
    }

    #[test]
    fn scan_header_whose_case_number_changes_is_kept_on_every_page() {
        // Numbers count: a header that differs in a number is not a repeat,
        // whether the number sits inside it or at its end.
        let fused = fuse_scans(
            (1..=4)
                .map(|page| {
                    let header = format!("Дело № 2-12{page}/2026");
                    scanned_page(page, Some(&header), 9.0)
                })
                .collect(),
            &strip_options(),
        );
        for page in 1..=4u32 {
            assert_eq!(
                pages_showing(&fused, &format!("Дело № 2-12{page}/2026")),
                [page]
            );
        }
        assert_eq!(repeated_lines(&fused), [0, 0, 0, 0]);

        let fused = fuse_scans(
            [(1, 4171), (2, 4382), (3, 4593)]
                .into_iter()
                .map(|(page, case)| {
                    let header = format!("Case file number {case}");
                    scanned_page(page, Some(&header), 9.0)
                })
                .collect(),
            &strip_options(),
        );
        assert_eq!(pages_showing(&fused, "Case file number 4382"), [2]);
        assert_eq!(pages_showing(&fused, "Case file number 4593"), [3]);
        assert_eq!(repeated_lines(&fused), [0, 0, 0]);
    }

    #[test]
    fn scan_header_with_its_page_number_still_strips_as_a_repeat() {
        // A running header that carries the page number at its end differs
        // only by a number that rises with the page: today's page-number
        // tolerance, kept.
        let fused = fuse_scans(
            (1..=3)
                .map(|page| {
                    let header = format!("Annual Report of the Board {page}");
                    scanned_page(page, Some(&header), 9.0)
                })
                .collect(),
            &strip_options(),
        );
        assert_eq!(pages_showing(&fused, "Annual Report of the Board"), [1]);
        assert_eq!(repeated_lines(&fused), [0, 1, 1]);
    }

    #[test]
    fn scan_header_repeated_on_two_pages_stays_on_both() {
        // Two pages are no repetition evidence. A shorter box set apart
        // from the body is no evidence either on OCR text: the per-page
        // typographic fallback for short native documents does not apply.
        let fused = fuse_scans(
            (1..=2)
                .map(|page| scanned_page(page, Some("Дело № 2-123/2026"), 6.0))
                .collect(),
            &strip_options(),
        );
        assert_eq!(pages_showing(&fused, "Дело № 2-123/2026"), [1, 2]);
        assert_eq!(repeated_lines(&fused), [0, 0]);
    }

    #[test]
    fn scan_headers_stay_when_stripping_is_off() {
        let options = OcrFusionOptions::new().markdown(MarkdownOptions {
            strip_headers_footers: false,
            ..MarkdownOptions::default()
        });
        let fused = fuse_scans(
            (1..=4)
                .map(|page| scanned_page(page, Some("Дело № 2-123/2026"), 9.0))
                .collect(),
            &options,
        );
        assert_eq!(pages_showing(&fused, "Дело № 2-123/2026"), [1, 2, 3, 4]);
        assert_eq!(repeated_lines(&fused), [0, 0, 0, 0]);
    }

    /// A scanned page opening with a priced table: a column-header row,
    /// then eight rows of items unique to the page, then body lines.
    fn scanned_table_page(page: u32) -> RoutedOcrPage {
        let mut routed = scanned_page(page, None, 9.0);
        let mut spans = Vec::new();
        let row = |spans: &mut Vec<OcrSpan>, top: f32, cells: [&str; 3]| {
            for (cell, (left, right)) in
                cells
                    .iter()
                    .zip([(30.0, 120.0), (150.0, 200.0), (220.0, 270.0)])
            {
                spans.push(positioned_span(cell, left, top, right, top + 9.0));
            }
        };
        row(&mut spans, 12.0, ["Description", "Quantity", "Amount"]);
        for line in 0..8u32 {
            let item = format!("Item {page}-{line} cable reel");
            let quantity = format!("{}", line + 2);
            let amount = format!("{}.00", (page * 100 + line) * 3);
            row(
                &mut spans,
                26.0 + line as f32 * 12.0,
                [&item, &quantity, &amount],
            );
        }
        for line in 0..10u32 {
            let top = 150.0 + line as f32 * 18.0;
            spans.push(positioned_span(
                &format!("Sheet {page} note {line}: delivery terms apply to every item."),
                30.0,
                top,
                270.0,
                top + 9.0,
            ));
        }
        routed.ocr.spans = spans;
        routed
    }

    #[test]
    fn a_tables_repeated_column_headers_stay_with_each_table() {
        let fused = fuse_scans((1..=3).map(scanned_table_page).collect(), &strip_options());
        assert_eq!(pages_showing(&fused, "Description"), [1, 2, 3]);
        assert_eq!(pages_showing(&fused, "Quantity"), [1, 2, 3]);
        assert_eq!(repeated_lines(&fused), [0, 0, 0]);
    }

    #[test]
    fn a_page_that_may_keep_its_native_text_takes_no_part_in_repeats() {
        // Page 1 has a native candidate without the header, so it may keep
        // that instead of its OCR: it cannot hold the first occurrence, and
        // two pages remain, too few to prove a repeat.
        let pages: Vec<RoutedOcrPage> = (1..=3)
            .map(|page| scanned_page(page, Some("Дело № 2-123/2026"), 9.0))
            .collect();
        let native: Vec<PageMarkdown> = (0..3).map(|page| native(page, "", true)).collect();
        let candidates = BTreeMap::from([(
            1,
            native_candidate(
                "Страница 1, строка 0: условия договора поставки, согласованные сторонами.\n",
            ),
        )]);
        let fused = fuse_ocr_pages_adaptive(&native, &run(pages), 3, &strip_options(), &candidates)
            .unwrap();
        assert_eq!(pages_showing(&fused, "Дело № 2-123/2026")[1..], [2, 3]);
        assert_eq!(repeated_lines(&fused), [0, 0, 0]);
    }

    #[test]
    fn a_fused_page_counts_only_rows_its_content_left_out() {
        let header = ["Дело № 2-123/2026".to_string()];
        let footer = ["Confidential: Acme".to_string()];
        let both = [header[0].clone(), footer[0].clone()];
        // Native text that kept the header brings it back.
        let shown = "Дело № 2-123/2026\n\nТекст страницы.\n";
        assert_eq!(
            repeated_lines_left_out(PageContentSource::Fused, shown, &[], &header),
            0
        );
        assert_eq!(
            repeated_lines_left_out(PageContentSource::Fused, "Текст.\n", &[], &header),
            1
        );
        // A row removed from both sources is one row; escapes and spacing
        // do not bring a row back.
        assert_eq!(
            repeated_lines_left_out(PageContentSource::Fused, "Текст.\n", &both, &header),
            2
        );
        assert_eq!(
            repeated_lines_left_out(
                PageContentSource::Fused,
                "Confi dential: Acme\n",
                &footer,
                &[]
            ),
            0
        );
        // A single source counts what it removed.
        assert_eq!(
            repeated_lines_left_out(PageContentSource::Native, shown, &both, &[]),
            2
        );
        assert_eq!(
            repeated_lines_left_out(PageContentSource::Ocr, shown, &both, &header),
            1
        );
    }
}
