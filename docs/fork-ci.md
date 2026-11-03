# Fork CI (lawkitt)

This fork exists for mdoc, which pins it by git rev with `features = ["ocr"]`
and builds it on macOS, Windows, iOS and Android. It does not publish packages
and does not use the Node, Python or WASM bindings. Decided with the maintainer
on 2026-11-03.

## Decisions

1. **Keep the strict Actions policy.** The repository allows GitHub-owned
   actions plus `Swatinem/rust-cache`, `softprops/action-gh-release` and
   `taiki-e/install-action`, every one pinned by full SHA. Workflows install
   Rust with `rustup toolchain install`, which reads `rust-toolchain.toml`, so
   the toolchain version lives in that one file (upstream's
   `dtolnay/rust-toolchain` and `oven-sh/setup-bun` are not allowed).
2. **One fork-owned `ci.yml`; no publish or pages workflows.** Upstream's
   `ci.yml` is replaced in place, and `publish.yml`, `publish-crate.yml`,
   `publish-pypi.yml`, `publish-wasm.yml` and `pages.yml` are deleted, so this
   fork can never publish under upstream's package names or deploy its site.
3. **Jobs, all on standard GitHub-hosted runners** (free for a public repo):
   - `Lint` (Linux): version sync, `cargo fmt --check`, clippy default and
     `--features ocr` with `-D warnings`.
   - `Test` (Linux): `cargo test` and the developer-script tests.
   - `OCR (ubuntu-latest | macos-latest | windows-latest)`:
     `cargo test --features ocr`, the configurations mdoc ships.
   - `OCR runtime smoke` (Linux): real PDFium and ONNX Runtime (SHA-checked
     downloads), model provisioning, the OCR CLI and its JSON contract, and
     `ocr_tests` with models.
   - Left out: upstream's Node/Python binding build in the runtime smoke, the
     WASM job and the release build matrix. iOS and Android cross-builds are
     covered by mdoc's CI when it bumps the pinned rev.
4. **Required checks.** After this CI is on `main` with a green run, the
   maintainer adds a ruleset on `main` requiring the jobs above (no review
   requirement). Repository settings are the maintainer's; agents do not change
   them.
5. **PRs wait for green CI**, including PRs opened before this CI existed (merge
   `main` into them to get a run).

## When syncing upstream

- `.github/workflows/ci.yml` conflicts whenever upstream changes its CI. Keep
  the fork's file; port any new check that applies to mdoc's use (Rust library,
  `ocr` feature, macOS/Windows), installing tools without new third-party
  actions.
- Upstream changes to the deleted publish/pages workflows arrive as
  modify/delete conflicts: resolve with `git rm`.
- A new upstream workflow file arrives as an addition: delete it or adapt it to
  the rules above before merging the sync.
- A toolchain bump in `rust-toolchain.toml` needs nothing here.
