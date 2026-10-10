# Development standards

These rules apply to contributors and coding agents. The enforceable policy is
in [Cargo.toml](Cargo.toml), [clippy.toml](clippy.toml), and
[CI](.github/workflows/ci.yml). See [lint decisions](docs/lint-policy.md) for the
reasoning and the complete exception audit.

1. Compile, lint, test, and package with exact Rust **1.99.0**. Keep edition 2021.
   Update the toolchain, MSRV, scripts, CI, release workflow, and documentation
   together when deliberately advancing the compiler.
2. Pass every configured rustc and Clippy lint. New warnings are errors; do not
   weaken groups, thresholds, or CI to accommodate a change.
3. Use a local `#[expect(lint, reason = "...")]` only for a reviewed technical
   invariant. Explain it in the lint-policy exception inventory. Obsolete
   expectations must fail. Never blanket-allow a lint group.
4. Document shared APIs, fallible operations, and error/ownership contracts.
   Exported fallible APIs need `# Errors`; unsafe APIs need `# Safety`.
   Keep visibility as narrow as callers permit.
5. Return contextual or typed errors for recoverable failures. Production code
   must not use `unwrap`, `expect`, panic shortcuts, or direct process exit.
   Fixture assertions can fail tests; they cannot justify production panics.
6. Keep unsafe regions minimal, with a nearby `SAFETY` comment establishing
   pointer validity, initialization, ownership, and platform assumptions.
7. Use checked conversions and explicit overflow behavior for lengths, offsets,
   counters, ports, PIDs, and deadlines. Production indexing must be statically
   safe or checked. Display-only floating-point exceptions stay local.
8. Keep Iced update/render work responsive. Perform blocking I/O, source builds,
   and process shutdown outside the update loop; avoid locks across awaits.
9. Own background tasks, bound queues and reads, join shutdown work, and preserve
   cancellation, generation identities, and rejection of stale results.
10. Pass process arguments separately. Construct child environments explicitly;
    never mutate the application's global environment. Drain both output streams,
    preserve graceful shutdown, and clean up/reap the complete owned process group.
11. Treat paths, configuration, network messages, cookies, environment variables,
    and recovered journals as untrusted. Validate before mutation. Preserve
    symlink/file-identity checks, permissions, locking, and durability ordering.
12. Protect node data, wallets, indexes, user configuration, and custom binaries.
    Preserve staged installation, signed-source verification, fail-closed signer
    policies, non-cancellable activation, rollback, and idempotent crash recovery.
13. Keep RPC cookies and authentication material private and redacted, including
    `Debug` output and chained errors. Maintain timeouts, response limits, and
    strict protocol/readiness classification.
14. Avoid unnecessary allocation, cloning, indirection, or lock contention in
    frequently executed paths. Prefer the standard library and existing crates;
    add dependencies only when necessary.
15. Make focused changes. Preserve UI behavior/hooks, serialization, managed and
    custom paths, and legacy environment/config compatibility. Avoid unrelated
    cleanup and abstractions that only satisfy a complexity metric.
16. Add focused regression tests for meaningful behavior changes. Use temporary
    directories, fake executables, and loopback peers. Never use real wallets or
    node data, or require an upstream node compilation for ordinary refactoring.
17. Preserve all supported targets: macOS Apple Silicon, Linux x86_64, and Linux
    ARM64. Do not introduce macOS Intel or universal packaging.
18. Pass the complete quality gate before handoff. Report commands, results,
    changed files, and limitations. Do not commit, push, tag, or publish without
    explicit authorization.

Run the same gate used by pinned CI and release packaging:

```bash
./scripts/quality-gate.sh
```

The gate prints and verifies both rustc and Cargo 1.99.0, then runs:

```bash
cargo fmt --all --check
cargo check --locked --workspace --all-targets --all-features
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings -D clippy::all -D clippy::pedantic -D clippy::nursery -D clippy::cargo
cargo test --locked --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --locked --workspace --all-features --no-deps --document-private-items
cargo build --locked --release
```

If formatting fails, format the touched Rust files with the pinned rustfmt and
rerun `cargo fmt --all --check`. Use targeted tests during development, followed
by the full gate. There is currently one binary target and no package features
or library doctests; `--all-features` is coherent. Existing ignored integration
tests need explicit external prerequisites and remain opt-in.

Linux needs the GUI development packages listed in CI. Platform jobs run
natively with `CARGO_BUILD_TARGET` set to their release target. The additional
future-stable job detects upcoming compiler changes; exact 1.99.0 gates remain
mandatory. A local packaging build also verifies the exact toolchain.
Container tests need a child-reaping init, such as `docker run --init`, so killed
helper descendants can be reaped after their direct parent exits.
