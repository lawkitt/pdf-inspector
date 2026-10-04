//! Offline, same-settings corpus capture for reviewing fork updates.
//! Usage: cargo run --features ocr --example ocr_corpus -- CORPUS OUTPUT RUNTIME_ROOT
//! RUNTIME_ROOT contains models/, libpdfium.dylib and libonnxruntime.1.27.0.dylib
//! (pdfium.dll and onnxruntime.dll on Windows). No model downloads are permitted.

#[cfg(all(feature = "ocr", not(target_arch = "wasm32")))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use pdf_inspector::vision::{
        process_pdf_with_ocr_mem, ModelDownloadPolicy, OcrMode, OcrOptions, OcrPdfOptions,
        RenderOptions, PP_OCR_CYRILLIC,
    };
    use std::{env, fs, path::PathBuf};

    let args: Vec<_> = env::args_os().skip(1).collect();
    if args.len() != 3 {
        return Err("usage: ocr_corpus CORPUS OUTPUT RUNTIME_ROOT".into());
    }
    let corpus = PathBuf::from(&args[0]);
    let output = PathBuf::from(&args[1]);
    let runtime = PathBuf::from(&args[2]);
    fs::create_dir_all(&output)?;
    let (pdfium, onnx) = if cfg!(windows) {
        ("pdfium.dll", "onnxruntime.dll")
    } else {
        ("libpdfium.dylib", "libonnxruntime.1.27.0.dylib")
    };
    let mut files: Vec<_> = fs::read_dir(corpus)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "pdf"))
        .collect();
    files.sort();
    if files.is_empty() {
        return Err("corpus contains no PDFs".into());
    }
    for path in files {
        let stem = path
            .file_stem()
            .ok_or("missing file stem")?
            .to_string_lossy();
        let bytes = fs::read(&path)?;
        let native = stem.ends_with(".native");
        let result = process_pdf_with_ocr_mem(
            &bytes,
            OcrPdfOptions {
                model_manifest: &PP_OCR_CYRILLIC,
                pdfium_library: Some(runtime.join(pdfium)),
                onnx_runtime_library: Some(runtime.join(onnx)),
                retain_recognition: true,
                ocr: OcrOptions::new()
                    .mode(if native { OcrMode::Off } else { OcrMode::Force })
                    .minimum_confidence(0.0)
                    .model_directory(runtime.join("models"))
                    .model_downloads(ModelDownloadPolicy::Offline),
                render: RenderOptions::new().dpi(150.0),
                ..OcrPdfOptions::default()
            },
        )?;
        if result.page_count != 1 || result.pages.len() != 1 {
            return Err(format!("{stem}: expected exactly one page").into());
        }
        if !native && (result.pages_routed_to_ocr != [1] || result.recognition.len() != 1) {
            return Err(format!("{stem}: missing forced OCR/raw recognition").into());
        }
        if native && (!result.recognition.is_empty() || !result.pages_routed_to_ocr.is_empty()) {
            return Err(format!("{stem}: native-only unexpectedly ran OCR").into());
        }
        fs::write(output.join(format!("{stem}.prepared.md")), &result.markdown)?;
        let raw = result
            .recognition
            .iter()
            .flat_map(|page| page.spans.iter())
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(output.join(format!("{stem}.recognition.txt")), raw)?;
        let metadata = format!(
            "source={:?}\nrouted={:?}\nrecommended={:?}\nwarnings={:?}\nmodel={:?}\n",
            result.pages[0].provenance.source,
            result.pages_routed_to_ocr,
            result.pages_recommended_for_ocr,
            result.pages[0].provenance.warnings,
            result.recognition.first().map(|page| &page.model),
        );
        fs::write(output.join(format!("{stem}.metadata.txt")), metadata)?;
        if fs::read(&path)? != bytes {
            return Err(format!("{stem}: source bytes changed").into());
        }
        println!(
            "{stem}: {:?}, {} recognition spans",
            result.pages[0].provenance.source,
            result
                .recognition
                .iter()
                .map(|page| page.spans.len())
                .sum::<usize>()
        );
    }
    Ok(())
}

#[cfg(not(all(feature = "ocr", not(target_arch = "wasm32"))))]
fn main() {
    eprintln!("ocr_corpus requires the native `ocr` feature");
    std::process::exit(1);
}
