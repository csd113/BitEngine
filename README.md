<div align="center">

# ⚙️ BitEngine

**A native macOS GUI for managing your Bitcoin Core and Electrs nodes on an external SSD**

Built with Rust · Iced · Metal-accelerated · Apple Silicon native

[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange?logo=rust)](https://www.rust-lang.org/)
[![CI](https://github.com/csd113/BitEngine/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/csd113/BitEngine/actions/workflows/ci.yml)
[![Platform](https://img.shields.io/badge/platform-macOS%2012%2B-blue?logo=apple)](https://www.apple.com/macos/)
[![Architecture](https://img.shields.io/badge/arch-arm64%20%7C%20x86__64-lightgrey)](#build)
[![License](https://img.shields.io/badge/license-MIT-green)](#license)

</div>

---

## What is BitEngine?

BitEngine is a macOS desktop application that lets you launch, monitor, and shut down a self-hosted **Bitcoin Core** (`bitcoind`) and **Electrs** indexer node — both stored on an external SSD — without touching the terminal.

- Dual side-by-side terminal panels with live log streaming
- Real-time block height display via JSON-RPC
- Green/grey status indicators: **Running · Synced · Ready** for each node
- One-click graceful shutdown (RPC stop → SIGTERM → SIGKILL)
- Binary updater: scans `~/Downloads/bitcoin_builds/` and atomically replaces binaries
- Fully configurable data paths, persisted across sessions
- Single-binary distribution — no runtime, no WebView, no Electron

---

## Screenshots

> _Dual terminal view with status indicators and live block height_

```
┌─────────────────────────────────────────────────────────────────────┐
│  BLOCK HEIGHT                                       Update Binaries… │
│  895,234                                                             │
├─────────────────────────────────────────────────────────────────────┤
│  DIRECTORY PATHS                                              [Hide] │
│  Binaries Folder        /Volumes/SSD/Binaries          [Browse…]  ● │
│  Bitcoin Data Directory /Volumes/SSD/BitcoinChain      [Browse…]  ● │
│  Electrs DB Directory   /Volumes/SSD/ElectrsDB         [Browse…]  ● │
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

### Binary updater
Click **Update Binaries…** to scan `~/Downloads/bitcoin_builds/binaries/` for versioned folders (`bitcoin-27.0`, `electrs-0.10.5`), pick the highest semantic version, and atomically replace binaries in your SSD `Binaries/` folder.

If `bitcoin_builds` is not found, BitEngine checks for **BitForge.app** in `/Applications` and offers to open it, or shows the download link.

### Graceful shutdown
- **Electrs only**: SIGTERM → 10 s wait → SIGKILL
- **Bitcoin (and Electrs)**: RPC `stop` command → 60 s wait → SIGKILL fallback
- Shutdown runs in a background thread so the UI stays responsive

### Configurable paths
All three data directories (Binaries, Bitcoin data, Electrs DB) are editable in the UI and persisted to `~/Library/Application Support/BitcoinNodeManager/config.json`. Changes take effect on the next node launch.

---

## SSD directory layout

BitEngine expects this structure on your external SSD:

```
<SSD root>/
├── BitEngine.app            ← this application
├── Binaries/
│   ├── bitcoind
│   ├── bitcoin-cli
│   ├── bitcoin-tx
│   ├── bitcoin-util
│   └── electrs
├── BitcoinChain/
│   └── bitcoin.conf         ← auto-created with sensible defaults if missing
└── ElectrsDB/
```

The SSD root is **auto-detected** from the binary's location. When running as a `.app` bundle the binary lives at `Contents/MacOS/`, so BitEngine walks up three directories to find the SSD root. You can override this with the `BITCOIN_NODE_MANAGER_ROOT` environment variable.

---

## Build

### Prerequisites

```bash
# Install Rust (skip if already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# Apple Silicon target (already present on arm64 Macs — add to be sure)
rustup target add aarch64-apple-darwin

# Intel Mac target
rustup target add x86_64-apple-darwin
```

> **Requires:** Rust 1.75+, macOS 12 Monterey or later, Xcode Command Line Tools (`xcode-select --install`)

### Development build

```bash
cargo build
./target/debug/bitcoin_node_manager
```

### Release build (optimised, ~5 MB)

```bash
# Apple Silicon
cargo build --release --target aarch64-apple-darwin

# Intel
cargo build --release --target x86_64-apple-darwin
```

### Bundle as a `.app`

```bash
./build_bundle.sh
# Output: ./dist/BitEngine.app

open dist/BitEngine.app
```

The script compiles, assembles the `.app` directory structure, writes `Info.plist`, copies the binary, and applies an ad-hoc codesign so Gatekeeper doesn't block local execution.

#### Universal binary (arm64 + x86_64)

```bash
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin

lipo -create \
  target/aarch64-apple-darwin/release/bitcoin_node_manager \
  target/x86_64-apple-darwin/release/bitcoin_node_manager \
  -output dist/BitEngine.app/Contents/MacOS/BitEngine

codesign --force --deep --sign "-" dist/BitEngine.app
```

Tagged `v*` releases are built automatically in GitHub Actions, packaged as a zipped universal `.app`, and attached to the GitHub Release.

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
~/Library/Application Support/BitcoinNodeManager/config.json
```

Example:

```json
{
  "binaries_path":     "/Volumes/SSD/Binaries",
  "bitcoin_data_path": "/Volumes/SSD/BitcoinChain",
  "electrs_data_path": "/Volumes/SSD/ElectrsDB"
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

**Update Binaries…** (toolbar button) runs the following flow:

1. Check `~/Downloads/bitcoin_builds/binaries/`
2. Scan for folders matching `bitcoin-X.Y.Z` and `electrs-X.Y.Z`
3. Pick the highest semantic version for each (major.minor.patch tuple comparison)
4. Copy binaries into the configured `Binaries/` folder:
   - Written to a `.tmp` file first
   - `chmod 755` applied
   - Atomically renamed to the final path — a running binary is never half-replaced
5. Report what was updated in an overlay dialog

If `bitcoin_builds` is not found:

| Condition | Behaviour |
|---|---|
| `/Applications/BitForge.app` exists | Offers to open BitForge |
| BitForge not found | Shows link to [BitForge on GitHub](https://github.com/csd113/BitForge-Python) |

---

## Architecture

```
src/
├── main.rs            Entry point
│                      · Single-instance lock (fcntl LOCK_EX | LOCK_NB)
│                      · SSD root auto-detection from binary path
│                      · Iced application bootstrap
│
├── config.rs          Persistent configuration
│                      · Serialised as JSON via serde_json
│                      · Stored in ~/Library/Application Support (macOS)
│                      · directories crate handles platform path resolution
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
│                      · SIGTERM → 10 s grace period → SIGKILL
│                      · Electrs sync-line detection (5 log patterns)
│
├── updater.rs         Binary update system
│                      · Semver folder scanning (tuple comparison, no regex)
│                      · Atomic copy: temp file → chmod 755 → rename
│                      · BitForge.app detection and fallback link
│
└── ui.rs              Iced 0.13 MVU application
                       · App state struct
                       · Message enum (all events)
                       · update() — state transitions + Task dispatch
                       · view()   — pure render (no side effects)
                       · subscription() — 100 ms output timer, 5 s RPC timer
```

### Threading model

```
Main thread (Iced / tokio event loop)
   ├─ OutputTick every 100 ms  → drains both output queues into terminal buffers
   └─ RpcTick every 5 s        → Task::perform(async getblockchaininfo)
                                      └─ reqwest HTTP → BlockchainInfoReceived

Per-process background threads (2 per running node)
   ├─ stdout reader  ─┐
   └─ stderr reader  ─┴→ push lines into Arc<Mutex<VecDeque<String>>>
```

The Iced update loop is the only writer to UI state. The background threads only write to the queues. No shared mutable state outside `Arc<Mutex<>>`.

---

## Dependencies

| Crate | Version | Purpose |
|---|---|---|
| `iced` | 0.13 | GUI framework (Metal-accelerated, Elm/MVU) |
| `tokio` | 1 | Async runtime (driven by iced's tokio feature) |
| `reqwest` | 0.12 | HTTP client for Bitcoin RPC (rustls, no OpenSSL) |
| `serde` / `serde_json` | 1 | Config and RPC serialisation |
| `anyhow` | 1 | Ergonomic error propagation |
| `thiserror` | 1 | Structured error type definitions |
| `rfd` | 0.15 | Native macOS file/folder picker dialog |
| `directories` | 5 | XDG / macOS Application Support path resolution |
| `libc` | 0.2 | `flock()` for single-instance guard, `SIGTERM` |
| `iced_runtime` | 0.13 | `Action<T>` type for scroll task mapping |

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
| Process shutdown | `terminate()` only | RPC stop → SIGTERM → SIGKILL |
| Binary copy safety | `shutil.copy2` (non-atomic) | temp file → chmod → atomic rename |
| Semver comparison | Regex + string sort | Tuple comparison `(major, minor, patch)` |
| Electrs sync detection | 3 log patterns | 5 log patterns |
| RPC auth | Cookie + fallback | Same, cleaner error messages |
| Single-instance guard | `fcntl.flock` | `libc::flock` (no GIL risk) |
| Error handling | `try/except`, silent failures | `Result<T,E>` throughout, no `unwrap()` |
| Type safety | Runtime | Compile-time |

---

## License

MIT — see [LICENSE](LICENSE).

---

## Related projects

- [BitForge](https://github.com/csd113/BitForge-Rust) — builds Bitcoin Core and Electrs binaries for use with BitEngine
- [Bitcoin Core](https://github.com/bitcoin/bitcoin)
- [Electrs](https://github.com/romanz/electrs)
