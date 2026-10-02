# Native binary build architecture

BitEngine owns Bitcoin Core and electrs builds directly. The toolbar's **Update Binaries** action changes the current BitEngine page; it does not launch, proxy, iframe, or visually embed another application.

## What moved from BitForge

The useful standalone implementation was separated from BitForge's egui/eframe shell and adapted to BitEngine's Iced task model:

- stable release lookup through the GitHub Releases API;
- platform-aware `PATH`, Cargo, LLVM, and `pkg-config` environment setup;
- macOS and Linux build-dependency detection;
- shallow tagged Git source acquisition;
- Bitcoin Core node-only CMake configuration and compilation;
- electrs Cargo compilation;
- concurrent stdout/stderr draining and bounded detailed build logs;
- CPU worker selection and executable permission handling; and
- cancellation through build-process-group termination.

BitForge's window, cards, modal channels, target selector, and standalone application bootstrap were not moved. The BitForge repository remains frozen as a historical reference; it is not a second active implementation or a runtime dependency of BitEngine.

## BitEngine module map

`src/binaries/mod.rs` defines validated release versions, binary/build-stage types, installed-version probes, and upstream release discovery.

`src/binaries/environment.rs` constructs the child environment without changing BitEngine's process-wide environment. It preserves package-manager, Cargo, LLVM, `pkg-config`, and plain-output settings needed by the former build pipeline.

`src/binaries/dependencies.rs` checks only requirements used by BitEngine's node-only Bitcoin Core and static-RocksDB electrs builds. The independent Advanced action validates resolved executable/library versions, supports narrowly scoped Homebrew and Debian/Ubuntu apt installation, and always scans both before and after an install attempt. Unsupported Linux distributions fail closed without guessed package-manager commands.

`src/binaries/process.rs` resolves and launches commands directly rather than through a shell, drains stdout and stderr concurrently, streams readable output to the UI and durable log, and checks cancellation while waiting. Each build command runs in its own process group; cancellation and task teardown terminate descendants as a group, with a bounded graceful wait and kill escalation.

`src/binaries/service.rs` is the application service. It enforces in-process and cross-process build exclusion, persists every human-readable stage, prepares a private source tree, authenticates the selected upstream tag, runs the target pipeline, verifies every discovered managed binary's reported version, and invokes installation only after every earlier step succeeds.

`src/binaries/install.rs` stages all files in the configured binaries filesystem, revalidates their identity, checks copied sizes, applies executable permissions, and syncs them before mutation. It then writes a durable transaction journal, backs up the existing managed set, and activates the complete staged set. A failed activation removes newly activated files and restores all backups. Existing binaries are not touched by a download, dependency, configuration, compilation, cancellation, source-authentication, or binary-version failure.

`src/ui.rs` owns page navigation, persistent Advanced settings, dependency actions, and build state. `src/ui_render.rs` presents installed/latest versions and the common action first, then progressively discloses build stages, compiler details, release selection, dependency status, workspace, and build settings.

## Job lifecycle and recovery

BitEngine derives `BitEngineBuilds/` beside the configured `Binaries/` directory. This keeps the existing BitEngine configuration authoritative and avoids a second output-path setting. Every job receives a private log directory. With Keep Source Code disabled, source and work trees remain disposable and are removed only after a successful committed installation. With it enabled, source and work are created in a private release-keyed cache entry marked uncommitted; only a successful binary-install commit removes that marker and activates the cache for reuse. Cached source is never trusted by location or metadata alone: origin, tag/commit, signature, signer policy, and pristine state are revalidated before every build. Clean Build removes only that cache entry's `work` child. Durable logs are capped at 64 MiB, and only the eight newest job directories/logs are retained.

The most recent job snapshot is stored as `build-job.json` beside `config.json`. If BitEngine starts with a job marked `Running`, the service converts it to `Interrupted`; it does not guess that an unknown process succeeded or attempt to resume it. Whether installed files need restoration or finalization is determined separately from the durable installation journal.

Only one Bitcoin Core or electrs job can own the in-process coordinator, and a filesystem lock also excludes other BitEngine processes from the workspace. Installation has a separate destination lock. A conflicting request is rejected before source or destination mutation. Cancellation stops the active build process group and records a cancelled result; cancellation is disabled once the short installation transaction begins.

Before any build, inventory, or node launch uses the destination, BitEngine checks for `.bitengine-install.json`. A `Prepared` journal means commit was not durable, so recovery restores the complete old managed set. A `Committed` journal means the new set is validated and retained, then stale backups and transaction metadata are removed. Recovery is idempotent and serialized by `.bitengine-install.lock`; ambiguous, symbolic-link, or non-regular state fails closed, retains the journal, and blocks use until it can be resolved safely.

## Verification and installation guarantees

Before compilation, BitEngine checks that the downloaded or retained repository has the exact expected upstream origin, that `HEAD` resolves to the selected annotated release tag, that the tag signature is trusted, and that tracked, untracked, and ignored inputs are pristine. Git is run with system and global configuration disabled for source acquisition and validation so repository URL rewriting, hooks, and similar ambient configuration cannot change the intended build input.

Bitcoin Core tags are verified through Git's raw OpenPGP status and must report exactly one non-expired, non-revoked primary fingerprint pinned from Bitcoin Core's official `contrib/verify-commits/trusted-keys` set. Older electrs tags are restricted to the maintainer's pinned OpenPGP fingerprint; electrs v0.11.1 uses Git's SSH verification against the pinned upstream maintainer key. BitEngine fails closed if the required verifier, key, signature, or signer authorization is missing. It intentionally does not download keys, grant owner trust, or bypass the upstream projects' official fingerprint-verification procedures. Unknown future electrs releases and signer rotations require a reviewed BitEngine policy update. See the [Bitcoin Core download-verification guidance](https://bitcoincore.org/en/download/), the upstream [trusted-key set](https://raw.githubusercontent.com/bitcoin/bitcoin/v31.1/contrib/verify-commits/trusted-keys), and Git's [signed-tag verification documentation](https://git-scm.com/docs/git-verify-tag).

After compilation, BitEngine executes every discovered managed artifact's version command, not only the primary daemon, and requires each executable to identify the requested project release. Installation is delayed until all source and output verification passes. Every output is copied to a transaction-specific temporary file in the destination filesystem, checked, synced, and then renamed. Existing destinations are first renamed to transaction backups. The durable journal's `Prepared` and `Committed` phases distinguish rollback from finalization across process or machine interruption.

The Bitcoin managed family is `bitcoind`, `bitcoin-cli`, `bitcoin`, `bitcoin-tx`, `bitcoin-util`, and `bitcoin-wallet`; electrs manages `electrs`. A managed binary not produced by the selected release is represented as a transactional removal, so obsolete companion tools cannot survive as a mixed-version installation. Successful builds take effect the next time the corresponding node is launched; a currently running process continues using its already-open executable.

## Build behavior retained

Bitcoin Core retains the former node-focused options: wallet, IPC, GUI, tests, benchmarks, MiniUPnP, NAT-PMP, and ZMQ are disabled. The exact supported output set is discovered from `build/bin`, restricted to BitEngine's known Bitcoin executables, and requires both `bitcoind` and `bitcoin-cli`.

electrs builds use the selected tagged source with Cargo release mode, bounded worker count, a job-local target directory, color-free streamed output, and `--locked` dependency resolution. The resulting `electrs` executable is required and version-checked.

Both pipelines reject unsupported platforms, missing dependencies, unsafe release tags, untrusted signatures, symbolic-link or escaping paths, insufficient disk space reported through `statvfs`, unexpected source state, missing outputs, version mismatches, and overlapping jobs. Build and install paths must be absolute, distinct, non-root locations; managed files and directories use restrictive permissions and non-regular filesystem entries fail safely.
