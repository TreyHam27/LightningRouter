# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

LightningRouter tethers a Mac to an iPhone's mobile data over USB by relaying a WireGuard (UDP) connection through the phone. Three independent Cargo crates (no workspace, separate lockfiles) plus an Xcode project:

| Piece | Path | Runs on |
|---|---|---|
| Frame codec and port constants, shared by both ends | `frame/` (crate `lightning-router-frame`) | both |
| Rust static library, exposed to Swift over a C FFI | `src/` (root crate `lightning-router`) | iPhone, inside the app |
| SwiftUI app | `LightningRouter/`, `LightningRouter.xcodeproj` | iPhone |
| Relay binary | `relay/` (crate `lightning-router-relay`) | Mac |

## Data path

```
WireGuard client ──UDP──> relay (Mac, 0.0.0.0:3400)
                              │  TCP via usbmuxd to device port 51820
                              ▼
                          app's Rust lib (TCP listener, all interfaces, port 51820)
                              │  UDP
                              ▼
                          WireGuard endpoint (host/port typed into the app)
```

UDP datagrams ride the usbmuxd TCP stream as **frames**: a `u16` little-endian length, then the datagram. A zero-length frame, or the stream ending on a frame boundary, means the peer is done. `frame/src/lib.rs` is the single source of truth for the frame and for both port numbers.

Both ends run the same shape: split the stream, then run one loop per direction under a `select!` (`session` in `src/lib.rs`, `bridge` in `relay/src/main.rs`). `FrameReader::next` and `FrameWriter::send` are not cancel-safe, so keep each inside its own loop; a `select!` branch that reads or writes a frame directly would desynchronise the stream when the other branch wins.

## Commands

```sh
make test                          # every crate's tests, on the host
cargo test relays_datagrams        # one test by name (run in the crate's directory)
cargo clippy --all-targets         # per crate; all three are warning-free

make targets                       # once: rustup target add for the five targets
make                               # build LightningRouterRS.xcframework + bundle.zip

cd relay && cargo run --release    # run the Mac relay
```

Tests use real loopback sockets and need no device. The root crate is a `staticlib`, so its tests are unit tests inside `src/lib.rs`; a `tests/` directory could not link against it.

### Building the app

1. `make` (from the repo root) builds the root crate for five targets and packages `LightningRouterRS.xcframework` at the repo root.
2. Build and install the app from Xcode.

Things the build files do not say plainly:

- The Xcode project has no script build phase. Run `make` by hand after every change to `src/` or `frame/`; Xcode links whatever xcframework is already on disk.
- `make CARGO=/path/to/cargo` overrides the cargo binary.
- `lightning_router_rs.h` is generated at the repo root by `build.rs` (cbindgen) on every cargo build of the root crate. The header, the xcframework and `bundle.zip` are gitignored build outputs.
- Signing: `LightningRouter/Build.xcconfig` holds the upstream team ID and bundle ID and optionally includes a gitignored `LightningRouter/Signing.xcconfig`. Put local `DEVELOPMENT_TEAM` / `PRODUCT_BUNDLE_IDENTIFIER` overrides there.
- `LightningRouter/` is a file-system-synchronized group: a file added to that folder joins the target without a `project.pbxproj` edit.
- To compile-check the Swift without signing or a device:
  `xcodebuild -project LightningRouter.xcodeproj -target LightningRouter -sdk iphonesimulator SYMROOT=<tmp> OBJROOT=<tmp> CODE_SIGNING_ALLOWED=NO build`. Keep `SYMROOT`/`OBJROOT` outside the repo, because the default `build/` is the Makefile's staging directory.

## FFI surface (`src/lib.rs`)

Swift imports the library as the module `LightningRouterRS` (`module.modulemap`). Three functions around an opaque `ProxyHandle`:

- `proxy_start(address, port, &proxy) -> bool` binds the listener synchronously (false if the bind fails) and spawns the accept loop on a process-global Tokio runtime. `address` is an IP or a hostname, resolved again for each relay connection.
- `proxy_stats(proxy) -> ProxyStats` reads atomics: relays connected now, bytes up, bytes down.
- `proxy_stop(proxy)` signals the accept loop, blocks until it has exited, and frees the handle. The port is free when it returns, and dropping the loop's `JoinSet` aborts every session. It blocks on the runtime, so call it from outside the runtime (tests are plain `#[test]`, not `#[tokio::test]`, for this reason).

A signature change here needs a `make` rebuild so the regenerated header reaches the xcframework, plus the matching call site in `LightningRouter/Tunnel.swift`.

## App (`LightningRouter/`)

`Tunnel.swift` is the only caller of the FFI: a main-actor `@Observable` model that owns the `ProxyHandle`, polls `proxy_stats` once a second for the status line, and starts and stops the background service with the proxy. `ContentView.swift` is the view over it.

iOS suspends a backgrounded app, which would stall the tunnel, so `BackgroundHelper/` (adapted from iTorrent) keeps the app alive. `BackgroundService` is a main-actor facade over two strategies chosen in the UI: `AudioBackgroundService` loops the bundled `silence.mp3`, and `LocationBackgroundService` runs background location updates. The facade tracks `isRunning` itself, because the audio strategy's own `isRunning` is false for a moment after `start()`. The matching `UIBackgroundModes` and location usage strings live in `LightningRouter/Info.plist`.

## Relay (`relay/src/main.rs`)

Binds UDP 3400 once, then loops: list usbmuxd devices (USB preferred, network as fallback), connect to the app, bridge until the link drops, retry. Devices are listed on every attempt because usbmuxd gives a replugged device a new ID. Replies from the app go to whichever address last sent a datagram.

## Not yet built

The README's long-term vision is a userspace TCP stack on the device acting as a NAT, replacing the WireGuard dependency. Nothing in the tree implements it; the commented-out packet parser that used to sit in `src/tcp/` was removed and is in git history (last present in commit `2784618`).
