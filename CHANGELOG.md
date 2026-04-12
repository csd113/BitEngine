# Changelog

## 0.1.1 - 2026-04-11

- Split the UI into smaller rendering and update modules to reduce the size of `src/ui.rs`
- Tightened configuration, path setup, and Bitcoin config preparation so filesystem failures are surfaced instead of being ignored
- Centralized RPC config parsing and trimmed unused blockchain fields
- Kept CI and release automation macOS-only, with strict formatting and Clippy checks
- Bumped the crate version to `0.1.1`

## 0.1.0 - Initial public cut

- First release of BitEngine
