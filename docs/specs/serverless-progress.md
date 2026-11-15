# Serverless mode: progress

Goal: tether the Mac through the phone with only the Mac and the phone. Today
the phone forwards WireGuard UDP to an endpoint the user must already have
(a VPN provider or their own server). Serverless mode removes that
requirement: the phone opens the Mac's connections itself.

Read `CLAUDE.md` first for the current architecture and commands.

## Current state

- Step 1 (spec) done: `docs/specs/serverless-spec.md` holds the design
  (D1 to D12), the flow-port wire protocol, every test case by name, and
  the build steps 2 to 13 with their models. Read it before any build step.
- Steps 2 to 5 done: flow codec, phone flow server, relay restructure, SOCKS5.
- Branch `feat/serverless`, based on local `improve/proxy-core` (commit
  `634cf9e`), which is unpublished. `origin` is the upstream author's repo,
  not a fork the user owns: push nothing and open no PR without asking.
- Baseline is green: `make test` (16 tests), `make`, and the simulator
  compile-check in `CLAUDE.md` all pass.

## Decisions in force

1. **The TCP/IP stack runs on the Mac, not the phone.** An iOS app cannot
   send raw IP packets, so flows must be re-opened with ordinary sockets
   somewhere. The phone side stays a thin socket opener ("connect to
   host:port", "send this datagram to host:port"): less code, less battery,
   and testable on the Mac. This replaces the README's sketch of a
   userspace stack on the device.
2. **WireGuard relay mode keeps working.** Serverless is a second mode, not
   a replacement. The existing frame format, ports and tests stay valid.
3. **Names resolve on the phone**, so lookups use the cellular connection.
4. **Reuse before writing.** Prefer maintained crates for the IP stack and
   for any standard protocol over hand-rolled parsing. Check each crate's
   current docs and release date before relying on it; do not trust
   remembered APIs.
5. **The relay must leave the Mac as it found it.** Anything that changes
   routes, DNS or interfaces is restored on exit, including on Ctrl-C and
   on a crash where that is achievable. This is the risky core: Opus owns it.

Decisions 6 onwards are D1 to D12 in the spec. The ones most easily
missed: one usbmuxd connection per flow (D1, gated by a measurement in
step 6); fake-IP DNS on the Mac, names resolved on the phone at connect
(D4); no tun2proxy or socks5-impl because of GPL (D5); root work happens
before the runtime starts, then privileges drop, and teardown is the utun
fd plus the SCDynamicStore session closing (D8); the flow server binds
loopback only (D9).

## Parallel tracks

Steps that do not depend on each other run in parallel, each on its own
branch and worktree, branched from `feat/serverless` at `9657647`. A track
touches only its own files and leaves this file and `CLAUDE.md` alone; its
status file lives on its own branch and says when the track is done.

| Track | Branch | Steps | Status file |
|---|---|---|---|
| net | `feat/serverless-net` | 8 (fakedns, Sonnet), then 9 (stack, Opus medium) | `docs/specs/serverless-track-net.md` |
| core | `feat/serverless-core` | 7a: `system/setup.rs`, its unit tests, the DNS-only stub stack; no privileged runs (Opus high) | `docs/specs/serverless-track-core.md` |
| app | `feat/serverless-app` | 11 except C2 (Sonnet) | `docs/specs/serverless-track-app.md` |

Steps 7a, 8, 9 and 11 are never handed off from the main line.

## Next step

The main line (this worktree) is idle until tracks finish. In order:

1. **Step 9m: merge finished tracks into `feat/serverless`. Model: Sonnet
   medium.** Merge a track once its status file says it is done; the app
   track can be merged alone, ahead of the others, to unblock step 6.
   Expect small conflicts in `relay/Cargo.toml`, `relay/Cargo.lock`
   (regenerate it), `relay/src/main.rs` and `relay/src/system/mod.rs`. Fold
   each merged status file into this file and `CLAUDE.md`, delete it, then
   run `make test` and clippy in each crate.
2. **Step 6: stage A on a device. Needs the user.** Needs the app track
   merged first, because the app cannot start flow mode until step 11 is
   in. The user must: run `make`, build and install the app from Xcode,
   start it in Serverless mode, turn Wi-Fi off on the Mac, run
   `cd relay && cargo run --release -- --socks`, then do D-A1 to D-A4 from
   the spec and report results. After that a Sonnet thread finishes
   `examples/flood.rs`, the D1 gate decision and the README section.
3. **Step 7b: the privileged runs M1, M2, M3, M7 and M8, from the core
   track's runbook. Model: Opus high. Needs the user** for sudo, and needs
   every other thread on this Mac idle, because the runs change its routes
   and DNS.
4. Step 10, then C2 on a device, then steps 12 and 13, as in the spec.

## Commands

```sh
make test          # all crates
make               # xcframework (after changing src/ or frame/)
cd relay && cargo run --release
```

## Finished steps

- Step 5: `relay/src/socks.rs`, hand-written RFC 1928 (no new crate): CONNECT, UDP ASSOCIATE, `--socks [ADDR]`; `Addr` gained `Hash`; 18 relay tests. The idle-timeout test uses a 300 ms real timeout, not `time::pause` (paused time would skip socket waits).
- Step 4: relay split into `device.rs`, `wireguard.rs`, `flow.rs`; 7 relay tests, `make test` and clippy clean.
- Step 3: phone flow server (`src/flow.rs`, `Service`, `flow_start`, rlib), 15 tests, `make` ok.
- Step 2: shared flow codec (`frame/src/flow.rs`, `FLOW_PORT`), 8 tests.
- Step 1: spec written (`serverless-spec.md`). Details in `serverless-log.md`.
