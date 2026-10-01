# Rust 1.99 compiler upgrade — 2026-10-01

Development and release builds now pin exact Rust 1.99.0 instead of floating
stable. CI also explicitly selects latest stable for formatting, strict Clippy
and full tests so new compiler releases are checked before advancing release
pins. The existing Rust 1.91 MSRV remains the compatibility floor required by
Arti 0.46. Edition 2021, application version 1.0.0, dependencies, vendored Tor
fix and Cargo.lock are unchanged. There is no background compiler updater.

Release and local macOS packaging builds now use locked dependency resolution.
All existing macOS Apple Silicon and Linux x86-64/ARM64 targets are retained.
The stable and MSRV jobs set RUSTUP_TOOLCHAIN explicitly so the repository pin
cannot silently replace the compiler those jobs intend to check.

## Code changes

- Shared the identical Task::none() return after the build-message match.
- Improved existing empty-value test assertions without changing coverage.
- Replaced deprecated atomic fetch_update in the scripted Tor test fixture with
  an equivalent compare-exchange loop. The renamed try_update API is unavailable
  on the retained Rust 1.91 MSRV; success/failure ordering and the no-underflow
  behavior are preserved.
- Fixed a confirmed argument-capture fixture race: it formerly published file
  existence before printf finished, allowing tests to read partial arguments.
  It now writes a temporary file and atomically renames it after completion.
  Listener assertions and their deadlines are unchanged.

## Validation

All checks passed locally on macOS Apple Silicon with Rust 1.99.0:

- cargo fmt --all --check
- cargo check --locked --workspace --all-features
- cargo clippy --locked --workspace --all-targets --all-features -- -D warnings -D clippy::all -D clippy::pedantic -D clippy::nursery -D clippy::cargo
- cargo test --locked --workspace --all-features: 284 passed, four existing optional tests ignored.
- cargo build --locked --release --workspace --all-features
- cargo +1.91.0 check --locked --workspace --all-targets --all-features
- Linux ARM64 container: locked workspace check, strict Clippy, all 284 tests,
  and the 13 existing Linux dependency-selection tests passed.
- Workflow YAML parsing, local packaging script syntax and git diff --check.

One initial macOS run caught the fixture publication race; the isolated test
passed, the race was fixed, and the complete macOS/Linux suites passed afterward.
No tests were removed, skipped beyond existing annotations, or weakened, and no
lint allowances were added.

No new native Linux release binary or Linux x86-64 CI execution is claimed.
The release build was verified on macOS; Linux source/tests were verified in
Debian 12 ARM64. The work is local only: no push, tag or published release.
