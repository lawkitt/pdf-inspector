# lawkitt upstream sync to 1.25.2

The fork merges Firecrawl revision
`ef52f77850b29189048797a48b24862251f54fe7`, retaining the local Cyrillic model
manifest/explicit runtime paths and optional raw recognition before fusion.
The CLI conflict keeps upstream's option-before-path parser and adds the fork's
`--ocr-model` value option, help and regression test.

The offline `ocr_corpus` example captures prepared Markdown, raw recognition and
provenance separately, requires one-page inputs, verifies forced/native routing
and checks source bytes after conversion. Its native-only runs initialize no
OCR. Use a runtime root containing `models/` (the explicit Cyrillic model
artifact directory), `libpdfium.dylib` and `libonnxruntime.1.27.0.dylib` on macOS,
or `pdfium.dll` and `onnxruntime.dll` on Windows. Models and libraries remain
external. The example never downloads artifacts.

```sh
cargo run --features ocr --example ocr_corpus -- CORPUS OUTPUT RUNTIME_ROOT
```

## Verification on Apple Silicon macOS, 2026-10-04

- Default and OCR-enabled tests pass (OCR run: 1,707 library tests, 292
  integration tests, and binary/renderer/contract/doc tests).
- Formatting, default/OCR clippy with denied warnings, release build, version
  consistency and 25 developer-script tests pass.
- The CLI suite additionally passes the custom-model argument-order regression.
- 12 public EN/RU pages, four native originals and two synthetic EN/RU fixtures
  were captured with the same harness/settings at fork baseline
  `7a11f9f3f2423edd0035f87e7152b49c08a7ce1a` and the merged revision.
  All 54 output artifacts are byte-identical: prepared Markdown, raw recognition
  and recorded provenance for each of the 18 captures. Both synthetic
  transcripts match after recorded normalization, including Cyrillic Ё/ё.
- The public corpus, pinned source/license/digest manifest, reviewed reference
  transcripts, captures and scorer live in mdoc's
  `tests/fixtures/ocr-upstream` and `tools/ocr-upstream/compare.py`.

References were checked by the agent, not independently verified by a human.
Existing difficult-layout/scan reading-order and recognition limitations remain;
unchanged output does not establish general OCR or table accuracy. Do not reset
snapshots to conceal output regressions.

The sibling `pdf-evals` suite is absent and `firecrawl/pdf-evals` is inaccessible
to this account, so its full `bench.py test`/semantic score are unverified.
Fresh Windows x64 app OCR smoke is an adoption gate in the accompanying mdoc PR;
macOS results do not establish Windows or native UI acceptance.
