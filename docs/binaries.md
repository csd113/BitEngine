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
- concurrent stdout/stderr draining and detailed build logs;
- CPU worker selection and executable permission handling; and
- cancellation through child-process termination.

BitForge's window, cards, modal channels, target selector, and standalone application bootstrap were not moved. The BitForge repository remains frozen as a historical reference; it is not a second active implementation or a runtime dependency of BitEngine.

## BitEngine module map

`src/binaries/mod.rs` defines validated release versions, binary/build-stage types, installed-version probes, and upstream release discovery.

`src/binaries/environment.rs` constructs the child environment without changing BitEngine's process-wide environment. It preserves package-manager, Cargo, LLVM, `pkg-config`, and plain-output settings needed by the former build pipeline.

`src/binaries/dependencies.rs` checks only requirements relevant to the selected source build. Missing packages are reported with platform guidance; BitEngine does not silently run a package-manager installation during an update.

`src/binaries/process.rs` launches commands directly rather than through a shell, drains stdout and stderr concurrently, streams readable output to the UI and durable log, and checks cancellation while waiting. Child handles use `kill_on_drop`.

`src/binaries/service.rs` is the application service. It enforces one active build, persists every human-readable stage, prepares or reuses verified source, runs the target pipeline, verifies the primary binary's reported version, and invokes installation only after every earlier step succeeds.

`src/binaries/install.rs` stages all files in the configured binaries filesystem, checks copied sizes, applies executable permissions, backs up the existing set, and activates the complete staged set. A failed activation removes newly activated files and restores backups. Existing binaries are never touched after a download, dependency, configuration, compilation, cancellation, or binary-version failure.

`src/ui.rs` owns page navigation and build state. `src/ui_render.rs` presents installed/latest versions and the common action first, then progressively discloses build stages, compiler details, release selection, workspace, and worker information.

## Job lifecycle and recovery

BitEngine derives `BitEngineBuilds/` beside the configured `Binaries/` directory. This keeps the existing BitEngine configuration authoritative and avoids a second output-path setting. The workspace contains a clean source cache and per-job work/log directories.

The most recent job snapshot is stored as `build-job.json` beside `config.json`. If BitEngine starts with a job marked `Running`, the service converts it to `Interrupted` and explains that existing binaries were left unchanged. It does not guess that a partially completed build succeeded or resume an unknown process.

Only one Bitcoin Core or electrs job can own the coordinator. A conflicting request is rejected before source or filesystem mutation. Cancellation stops the active child and records a cancelled result.

## Verification and installation guarantees

Before compilation, BitEngine checks that the cached or downloaded repository has the expected upstream origin, that `HEAD` resolves to the selected release tag, and that tracked files are clean. After compilation, it runs `bitcoind --version` or `electrs --version` and requires the reported version to match the selected stable release.

The former BitForge project did not implement release-signature or source-archive hash verification. This integration therefore does not claim cryptographic source authentication: it preserves the HTTPS/Git trust model while adding explicit origin/tag/commit/clean-tree validation. Adding maintainer-key provisioning and signed-tag verification remains separate future security work.

Installation is delayed until source and output verification pass. Every output is copied to a transaction-specific temporary file in the destination filesystem, checked, synced, and then renamed. Existing destinations are first renamed to transaction backups. If activation of any member fails, already activated members are removed and backups are restored. Successful builds take effect the next time the corresponding node is launched; a currently running process continues using its already-open executable.

## Build behavior retained

Bitcoin Core retains the former node-focused options: wallet, IPC, GUI, tests, benchmarks, MiniUPnP, NAT-PMP, and ZMQ are disabled. The exact supported output set is discovered from `build/bin`, restricted to BitEngine's known Bitcoin executables, and requires `bitcoind`.

electrs builds use the selected tagged source with Cargo release mode, bounded worker count, a job-local target directory, color-free streamed output, and `--locked` dependency resolution. The resulting `electrs` executable is required and version-checked.

Both pipelines reject unsupported platforms, missing dependencies, unsafe release tags, symbolic-link build workspaces, insufficient disk space when `df` is available, unexpected source state, missing outputs, version mismatches, and overlapping jobs.
