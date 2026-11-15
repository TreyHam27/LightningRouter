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
- Steps 2 to 4 done: flow codec, phone flow server, relay restructure.
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

## Next step

**Step 5: SOCKS5 server on the relay (`socks.rs`, `--socks [ADDR]`). Model: see the spec's step table.**

Read the spec's "Stage A" section and step 5 row. Hooks already in place: `flow::open_flow`, `splice`, `pump_udp`, `device::Cache::watch` (spawn it in `--socks` mode), and the CLI match in `relay/src/main.rs`.

## Commands

```sh
make test          # all crates
make               # xcframework (after changing src/ or frame/)
cd relay && cargo run --release
```

## Finished steps

- Step 4: relay split into `device.rs`, `wireguard.rs`, `flow.rs`; 7 relay tests, `make test` and clippy clean.
- Step 3: phone flow server (`src/flow.rs`, `Service`, `flow_start`, rlib), 15 tests, `make` ok.
- Step 2: shared flow codec (`frame/src/flow.rs`, `FLOW_PORT`), 8 tests.
- Step 1: spec written (`serverless-spec.md`). Details in `serverless-log.md`.
