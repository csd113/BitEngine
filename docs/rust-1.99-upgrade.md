# Rust 1.99 compiler upgrade

## Dependency refresh — 2026-10-04

The declared MSRV, CI compatibility job, and in-app Rust dependency checks now
require Rust 1.99. The exact 1.99.0 development/release pin already matched this
version. Direct dependencies are updated to their latest stable releases and
the application lockfile is refreshed to the newest versions allowed by the
upstream dependency requirements. This includes Arti 0.47, fs-mistrust 0.16,
futures-copy 0.5, safelog 0.10, and qrcode 0.14.1.

The vendored tor-hsservice source is rebased on 0.47.0. This upstream release
still has the inverted publication-expiry predicate, so the existing fix and
regression test remain. Its shipped manifest and lockfile are retained with
updated package provenance in vendor/tor-hsservice/PATCH.md.

Three transitive packages remain behind newer releases because upstream
requirements constrain them: derive-deftly and derive-deftly-macros use the
Arti-required 1.12 series, and crypto-common pins generic-array to 0.14.7.

Validation passed on macOS Apple Silicon with Rust 1.99.0:

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings -D clippy::all -D clippy::pedantic -D clippy::nursery -D clippy::cargo`
- `cargo test --workspace --all-features`: 284 passed, four existing optional tests ignored.
- `cargo test --manifest-path vendor/tor-hsservice/Cargo.toml --locked --features full publication_expiry_retains_only_still_valid_records`: one passed.
- `cargo upgrade --dry-run --incompatible --verbose --verbose`: all 25 direct dependencies at their latest stable releases.
- `cargo update --dry-run --verbose`: no further resolvable application lockfile updates.
- `git diff --check`

The vendored source was compared with the published 0.47.0 crate: the only
source deviation is the existing expiry correction and regression test.
Linux builds and live Tor-network behavior were not exercised in this refresh.
No commits or releases were created.

## Original compiler upgrade — 2026-10-01

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
