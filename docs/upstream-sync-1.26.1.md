# lawkitt upstream sync to 1.26.1

The fork merges Firecrawl revision
`c09c4f74680868e21d3bb0dff0a7467115313115` (1.26.1) into fork `main` at
`727f3228f43aab9ff3f24ab64ede578ef7451121`, as a merge commit that keeps both histories and the local OCR
extensions (pinned Cyrillic manifest, explicit runtime paths, offline
recognition, explicit consent, recognition output before fusion). It also
ports eleven unmerged community pull requests, one commit each with the
author's attribution, and adds two fork changes. Decided in
[lawkitt/mdoc#925](https://github.com/lawkitt/mdoc/issues/925) (item M1).

## What came in from upstream

- [#618](https://github.com/firecrawl/pdf-inspector/pull/618): cmap-less
  core Windows subsets read by their own glyph order, blank glyphs as spaces.
- [#663](https://github.com/firecrawl/pdf-inspector/pull/663): repeated
  strikethroughs, and strikethroughs inside tables, are kept.
- [#666](https://github.com/firecrawl/pdf-inspector/pull/666): a word space
  the next word is kerned over is kept.
- The 1.26.0 and 1.26.1 release bumps (manifests, lockfiles, changelog).

### Merge conflicts

- `src/markdown/postprocess.rs`: the fork marks the line breaks it joins
  with U+FDD0; upstream lets a decoration close and reopen around a broken
  word (`Perfor-</s> <s>mance`). The split-span regex keeps both:
  `({WORD}{2,})-(\*{1,2}|</s>)([ \x{FDD0}])(\*{1,2}|<s>)({WORD}{2,})`, and
  the markers match when `caps[2].replacen("</", "<", 1) == caps[4]`.
- `CHANGELOG.md`: the fork's `Unreleased` section stays above 1.26.1.
- No textual conflict, but needed: the fork's test item helper in
  `src/tables/detect_heuristic.rs` sets upstream's new
  `layout_strikeout: false`.
- `.github/workflows/` did not conflict; upstream did not change CI in this
  range, so `docs/fork-ci.md` needed nothing.

## Ported community pull requests

In commit order. Each commit message names the PR and its authors.

| PR | Author(s) | What it fixes | Port notes |
|---|---|---|---|
| [#368](https://github.com/firecrawl/pdf-inspector/pull/368) (contains [#287](https://github.com/firecrawl/pdf-inspector/pull/287)) | Lorenzo Pinosa (@IngLP); mathurshubham (#287) | Folio bands scale with the page's real height and the top band is measured from the page's own top edge (A4 folios dropped, body digits near the bottom kept) | Conflicts in `src/extractor/mod.rs` (keep the fork's `PageRotations`/`DocumentExtraction` beside `PageVerticalBounds`) and `src/tables/mod.rs` (the fork's two-pass row grouping passes `page_bounds`); two fork tests pass empty bounds. #287 is never taken alone. |
| [#601](https://github.com/firecrawl/pdf-inspector/pull/601) | Mangesh Raut (@mangeshraut712) | Repeated body text on short pages is no longer stripped as a running header | Clean. |
| [#598](https://github.com/firecrawl/pdf-inspector/pull/598) | Mangesh Raut (@mangeshraut712) | Text rendering mode persists across text objects, so invisible (`3 Tr`) text stays out | Conflict in `content_stream.rs`: the PR's ActualText push plus `layout_strikeout: false`. The reset at `BT` is gone. |
| [#312](https://github.com/firecrawl/pdf-inspector/pull/312) | Vibhanshu Mishra (@vibhanshu-mishra) | A Form XObject without `/Resources` inherits its caller's | Clean. |
| [#407](https://github.com/firecrawl/pdf-inspector/pull/407) | Madeniyou Sall (@MADENIYOU) | Indirect form `/Resources` resolved during CMap discovery | Clean. |
| [#616](https://github.com/firecrawl/pdf-inspector/pull/616) | Mohammad (@litoosh13) | AcroForm names and values decoded as PDF text strings | Clean; #600 (same fix) not taken. |
| [#626](https://github.com/firecrawl/pdf-inspector/pull/626) | Deepanshu Pal (@deepanshupal) | `%` inside inline image data no longer cut as a comment | Clean. |
| [#621](https://github.com/firecrawl/pdf-inspector/pull/621) | Terry (@tlq5l) | A simple font's sparse ToUnicode CMap reads the codes its program does not | Changelog entry moved into the fork's `Unreleased`. |
| [#317](https://github.com/firecrawl/pdf-inspector/pull/317) | Ryan M (@rmacy) | Exact duplicate text overlays collapse in Markdown | Its test sets `page_bounds` (from #368). Taken over #377. |
| [#384](https://github.com/firecrawl/pdf-inspector/pull/384) | jeesonwang (@jeesonwang) | Page-sized background/clip rects add no row or column edges | Clean. |
| [#540](https://github.com/firecrawl/pdf-inspector/pull/540) | sasuketorii (@sasuketorii) | Multi-page OCR no longer deadlocks on a worker's session lock | `use`-line conflict in `src/vision/oar.rs`; changelog entry moved. |

## Fork changes in this sync

- **AcroForm field names leave the text.** `extract_form_fields` wrote
  `name: value`; a field item now carries its value only, since the name
  (`/T`) is the form's internal key and the page never draws it. Per-page
  Markdown extraction and the OCR pipeline report the names left out per
  page (`form_field_names_by_page`, a list of `PageFormFieldNames`, beside
  `images_by_page`). A checked box or selected radio reads `☑` instead of
  its on-state name (`Yes`). Hidden content policy: lawkitt/mdoc#925
  decision 9.
- **Lettered list labels of any script.** Russian `а)`–`я)` already became
  `- а) …` items through `split_list_label` (`d28ce29`), but
  `is_list_item`'s quick check was ASCII-only, so a bare Cyrillic label
  (`б)` with its text on the next line) or one before a digit fell back to
  prose. A lowercase letter of any script before `)` now passes like a
  Latin one; a non-Latin letter before `.` does not (`т.`, `г.`).

## Verification on Apple Silicon macOS (10 cores), 2026-11-18

- `cargo fmt --check`, `cargo clippy -- -D warnings` (default and
  `--features ocr`), `cargo test` and `cargo test --features ocr` pass
  after every commit (OCR run at the head: 1,842 library tests, 300
  integration tests, plus the binary, renderer and doc tests).
- **mdoc corpus, before/after.** The offline `ocr_corpus` example was
  built at the fork baseline `727f322` and at this sync's head and run on
  mdoc's `tests/fixtures/ocr-upstream` (12 public pages, four native
  originals) and `tests/fixtures/ocr-qualification` (two synthetic EN/RU
  pages): same pinned PDFium `native-v7988`, ONNX Runtime 1.27.0 and
  `pp-ocr-cyrillic` models (SHA-256 checked), confidence 0, 150 dpi,
  offline. mdoc's `tools/ocr-upstream/compare.py` reports 18 captures and
  0 changed outputs: all 54 artifacts (prepared Markdown, raw recognition,
  provenance) are byte-identical.
- **Native (Off) Markdown** of mdoc's import fixture PDFs (`text.pdf`,
  `handmade-mixed.pdf`, `handmade-partly-scanned.pdf`,
  `handmade-scanned.pdf`), through the call mdoc makes
  (`process_pdf_with_ocr_mem(bytes, OcrPdfOptions::new())`): Markdown and
  routing metadata byte-identical.
- **The fork's own fixtures** (87 PDFs in `tests/fixtures`, the same Off
  call): two change, each bisected to one commit.
  - `bits_pilani_feedback.pdf`, from #368: on its A4 pages the
    Letter-calibrated folio bands dropped numbers that are text, and they
    come back: two years (`pp.1585-1602, 2018`) and ten list numbers
    (`18 B. Siva Prasad …`). No word is lost. One side effect: on page
    114 the subscript digits of `C3`/`C4` near the top of the page, which
    the old band set aside as folio candidates, now count as layout
    evidence. That tips a five-line block ("Novel Distributor Plates …",
    its keywords) into a two-column split, so those lines come out as
    separate paragraphs in a different order. All of their words are still
    there.
  - `multiline_indent_cell_rect_grid.pdf`, from #384: the page-sized clip
    no longer adds row edges at the page border, so the 105 running
    `Page N` footers stop landing in the last table cell and are stripped
    as folios. Label rows (`Remediation:`, `References:`, `Audit:`) stop
    being fake table rows and become headings with their text. In the
    two-column "Note:" box, one sentence ("This Group Policy path may not
    exist by default…") now comes after the URLs instead of before them.
    That box was already out of order at the baseline.
- **#540 deadlock check.** Forced OCR of a 28-page image-only PDF (the
  corpus pages twice) at the engine's default, full-width concurrency, six
  times in one process: every run finished (24–39 s; ~3.5 cores busy on
  average) with identical Markdown. #540's 31 `vision::oar` tests,
  covering concurrent callers, reentry, nested global-pool work and
  fallback admission, passed 30 times in a row.
  The same six runs at the baseline `727f322` also finished (28–37 s,
  same Markdown hash), so this Mac does not reproduce the hang. The check
  shows the new dispatch neither hangs nor changes the output at full
  width; it does not show the old hang going away.
- macOS OCR smoke: the corpus run above uses real PDFium and ONNX Runtime
  on this Mac, so it doubles as the macOS smoke check. The Windows OCR
  smoke check and #540 on a 24-thread PC are run separately by the Windows
  hub (mdoc#925 W2).

References in mdoc's corpus are agent-reviewed, not human-verified, and
unchanged output is not evidence of good recognition. The upstream
`pdf-evals` suite is still unavailable to this fork, so its snapshot and
semantic scores are unverified.

## Not in this sync

- #598 now keeps invisible (`3 Tr`) text out of the Markdown, but the fork
  does not yet count it the way it counts form field names and pictures.
  An import note that wants to count invisible text needs that count first
  (mdoc#925 decision 9).
- The phase 2 items in mdoc#925 (white text, headers and footers, tables,
  OCR on scans) rebase onto this sync.
