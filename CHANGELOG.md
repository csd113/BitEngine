# Changelog

## 0.1.2 - 2026-05-15

- Renamed the app, config namespace, and packaged binary to `BitEngine`
- Bumped the crate and release version to `0.1.2`
- Added cross-platform support boundaries for macOS Apple Silicon, Linux x86_64, and Linux ARM64
- Replaced universal/macOS Intel release packaging with supported-platform artifacts only
- Updated documentation for platform config paths, binary names, and release artifacts

## 0.1.1 - 2026-04-11

- Split the UI into smaller rendering and update modules to reduce the size of `src/ui.rs`
- Tightened configuration, path setup, and Bitcoin config preparation so filesystem failures are surfaced instead of being ignored
- Centralized RPC config parsing and trimmed unused blockchain fields
- Kept CI and release automation macOS-only, with strict formatting and Clippy checks
- Bumped the crate version to `0.1.1`

## 0.1.0 - Initial public cut

- First release of BitEngine
