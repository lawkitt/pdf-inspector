//! PP-OCRv6 Small implementation backed by OAR and ONNX Runtime.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::Instant;

use image::RgbImage;
use oar_ocr::core::config::onnx::{OrtExecutionProvider, OrtSessionConfig};
use oar_ocr::domain::tasks::TextDetectionConfig;
use oar_ocr::oarocr::{EdgeProcessor, TextCroppingProcessor};
use oar_ocr::predictors::{TextDetectionPredictor, TextRecognitionPredictor};
use oar_ocr::processors::BoundingBox;
use thiserror::Error;

use super::{
    ImagePoint, ImageQuad, ModelArtifactKind, ModelIdentity, ModelPaths, OcrEngine, OcrMode,
    OcrOptions, OcrPage, OcrSpan, RenderPixelFormat, RenderedPage,
};

/// Environment variable selecting the ONNX Runtime shared library.
pub const ONNX_RUNTIME_LIBRARY_ENV: &str = "ORT_DYLIB_PATH";

/// Failures while constructing or running the OAR OCR backend.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OarOcrError {
    /// A required file is missing from the resolved model set.
    #[error("resolved OCR model set is missing {kind:?}")]
    MissingModelArtifact {
        /// Missing artifact role.
        kind: ModelArtifactKind,
    },
    /// OCR was invoked while the caller explicitly disabled it.
    #[error("OCR is disabled; select Auto or Force before invoking the engine")]
    OcrDisabled,
    /// A poolless fallback worker is already processing a page. Retrying
    /// after the active call completes is safe; waiting here is not, since
    /// the caller may be a Rayon task needed by that active call.
    #[error("sequential OCR fallback worker is busy; retry after the active call completes")]
    FallbackWorkerBusy,
    /// A panic may have left the fallback worker's model sessions unusable.
    #[error("sequential OCR fallback worker was poisoned by a panic; recreate the OCR engine")]
    FallbackWorkerPoisoned,
    /// Confidence thresholds must match the normalized engine output range.
    #[error("minimum OCR confidence must be finite and between 0 and 1, got {value}")]
    InvalidMinimumConfidence {
        /// Invalid threshold.
        value: f32,
    },
    /// Bitmap dimension arithmetic exceeded the host address space.
    #[error("rendered page {page} bitmap dimensions overflow the host address space")]
    ImageSizeOverflow {
        /// 1-indexed page number.
        page: u32,
    },
    /// A validated renderer buffer could not be represented as an RGB image.
    #[error("rendered page {page} could not be converted to an RGB image")]
    InvalidImageBuffer {
        /// 1-indexed page number.
        page: u32,
    },
    /// The external ONNX Runtime shared library could not be loaded.
    #[error(
        "failed to load ONNX Runtime from {path}; install a compatible ONNX Runtime shared library or set ORT_DYLIB_PATH to its path: {source}"
    )]
    OnnxRuntimeLoad {
        /// Requested shared-library path or platform library name.
        path: PathBuf,
        /// Dynamic-loader failure.
        #[source]
        source: ort::LoadDynamicError,
    },
    /// OAR returned no result for a submitted page.
    #[error("OAR returned no result for rendered page {page}")]
    MissingPageResult {
        /// 1-indexed page number.
        page: u32,
    },
    /// OAR or ONNX Runtime rejected the models or failed during inference.
    #[error(transparent)]
    Backend(#[from] oar_ocr::core::OCRError),
}

/// Standard detection input cap. PP-OCR detection resizes each page so its
/// longest side fits this before inference; it is the PaddleOCR default and
/// is sufficient for ordinary body text at 150 DPI.
const DETECTION_LIMIT_STANDARD: u32 = 960;

/// Escalated detection input cap for dense fine-print pages. Beyond this the
/// measured recall plateaus while inference cost keeps growing.
const DETECTION_LIMIT_ESCALATED: u32 = 2560;

/// Hard ceiling protecting detection from out-of-memory on giant renders.
const DETECTION_MAXIMUM_SIDE: u32 = 4000;

/// Escalate pages dense with small text: at least this many detected
/// regions in the standard pass...
const ESCALATION_MINIMUM_REGIONS: usize = 80;

/// ...whose median height, at detection scale, is below this. Calibrated at
/// `unclip_ratio` 2.0 (the expansion inflates measured heights, so this
/// constant is coupled to [`detection_config`]): dense fine-print pages that
/// gain from escalation measure 12.0–14.2 px with 144+ regions; the nearest
/// non-gaining page above the region gate (an engineering drawing) measures
/// 15.7 px, and prose/typewriter pages measure 14.5 px+ with too few
/// regions to qualify at all.
const ESCALATION_MAXIMUM_MEDIAN_HEIGHT: f32 = 15.0;

/// Also escalate sparse small print, however few its rows: a median height
/// at detection scale below this...
///
/// At the standard limit a 6–7 pt page at 150 DPI is detected at about
/// 82 DPI, where adjacent rows merge into one region and whole rows drop out
/// of recognition (firecrawl/pdf-inspector#623). Measured heights come in
/// steps of about 3 px (unclip 2.0 roughly triples the detector's kernel
/// height, so this constant is coupled to [`detection_config`] like the one
/// above): 6–9 pt Arial measures 12.0 px on A4 and 11.6 px on Letter, 10 pt
/// and up 14.5 px or more. Every page measured below 13 px gained or was
/// unchanged under escalation (30-row statements at 6–7 pt recover every
/// dropped row); ordinary pages measure 14.1 px and up, and escalating those
/// both gained and lost lines, so they stay on the standard pass. Detection
/// cannot tell 8–9 pt from 6–7 pt at this scale, so those pages pay too.
const SPARSE_ESCALATION_MAXIMUM_MEDIAN_HEIGHT: f32 = 13.0;

/// ...with at least this many regions. An 8-row statement at 6 pt (9 regions
/// with its title) still garbled a row past use at the standard limit (its
/// date and payee gone); 4-row ones (5 regions) and a scan with six tiny
/// regions read the same either way, so pages with a stray caption or
/// speckle stay on the standard pass.
const SPARSE_ESCALATION_MINIMUM_REGIONS: usize = 8;

/// A page is on its side when at least this many detected lines run
/// vertically (a sideways note of two lines is still a page on its side)...
const SIDEWAYS_MINIMUM_LINES: usize = 2;

/// ...and they outnumber the horizontal lines by more than this factor. A
/// line counts in either direction only when it is at least
/// [`LINE_ELONGATION`] times as long as it is thick: a word or a digit on its
/// own is often taller than wide.
const SIDEWAYS_DOMINANCE: usize = 2;

/// Length-to-thickness ratio from which a detected region counts as a line.
const LINE_ELONGATION: f32 = 3.0;

/// Pages read below this mean confidence are checked for being upside down.
/// Upright pages of mdoc's OCR corpus, photographs and old scans included,
/// read at 0.93 or more; upside-down text reads far lower (#622 reports
/// 0.59–0.69).
const UPSIDE_DOWN_CHECK_CONFIDENCE: f32 = 0.9;

/// How many of the page's longest regions the upside-down check reads again
/// turned half round.
const UPSIDE_DOWN_SAMPLE: usize = 8;

/// The half-turned sample must read this much more confidently on average
/// than the same regions as they are for the page to be upside down.
const UPSIDE_DOWN_MARGIN: f32 = 0.15;

/// A page turned upright that still reads below this mean confidence keeps a
/// warning: the turn did not make it reliable.
const TURNED_PAGE_WEAK_CONFIDENCE: f32 = 0.8;

/// One worker's model sessions: a standard-limit detector plus a recognizer,
/// and that worker's own lazily built escalated-limit detector.
/// Staged (detect, crop, recognize as separate calls) rather than OAROCR's
/// combined `predict` so an escalated page replaces only its detection pass —
/// recognition runs exactly once, on the final region set.
struct OcrWorker {
    detector: TextDetectionPredictor,
    recognizer: TextRecognitionPredictor,
    /// Built on this worker's first dense fine-print page. `None` inside the
    /// cell records a failed build so it is not retried per page.
    escalated: std::sync::OnceLock<Option<TextDetectionPredictor>>,
    /// Invariant: both `recognize` branches (sequential and parallel) run a
    /// page's work inside `pool.install(..)`. oar-ocr calls rayon
    /// internally (`par_iter`/`par_chunks*`) while holding this worker's
    /// session mutex; on a one-thread pool that call only ever finds work
    /// in its own local deque, so it cannot pick up another page's job.
    /// If the first pool cannot start, the fallback admits only one page
    /// at a time and rejects competing or reentrant callers without
    /// blocking a Rayon thread needed by the active page.
    pool: WorkerPool,
}

/// A worker's dedicated pool, or a nonblocking admission gate when no
/// dedicated thread could be started. The gate covers the entire page,
/// including nested Rayon work performed while OAR holds its session lock.
enum WorkerPool {
    Dedicated(rayon::ThreadPool),
    Sequential(Mutex<()>),
}

/// Runs page work with exclusive access to one worker's model sessions.
/// Admission failures and page failures share the worker's error type so
/// neither dispatch branch can accidentally ignore a failed admission.
trait PageWorker: Sync {
    type Error: Send;

    fn install<R: Send>(
        &self,
        f: impl FnOnce() -> Result<R, Self::Error> + Send,
    ) -> Result<R, Self::Error>;
}

impl PageWorker for WorkerPool {
    type Error = OarOcrError;

    fn install<R: Send>(
        &self,
        f: impl FnOnce() -> Result<R, Self::Error> + Send,
    ) -> Result<R, Self::Error> {
        match self {
            Self::Dedicated(pool) => pool.install(f),
            Self::Sequential(gate) => {
                // Never wait, spin, or yield while another page owns this
                // gate. This caller may be nested Rayon work that the owner
                // itself is waiting for, including reentry on the same thread.
                let _guard = gate.try_lock().map_err(|error| match error {
                    TryLockError::WouldBlock => OarOcrError::FallbackWorkerBusy,
                    TryLockError::Poisoned(_) => OarOcrError::FallbackWorkerPoisoned,
                })?;
                f()
            }
        }
    }
}

impl PageWorker for OcrWorker {
    type Error = OarOcrError;

    fn install<R: Send>(
        &self,
        f: impl FnOnce() -> Result<R, Self::Error> + Send,
    ) -> Result<R, Self::Error> {
        self.pool.install(f)
    }
}

/// CPU PP-OCRv6 Small engine using OAR's detection and recognition components.
///
/// Construction accepts only [`ModelPaths`] that have already passed
/// pdf-inspector's manifest size and SHA-256 verification. OAR's independent
/// model auto-download feature is deliberately not enabled.
///
/// If no dedicated worker thread can start, uncontended calls still run
/// sequentially. Overlapping page calls in that fallback return
/// [`OarOcrError::FallbackWorkerBusy`] rather than blocking; callers may
/// retry after the active call completes.
pub struct OarOcrEngine {
    workers: Vec<OcrWorker>,
    detection_path: PathBuf,
    intra_threads: usize,
    model: ModelIdentity,
}

impl std::fmt::Debug for OarOcrEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OarOcrEngine")
            .field("workers", &self.workers.len())
            .field("parallel", &(self.workers.len() > 1))
            .field("model", &self.model)
            .finish()
    }
}

/// Pages processed concurrently: one OAROCR pipeline (and its ONNX sessions)
/// per worker, because oar-ocr serializes each session behind a mutex.
/// Measured on CPU: workers beyond 3 stop scaling (memory-bandwidth bound)
/// and each worker is fastest with 2 intra-op threads.
fn pipeline_concurrency() -> usize {
    let cores = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);
    (cores / 4).clamp(1, 3)
}

fn intra_threads_per_pipeline(concurrency: usize) -> usize {
    let cores = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);
    if concurrency > 1 {
        2
    } else {
        cores.min(4)
    }
}

/// True when a standard-limit detection pass over a downscaled page shows
/// dense small text, or very small text in any quantity: the page deserves a
/// second pass at the escalated limit.
fn should_escalate_detection(
    median_detection_height: f32,
    region_count: usize,
    downscale: f32,
) -> bool {
    let dense = region_count >= ESCALATION_MINIMUM_REGIONS
        && median_detection_height < ESCALATION_MAXIMUM_MEDIAN_HEIGHT;
    let sparse_small_print = region_count >= SPARSE_ESCALATION_MINIMUM_REGIONS
        && median_detection_height < SPARSE_ESCALATION_MAXIMUM_MEDIAN_HEIGHT;
    downscale < 1.0 && (dense || sparse_small_print)
}

/// Median detected-region height in detection-input pixels: original-image
/// heights multiplied by the downscale detection applied.
fn median_detection_height(heights: &mut [f32], downscale: f32) -> f32 {
    if heights.is_empty() {
        return f32::MAX;
    }
    heights.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let middle = heights.len() / 2;
    let median = if heights.len().is_multiple_of(2) {
        (heights[middle - 1] + heights[middle]) / 2.0
    } else {
        heights[middle]
    };
    median * downscale
}

/// Detection preprocessing config at a given input cap.
///
/// Supplying an explicit config suppresses OAROCR's "general" text-type
/// overrides, so every field the override would have set must be pinned
/// here to match what the combined pipeline ran with before the staged
/// split: score 0.3 and box 0.6 (equal to [`TextDetectionConfig`]'s
/// defaults) and unclip 2.0 (the default is 1.5 — leaving it would
/// silently shrink detection-box expansion and risk clipping edge glyphs).
fn detection_config(detection_limit: u32) -> TextDetectionConfig {
    TextDetectionConfig {
        limit_side_len: Some(detection_limit),
        limit_type: Some(oar_ocr::processors::LimitType::Max),
        max_side_len: Some(DETECTION_MAXIMUM_SIDE),
        unclip_ratio: 2.0,
        ..Default::default()
    }
}

fn build_detector(
    detection: &std::path::Path,
    detection_limit: u32,
    intra_threads: usize,
) -> Result<TextDetectionPredictor, OarOcrError> {
    Ok(TextDetectionPredictor::builder()
        .with_config(detection_config(detection_limit))
        .with_ort_config(ocr_session_config(intra_threads))
        .build(detection)?)
}

fn build_workers(
    detection: &std::path::Path,
    recognition: &std::path::Path,
    dictionary: &std::path::Path,
    count: usize,
    intra_threads: usize,
) -> Result<Vec<OcrWorker>, OarOcrError> {
    // Pools are planned (and any failure resolved) before any model session
    // is built, so a worker whose pool never starts never wastes a session.
    let pools = plan_worker_pools(count, |index| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .thread_name(move |_| format!("pdf-inspector-ocr-{index}"))
            .build()
    });
    let mut workers = Vec::with_capacity(pools.len());
    for pool in pools {
        let detector = build_detector(detection, DETECTION_LIMIT_STANDARD, intra_threads)?;
        let recognizer = TextRecognitionPredictor::builder()
            .dict_path(dictionary)
            .with_ort_config(ocr_session_config(intra_threads))
            .build(recognition)?;
        workers.push(OcrWorker {
            detector,
            recognizer,
            escalated: std::sync::OnceLock::new(),
            pool,
        });
    }
    Ok(workers)
}

/// Builds up to `count` worker pools with `build`, stopping at the first
/// failure instead of returning it: environments with a thread limit (a
/// container's pids limit, for example) must still be able to run OCR.
///
/// A failure at index 0 falls back to a single worker with a nonblocking
/// admission gate. Uncontended calls still run sequentially, but competing
/// or reentrant calls fail with `FallbackWorkerBusy` before entering OAR.
/// A failure at any later index keeps the workers already built and drops
/// the rest, rather than falling back further.
fn plan_worker_pools<E: std::fmt::Display>(
    count: usize,
    mut build: impl FnMut(usize) -> Result<rayon::ThreadPool, E>,
) -> Vec<WorkerPool> {
    let mut pools = Vec::with_capacity(count);
    for index in 0..count {
        match build(index) {
            Ok(pool) => pools.push(WorkerPool::Dedicated(pool)),
            Err(error) if index == 0 => {
                log::warn!(
                    "OCR worker thread pool unavailable, falling back to a single \
                     sequential worker (concurrent calls return a busy error): {error}"
                );
                pools.push(WorkerPool::Sequential(Mutex::new(())));
                break;
            }
            Err(error) => {
                log::warn!(
                    "OCR worker thread pool unavailable after {index} worker(s) started; \
                     continuing with {index}: {error}"
                );
                break;
            }
        }
    }
    pools
}

impl OarOcrEngine {
    /// Loads PP-OCRv6 Small from a resolved, verified model set.
    pub fn from_models(models: &ModelPaths) -> Result<Self, OarOcrError> {
        Self::from_models_with_runtime(models, None)
    }

    /// Loads a verified model set with an optional explicit ONNX Runtime path.
    /// ONNX Runtime is process-wide; select its path before the first engine.
    pub fn from_models_with_runtime(
        models: &ModelPaths,
        runtime: Option<&Path>,
    ) -> Result<Self, OarOcrError> {
        load_onnx_runtime(runtime)?;
        let detection = required_model(models, ModelArtifactKind::TextDetection)?;
        let recognition = required_model(models, ModelArtifactKind::TextRecognition)?;
        let dictionary = required_model(models, ModelArtifactKind::CharacterDictionary)?;

        let concurrency = pipeline_concurrency();
        let intra_threads = intra_threads_per_pipeline(concurrency);
        let workers = build_workers(
            detection,
            recognition,
            dictionary,
            concurrency,
            intra_threads,
        )?;
        let model = ModelIdentity::new(models.manifest_id(), models.revision());
        Ok(Self {
            workers,
            detection_path: detection.to_path_buf(),
            intra_threads,
            model,
        })
    }

    /// This worker's escalated-limit detector, built on first use.
    fn escalated_detector<'w>(&self, worker: &'w OcrWorker) -> Option<&'w TextDetectionPredictor> {
        worker
            .escalated
            .get_or_init(|| {
                match build_detector(
                    &self.detection_path,
                    DETECTION_LIMIT_ESCALATED,
                    self.intra_threads,
                ) {
                    Ok(detector) => Some(detector),
                    Err(error) => {
                        log::warn!(
                            "escalated OCR detection unavailable, keeping standard pass: {error}"
                        );
                        None
                    }
                }
            })
            .as_ref()
    }

    /// Detects text regions for one page: a standard-limit pass first, then —
    /// for pages the standard limit demonstrably under-resolves — a second
    /// pass at the escalated limit whose boxes replace the first. Pages whose
    /// render dwarfs even the escalated limit skip the standard pass outright.
    fn detect_boxes(
        &self,
        page: &RenderedPage,
        image: &Arc<RgbImage>,
        worker: &OcrWorker,
    ) -> Result<Vec<BoundingBox>, OarOcrError> {
        let longest_side = page.width().max(page.height()) as f32;

        // A page more than twice the standard limit loses over half its
        // resolution before detection even runs; go straight to the escalated
        // detector instead of paying a doomed standard pass.
        if longest_side > (DETECTION_LIMIT_STANDARD * 2) as f32 {
            if let Some(escalated) = self.escalated_detector(worker) {
                log::debug!(
                    "page {}: direct escalated detection (render {longest_side}px)",
                    page.page(),
                );
                match detect_with(escalated, image, page.page()) {
                    Ok(boxes) => return Ok(boxes),
                    Err(error) => {
                        // Same degradation as the adaptive branch below: a
                        // failing escalated pass falls back to standard
                        // detection instead of failing the page outright.
                        // Return the standard boxes directly — the adaptive
                        // trigger would only re-invoke the detector that
                        // just failed (repeating an OOM on a dense page).
                        log::warn!(
                            "page {}: direct escalated detection failed, using standard pass: {error}",
                            page.page()
                        );
                        return detect_with(&worker.detector, image, page.page());
                    }
                }
            }
        }

        let detections = detect_with(&worker.detector, image, page.page())?;

        // Dense fine-print pages (broadsheets, pricing sheets) lose most of
        // their text when detection downscales them to the standard limit,
        // and sparse small print (a 6–7 pt statement) loses whole rows.
        // When the standard pass shows many regions of small detection-scale
        // height, or fewer regions of very small height, rerun detection at
        // the escalated limit.
        let downscale = (DETECTION_LIMIT_STANDARD as f32 / longest_side).min(1.0);
        let mut heights: Vec<f32> = detections.iter().map(polygon_height).collect();
        let median = median_detection_height(&mut heights, downscale);
        log::trace!(
            "page {}: standard pass {} regions, median height {:.1}px at detection scale",
            page.page(),
            detections.len(),
            median
        );
        if should_escalate_detection(median, detections.len(), downscale) {
            log::debug!(
                "page {}: escalating detection ({} regions, median height {:.1}px at detection scale)",
                page.page(),
                detections.len(),
                median
            );
            if let Some(escalated) = self.escalated_detector(worker) {
                match detect_with(escalated, image, page.page()) {
                    Ok(escalated_boxes) => return Ok(escalated_boxes),
                    Err(error) => {
                        log::warn!(
                            "page {}: escalated detection failed, keeping standard pass: {error}",
                            page.page()
                        );
                    }
                }
            }
        }
        Ok(detections)
    }

    fn recognize_page(
        &self,
        page: &RenderedPage,
        options: &OcrOptions,
        worker: &OcrWorker,
    ) -> Result<OcrPage, OarOcrError> {
        let started = Instant::now();
        let mut image = Arc::new(rendered_page_to_rgb(page)?);
        let mut boxes = self.detect_boxes(page, &image, worker)?;

        // A page scanned on its side or upside down (#622). The pinned model
        // set has no orientation classifier, so the turn is found from what
        // detection and recognition already produce: lines that run
        // vertically put the page on its side, and a page that reads poorly
        // is tried half turned on a sample of its lines. The page is turned
        // upright and detected again, so the layout sees level lines.
        let mut turns = 0u8;
        if lines_run_vertically(&boxes) {
            turns = 1;
            image = Arc::new(image::imageops::rotate90(&*image));
            boxes = self.detect_boxes(page, &image, worker)?;
        }
        let mut regions = recognize_regions(&image, boxes, worker)?;
        // A page set on its side may have been turned the wrong way round,
        // so it is always checked; an unturned page only when it reads
        // poorly.
        let checked = turns != 0 || mean_region_confidence(&regions) < UPSIDE_DOWN_CHECK_CONFIDENCE;
        let mut retry_failed = false;
        if checked && reads_upside_down(&image, &regions, worker)? {
            // The sample says half turned; the whole page turned must agree
            // before it replaces the first reading, which is kept if the
            // retry fails.
            let flipped = Arc::new(image::imageops::rotate180(&*image));
            match self
                .detect_boxes(page, &flipped, worker)
                .and_then(|boxes| recognize_regions(&flipped, boxes, worker))
            {
                Ok(flipped_regions) if reads_better(&flipped_regions, &regions) => {
                    turns += 2;
                    image = flipped;
                    regions = flipped_regions;
                }
                Ok(_) => {}
                Err(error) => {
                    log::warn!(
                        "page {}: half-turned retry failed, keeping the first reading: {error}",
                        page.page()
                    );
                    retry_failed = true;
                }
            }
        }
        // Judged on every region, before the confidence filter below drops
        // the weak ones.
        let checked_confidence = mean_region_confidence(&regions);
        let (turned_width, turned_height) = image.dimensions();
        drop(image);

        let orientation_degrees = (turns != 0).then(|| f32::from((4 - turns) % 4) * 90.0);
        let mut spans = Vec::with_capacity(regions.len());
        let mut invalid_geometry = 0usize;
        let mut missing_recognition = 0usize;
        for region in regions {
            let (text, confidence) = match region.outcome {
                RegionOutcome::InvalidGeometry => {
                    invalid_geometry += 1;
                    continue;
                }
                RegionOutcome::Unrecognized => {
                    missing_recognition += 1;
                    continue;
                }
                RegionOutcome::Recognized { text, confidence } => (text, confidence),
            };
            if confidence < options.minimum_confidence {
                continue;
            }

            let Some(polygon) =
                bounding_box_to_quad(&region.bounding_box, turned_width, turned_height)
            else {
                invalid_geometry += 1;
                continue;
            };
            spans.push(OcrSpan {
                text,
                polygon: unturn_quad(polygon, turns, page.width(), page.height()),
                confidence,
                // No text-line orientation classifier is loaded (the
                // combined pipeline's per-line angle was always None too).
                // A whole page read turned upright reports that page's turn
                // on every span; region rotation within the page is carried
                // by the polygon itself.
                orientation_degrees,
            });
        }

        let mut warnings = Vec::new();
        if missing_recognition > 0 {
            warnings.push(format!(
                "discarded {missing_recognition} regions without usable recognition output"
            ));
        }
        if invalid_geometry > 0 {
            warnings.push(format!(
                "discarded {invalid_geometry} recognized regions with invalid geometry"
            ));
        }

        let mean_confidence = if spans.is_empty() {
            None
        } else {
            Some(spans.iter().map(|span| span.confidence).sum::<f32>() / spans.len() as f32)
        };
        if let Some(degrees) = orientation_degrees {
            log::debug!("page {}: text turned {degrees}° clockwise", page.page());
        }
        warnings.extend(orientation_warning(
            orientation_degrees,
            checked,
            Some(checked_confidence),
        ));
        if retry_failed {
            warnings.push(
                "page text may be upside down; reading it turned half round failed".to_string(),
            );
        }
        let processing_time_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

        Ok(OcrPage {
            page_number: page.page(),
            spans,
            mean_confidence,
            model: self.model.clone(),
            processing_time_ms,
            warnings,
        })
    }
}

/// The warning for a page whose orientation was checked (it lay on its
/// side, or read below [`UPSIDE_DOWN_CHECK_CONFIDENCE`]) and that still
/// reads below [`TURNED_PAGE_WEAK_CONFIDENCE`]: the turn found, if any, did
/// not make it reliable, so the text may be garbled.
fn orientation_warning(
    orientation_degrees: Option<f32>,
    checked: bool,
    mean_confidence: Option<f32>,
) -> Option<String> {
    let weak = mean_confidence.is_none_or(|confidence| confidence < TURNED_PAGE_WEAK_CONFIDENCE);
    if !checked || !weak {
        return None;
    }
    Some(match orientation_degrees {
        Some(degrees) => format!(
            "page text is turned {degrees}° clockwise; it was read turned upright, \
             but recognition stays weak"
        ),
        None => "page text reads weakly and no turn of the page reads clearly better; \
                 it may be turned, slanted or hard to read"
            .to_string(),
    })
}

/// What recognition made of one detected region.
enum RegionOutcome {
    /// The region could not be cropped from the page.
    InvalidGeometry,
    /// The recognizer returned no usable text for it.
    Unrecognized,
    /// Recognized text and its confidence in 0–1.
    Recognized { text: String, confidence: f32 },
}

/// One detected region and what recognition made of it.
struct RecognizedRegion {
    bounding_box: BoundingBox,
    outcome: RegionOutcome,
}

/// Crops and recognizes every detected region of `image`, in reading order.
fn recognize_regions(
    image: &Arc<RgbImage>,
    boxes: Vec<BoundingBox>,
    worker: &OcrWorker,
) -> Result<Vec<RecognizedRegion>, OarOcrError> {
    // Reading order, matching what the combined pipeline produced.
    let boxes = oar_ocr::processors::sort_quad_boxes(&boxes);
    // Same rotation-aware cropping the combined pipeline uses.
    let crops = TextCroppingProcessor::new(true).process((Arc::clone(image), boxes.clone()))?;
    let mut regions = Vec::with_capacity(boxes.len());
    for (bounding_box, crop) in boxes.into_iter().zip(crops) {
        let outcome = match crop {
            None => RegionOutcome::InvalidGeometry,
            // One crop per call: document line crops often have very
            // different widths, and batching pads every crop to the widest
            // line. Measured on CPU, batched recognition (even width-sorted)
            // is 2–3× slower than per-crop calls.
            Some(crop) => recognize_crop(
                &worker.recognizer,
                Arc::try_unwrap(crop).unwrap_or_else(|shared| (*shared).clone()),
            )?,
        };
        regions.push(RecognizedRegion {
            bounding_box,
            outcome,
        });
    }
    Ok(regions)
}

fn recognize_crop(
    recognizer: &TextRecognitionPredictor,
    crop: RgbImage,
) -> Result<RegionOutcome, OarOcrError> {
    let recognized = recognizer.predict(vec![crop])?;
    let (Some(text), Some(confidence)) = (
        recognized.texts.into_iter().next(),
        recognized.scores.into_iter().next(),
    ) else {
        return Ok(RegionOutcome::Unrecognized);
    };
    if text.trim().is_empty() || !confidence.is_finite() {
        return Ok(RegionOutcome::Unrecognized);
    }
    Ok(RegionOutcome::Recognized {
        text,
        confidence: confidence.clamp(0.0, 1.0),
    })
}

/// Confidence of a region for orientation checks: zero when nothing usable
/// was recognized, which is what upside-down text often gives.
fn region_confidence(region: &RecognizedRegion) -> f32 {
    match region.outcome {
        RegionOutcome::Recognized { confidence, .. } => confidence,
        _ => 0.0,
    }
}

/// Mean confidence over the recognized regions; zero when there are none
/// but there were regions to read, one when the page has no regions at all.
fn mean_region_confidence(regions: &[RecognizedRegion]) -> f32 {
    let recognized: Vec<f32> = regions
        .iter()
        .filter_map(|region| match region.outcome {
            RegionOutcome::Recognized { confidence, .. } => Some(confidence),
            _ => None,
        })
        .collect();
    if regions.is_empty() {
        1.0
    } else if recognized.is_empty() {
        0.0
    } else {
        recognized.iter().sum::<f32>() / recognized.len() as f32
    }
}

/// True when `candidate` reads better than `current`: a higher mean
/// confidence over at least half as many recognized regions.
fn reads_better(candidate: &[RecognizedRegion], current: &[RecognizedRegion]) -> bool {
    let recognized = |regions: &[RecognizedRegion]| {
        regions
            .iter()
            .filter(|region| matches!(region.outcome, RegionOutcome::Recognized { .. }))
            .count()
    };
    mean_region_confidence(candidate) > mean_region_confidence(current)
        && recognized(candidate) * 2 >= recognized(current)
}

/// Length and thickness of a detected region: its longer and shorter edge,
/// and whether the longer one runs vertically.
fn region_extent(region: &BoundingBox) -> Option<(f32, f32, bool)> {
    let [p0, p1, p2, ..] = region.points.as_slice() else {
        return None;
    };
    let first = (p1.x - p0.x, p1.y - p0.y);
    let second = (p2.x - p1.x, p2.y - p1.y);
    let (first_length, second_length) = (first.0.hypot(first.1), second.0.hypot(second.1));
    let (long, length, thickness) = if first_length >= second_length {
        (first, first_length, second_length)
    } else {
        (second, second_length, first_length)
    };
    (length.is_finite() && thickness.is_finite()).then_some((
        length,
        thickness,
        long.1.abs() > long.0.abs(),
    ))
}

/// True when most of the page's text lines run vertically: the page lies on
/// its side. A vertical margin note on an upright page does not tip it.
fn lines_run_vertically(boxes: &[BoundingBox]) -> bool {
    let (mut vertical, mut horizontal) = (0usize, 0usize);
    for (length, thickness, runs_vertically) in boxes.iter().filter_map(region_extent) {
        if thickness <= 0.0 || length < thickness * LINE_ELONGATION {
            continue;
        }
        if runs_vertically {
            vertical += 1;
        } else {
            horizontal += 1;
        }
    }
    vertical >= SIDEWAYS_MINIMUM_LINES && vertical > horizontal * SIDEWAYS_DOMINANCE
}

/// True when the page's longest regions read clearly better turned half
/// round: the page is upside down.
fn reads_upside_down(
    image: &Arc<RgbImage>,
    regions: &[RecognizedRegion],
    worker: &OcrWorker,
) -> Result<bool, OarOcrError> {
    let mut sample: Vec<(f32, &RecognizedRegion)> = regions
        .iter()
        .filter_map(|region| {
            let (length, ..) = region_extent(&region.bounding_box)?;
            Some((length, region))
        })
        .collect();
    sample.sort_by(|a, b| b.0.total_cmp(&a.0));
    sample.truncate(UPSIDE_DOWN_SAMPLE);
    if sample.is_empty() {
        return Ok(false);
    }
    let boxes = sample
        .iter()
        .map(|(_, region)| region.bounding_box.clone())
        .collect();
    let crops = TextCroppingProcessor::new(true).process((Arc::clone(image), boxes))?;
    let (mut as_is, mut turned) = (0.0f32, 0.0f32);
    for ((_, region), crop) in sample.iter().zip(crops) {
        as_is += region_confidence(region);
        if let Some(crop) = crop {
            let crop = image::imageops::rotate180(&*crop);
            if let RegionOutcome::Recognized { confidence, .. } =
                recognize_crop(&worker.recognizer, crop)?
            {
                turned += confidence;
            }
        }
    }
    let count = sample.len() as f32;
    let (as_is, turned) = (as_is / count, turned / count);
    log::debug!("upside-down check: {as_is:.3} as is, {turned:.3} turned half round");
    Ok(turned >= as_is + UPSIDE_DOWN_MARGIN)
}

/// Maps a quad found on the page turned `turns` quarter turns clockwise back
/// onto the page as rendered, `width` × `height` pixels.
fn unturn_quad(quad: ImageQuad, turns: u8, width: u32, height: u32) -> ImageQuad {
    let (width, height) = (width as f32, height as f32);
    ImageQuad::new(quad.points.map(|point| {
        let (u, v) = (point.x, point.y);
        match turns % 4 {
            1 => ImagePoint::new(v, height - u),
            2 => ImagePoint::new(width - u, height - v),
            3 => ImagePoint::new(width - v, u),
            _ => point,
        }
    }))
}

/// Runs one detector over one page image and returns its region polygons.
fn detect_with(
    detector: &TextDetectionPredictor,
    image: &Arc<RgbImage>,
    page_number: u32,
) -> Result<Vec<BoundingBox>, OarOcrError> {
    let mut result = detector.predict(vec![(**image).clone()])?;
    if result.detections.is_empty() {
        return Err(OarOcrError::MissingPageResult { page: page_number });
    }
    Ok(result
        .detections
        .swap_remove(0)
        .into_iter()
        .map(|detection| detection.bbox)
        .collect())
}

/// Vertical extent of a detection polygon in original-image pixels.
fn polygon_height(polygon: &BoundingBox) -> f32 {
    let mut min_y = f32::MAX;
    let mut max_y = f32::MIN;
    for point in &polygon.points {
        min_y = min_y.min(point.y);
        max_y = max_y.max(point.y);
    }
    if max_y > min_y {
        max_y - min_y
    } else {
        0.0
    }
}

/// Sessions hand their working memory back after every run. The engine is
/// cached for the life of the process, and ONNX Runtime's defaults keep it:
/// the CPU arena grows to the largest page a session has seen and never
/// shrinks, and the memory pattern plans each new input shape and caches
/// the plan. Page sizes vary, so the plans are rarely reused, and one large
/// page left a process holding gigabytes it no longer used. The explicit CPU
/// provider is `ort::ep::CPU::default()`, which turns the arena off; output
/// is unchanged.
fn ocr_session_config(intra_threads: usize) -> OrtSessionConfig {
    OrtSessionConfig::new()
        .with_intra_threads(intra_threads.max(1))
        .with_inter_threads(1)
        .with_parallel_execution(false)
        .with_execution_providers(vec![OrtExecutionProvider::CPU])
        .with_memory_pattern(false)
}

fn load_onnx_runtime(runtime: Option<&Path>) -> Result<(), OarOcrError> {
    let path = runtime
        .map(Path::to_path_buf)
        .unwrap_or_else(onnx_runtime_library_path);
    drop(
        ort::init_from(&path).map_err(|source| OarOcrError::OnnxRuntimeLoad {
            path: path.clone(),
            source,
        })?,
    );
    Ok(())
}

pub(crate) fn onnx_runtime_library_path() -> PathBuf {
    std::env::var_os(ONNX_RUNTIME_LIBRARY_ENV)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(default_onnx_runtime_library)
}

fn default_onnx_runtime_library() -> PathBuf {
    #[cfg(target_os = "windows")]
    const NAME: &str = "onnxruntime.dll";
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    const NAME: &str = "libonnxruntime.so";
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    const NAME: &str = "libonnxruntime.dylib";
    PathBuf::from(NAME)
}

/// Runs `f` over `pages` on `workers.len()` dedicated dispatch threads, each
/// pulling the next unclaimed page index from a shared atomic cursor and
/// running its job through that worker's own pool (`PageWorker::install`),
/// never directly on the dispatch thread or the global rayon pool.
///
/// Pages are returned in their original order. On error, the error for the
/// lowest page index that failed is returned — matching
/// `Iterator::collect::<Result<Vec<_>, _>>()` — and once any thread observes
/// a failure or panic, threads stop claiming further pages. A panic in `f`
/// is caught and re-raised on the caller's thread after every dispatch
/// thread has stopped, matching rayon's panic propagation.
///
/// A dispatch thread that cannot be started (the process is at its thread
/// limit) is logged and skipped: the threads that did start claim its
/// pages, and if none started, the calling thread dispatches them all
/// through the first worker, as the single-worker path does.
fn map_pages_on_workers<W: PageWorker, P: Sync, R: Send>(
    workers: &[W],
    pages: &[P],
    f: impl Fn(&W, &P) -> Result<R, W::Error> + Sync,
) -> Result<Vec<R>, W::Error> {
    map_pages_on_workers_spawning(workers, pages, f, |_| true)
}

/// [`map_pages_on_workers`], where `may_spawn(index)` returning false stands
/// for the operating system refusing worker `index`'s dispatch thread.
fn map_pages_on_workers_spawning<W: PageWorker, P: Sync, R: Send>(
    workers: &[W],
    pages: &[P],
    f: impl Fn(&W, &P) -> Result<R, W::Error> + Sync,
    may_spawn: impl Fn(usize) -> bool,
) -> Result<Vec<R>, W::Error> {
    if pages.is_empty() {
        return Ok(Vec::new());
    }
    debug_assert!(!workers.is_empty(), "at least one worker is required");

    let cursor = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let error_slot: Mutex<Option<(usize, W::Error)>> = Mutex::new(None);
    let panic_slot: Mutex<Option<Box<dyn std::any::Any + Send>>> = Mutex::new(None);
    let results: Vec<Mutex<Option<R>>> = (0..pages.len()).map(|_| Mutex::new(None)).collect();

    let dispatch = |worker: &W| loop {
        if failed.load(Ordering::Relaxed) {
            return;
        }
        let index = cursor.fetch_add(1, Ordering::Relaxed);
        if index >= pages.len() {
            return;
        }
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            worker.install(|| f(worker, &pages[index]))
        }));
        match outcome {
            Ok(Ok(value)) => {
                *results[index].lock().unwrap() = Some(value);
            }
            Ok(Err(error)) => {
                failed.store(true, Ordering::Relaxed);
                let mut slot = error_slot.lock().unwrap();
                if slot.as_ref().is_none_or(|(existing, _)| index < *existing) {
                    *slot = Some((index, error));
                }
            }
            Err(payload) => {
                failed.store(true, Ordering::Relaxed);
                let mut slot = panic_slot.lock().unwrap();
                if slot.is_none() {
                    *slot = Some(payload);
                }
            }
        }
    };

    std::thread::scope(|scope| {
        let dispatch = &dispatch;
        let mut started = 0;
        for (index, worker) in workers.iter().enumerate() {
            let spawned = if may_spawn(index) {
                std::thread::Builder::new()
                    .name(format!("pdf-inspector-ocr-dispatch-{index}"))
                    .spawn_scoped(scope, move || dispatch(worker))
                    .map_err(|error| error.to_string())
            } else {
                Err("refused".to_string())
            };
            match spawned {
                Ok(_) => started += 1,
                Err(error) => log::warn!(
                    "OCR dispatch thread {index} could not start; its pages go to the \
                     other workers: {error}"
                ),
            }
        }
        if started == 0 {
            dispatch(&workers[0]);
        }
    });

    if let Some(payload) = panic_slot.into_inner().unwrap() {
        std::panic::resume_unwind(payload);
    }
    if let Some((_, error)) = error_slot.into_inner().unwrap() {
        return Err(error);
    }
    Ok(results
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .unwrap()
                .expect("every page without an error was processed")
        })
        .collect())
}

/// Dispatches `pages` across `workers`, choosing the same branch
/// `OarOcrEngine::recognize` needs: a single worker or a single page runs
/// pages one at a time through `workers[0].install` — normally on that
/// worker's own pool thread, or under the nonblocking fallback gate on the
/// calling thread if its pool failed to start. More than one of each
/// dispatches across workers with [`map_pages_on_workers`].
fn dispatch_pages_on_workers<W: PageWorker, P: Sync, R: Send>(
    workers: &[W],
    pages: &[P],
    f: impl Fn(&W, &P) -> Result<R, W::Error> + Sync,
) -> Result<Vec<R>, W::Error> {
    debug_assert!(!workers.is_empty(), "at least one worker is required");
    if workers.len() <= 1 || pages.len() <= 1 {
        return pages
            .iter()
            .map(|page| workers[0].install(|| f(&workers[0], page)))
            .collect();
    }
    map_pages_on_workers(workers, pages, f)
}

impl OcrEngine for OarOcrEngine {
    type Error = OarOcrError;

    fn model(&self) -> &ModelIdentity {
        &self.model
    }

    fn recognize(
        &self,
        pages: &[RenderedPage],
        options: &OcrOptions,
    ) -> Result<Vec<OcrPage>, Self::Error> {
        validate_options(options)?;

        dispatch_pages_on_workers(&self.workers, pages, |worker, page| {
            self.recognize_page(page, options, worker)
        })
    }

    fn preferred_page_concurrency(&self) -> usize {
        // Recognition dispatches across all workers only when there is more
        // than one; report that honestly so the pipeline doesn't render
        // oversized page batches for parallelism that isn't there.
        if self.workers.len() > 1 {
            self.workers.len()
        } else {
            1
        }
    }
}

fn validate_options(options: &OcrOptions) -> Result<(), OarOcrError> {
    if options.mode == OcrMode::Off {
        return Err(OarOcrError::OcrDisabled);
    }
    if !options.minimum_confidence.is_finite() || !(0.0..=1.0).contains(&options.minimum_confidence)
    {
        return Err(OarOcrError::InvalidMinimumConfidence {
            value: options.minimum_confidence,
        });
    }
    Ok(())
}

fn required_model(
    models: &ModelPaths,
    kind: ModelArtifactKind,
) -> Result<&std::path::Path, OarOcrError> {
    models
        .get(kind)
        .ok_or(OarOcrError::MissingModelArtifact { kind })
}

fn rendered_page_to_rgb(page: &RenderedPage) -> Result<RgbImage, OarOcrError> {
    let width = usize::try_from(page.width())
        .map_err(|_| OarOcrError::ImageSizeOverflow { page: page.page() })?;
    let height = usize::try_from(page.height())
        .map_err(|_| OarOcrError::ImageSizeOverflow { page: page.page() })?;
    let output_len = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or(OarOcrError::ImageSizeOverflow { page: page.page() })?;
    let input_bpp = page.format().bytes_per_pixel();
    let active_input_row = width
        .checked_mul(input_bpp)
        .ok_or(OarOcrError::ImageSizeOverflow { page: page.page() })?;
    let output_row = width
        .checked_mul(3)
        .ok_or(OarOcrError::ImageSizeOverflow { page: page.page() })?;

    let mut rgb = vec![0u8; output_len];
    for row in 0..height {
        let input_start = row * page.stride();
        let input = &page.pixels()[input_start..input_start + active_input_row];
        let output_start = row * output_row;
        let output = &mut rgb[output_start..output_start + output_row];
        match page.format() {
            RenderPixelFormat::Rgb8 => output.copy_from_slice(input),
            RenderPixelFormat::Rgba8 => {
                for (rgba, rgb) in input
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .zip(output.as_chunks_mut::<3>().0)
                {
                    rgb.copy_from_slice(&rgba[..3]);
                }
            }
            RenderPixelFormat::Gray8 => {
                for (&gray, rgb) in input.iter().zip(output.as_chunks_mut::<3>().0) {
                    rgb.fill(gray);
                }
            }
        }
    }

    RgbImage::from_raw(page.width(), page.height(), rgb)
        .ok_or(OarOcrError::InvalidImageBuffer { page: page.page() })
}

fn bounding_box_to_quad(bounding_box: &BoundingBox, width: u32, height: u32) -> Option<ImageQuad> {
    let points: Vec<ImagePoint> = bounding_box
        .points
        .iter()
        .filter(|point| point.x.is_finite() && point.y.is_finite())
        .map(|point| {
            ImagePoint::new(
                point.x.clamp(0.0, width as f32),
                point.y.clamp(0.0, height as f32),
            )
        })
        .collect();

    if bounding_box.points.len() == 4 && points.len() == 4 && is_ordered_convex_quad(&points) {
        return Some(ImageQuad::new([points[0], points[1], points[2], points[3]]));
    }
    if points.len() < 3 {
        return None;
    }

    let min_x = points
        .iter()
        .map(|point| point.x)
        .fold(f32::INFINITY, f32::min);
    let max_x = points
        .iter()
        .map(|point| point.x)
        .fold(f32::NEG_INFINITY, f32::max);
    let min_y = points
        .iter()
        .map(|point| point.y)
        .fold(f32::INFINITY, f32::min);
    let max_y = points
        .iter()
        .map(|point| point.y)
        .fold(f32::NEG_INFINITY, f32::max);
    if max_x <= min_x || max_y <= min_y {
        return None;
    }
    Some(ImageQuad::new([
        ImagePoint::new(min_x, min_y),
        ImagePoint::new(max_x, min_y),
        ImagePoint::new(max_x, max_y),
        ImagePoint::new(min_x, max_y),
    ]))
}

fn is_ordered_convex_quad(points: &[ImagePoint]) -> bool {
    if points.len() != 4 {
        return false;
    }
    let mut orientation = 0.0_f32;
    for index in 0..4 {
        let first = points[index];
        let second = points[(index + 1) % 4];
        let third = points[(index + 2) % 4];
        let cross = (second.x - first.x) * (third.y - second.y)
            - (second.y - first.y) * (third.x - second.x);
        if cross.abs() <= f32::EPSILON {
            return false;
        }
        if orientation == 0.0 {
            orientation = cross.signum();
        } else if cross.signum() != orientation {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use oar_ocr::processors::Point;

    use super::*;
    use crate::vision::PageTransform;

    #[test]
    fn cpu_session_budget_is_bounded_for_small_ocr_models() {
        let concurrency = pipeline_concurrency();
        let config = ocr_session_config(intra_threads_per_pipeline(concurrency));
        assert!((1..=4).contains(&config.intra_threads.unwrap()));
        assert_eq!(config.inter_threads, Some(1));
        assert_eq!(config.parallel_execution, Some(false));
        // Zero requests are clamped so a session always has a thread.
        assert_eq!(ocr_session_config(0).intra_threads, Some(1));
        // Working memory goes back after each run: no CPU arena, no plans.
        assert_eq!(
            config.execution_providers,
            Some(vec![OrtExecutionProvider::CPU])
        );
        assert_eq!(config.enable_mem_pattern, Some(false));
    }

    fn page(format: RenderPixelFormat, stride: usize, pixels: Vec<u8>) -> RenderedPage {
        let transform =
            PageTransform::from_corners(2, 2, (0.0, 2.0), (2.0, 2.0), (0.0, 0.0)).unwrap();
        RenderedPage::new(1, 2.0, 2.0, 2, 2, stride, format, pixels, transform).unwrap()
    }

    #[test]
    fn converts_padded_rgb_without_exposing_padding() {
        let page = page(
            RenderPixelFormat::Rgb8,
            8,
            vec![1, 2, 3, 4, 5, 6, 99, 99, 7, 8, 9, 10, 11, 12, 99, 99],
        );
        let image = rendered_page_to_rgb(&page).unwrap();
        assert_eq!(image.as_raw(), &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
    }

    #[test]
    fn converts_rgba_and_gray_to_rgb() {
        let rgba = page(
            RenderPixelFormat::Rgba8,
            8,
            vec![1, 2, 3, 44, 4, 5, 6, 55, 7, 8, 9, 66, 10, 11, 12, 77],
        );
        assert_eq!(
            rendered_page_to_rgb(&rgba).unwrap().as_raw(),
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]
        );

        let gray = page(RenderPixelFormat::Gray8, 2, vec![1, 2, 3, 4]);
        assert_eq!(
            rendered_page_to_rgb(&gray).unwrap().as_raw(),
            &[1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4]
        );
    }

    #[test]
    fn preserves_quads_and_clamps_them_to_the_bitmap() {
        let bbox = BoundingBox::new(vec![
            Point::new(-1.0, 2.0),
            Point::new(11.0, 2.0),
            Point::new(11.0, 9.0),
            Point::new(-1.0, 9.0),
        ]);
        let quad = bounding_box_to_quad(&bbox, 10, 8).unwrap();
        assert_eq!(quad.points[0], ImagePoint::new(0.0, 2.0));
        assert_eq!(quad.points[2], ImagePoint::new(10.0, 8.0));
    }

    #[test]
    fn reduces_polygons_to_a_stable_axis_aligned_quad() {
        let bbox = BoundingBox::new(vec![
            Point::new(2.0, 1.0),
            Point::new(7.0, 2.0),
            Point::new(8.0, 6.0),
            Point::new(5.0, 9.0),
            Point::new(1.0, 5.0),
        ]);
        let quad = bounding_box_to_quad(&bbox, 10, 10).unwrap();
        assert_eq!(quad.points[0], ImagePoint::new(1.0, 1.0));
        assert_eq!(quad.points[2], ImagePoint::new(8.0, 9.0));
    }

    #[test]
    fn normalizes_unordered_or_partially_invalid_quads() {
        let unordered = BoundingBox::new(vec![
            Point::new(1.0, 1.0),
            Point::new(8.0, 8.0),
            Point::new(8.0, 1.0),
            Point::new(1.0, 8.0),
        ]);
        let quad = bounding_box_to_quad(&unordered, 10, 10).unwrap();
        assert_eq!(quad.points[0], ImagePoint::new(1.0, 1.0));
        assert_eq!(quad.points[1], ImagePoint::new(8.0, 1.0));
        assert_eq!(quad.points[2], ImagePoint::new(8.0, 8.0));

        let partially_invalid = BoundingBox::new(vec![
            Point::new(8.0, 8.0),
            Point::new(f32::NAN, 4.0),
            Point::new(1.0, 8.0),
            Point::new(8.0, 1.0),
            Point::new(1.0, 1.0),
        ]);
        let quad = bounding_box_to_quad(&partially_invalid, 10, 10).unwrap();
        assert_eq!(quad.points[0], ImagePoint::new(1.0, 1.0));
        assert_eq!(quad.points[1], ImagePoint::new(8.0, 1.0));
        assert_eq!(quad.points[2], ImagePoint::new(8.0, 8.0));
    }

    #[test]
    fn refuses_disabled_or_invalid_options_before_inference() {
        assert!(matches!(
            validate_options(&OcrOptions::new()),
            Err(OarOcrError::OcrDisabled)
        ));
        for value in [-0.1, 1.1, f32::NAN, f32::INFINITY] {
            let options = OcrOptions::new()
                .mode(OcrMode::Force)
                .minimum_confidence(value);
            assert!(matches!(
                validate_options(&options),
                Err(OarOcrError::InvalidMinimumConfidence { .. })
            ));
        }
        assert!(validate_options(
            &OcrOptions::new()
                .mode(OcrMode::Auto)
                .minimum_confidence(1.0)
        )
        .is_ok());
    }

    #[test]
    fn escalation_fires_for_dense_fine_print_pages() {
        // Measured cases (at unclip 2.0) that gain from escalation: dense
        // tiled ad pages (12.3–14.2px, 158–286 regions), and a dense pricing
        // sheet (12.0px, 144 regions), all downscaled by the standard limit.
        assert!(should_escalate_detection(14.2, 186, 0.55));
        assert!(should_escalate_detection(13.1, 286, 0.55));
        assert!(should_escalate_detection(12.3, 158, 0.55));
        assert!(should_escalate_detection(12.0, 144, 0.55));
    }

    #[test]
    fn escalation_skips_ordinary_pages() {
        // Academic prose: too few regions (and tall enough at unclip 2.0).
        assert!(!should_escalate_detection(14.5, 47, 0.58));
        // Engineering drawing: many regions but tall enough text.
        assert!(!should_escalate_detection(15.7, 205, 0.58));
        // Typewriter scan: tall text, few regions.
        assert!(!should_escalate_detection(17.5, 77, 0.55));
        // Page not downscaled at all: escalation cannot add pixels.
        assert!(!should_escalate_detection(9.0, 300, 1.0));
        assert!(!should_escalate_detection(9.0, 30, 1.0));
    }

    #[test]
    fn escalation_fires_for_sparse_small_print() {
        // 30-row statements at 6–9 pt, A4 and Letter at 150 DPI: 6–7 pt drop
        // whole rows on the standard pass and recover them all escalated.
        assert!(should_escalate_detection(12.0, 30, 0.547));
        assert!(should_escalate_detection(11.6, 44, 0.582));
        // An 8-row statement at 6 pt (9 regions with its title) garbled a row.
        assert!(should_escalate_detection(12.0, 9, 0.547));
        // A dense table page measuring 12.2 px with 65 regions.
        assert!(should_escalate_detection(12.2, 65, 0.582));
    }

    #[test]
    fn sparse_escalation_skips_ordinary_and_near_empty_pages() {
        // 10 pt statements and ordinary prose/table pages: 14.1 px and up.
        assert!(!should_escalate_detection(14.8, 31, 0.547));
        assert!(!should_escalate_detection(14.1, 43, 0.582));
        assert!(!should_escalate_detection(13.0, 40, 0.547));
        // A few tiny regions (a caption, a stamp, speckle): too few to pay.
        assert!(!should_escalate_detection(11.6, 6, 0.582));
        assert!(!should_escalate_detection(12.0, 7, 0.547));
    }

    #[test]
    fn median_detection_height_scales_and_handles_empty() {
        let mut heights = vec![30.0, 10.0, 20.0];
        assert_eq!(median_detection_height(&mut heights, 0.5), 10.0);
        // Even counts average the two middle values instead of picking the
        // upper one, so borderline pages don't skew away from escalation.
        let mut even = vec![10.0, 12.0, 14.0, 30.0];
        assert_eq!(median_detection_height(&mut even, 1.0), 13.0);
        let mut empty: Vec<f32> = Vec::new();
        assert_eq!(median_detection_height(&mut empty, 0.5), f32::MAX);
    }

    #[test]
    fn concurrency_derivations_stay_in_bounds() {
        let concurrency = pipeline_concurrency();
        assert!((1..=3).contains(&concurrency));
        assert!(intra_threads_per_pipeline(2) == 2);
        assert!((1..=4).contains(&intra_threads_per_pipeline(1)));
    }

    fn tiny_pool() -> rayon::ThreadPool {
        rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap()
    }

    #[test]
    fn plan_worker_pools_builds_every_pool_when_all_succeed() {
        let pools = plan_worker_pools(3, |_index| Ok::<_, &str>(tiny_pool()));
        assert_eq!(pools.len(), 3);
        assert!(pools
            .iter()
            .all(|pool| matches!(pool, WorkerPool::Dedicated(_))));
    }

    #[test]
    fn plan_worker_pools_keeps_earlier_pools_when_a_later_one_fails() {
        let pools = plan_worker_pools(4, |index| {
            if index == 2 {
                Err("boom")
            } else {
                Ok(tiny_pool())
            }
        });
        assert_eq!(pools.len(), 2);
        assert!(pools
            .iter()
            .all(|pool| matches!(pool, WorkerPool::Dedicated(_))));
    }

    #[test]
    fn plan_worker_pools_falls_back_to_one_poolless_worker_when_the_first_fails() {
        let pools = plan_worker_pools(4, |_index| Err::<rayon::ThreadPool, _>("boom"));
        assert_eq!(pools.len(), 1);
        assert!(matches!(&pools[0], WorkerPool::Sequential(_)));
    }

    #[test]
    fn worker_pool_without_a_pool_runs_install_on_the_calling_thread() {
        let pool = WorkerPool::Sequential(Mutex::new(()));
        let caller_thread = std::thread::current().id();
        let observed = pool.install(|| Ok(std::thread::current().id())).unwrap();
        assert_eq!(observed, caller_thread);
    }

    fn assert_completes_without_blocking(f: impl FnOnce() + Send + 'static) {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            f();
            let _ = done_tx.send(());
        });
        match done_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(()) => handle.join().unwrap(),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("the fallback worker blocked instead of rejecting a contended call");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let payload = handle
                    .join()
                    .expect_err("sender was dropped without the worker thread panicking");
                std::panic::resume_unwind(payload);
            }
        }
    }

    #[test]
    fn fallback_worker_preserves_page_order_and_releases_after_errors() {
        let pools = plan_worker_pools(3, |_| Err::<rayon::ThreadPool, _>("boom"));
        let result =
            dispatch_pages_on_workers(&pools, &[0, 1, 2], |_worker, page| Ok(page * 2)).unwrap();
        assert_eq!(result, vec![0, 2, 4]);
        assert!(matches!(
            pools[0].install(|| Err::<(), _>(OarOcrError::OcrDisabled)),
            Err(OarOcrError::OcrDisabled)
        ));
        assert_eq!(pools[0].install(|| Ok(7)).unwrap(), 7);
    }

    #[test]
    fn fallback_worker_rejects_reentry_before_running_the_page() {
        assert_completes_without_blocking(|| {
            let pools = plan_worker_pools(3, |_| Err::<rayon::ThreadPool, _>("boom"));
            let entered = AtomicUsize::new(0);
            pools[0]
                .install(|| {
                    let result = dispatch_pages_on_workers(&pools, &[0], |_worker, _page| {
                        entered.fetch_add(1, Ordering::Relaxed);
                        Ok(())
                    });
                    assert!(matches!(result, Err(OarOcrError::FallbackWorkerBusy)));
                    let empty: Vec<usize> = Vec::new();
                    assert!(
                        dispatch_pages_on_workers(&pools, &empty, |_worker, _page| Ok(()))
                            .unwrap()
                            .is_empty()
                    );
                    Ok(())
                })
                .unwrap();
            assert_eq!(entered.load(Ordering::Relaxed), 0);
            assert_eq!(pools[0].install(|| Ok(7)).unwrap(), 7);
        });
    }

    #[test]
    fn fallback_worker_rejects_concurrent_threads_without_waiting() {
        assert_completes_without_blocking(|| {
            let pools = plan_worker_pools(3, |_| Err::<rayon::ThreadPool, _>("boom"));
            pools[0]
                .install(|| {
                    std::thread::scope(|scope| {
                        for _ in 0..4 {
                            scope.spawn(|| {
                                let result = pools[0].install::<()>(|| {
                                    panic!("a competing caller entered the active worker");
                                });
                                assert!(matches!(result, Err(OarOcrError::FallbackWorkerBusy)));
                            });
                        }
                    });
                    Ok(())
                })
                .unwrap();
        });
    }

    #[test]
    fn fallback_worker_does_not_block_nested_global_rayon_work() {
        assert_completes_without_blocking(|| {
            use rayon::prelude::*;
            let pools = plan_worker_pools(3, |_| Err::<rayon::ThreadPool, _>("boom"));
            pools[0]
                .install(|| {
                    // The admitted page waits for global Rayon work while
                    // holding its gate. A blocking lock in any competing
                    // call would therefore deadlock even on a one-thread pool.
                    (0..rayon::current_num_threads().max(2))
                        .into_par_iter()
                        .for_each(|_| {
                            let result = dispatch_pages_on_workers(
                                &pools,
                                &[0, 1],
                                |_worker, _page| -> Result<(), OarOcrError> {
                                    panic!("nested Rayon work reentered the active worker");
                                },
                            );
                            assert!(matches!(result, Err(OarOcrError::FallbackWorkerBusy)));
                        });
                    Ok(())
                })
                .unwrap();
            assert_eq!(pools[0].install(|| Ok(7)).unwrap(), 7);
        });
    }

    #[test]
    fn fallback_worker_preserves_panic_payload_and_rejects_poisoned_sessions() {
        let pool = WorkerPool::Sequential(Mutex::new(()));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pool.install::<()>(|| std::panic::panic_any(73usize))
        }));
        let payload = outcome.expect_err("the original page panic must propagate");
        assert_eq!(payload.downcast_ref::<usize>(), Some(&73));
        assert!(matches!(
            pool.install::<()>(|| panic!("a poisoned worker must not be entered")),
            Err(OarOcrError::FallbackWorkerPoisoned)
        ));
    }

    /// A fake worker whose `install` runs `f` directly on the calling
    /// thread. Sufficient for tests that only care about
    /// `map_pages_on_workers`'s own bookkeeping (order, errors, panics),
    /// not about which thread a page actually runs on.
    struct DirectWorker;

    impl PageWorker for DirectWorker {
        type Error = usize;

        fn install<R: Send>(
            &self,
            f: impl FnOnce() -> Result<R, Self::Error> + Send,
        ) -> Result<R, Self::Error> {
            f()
        }
    }

    #[test]
    fn map_pages_on_workers_preserves_page_order() {
        let workers = [DirectWorker, DirectWorker, DirectWorker];
        let pages: Vec<usize> = (0..37).collect();
        let result = map_pages_on_workers(&workers, &pages, |_worker, page| {
            Ok::<usize, usize>(page * 2)
        })
        .unwrap();
        let expected: Vec<usize> = pages.iter().map(|page| page * 2).collect();
        assert_eq!(result, expected);
    }

    #[test]
    fn map_pages_on_workers_survives_dispatch_threads_that_cannot_start() {
        let workers = [DirectWorker, DirectWorker, DirectWorker];
        let pages: Vec<usize> = (0..23).collect();
        let expected: Vec<usize> = pages.iter().map(|page| page * 3).collect();
        let threads = |may_spawn: &dyn Fn(usize) -> bool| {
            let seen = Mutex::new(std::collections::HashSet::new());
            let result = map_pages_on_workers_spawning(
                &workers,
                &pages,
                |_worker, page| {
                    seen.lock().unwrap().insert(std::thread::current().id());
                    Ok::<usize, usize>(page * 3)
                },
                may_spawn,
            )
            .unwrap();
            assert_eq!(result, expected);
            seen.into_inner().unwrap()
        };
        // No dispatch thread starts: the calling thread runs every page.
        let caller = std::thread::current().id();
        assert_eq!(threads(&|_| false), [caller].into_iter().collect());
        // Only the third starts: it alone claims every page.
        let one = threads(&|index| index == 2);
        assert_eq!(one.len(), 1);
        assert!(!one.contains(&caller));
    }

    #[test]
    fn map_pages_on_workers_returns_empty_for_empty_input() {
        let workers = [DirectWorker, DirectWorker];
        let pages: Vec<usize> = Vec::new();
        let result = map_pages_on_workers(&workers, &pages, |_worker, page: &usize| {
            Ok::<usize, usize>(*page)
        })
        .unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn map_pages_on_workers_returns_the_lowest_indexed_error() {
        let workers = [DirectWorker, DirectWorker, DirectWorker, DirectWorker];
        let pages: Vec<usize> = (0..200).collect();
        let result = map_pages_on_workers(&workers, &pages, |_worker, page| {
            // Several pages fail; the lowest page index's error must win,
            // matching `collect::<Result<Vec<_>, _>>()` semantics.
            if page % 17 == 0 && *page > 0 {
                Err(*page)
            } else {
                Ok(*page)
            }
        });
        assert_eq!(result, Err(17));
    }

    #[test]
    fn map_pages_on_workers_propagates_the_first_panic_payload() {
        let workers = [DirectWorker, DirectWorker, DirectWorker];
        let pages: Vec<usize> = (0..40).collect();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            map_pages_on_workers(&workers, &pages, |_worker, page| {
                if *page == 5 {
                    panic!("page {page} exploded");
                }
                Ok::<usize, usize>(*page)
            })
        }));
        let payload = outcome.expect_err("a page panic must unwind past map_pages_on_workers");
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or_default();
        assert!(
            message.contains("exploded"),
            "unexpected payload: {message}"
        );
    }

    /// A fake worker with its own dedicated single-thread pool, mirroring
    /// production `OcrWorker`. `name` lets a test assert which pool ran a
    /// given page, and `session` doubles as a reentrancy probe.
    struct PoolWorker {
        name: String,
        pool: rayon::ThreadPool,
        session: Mutex<()>,
    }

    impl PoolWorker {
        fn new(name: impl Into<String>) -> Self {
            let name = name.into();
            let thread_name = name.clone();
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .thread_name(move |_| thread_name.clone())
                .build()
                .unwrap();
            Self {
                name,
                pool,
                session: Mutex::new(()),
            }
        }
    }

    impl PageWorker for PoolWorker {
        type Error = String;

        fn install<R: Send>(
            &self,
            f: impl FnOnce() -> Result<R, Self::Error> + Send,
        ) -> Result<R, Self::Error> {
            self.pool.install(f)
        }
    }

    #[test]
    fn map_pages_on_workers_runs_every_page_on_its_own_workers_pool() {
        let workers: Vec<PoolWorker> = (0..3)
            .map(|index| PoolWorker::new(format!("probe-{index}")))
            .collect();
        let pages: Vec<usize> = (0..30).collect();

        let result =
            map_pages_on_workers(
                &workers,
                &pages,
                |worker, _page| match std::thread::current().name() {
                    Some(actual) if actual == worker.name => Ok(()),
                    other => Err(format!("expected {}, saw {other:?}", worker.name)),
                },
            );

        assert_eq!(result, Ok(vec![(); 30]));
    }

    /// Runs `pages` through `dispatch_pages_on_workers`, checking that
    /// every page executed on its assigned worker's own pool thread rather
    /// than on the calling thread.
    fn assert_dispatch_uses_worker_pools(workers: &[PoolWorker], pages: &[usize]) {
        let caller_thread = std::thread::current().name().map(str::to_owned);
        let ran_on_own_pool = |worker: &PoolWorker, _page: &usize| -> Result<String, String> {
            match std::thread::current().name() {
                Some(actual) if actual == worker.name => Ok(actual.to_owned()),
                other => Err(format!("expected {}, saw {other:?}", worker.name)),
            }
        };
        let result = dispatch_pages_on_workers(workers, pages, ran_on_own_pool)
            .expect("every page should run on its worker's own pool");
        for name in &result {
            assert_ne!(Some(name.clone()), caller_thread);
        }
    }

    #[test]
    fn dispatch_pages_on_workers_runs_sequentially_with_one_worker() {
        let workers = [PoolWorker::new("solo")];
        let pages: Vec<usize> = (0..5).collect();
        assert_dispatch_uses_worker_pools(&workers, &pages);
    }

    #[test]
    fn dispatch_pages_on_workers_runs_sequentially_with_one_page() {
        let workers: Vec<PoolWorker> = (0..3)
            .map(|index| PoolWorker::new(format!("probe-{index}")))
            .collect();
        let pages: Vec<usize> = vec![0];
        assert_dispatch_uses_worker_pools(&workers, &pages);
    }

    #[test]
    fn dispatch_pages_on_workers_runs_in_parallel_with_multiple_workers_and_pages() {
        let workers: Vec<PoolWorker> = (0..3)
            .map(|index| PoolWorker::new(format!("probe-{index}")))
            .collect();
        let pages: Vec<usize> = (0..30).collect();
        assert_dispatch_uses_worker_pools(&workers, &pages);
    }

    #[test]
    fn dispatch_pages_on_workers_handles_concurrent_callers_on_shared_workers() {
        let workers = Arc::new(
            (0..3)
                .map(|index| PoolWorker::new(format!("shared-{index}")))
                .collect::<Vec<_>>(),
        );
        let reentries = Arc::new(AtomicUsize::new(0));
        let (done_tx, done_rx) = std::sync::mpsc::channel();

        for _ in 0..4 {
            let workers = Arc::clone(&workers);
            let reentries = Arc::clone(&reentries);
            let done_tx = done_tx.clone();
            std::thread::spawn(move || {
                let pages: Vec<usize> = (0..10).collect();
                let result = dispatch_pages_on_workers(&workers, &pages, |worker, _page| {
                    let guard = worker.session.try_lock();
                    if guard.is_err() {
                        reentries.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok::<usize, String>(0)
                });
                let _ = done_tx.send(result.is_ok());
            });
        }
        drop(done_tx);

        for _ in 0..4 {
            let ok = done_rx
                .recv_timeout(Duration::from_secs(30))
                .expect("a concurrent dispatch caller did not finish in time");
            assert!(ok);
        }
        assert_eq!(reentries.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn map_pages_on_workers_never_reenters_a_worker_via_inner_rayon_work() {
        // While a page holds its worker's session lock and runs a
        // sleep-based inner `par_iter`, no other page's job may observe
        // that same lock as held.
        let workers: Vec<PoolWorker> = (0..3).map(|_| PoolWorker::new("worker")).collect();
        let pages: Vec<usize> = (0..20).collect();
        let reentries = AtomicUsize::new(0);

        let result = map_pages_on_workers(&workers, &pages, |worker, _page| {
            let guard = worker.session.try_lock();
            let held = guard.is_ok();
            if !held {
                reentries.fetch_add(1, Ordering::Relaxed);
            }
            use rayon::prelude::*;
            let sum: usize = (0..4)
                .into_par_iter()
                .map(|value| {
                    std::thread::sleep(Duration::from_millis(1));
                    value + 1
                })
                .sum();
            drop(guard);
            Ok::<usize, String>(sum)
        });

        assert!(result.is_ok());
        assert_eq!(
            reentries.load(Ordering::Relaxed),
            0,
            "a worker's session lock was re-entered by a stolen page job"
        );
    }

    #[test]
    fn map_pages_on_workers_does_not_starve_global_pool_callers() {
        // Callers that are themselves global-rayon-pool workers (e.g.
        // `files.par_iter().map(|p| process_pdf_with_ocr(p, opts))`) must
        // still be able to complete; bounded by a timeout so a real
        // regression fails this test instead of hanging it.
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            use rayon::prelude::*;
            let thread_count = rayon::current_num_threads().clamp(1, 8);
            (0..thread_count).into_par_iter().for_each(|_| {
                let workers: Vec<PoolWorker> = (0..3).map(|_| PoolWorker::new("worker")).collect();
                let pages: Vec<usize> = (0..10).collect();
                let result = map_pages_on_workers(&workers, &pages, |_worker, _page| {
                    use rayon::prelude::*;
                    let sum: usize = (0..4)
                        .into_par_iter()
                        .map(|value| {
                            std::thread::sleep(Duration::from_millis(1));
                            value + 1
                        })
                        .sum();
                    Ok::<usize, String>(sum)
                });
                assert!(result.is_ok());
            });
            let _ = done_tx.send(());
        });
        match done_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(()) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("map_pages_on_workers starved the global rayon pool");
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let payload = handle
                    .join()
                    .expect_err("sender was dropped without the worker thread panicking");
                std::panic::resume_unwind(payload);
            }
        }
    }

    fn rectangle(left: f32, top: f32, right: f32, bottom: f32) -> BoundingBox {
        BoundingBox::new(vec![
            Point::new(left, top),
            Point::new(right, top),
            Point::new(right, bottom),
            Point::new(left, bottom),
        ])
    }

    #[test]
    fn a_page_of_vertical_lines_lies_on_its_side() {
        let sideways: Vec<_> = (0..4)
            .map(|i| {
                rectangle(
                    100.0 + 40.0 * i as f32,
                    50.0,
                    120.0 + 40.0 * i as f32,
                    600.0,
                )
            })
            .collect();
        assert!(lines_run_vertically(&sideways));

        // Two is the least that counts.
        assert!(!lines_run_vertically(&sideways[..1]));
        assert!(lines_run_vertically(&sideways[..2]));
    }

    #[test]
    fn margin_notes_and_short_words_do_not_put_a_page_on_its_side() {
        let mut upright: Vec<_> = (0..10)
            .map(|i| rectangle(50.0, 50.0 + 30.0 * i as f32, 550.0, 70.0 + 30.0 * i as f32))
            .collect();
        // A vertical stamp in the margin.
        upright.push(rectangle(10.0, 100.0, 30.0, 700.0));
        upright.push(rectangle(570.0, 100.0, 590.0, 700.0));
        assert!(!lines_run_vertically(&upright));

        // Digits and single words, taller than wide but not lines.
        let digits: Vec<_> = (0..10)
            .map(|i| rectangle(50.0 + 30.0 * i as f32, 50.0, 64.0 + 30.0 * i as f32, 75.0))
            .collect();
        assert!(!lines_run_vertically(&digits));
    }

    #[test]
    fn region_extent_reads_the_longer_edge_as_the_length() {
        let (length, thickness, vertical) =
            region_extent(&rectangle(0.0, 0.0, 20.0, 200.0)).unwrap();
        assert_eq!((length, thickness, vertical), (200.0, 20.0, true));
        let (length, thickness, vertical) =
            region_extent(&rectangle(0.0, 0.0, 200.0, 20.0)).unwrap();
        assert_eq!((length, thickness, vertical), (200.0, 20.0, false));
        assert!(region_extent(&BoundingBox::new(vec![Point::new(0.0, 0.0)])).is_none());
    }

    #[test]
    fn quads_found_on_a_turned_page_map_back_onto_the_rendered_page() {
        // The rendered page is 300 × 100; its point (10, 20) is where each
        // turned image puts it.
        let (width, height) = (300, 100);
        let at = |x: f32, y: f32| {
            ImageQuad::new([
                ImagePoint::new(x, y),
                ImagePoint::new(x, y),
                ImagePoint::new(x, y),
                ImagePoint::new(x, y),
            ])
        };
        let original = ImagePoint::new(10.0, 20.0);
        // One quarter turn clockwise: (x, y) -> (height - y, x).
        assert_eq!(
            unturn_quad(at(80.0, 10.0), 1, width, height).points[0],
            original
        );
        // A half turn: (x, y) -> (width - x, height - y).
        assert_eq!(
            unturn_quad(at(290.0, 80.0), 2, width, height).points[0],
            original
        );
        // Three quarter turns: (x, y) -> (y, width - x).
        assert_eq!(
            unturn_quad(at(20.0, 290.0), 3, width, height).points[0],
            original
        );
        assert_eq!(
            unturn_quad(at(10.0, 20.0), 0, width, height).points[0],
            original
        );
    }

    #[test]
    fn a_checked_page_that_stays_weak_is_warned_about() {
        // Turned upright and read well: no warning.
        assert_eq!(orientation_warning(Some(90.0), true, Some(0.95)), None);
        // Turned, still weak.
        assert!(orientation_warning(Some(180.0), true, Some(0.6))
            .unwrap()
            .contains("turned 180° clockwise"));
        // Checked, no turn read clearly better, still weak (#622: an
        // upside-down page whose half-turned sample improves too little).
        assert!(orientation_warning(None, true, Some(0.69))
            .unwrap()
            .contains("may be turned"));
        assert!(orientation_warning(None, true, None).is_some());
        // A page read at 0.9 or better is never checked.
        assert_eq!(orientation_warning(None, false, Some(0.7)), None);
        assert_eq!(orientation_warning(None, true, Some(0.85)), None);
    }

    #[test]
    fn a_turn_replaces_the_first_reading_only_when_it_reads_better() {
        let regions = |confidences: &[Option<f32>]| -> Vec<RecognizedRegion> {
            confidences
                .iter()
                .map(|confidence| RecognizedRegion {
                    bounding_box: rectangle(0.0, 0.0, 100.0, 10.0),
                    outcome: match confidence {
                        Some(confidence) => RegionOutcome::Recognized {
                            text: "a".to_string(),
                            confidence: *confidence,
                        },
                        None => RegionOutcome::Unrecognized,
                    },
                })
                .collect()
        };
        let first = regions(&[Some(0.6), Some(0.6), Some(0.6), Some(0.6)]);
        assert!(reads_better(&regions(&[Some(0.95); 4]), &first));
        // Fewer regions read (a few long stamps) at higher confidence: no.
        assert!(!reads_better(
            &regions(&[Some(0.95), None, None, None]),
            &first
        ));
        assert!(!reads_better(&regions(&[Some(0.5); 4]), &first));
    }

    #[test]
    fn region_confidence_counts_unread_regions_as_zero() {
        let region = |outcome| RecognizedRegion {
            bounding_box: rectangle(0.0, 0.0, 100.0, 10.0),
            outcome,
        };
        let regions = vec![
            region(RegionOutcome::Recognized {
                text: "a".to_string(),
                confidence: 0.8,
            }),
            region(RegionOutcome::Unrecognized),
            region(RegionOutcome::InvalidGeometry),
        ];
        assert_eq!(mean_region_confidence(&regions), 0.8);
        assert_eq!(region_confidence(&regions[1]), 0.0);
        assert_eq!(mean_region_confidence(&regions[1..]), 0.0);
        assert_eq!(mean_region_confidence(&[]), 1.0);
    }
}
