<div align="center">

# ⚙️ BitEngine

**A native cross-platform GUI for managing Bitcoin Core and Electrs nodes**

Built with Rust · Iced · Native desktop rendering

Current release: `0.1.2`

[![Rust](https://img.shields.io/badge/rust-1.88%2B-orange?logo=rust)](https://www.rust-lang.org/)
[![CI](https://github.com/csd113/BitEngine/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/csd113/BitEngine/actions/workflows/ci.yml)
[![Platform](https://img.shields.io/badge/platform-macOS%20arm64%20%7C%20Linux%20x64%2Farm64-blue)](#supported-platforms)
[![Architecture](https://img.shields.io/badge/macos-Apple%20Silicon%20only-lightgrey)](#supported-platforms)
[![License](https://img.shields.io/badge/license-MIT-green)](#license)

</div>

---

## What is BitEngine?

BitEngine is a desktop application that lets you launch, monitor, and shut down a self-hosted **Bitcoin Core** (`bitcoind`) and **Electrs** indexer node without touching the terminal.

- Dual side-by-side terminal panels with live log streaming
- Real-time block height display via JSON-RPC
- Green/grey status indicators: **Running · Synced · Ready** for each node
- One-click shutdown (Bitcoin RPC stop first, then platform fallback)
- Native Bitcoin Core and electrs source builds with version checks, progress, logs, and transactional installation
- Fully configurable data paths, persisted across sessions
- Single-binary distribution — no runtime, no WebView, no Electron

Recent release work in `0.1.2`:
- Renamed the app, config namespace, and built binary to `BitEngine`
- Bumped the crate and release version to `0.1.2`
- Added first-class release artifacts for macOS Apple Silicon, Linux x86_64, and Linux ARM64

---

## Supported platforms

BitEngine supports these release targets:

| Platform | Target | Release artifact |
|---|---|---|
| macOS Apple Silicon | `aarch64-apple-darwin` | `BitEngine-macos-arm64.zip` containing `BitEngine.app` |
| Linux x86_64 | `x86_64-unknown-linux-gnu` | `BitEngine-linux-x64.tar.gz` |
| Linux ARM64 | `aarch64-unknown-linux-gnu` | `BitEngine-linux-arm64.tar.gz` |

macOS Intel/x86_64 and universal macOS app bundles are intentionally not built or supported.

---

## Screenshots

> _Dual terminal view with status indicators and live block height_

```
┌─────────────────────────────────────────────────────────────────────┐
│  BLOCK HEIGHT                                       Update Binaries… │
│  895,234                                                             │
├─────────────────────────────────────────────────────────────────────┤
│  DIRECTORY PATHS                                              [Hide] │
│  Binaries Folder        /path/to/BitEngine/Binaries    [Browse…]  ● │
│  Bitcoin Data Directory /path/to/BitEngine/BitcoinChain[Browse…]  ● │
│  Electrs DB Directory   /path/to/BitEngine/ElectrsDB   [Browse…]  ● │
│                          Changes take effect on next launch [Save]   │
├───────────────────────────────┬─────────────────────────────────────┤
│ Bitcoin              [Launch] │ Electrs              [Launch]        │
│ ● Running  ○ Synced  ○ Ready  │ ● Running  ○ Synced  ○ Ready         │
├───────────────────────────────┼─────────────────────────────────────┤
│ $ bitcoind -datadir=…         │ $ electrs --network bitcoin …        │
│ 2025-01-15T12:00:01Z Loaded   │ [2025-01-15T12:00:05Z INFO ] Opening │
│ 2025-01-15T12:00:02Z Opening  │ [2025-01-15T12:00:06Z INFO ] Indexin │
│ ...                           │ ...                                  │
├─────────────────────────────────────────────────────────────────────┤
│  [Shutdown Bitcoind & Electrs]   [Shutdown Electrs Only]            │
└─────────────────────────────────────────────────────────────────────┘
```

---

## Features

### Dual terminal interface
Each node gets its own scrollable terminal panel showing real-time stdout and stderr. Output is streamed on dedicated OS threads and drained into the UI every 100 ms — the interface never blocks.

### Status indicators
Three per node, updated automatically:

| Indicator | Condition |
|---|---|
| **Running** | Process is alive |
| **Synced** | Bitcoin: `verificationprogress > 99.99%` via RPC · Electrs: key log phrases detected |
| **Ready** | Running AND Synced |

### Live block height
Polls `getblockchaininfo` via JSON-RPC every 5 seconds and displays the current block height with comma formatting (e.g. `895,234`).

### Binaries and updates
Click **Update Binaries** to open BitEngine's native binaries page. Installed versions are detected directly from `bitcoind --version` and `electrs --version` while stable upstream releases load separately. Each row tells you whether the binary is missing, current, or has an update available.

Builds run in the background with clear download, source-authentication, preparation, compilation, binary-verification, installation, and completion stages. Detailed Git/CMake/Cargo output is hidden behind **Build Details** and each durable build log is capped at 64 MiB. Advanced controls allow a specific stable release to be selected without cluttering the normal update flow.

### Graceful shutdown
- **Electrs only**: graceful termination on Unix where available, then kill fallback
- **Bitcoin (and Electrs)**: RPC `stop` command → 60 s wait → platform kill fallback
- Shutdown runs in a background thread so the UI stays responsive

### Configurable paths
All three data directories (Binaries, Bitcoin data, Electrs DB) are editable in the UI and persisted in the platform config directory. Changes take effect on the next node launch.

---

## Default directory layout

BitEngine derives default paths from the directory containing the executable. On macOS `.app` bundles, it walks from `BitEngine.app/Contents/MacOS/` to the bundle's parent directory for compatibility with the original external SSD layout.

```
<root>/
├── BitEngine.app or bitengine executable
├── Binaries/
│   ├── bitcoind
│   ├── bitcoin-cli
│   ├── bitcoin
│   ├── bitcoin-tx
│   ├── bitcoin-util
│   ├── bitcoin-wallet
│   └── electrs
├── BitEngineBuilds/           ← private per-job sources/workspaces and bounded retained logs
├── BitcoinChain/
│   └── bitcoin.conf         ← auto-created with sensible defaults if missing
└── ElectrsDB/
```

You can override this with the `BITENGINE_ROOT` environment variable; the legacy `BITCOIN_NODE_MANAGER_ROOT` name is still accepted for compatibility.

The Bitcoin utility entries above are the complete managed filename family; the exact subset present depends on the selected upstream release and enabled build features.

---

## Build

### Prerequisites

```bash
# Install Rust (skip if already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# Supported release targets
rustup target add aarch64-apple-darwin
rustup target add x86_64-unknown-linux-gnu
rustup target add aarch64-unknown-linux-gnu
```

> **Requires:** Rust 1.88+. macOS releases require Apple Silicon and macOS 12 Monterey or later. Linux builds need the native GUI development libraries used by `iced`/`rfd` (`libx11`, `libxkbcommon`, Wayland/EGL, GTK 3).

### Development build

```bash
cargo build
./target/debug/bitengine
```

The GitHub Actions CI workflow runs:

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo test --workspace --all-features`
- release build checks for macOS Apple Silicon, Linux x86_64, and Linux ARM64

### Release build (optimised, ~5 MB)

```bash
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-unknown-linux-gnu
cargo build --release --target aarch64-unknown-linux-gnu
```

### Bundle as a `.app`

```bash
./build-mac-app.sh
# Output: ./BitEngine.app

open BitEngine.app
```

The script compiles, assembles the `.app` directory structure, writes `Info.plist`, copies the binary, and applies an ad-hoc codesign so Gatekeeper doesn't block local execution.

Tagged `v*` releases are built automatically in GitHub Actions. The macOS artifact is Apple Silicon only; no macOS Intel or universal artifact is produced.

---

## Distribution & codesigning

For distribution outside the App Store you need a **Developer ID Application** certificate from Apple:

```bash
# Sign
codesign --force --deep \
  --sign "Developer ID Application: Your Name (TEAMID)" \
  --options runtime \
  dist/BitEngine.app

# Notarise (requires app-specific password from appleid.apple.com)
xcrun notarytool submit dist/BitEngine.app \
  --apple-id you@example.com \
  --team-id TEAMID \
  --password APP_SPECIFIC_PASSWORD \
  --wait

# Staple the ticket so the app passes Gatekeeper offline
xcrun stapler staple dist/BitEngine.app
```

---

## Configuration

Config is stored at:

```
macOS:   ~/Library/Application Support/BitEngine/config.json
Linux:   $XDG_CONFIG_HOME/BitEngine/config.json or ~/.config/BitEngine/config.json
```

Example:

```json
{
  "binaries_path":     "/path/to/BitEngine/Binaries",
  "bitcoin_data_path": "/path/to/BitEngine/BitcoinChain",
  "electrs_data_path": "/path/to/BitEngine/ElectrsDB"
}
```

If no config exists on first launch, defaults are derived from the SSD root.

### `bitcoin.conf`

If `<bitcoin_data_path>/bitcoin.conf` does not exist, BitEngine creates one automatically:

```ini
# Bitcoin Core — auto-generated by BitEngine
server=1
txindex=1
rpcport=8332
rpcallowip=127.0.0.1
# Cookie-based authentication is active by default.
```

Cookie-based RPC authentication (`.cookie` file) is used by default. BitEngine checks `<datadir>/.cookie` and `<datadir>/mainnet/.cookie` before falling back to `rpcuser`/`rpcpassword` from `bitcoin.conf`.

---

## Binary update system

**Update Binaries** routes to a dedicated BitEngine page and runs the following native flow:

1. Detect installed versions and fetch stable upstream releases
2. Validate build tools and available disk space using the filesystem's native free-space information
3. Clone the selected release into a fresh private per-job source directory
4. Check the Git origin, selected tag/commit, signed tag, and pristine working tree
5. Compile Bitcoin Core with the node-only CMake flags or electrs with Cargo `--locked`
6. Run every discovered managed output and confirm that it reports the requested version
7. Stage the complete binary set inside the configured destination filesystem
8. Durably journal, back up, activate, and commit the complete managed set, rolling back on failure

Bitcoin Core tags are accepted only when Git verifies them through GnuPG and the reported primary fingerprint is in BitEngine's pinned copy of Bitcoin Core's official trusted-key set. Older electrs tags use the maintainer's pinned OpenPGP fingerprint; electrs v0.11.1 uses the maintainer's pinned SSH signing key. A missing verifier, missing key, invalid signature, expired/revoked key, or unexpected signer fails closed before compilation. Maintainer keys must be obtained and their full fingerprints checked using the upstream projects' official verification guidance; BitEngine does not fetch keys or weaken authentication automatically. Unknown future electrs releases are withheld until a reviewed BitEngine signer-policy update covers them.

In-process coordination and cross-process workspace and destination locks prevent overlapping mutation. Builds can be cancelled before installation; BitEngine terminates the complete build process group and escalates to a kill if it does not exit. Installation itself is a short, non-cancellable transaction. A durable `Prepared`/`Committed` journal lets startup recovery either restore the entire old managed set or validate and finish the new set before the destination can be used again. Outputs absent from the new Bitcoin release are transactionally removed from the managed `bitcoind`, `bitcoin-cli`, `bitcoin`, `bitcoin-tx`, `bitcoin-util`, and `bitcoin-wallet` family rather than leaving mixed-version tools behind.

Job stage, result, and log location are persisted to `build-job.json` in BitEngine's config directory. Each log is capped at 64 MiB, only the eight newest job directories/logs are retained, and private source and work trees are removed after the job. Existing binaries are not touched until source, compilation, and every-artifact verification have succeeded.

See [Native binary build architecture](docs/binaries.md) for the module map, safety model, and former BitForge boundary.

---

## Architecture

```
src/
├── main.rs            Entry point
│                      · Cross-platform single-instance lock
│                      · Default root auto-detection from binary path
│                      · Iced application bootstrap
│
├── platform.rs        Platform boundary
│                      · Supported target detection
│                      · Unix executable permissions and termination signal
│
├── config.rs          Persistent configuration
│                      · Serialised as JSON via serde_json
│                      · directories crate handles platform config path resolution
│
├── rpc.rs             Bitcoin JSON-RPC client
│                      · reqwest + rustls (no OpenSSL dependency)
│                      · Cookie-file auth with bitcoin.conf fallback
│                      · Auto-creates bitcoin.conf when missing
│                      · getblockchaininfo polling, stop command
│
├── process_manager.rs Child process lifecycle
│                      · Spawns bitcoind / electrs with stdout+stderr pipes
│                      · Two OS reader threads per process → Arc<Mutex<VecDeque>>
│                      · Platform termination request → 10 s grace period → kill
│
├── binaries/          Native binary build/update service
│   ├── mod.rs         Versions, release discovery, installed detection, shared types
│   ├── environment.rs PATH, pkg-config, Cargo, and LLVM environment setup
│   ├── dependencies.rs Target-specific build requirement checks and guidance
│   ├── process.rs     Cancellable process execution and streamed output
│   ├── install.rs     Transactional multi-binary installation and rollback
│   └── service.rs     Single-job orchestration, persistence, build pipelines
│
├── ui.rs              Iced 0.14 MVU state, routing, and task dispatch
└── ui_render.rs       Dashboard and native binaries-page rendering
                       · App state struct
                       · Message enum (all events)
                       · update() — state transitions + Task dispatch
                       · view()   — pure render (no side effects)
                       · subscription() — 100 ms output timer, 5 s RPC timer
```

### Threading model

```
Main thread (Iced / tokio event loop)
   ├─ OutputTick every 100 ms  → drains node output and native build events
   └─ RpcTick every 5 s        → Task::perform(async getblockchaininfo)
                                      └─ reqwest HTTP → BlockchainInfoReceived

Native build task (at most one)
   ├─ release/version HTTP and local probes
   ├─ git / cmake / cargo child processes
   │    ├─ stdout reader task ─┐
   │    └─ stderr reader task ─┴→ progress UI + durable build log
   └─ verified staging → transactional install or rollback

Per-process background threads (2 per running node)
   ├─ stdout reader  ─┐
   └─ stderr reader  ─┴→ push lines into Arc<Mutex<VecDeque<String>>>
```

The Iced update loop is the only writer to UI state. The background threads only write to the queues. No shared mutable state outside `Arc<Mutex<>>`.

---

## Dependencies

| Crate | Version | Purpose |
|---|---|---|
| `iced` | 0.14 | GUI framework (native rendering, Elm/MVU) |
| `tokio` | 1 | Async runtime (driven by iced's tokio feature) |
| `reqwest` | 0.13 | HTTP client for Bitcoin RPC (rustls, no OpenSSL) |
| `serde` / `serde_json` | 1 | Config and RPC serialisation |
| `anyhow` | 1 | Ergonomic error propagation |
| `thiserror` | 2 | Structured error type definitions |
| `rfd` | 0.17 | Native file/folder picker dialog |
| `directories` | 6 | Platform config and user directory resolution |
| `libc` | 0.2 | Unix `SIGTERM` for graceful process shutdown |
| `iced_runtime` | 0.14 | `Action<T>` type for scroll task mapping |

---

## Comparison with the Python predecessor

| Area | Python (tkinter) | BitEngine (Rust / Iced) |
|---|---|---|
| Language | Interpreted | Native compiled |
| Startup time | ~1–2 s | <100 ms |
| Bundle size | 40+ MB (Python + tkinter) | ~5 MB |
| Threading | GIL limits true parallelism | Real OS threads |
| Terminal memory | Unbounded growth | Hard cap: 5 000 lines per panel |
| UI blocking | `messagebox` blocks event loop | Overlay widget, never blocks |
| Process shutdown | `terminate()` only | RPC stop → platform termination → kill |
| Binary copy safety | `shutil.copy2` (non-atomic) | temp file → executable bit on Unix → atomic rename |
| Semver comparison | Regex + string sort | Tuple comparison `(major, minor, patch)` |
| Electrs sync detection | 3 log patterns | 5 log patterns |
| RPC auth | Cookie + fallback | Same, cleaner error messages |
| Single-instance guard | Unix file lock | localhost listener guard |
| Error handling | `try/except`, silent failures | `Result<T,E>` throughout, no `unwrap()` |
| Type safety | Runtime | Compile-time |

---

## License

MIT — see [LICENSE](LICENSE).

---

## Related projects

- [BitForge](https://github.com/csd113/BitForge) — frozen historical standalone implementation; active build/update code now lives in BitEngine
- [Bitcoin Core](https://github.com/bitcoin/bitcoin)
- [Electrs](https://github.com/romanz/electrs)
