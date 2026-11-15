# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

LightningRouter tethers a Mac to an iPhone's mobile data over USB by relaying a WireGuard (UDP) connection through the phone. Three pieces, in two independent Cargo crates (no workspace) plus an Xcode project:

| Piece | Path | Runs on |
|---|---|---|
| Rust static library, exposed to Swift over a C FFI | `src/` (root crate `lightning-router`) | iPhone, inside the app |
| SwiftUI app | `LightningRouter/`, `LightningRouter.xcodeproj` | iPhone |
| Relay binary | `relay/` (crate `lightning-router-relay`) | Mac |

## Data path

```
WireGuard client ──UDP──> relay (Mac, 0.0.0.0:3400)
                              │  TCP via usbmuxd to device port 51820
                              ▼
                          app's Rust lib (TCP listener 0.0.0.0:51820)
                              │  UDP
                              ▼
                          WireGuard endpoint (IP/port typed into the app)
```

UDP datagrams ride the usbmuxd TCP stream with a **frame**: a `u16` little-endian length, then the datagram. Both directions use the same frame, and a zero-length frame means "connection closed". The frame is implemented twice, in `src/lib.rs` and `relay/src/main.rs`; change both together.

The ports (3400 on the Mac, 51820 on the device) are hardcoded in those same two files.

## Commands

```sh
# Compile-check on the host (fast; no iOS toolchain needed)
cargo check                      # root crate
cd relay && cargo check          # relay

# Build the xcframework the app links against (run from the repo root)
make CARGO="$(which cargo)"

# Run the Mac relay (needs the app running with the tunnel enabled)
cd relay && cargo run --release
```

There are no tests and no lint configuration.

### Building the app

1. `make` builds the root crate for five targets and packages `LightningRouterRS.xcframework` at the repo root. Install them first: `rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios aarch64-apple-darwin x86_64-apple-darwin`.
2. Build and install the app from Xcode.

Things the build files do not say plainly:

- The Makefile hardcodes `CARGO := $(HOME)/.cargo/bin/cargo` for the iOS targets. Pass `CARGO=` when cargo lives elsewhere (e.g. a Homebrew rustup).
- The Makefile header says Xcode runs it, but the Xcode project has no script build phase. Run `make` by hand after every change to `src/`; Xcode links whatever xcframework is already on disk.
- `lightning_router_rs.h` is generated at the repo root by `build.rs` (cbindgen) on every cargo build of the root crate. The header, the xcframework and `bundle.zip` are gitignored build outputs.
- The README's `cd LightningRouter && make` means the repo root, not the `LightningRouter/` app folder.
- Signing: `LightningRouter/Build.xcconfig` holds the upstream team ID and bundle ID and optionally includes a gitignored `LightningRouter/Signing.xcconfig`. Put local `DEVELOPMENT_TEAM` / `PRODUCT_BUNDLE_IDENTIFIER` overrides there.
- `LightningRouter/` is a file-system-synchronized group: a file added to that folder joins the target without a `project.pbxproj` edit.

## FFI surface (`src/lib.rs`)

Swift imports the library as the module `LightningRouterRS` (`module.modulemap`). Two functions:

- `start_simple_udp_proxy(address, port, &threadKiller) -> bool` binds the TCP listener synchronously (returns `false` if the bind fails), then spawns the accept loop on a process-global Tokio runtime and hands back an opaque `ThreadKiller`.
- `thread_killer_kill(killer)` signals the accept loop and frees the `ThreadKiller`; each pointer is killed once.

Shutdown is polled, not immediate: the accept loop checks the kill signal between `select!` wake-ups, at most 5 s apart, and holds port 51820 until it exits. Per-connection relay tasks are spawned separately and end on their own socket errors, not on the kill signal.

Any signature change here needs a `make` rebuild so the regenerated header reaches the xcframework, plus the matching call-site change in `LightningRouter/ContentView.swift`.

## App (`LightningRouter/`)

`ContentView.swift` is the whole UI: the "Enable Tunnel" toggle calls the two FFI functions and holds the `ThreadKiller` pointer in view state.

iOS suspends a backgrounded app, which would stall the tunnel, so `BackgroundHelper/` (adapted from iTorrent) keeps the app alive. `BackgroundService` is a facade over two strategies chosen in the UI: `AudioBackgroundService` loops the bundled `silence.mp3`, and `LocationBackgroundService` runs background location updates. The matching `UIBackgroundModes` and location usage strings live in `LightningRouter/Info.plist`.

## Relay (`relay/src/main.rs`)

Picks the first USB device from usbmuxd, falling back to a network device with a warning. It reconnects in a loop after the app drops the connection, but it panics if the app is not listening when it tries to connect, so start the tunnel in the app first.

## Planned work

`src/tcp/packets.rs` is entirely commented out: an IP/TCP packet parser copied from the `idevice` crate, kept as the starting point for the userspace TCP stack / NAT the README describes. The crate compiles as if the `tcp` module were empty.
