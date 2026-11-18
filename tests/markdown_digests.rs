//! Pins the Markdown of every PDF fixture, so a change that should leave the
//! Markdown alone (annotation extraction, which reports beside it) shows that
//! it did.
//!
//! `tests/snapshots/markdown_digests.txt` holds, per fixture, a 64-bit
//! FNV-1a digest of the Markdown three surfaces produce: `process_pdf_mem`
//! (what `pdf2md` prints), `extract_pages_markdown_mem` (the pages joined
//! with form feeds) and, with the `ocr` feature, the native pass of
//! `vision::process_pdf_with_ocr_mem` with default options (what mdoc
//! imports). A fixture that fails to open records `error` instead.
//!
//! A debug build (plain `cargo test`, as CI runs it) skips the fixtures in
//! [`SLOW_IN_DEBUG`], whose full Markdown takes minutes unoptimized; a
//! release build checks every fixture:
//!
//!   cargo test --release --features ocr --test markdown_digests
//!
//! An intentional Markdown change regenerates the file (in release, so the
//! slow fixtures are written too):
//!
//!   UPDATE_MARKDOWN_DIGESTS=1 cargo test --release --features ocr --test markdown_digests
//!
//! and its PR explains every fixture whose line changed.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

const MANIFEST: &str = "tests/snapshots/markdown_digests.txt";

/// Fixtures a debug build does not convert: about 30 s each in release.
const SLOW_IN_DEBUG: &[&str] = &["bits_pilani_feedback.pdf"];

fn checks_slow_fixtures() -> bool {
    !cfg!(debug_assertions)
}

fn fnv1a(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn digest<E>(markdown: Result<String, E>) -> String {
    markdown.map_or_else(|_| "error".to_string(), |text| fnv1a(&text))
}

#[cfg(feature = "ocr")]
fn ocr_native_digest(bytes: &[u8]) -> String {
    use pdf_inspector::vision::{process_pdf_with_ocr_mem, OcrPdfOptions};
    digest(process_pdf_with_ocr_mem(bytes, OcrPdfOptions::new()).map(|result| result.markdown))
}

#[cfg(not(feature = "ocr"))]
fn ocr_native_digest(_bytes: &[u8]) -> String {
    "-".to_string()
}

fn fixture_digests(path: &Path) -> [String; 3] {
    let bytes = std::fs::read(path).expect("fixture bytes");
    let process =
        digest(pdf_inspector::process_pdf_mem(&bytes).map(|r| r.markdown.unwrap_or_default()));
    let pages = digest(
        pdf_inspector::extract_pages_markdown_mem(&bytes, None).map(|r| {
            r.pages
                .into_iter()
                .map(|page| page.markdown)
                .collect::<Vec<_>>()
                .join("\u{c}")
        }),
    );
    [process, pages, ocr_native_digest(&bytes)]
}

/// Every fixture's name, sorted, with its digests (`None` when skipped),
/// converted on as many threads as the machine offers.
fn digests() -> Vec<(String, Option<[String; 3]>)> {
    let mut fixtures: Vec<PathBuf> = std::fs::read_dir("tests/fixtures")
        .expect("fixture directory")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "pdf"))
        .collect();
    fixtures.sort();
    let names: Vec<String> = fixtures
        .iter()
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    let results: Mutex<Vec<Option<[String; 3]>>> = Mutex::new(vec![None; fixtures.len()]);
    let next = AtomicUsize::new(0);
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get().min(8));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(path) = fixtures.get(index) else {
                    break;
                };
                if !checks_slow_fixtures() && SLOW_IN_DEBUG.contains(&names[index].as_str()) {
                    continue;
                }
                let digests = fixture_digests(path);
                results.lock().unwrap()[index] = Some(digests);
            });
        }
    });
    names
        .into_iter()
        .zip(results.into_inner().unwrap())
        .collect()
}

#[test]
fn every_fixture_keeps_its_markdown() {
    let actual = digests();
    if std::env::var_os("UPDATE_MARKDOWN_DIGESTS").is_some() {
        assert!(
            cfg!(feature = "ocr") && checks_slow_fixtures(),
            "regenerate with --release --features ocr so every fixture and the OCR column are written"
        );
        let mut out = String::from(
            "# fixture  process_pdf_mem  extract_pages_markdown_mem  process_pdf_with_ocr_mem (native)\n",
        );
        for (name, digests) in &actual {
            let [a, b, c] = digests.as_ref().expect("every fixture converted");
            writeln!(out, "{name} {a} {b} {c}").unwrap();
        }
        std::fs::write(Path::new(MANIFEST), out).expect("write manifest");
        return;
    }

    let manifest = std::fs::read_to_string(MANIFEST).expect("read manifest");
    let expected: Vec<(&str, Vec<&str>)> = manifest
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| {
            let mut fields = line.split_whitespace();
            (fields.next().unwrap(), fields.collect())
        })
        .collect();

    let mut problems = Vec::new();
    let expected_names: Vec<&str> = expected.iter().map(|(name, _)| *name).collect();
    let actual_names: Vec<&str> = actual.iter().map(|(name, _)| name.as_str()).collect();
    if expected_names != actual_names {
        problems.push(format!(
            "fixture list differs: manifest {expected_names:?}, directory {actual_names:?}"
        ));
    }
    let surfaces = [
        "process_pdf_mem",
        "extract_pages_markdown_mem",
        "process_pdf_with_ocr_mem",
    ];
    for (name, digests) in &actual {
        let Some(digests) = digests else {
            continue;
        };
        let Some((_, pinned)) = expected.iter().find(|(pinned, _)| pinned == name) else {
            continue;
        };
        // The OCR column is only checked when the feature is built.
        let checked = if cfg!(feature = "ocr") { 3 } else { 2 };
        for column in 0..checked {
            if pinned.get(column).copied() != Some(digests[column].as_str()) {
                problems.push(format!(
                    "{name}: {} Markdown changed ({:?} -> {})",
                    surfaces[column],
                    pinned.get(column),
                    digests[column]
                ));
            }
        }
    }
    assert!(
        problems.is_empty(),
        "{}\n\nIf the change is intended, regenerate {MANIFEST} (see this file's header) and explain each changed fixture.",
        problems.join("\n")
    );
}
