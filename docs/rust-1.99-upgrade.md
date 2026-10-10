# Rust 1.99 compiler upgrade

## Permanent quality policy — 2026-10-10

This audit began with a clean checkout already using rustc and Cargo 1.99.0.
The exact toolchain pin remains 1.99.0, the manifest MSRV now explicitly spells
`1.99.0`, and edition 2021/application version 1.0.0 are unchanged. No dependency,
application lockfile, or vendored source changes were necessary.

The initial macOS ARM64 baseline passed formatting, locked all-target/all-feature
check, strict all/pedantic/nursery/cargo Clippy, all 284 unit tests (four existing
integration tests ignored), and rustdoc. The pre-cleanup release build also
completed successfully. The old Clippy policy lived only in invocation flags;
the new manifest enforces it during normal development.

Added deny-level compiler groups, API/visibility hygiene, explicit unsafe/raw
borrow checks, and selected restriction lints. Checked arithmetic is mandatory
in sensitive process, configuration, protocol, Tor, and binary-build modules.
The initial stricter probe exposed 322 distinct diagnostic locations, including
109 visibility diagnostics, 14 hidden Iced lifetimes, 67 indexing diagnostics,
and 44 arithmetic diagnostics, plus conversion, unsafe-region, fixture panic,
and obsolete-allow reports. Compilation of both binary and test targets emitted
505 reports before deduplication; these counts include fixture code.
Fixture panic/index exemptions use dedicated Clippy settings rather
than blanket allowances. All retained expectations were individually reviewed.
See [the full policy and exception audit](lint-policy.md).

The cleanup narrows internal visibility, checks process/deadline/length
arithmetic, removes temporary version-parsing/task collections, reuses owned
UTF-8 log buffers, and traverses QR runs safely. RpcAuth Debug now redacts
credentials; RPC responses are bounded to 1 MiB and managed RPC bypasses proxy
environment variables so cookies stay on the snapshotted local socket.
Durable-log I/O and task-join
failures reach the GUI while preserving the committed installation outcome.
Eight focused tests supplement existing process, cancellation, exclusion,
readiness, configuration, verification, installation, and recovery coverage.
The RPC peer fixture consumes complete HTTP requests before closing, avoiding a
TCP-reset race. An existing verack-timeout fixture now holds its connection until
the client closes and allows a one-second scheduling margin instead of 20 ms;
the precise timeout-phase assertion and production deadlines are unchanged.
The refused-candidate fallback fixture now exercises the production probe API
with its normal deadlines, preserving the selected-endpoint assertion without a
fragile 100 ms test-only handshake limit.

[CONTRIBUTING.md](../CONTRIBUTING.md) makes the permanent rules visible to
contributors and coding agents. [The shared gate](../scripts/quality-gate.sh)
verifies exact compiler/Cargo versions and performs the same full validation in
local development, pinned three-platform CI, and release jobs before packaging.
The supplementary future-stable job remains an early warning. macOS bundle
metadata consistently requires macOS 12.0, matching the documented platform.

### Final validation commands

`scripts/quality-gate.sh` prints and verifies rustc 1.99.0
(`b940084d7`, 2026-09-28) and Cargo 1.99.0 (`5f94df478`, 2026-08-27), then runs:

```bash
cargo fmt --all --check
cargo check --locked --workspace --all-targets --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings -D clippy::all -D clippy::pedantic -D clippy::nursery -D clippy::cargo
cargo test --locked --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --all-features --no-deps --document-private-items
cargo build --locked --release
```

The script preserves existing rustdoc flags and appends `-D warnings`. Targeted
RPC/proxy and P2P timeout tests were also run during development. Workflow YAML,
native target/pin/gate assertions, embedded Bash blocks, shell script syntax,
generated local Info.plist metadata, new documentation links, and
`git diff --check` are checked separately. The complete installed restriction
catalogue is accounted for in the policy.

All six commands passed on the final source tree, and each shared-gate invocation
exited successfully:

| Local validation platform | Tests | Documentation and release |
|---|---|---|
| macOS Apple Silicon, native `aarch64-apple-darwin` | 292 passed, four existing integrations ignored | Passed; Mach-O arm64 verified |
| Debian 13 ARM64 container, native ARM64 CPU in the Linux VM | 292 passed, four existing integrations ignored | Passed; ELF AArch64 verified |
| Debian 13 x86_64 container, CPU emulation on Apple Silicon | 292 passed, four existing integrations ignored | Passed; ELF x86-64 verified |

Linux tests used an init/subreaper. Initial containers with `sleep` as PID 1 left
killed helper descendants as zombies and failed process-existence assertions;
proper reaping resolved those failures. A highly parallel x86_64 emulated test
harness also stalled during concurrent LLVM compilation. Its final complete
gate used `RUST_TEST_THREADS=4`; all tests and concurrency assertions remained
active. Native CI retains its default test parallelism. The two protocol fixture
timing corrections above also address failures exposed by CPU emulation.

A final Linux ARM64 release GUI smoke test under Xvfb mapped a visible
1440×960 BitEngine window and kept it alive, using private temporary XDG config,
cache, runtime, and node-root directories. An initial startup exposed the missing
`libxkbcommon-x11` runtime library; CI/release package lists and prerequisites now
include it explicitly. No real node or user data was used.

The four pre-existing ignored tests require live Core endpoints, a real Core
executable, a complete upstream electrs build, or managed live RPC/electrs.
They remain opt-in. Live Tor behavior, an interactive macOS GUI session, macOS
bundle assembly, and actual GitHub Actions execution were not exercised here.
Native CI is configured for all three targets. No commits, pushes, tags, or
published releases were created.

### Changed files

| Area | Files |
|---|---|
| Policy | `Cargo.toml`, `clippy.toml` |
| Enforcement | `.github/workflows/ci.yml`, `.github/workflows/release.yml`, `scripts/check-rust-toolchain.sh`, `scripts/quality-gate.sh`, `build-mac-app.sh` |
| Documentation | `README.md`, `CONTRIBUTING.md`, `docs/lint-policy.md`, `docs/rust-1.99-upgrade.md` |
| Entry/platform/configuration | `src/main.rs`, `src/platform.rs`, `src/config.rs`, `src/bitcoin_config.rs` |
| Processes/protocols | `src/process_manager.rs`, `src/bitcoin_status.rs`, `src/electrs_status.rs`, `src/rpc.rs`, `src/connection.rs`, `src/tor.rs` |
| Binary service | `src/binaries/mod.rs`, `src/binaries/environment.rs`, `src/binaries/dependencies.rs`, `src/binaries/process.rs`, `src/binaries/install.rs`, `src/binaries/service.rs` |
| UI | `src/ui.rs`, `src/ui_render.rs` |

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
