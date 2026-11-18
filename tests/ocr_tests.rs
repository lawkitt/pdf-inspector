#![cfg(all(feature = "ocr-oar", not(target_arch = "wasm32")))]

#[cfg(feature = "ocr")]
use pdf_inspector::vision::{
    process_pdf_with_ocr_mem, ModelDownloadPolicy, OcrPdfOptions, OcrPipelineError,
    PageContentSource,
};
use pdf_inspector::vision::{
    ModelStore, OarOcrEngine, OcrEngine, OcrMode, OcrOptions, PageTransform, RenderPixelFormat,
    RenderedPage, PP_OCR_V6_SMALL,
};
#[cfg(feature = "render-pdfium")]
use pdf_inspector::vision::{PdfiumRenderer, RenderError, RenderOptions};
#[cfg(feature = "ocr")]
use pdf_inspector::AnnotationKind;

const MODEL_DIRECTORY_ENV: &str = "PDF_INSPECTOR_OCR_TEST_MODELS";
const IMAGE_ENV: &str = "PDF_INSPECTOR_OCR_TEST_IMAGE";
const EXPECTED_TEXT_ENV: &str = "PDF_INSPECTOR_OCR_TEST_EXPECTED";

#[cfg(feature = "render-pdfium")]
fn load_renderer() -> Option<PdfiumRenderer> {
    match PdfiumRenderer::load() {
        Ok(renderer) => Some(renderer),
        Err(RenderError::PdfiumLoad { .. }) => {
            eprintln!("skipping OCR runtime test because no native PDFium library is installed");
            None
        }
        Err(error) => panic!("failed to load PDFium: {error}"),
    }
}

#[test]
fn recognizes_an_rgb_image_with_verified_models() {
    let Some(model_directory) = std::env::var_os(MODEL_DIRECTORY_ENV) else {
        eprintln!("skipping OCR runtime test because {MODEL_DIRECTORY_ENV} is not set");
        return;
    };
    let Some(image_path) = std::env::var_os(IMAGE_ENV) else {
        eprintln!("skipping OCR runtime test because {IMAGE_ENV} is not set");
        return;
    };

    let image = image::open(image_path).unwrap().into_rgb8();
    let (width, height) = image.dimensions();
    let transform = PageTransform::from_corners(
        width,
        height,
        (0.0, f64::from(height)),
        (f64::from(width), f64::from(height)),
        (0.0, 0.0),
    )
    .unwrap();
    let page = RenderedPage::new(
        1,
        width as f32,
        height as f32,
        width,
        height,
        width as usize * 3,
        RenderPixelFormat::Rgb8,
        image.into_raw(),
        transform,
    )
    .unwrap();
    let results = recognize(&model_directory, &[page]);
    assert_usable_result(&results);

    let text = results[0]
        .spans
        .iter()
        .map(|span| span.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    eprintln!("recognized: {text}");
    if let Ok(expected) = std::env::var(EXPECTED_TEXT_ENV) {
        assert!(
            text.to_lowercase().contains(&expected.to_lowercase()),
            "expected OCR output to contain {expected:?}, got {text:?}"
        );
    }
}

#[cfg(feature = "render-pdfium")]
#[test]
fn recognizes_a_pdfium_rendered_fixture_with_verified_models() {
    let Some(model_directory) = std::env::var_os(MODEL_DIRECTORY_ENV) else {
        eprintln!("skipping OCR runtime test because {MODEL_DIRECTORY_ENV} is not set");
        return;
    };
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
    let results = recognize(&model_directory, &pages);
    assert_usable_result(&results);
}

#[cfg(all(feature = "ocr", feature = "render-pdfium"))]
#[test]
fn complete_ocr_pipeline_routes_and_assembles_a_scanned_fixture() {
    let Some(model_directory) = std::env::var_os(MODEL_DIRECTORY_ENV) else {
        eprintln!("skipping OCR runtime test because {MODEL_DIRECTORY_ENV} is not set");
        return;
    };
    let Some(_renderer) = load_renderer() else {
        return;
    };

    let bytes = std::fs::read("tests/fixtures/scan_with_native_header_text.pdf").unwrap();
    let ocr = OcrOptions::new()
        .mode(OcrMode::Auto)
        .minimum_confidence(0.3)
        .model_directory(model_directory)
        .model_downloads(ModelDownloadPolicy::Offline);
    let options = OcrPdfOptions::new().ocr(ocr);
    let result = process_pdf_with_ocr_mem(&bytes, options.clone()).unwrap();
    let repeated = process_pdf_with_ocr_mem(&bytes, options).unwrap();

    assert_eq!(result.pages_routed_to_ocr, vec![1]);
    assert!(!result.markdown.trim().is_empty());
    assert!(result
        .markdown
        .contains("Order Date Item Code Description Status Unit Cost\n\n03/14/2024"));
    assert!(result.markdown.contains("$482,110.40\n\n05/02/2024"));
    assert_eq!(result.pages[0].provenance.source, PageContentSource::Fused);
    assert!(result.pages[0]
        .provenance
        .warnings
        .iter()
        .any(|warning| warning.contains("complementary OCR")));
    assert_eq!(
        result.pages[0].provenance.ocr_model.as_ref().unwrap().name,
        PP_OCR_V6_SMALL.id
    );
    assert_eq!(repeated.markdown, result.markdown);
}

#[cfg(all(feature = "ocr", feature = "render-pdfium"))]
#[test]
fn auto_recovers_credible_native_text_before_loading_ocr_models() {
    let Some(_renderer) = load_renderer() else {
        return;
    };

    let bytes = std::fs::read("tests/fixtures/shinagawa_identity_h.pdf").unwrap();
    let ocr = OcrOptions::new()
        .mode(OcrMode::Auto)
        .model_directory("/models/must-not-be-read")
        .model_downloads(ModelDownloadPolicy::Offline);
    let result = process_pdf_with_ocr_mem(&bytes, OcrPdfOptions::new().ocr(ocr)).unwrap();

    assert_eq!(result.pages_recommended_for_ocr, vec![1]);
    assert!(result.pages_routed_to_ocr.is_empty());
    assert!(result.markdown.contains("羽田空港新飛行経路"));
    assert!(result.markdown.contains("|4月30日|有|81.0|"));
    assert!(result.markdown.contains("※１ 最大騒音レベル"));
    assert!(result.pages_with_tables.contains(&1));
    assert_eq!(result.pages[0].provenance.source, PageContentSource::Native);
    assert!(result.pages[0].provenance.ocr_model.is_none());
}

#[cfg(all(feature = "ocr", feature = "render-pdfium"))]
#[test]
fn auto_rejects_garbled_native_recovery_and_continues_to_ocr() {
    let Some(_renderer) = load_renderer() else {
        return;
    };

    let bytes = std::fs::read("tests/fixtures/shifted_cipher_tounicode.pdf").unwrap();
    let ocr = OcrOptions::new()
        .mode(OcrMode::Auto)
        .model_directory("/models/must-not-be-read")
        .model_downloads(ModelDownloadPolicy::Offline);
    let error = process_pdf_with_ocr_mem(&bytes, OcrPdfOptions::new().ocr(ocr)).unwrap_err();

    assert!(matches!(
        error,
        OcrPipelineError::ModelAcquire(_) | OcrPipelineError::ModelStore(_)
    ));
}

/// A scanned page under a FreeText box, a note and a highlight with a
/// comment, and a stamp: OCR reads the page and the stamp, the comments stay
/// out of the Markdown and are reported as annotations, and asking for
/// comment annotations to be drawn brings their text back.
#[cfg(all(feature = "ocr", feature = "render-pdfium"))]
#[test]
fn ocr_leaves_comment_annotations_out_of_a_scanned_page() {
    let Some(model_directory) = std::env::var_os(MODEL_DIRECTORY_ENV) else {
        eprintln!("skipping OCR runtime test because {MODEL_DIRECTORY_ENV} is not set");
        return;
    };
    let Some(_renderer) = load_renderer() else {
        return;
    };

    let bytes = std::fs::read("tests/fixtures/scanned_comments.pdf").unwrap();
    let ocr = OcrOptions::new()
        .mode(OcrMode::Force)
        .minimum_confidence(0.3)
        .model_directory(model_directory)
        .model_downloads(ModelDownloadPolicy::Offline);
    let result = process_pdf_with_ocr_mem(&bytes, OcrPdfOptions::new().ocr(ocr.clone())).unwrap();

    assert_eq!(result.pages_routed_to_ocr, vec![1]);
    let markdown = result.markdown.to_lowercase();
    for kept in ["approved", "delivery", "otter"] {
        assert!(
            markdown.contains(kept),
            "{kept:?} missing from {markdown:?}"
        );
    }
    for comment in ["zebra", "walrus", "heron", "reviewer", "sticky"] {
        assert!(
            !markdown.contains(comment),
            "{comment:?} reached the Markdown: {markdown:?}"
        );
    }
    let reported: Vec<_> = result
        .annotations
        .iter()
        .map(|annotation| (annotation.kind, annotation.text.as_str()))
        .collect();
    assert_eq!(
        reported,
        [
            (AnnotationKind::FreeText, "Reviewer remark zebra"),
            (AnnotationKind::Text, "Sticky note walrus"),
            (AnnotationKind::Highlight, "Highlight comment heron"),
        ]
    );

    let drawn = process_pdf_with_ocr_mem(
        &bytes,
        OcrPdfOptions::new()
            .ocr(ocr)
            .render(RenderOptions::new().comment_annotations(true)),
    )
    .unwrap();
    let markdown = drawn.markdown.to_lowercase();
    for comment in ["zebra", "walrus"] {
        assert!(
            markdown.contains(comment),
            "{comment:?} missing from {markdown:?}"
        );
    }
}

fn recognize(
    model_directory: &std::ffi::OsStr,
    pages: &[RenderedPage],
) -> Vec<pdf_inspector::vision::OcrPage> {
    let store = ModelStore::new(model_directory).override_root(model_directory);
    let models = store.resolve(&PP_OCR_V6_SMALL).unwrap();
    let engine = OarOcrEngine::from_models(&models).unwrap();
    engine
        .recognize(
            pages,
            &OcrOptions::new()
                .mode(OcrMode::Force)
                .minimum_confidence(0.3),
        )
        .unwrap()
}

fn assert_usable_result(results: &[pdf_inspector::vision::OcrPage]) {
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].page_number, 1);
    assert_eq!(results[0].model.name, PP_OCR_V6_SMALL.id);
    assert_eq!(results[0].model.revision, PP_OCR_V6_SMALL.revision);
    assert!(!results[0].spans.is_empty());
    assert!(results[0].spans.iter().all(|span| span.confidence >= 0.3));
}

/// The model set the runtime tests resolve from `PDF_INSPECTOR_OCR_TEST_MODELS`:
/// PP-OCRv6 Small unless `PDF_INSPECTOR_OCR_TEST_MANIFEST` names the Cyrillic
/// set mdoc ships.
#[cfg(feature = "ocr")]
fn test_manifest() -> &'static pdf_inspector::vision::ModelManifest {
    match std::env::var("PDF_INSPECTOR_OCR_TEST_MANIFEST").as_deref() {
        Ok(id) if id == pdf_inspector::vision::PP_OCR_CYRILLIC.id => {
            &pdf_inspector::vision::PP_OCR_CYRILLIC
        }
        _ => &PP_OCR_V6_SMALL,
    }
}

#[cfg(all(feature = "ocr", feature = "render-pdfium"))]
fn force_ocr(path: &str, model_directory: &std::ffi::OsStr) -> pdf_inspector::vision::OcrPdfResult {
    let bytes = std::fs::read(path).unwrap();
    let ocr = OcrOptions::new()
        .mode(OcrMode::Force)
        .model_directory(model_directory)
        .model_downloads(ModelDownloadPolicy::Offline);
    let options = OcrPdfOptions {
        model_manifest: test_manifest(),
        retain_recognition: true,
        ..OcrPdfOptions::new().ocr(ocr)
    };
    process_pdf_with_ocr_mem(&bytes, options).unwrap()
}

/// #622: a page scanned sideways or upside down reads as the upright page
/// does, and every span reports how the text was turned.
#[cfg(all(feature = "ocr", feature = "render-pdfium"))]
#[test]
fn turned_scans_read_like_the_upright_page() {
    let Some(model_directory) = std::env::var_os(MODEL_DIRECTORY_ENV) else {
        eprintln!("skipping OCR runtime test because {MODEL_DIRECTORY_ENV} is not set");
        return;
    };
    let Some(_renderer) = load_renderer() else {
        return;
    };

    let upright = force_ocr(
        "tests/fixtures/ocr_qualification_english.pdf",
        &model_directory,
    );
    assert!(upright
        .markdown
        .contains("Payment must arrive within thirty working days."));
    assert!(upright.recognition[0]
        .spans
        .iter()
        .all(|span| span.orientation_degrees.is_none()));
    for degrees in [90.0, 180.0, 270.0] {
        let turned = force_ocr(
            &format!("tests/fixtures/ocr_rotated_{degrees}.pdf"),
            &model_directory,
        );
        eprintln!("{degrees}: {}", turned.markdown);
        assert_eq!(turned.markdown, upright.markdown, "turned {degrees}");
        let spans = &turned.recognition[0].spans;
        assert_eq!(spans.len(), upright.recognition[0].spans.len());
        assert!(spans
            .iter()
            .all(|span| span.orientation_degrees == Some(degrees)));
        assert!(turned.pages[0].provenance.warnings.is_empty());
        assert!(turned.pages_recommending_hosted.is_empty());
    }
}

/// #624: a clean scan keeps the native page's headings, paragraphs and list
/// items instead of running the page together.
#[cfg(all(feature = "ocr", feature = "render-pdfium"))]
#[test]
fn a_clean_scan_keeps_the_native_pages_structure() {
    let Some(model_directory) = std::env::var_os(MODEL_DIRECTORY_ENV) else {
        eprintln!("skipping OCR runtime test because {MODEL_DIRECTORY_ENV} is not set");
        return;
    };
    let Some(_renderer) = load_renderer() else {
        return;
    };

    let native = process_pdf_with_ocr_mem(
        &std::fs::read("tests/fixtures/ocr_clean_scan_paragraphs_native.pdf").unwrap(),
        OcrPdfOptions::new(),
    )
    .unwrap();
    let scanned = force_ocr(
        "tests/fixtures/ocr_clean_scan_paragraphs.pdf",
        &model_directory,
    );
    eprintln!("native:\n{}\nOCR:\n{}", native.markdown, scanned.markdown);

    // Block by block: a heading (at any level), a list item or a paragraph,
    // with the same words.
    let blocks = |markdown: &str| -> Vec<String> {
        markdown
            .split("\n\n")
            .flat_map(|block| {
                if block.lines().all(|line| line.starts_with(char::is_numeric)) {
                    block.lines().map(str::to_string).collect::<Vec<_>>()
                } else {
                    vec![block.to_string()]
                }
            })
            .map(|block| {
                let heading = block.starts_with('#');
                let words = block
                    .trim_start_matches('#')
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("{}{words}", if heading { "# " } else { "" })
            })
            .filter(|block| !block.is_empty())
            .collect()
    };
    assert_eq!(blocks(&scanned.markdown), blocks(&native.markdown));
}
