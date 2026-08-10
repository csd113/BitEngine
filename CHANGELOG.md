# Changelog

## 1.0.0 - 2026-08-09

- Bumped the crate and release version to `1.0.0`
- Hardened node lifecycle supervision with owned-process generation tracking, duplicate-launch guards, path locking while nodes are active, stale-poll rejection, and background graceful shutdown
- Replaced log-based readiness with authenticated Bitcoin RPC and mainnet P2P handshakes plus correlated Electrs protocol and metrics probes
- Added fail-closed Bitcoin configuration inspection, managed-generation RPC cookie and endpoint snapshots, verified P2P endpoint selection, and compatibility checks before Electrs launch
- Added bounded Bitcoin RPC startup retries with warm-up progress and actionable timeout, authentication, fatal-error, and early-exit diagnostics
- Integrated Bitcoin Core and electrs source builds as a native BitEngine binaries page
- Added installed/latest version detection, stable release selection, staged progress, cancellation, and expandable build logs
- Added dependency and native disk-space checks, fresh private source trees, authorized-signer-pinned tag authentication, and all-artifact version validation
- Added bounded build logs and retention, process-group cancellation, cross-process workspace/destination locks, and path-confinement checks
- Added durable prepared/committed installation recovery with startup gating, complete-set rollback, and transactional removal of obsolete managed binaries
- Corrected the declared Rust minimum to 1.97.1, pinned the project toolchain, and moved Iced rendering to Tiny-Skia
- Removed the legacy Downloads-folder updater and BitForge application-launch fallback
- Refreshed the application icon and README branding
- Documented the native build architecture and the frozen historical BitForge boundary

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
