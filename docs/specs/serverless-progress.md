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

## Tracks (merged into `feat/serverless`, step 9m done)

- app (step 11 except C2): `Tunnel.Start`, mode picker, status text.
- core (7a): `relay/src/system/setup.rs`, `stub.rs`, `examples/system_spike.rs`; 7b not run. Its runbook (M1, M2, M3, M7, M8 and the fallbacks per failure) is in git history at `feat/serverless-core`: `docs/specs/serverless-track-core.md`.
- net (8, 9): `fakedns.rs`, `stack.rs`; deps `ipstack`, `tun-rs`, `system-configuration`.
- Merged state: `make test` all green (21 + 16 + 45 relay), clippy clean in all three crates, `make` ok.
- Open item from the app track: C1 (simulator run) not done; C2 (device: picker persists, disabled while on, WireGuard still works) needs a device.

## Next step

Tracks are merged. In order:

1. Step 9m (merge tracks) done.
2. Step 6 (stage A on a device) done; results below.
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

- Step 6: stage A on an iPhone 14 over USB, run 2026-11-15. Signing: the user's own team via the gitignored `LightningRouter/Signing.xcconfig` (bundle ID `com.treyhamilton.LightningRouter`). `relay/examples/flood.rs` and a README section added.
  - D-A1 passed, run differently from the spec: the Mac's Wi-Fi stayed on, since turning it off would cut the agent's own connection. `curl --socks5-hostname 127.0.0.1:1080 https://ifconfig.me` returned 146.70.202.190 (AS9009 M247, a hosting provider, so the phone may have a VPN or Private Relay on, unchecked); a direct curl returned 73.233.211.106 (Comcast). The addresses differ, so traffic leaves through the phone. Safari was not tried.
  - D-A2: the D1 gate **fails as written**. One TCP flow opens in about 65 ms; a ping flow (usbmuxd connect plus phone accept only) in 1.7 ms (8 flows: 3.4 ms p50; 32: 10 ms p50). TCP flows opened at once stall in about 1 s steps (1, 3, 5, 7 s, like connect retries): 8 flows p50 1.07 s, 16 p50 1.1 s, 256 gave 75 to 86 successes with p50 1.4 to 3.3 s and p99 about 7.2 s (statuses Failed and TimedOut). The stall is the same for 1.1.1.1:443, 8.8.8.8:53 and example.com:80, so it comes from the phone's outbound connects, not usbmuxd, and yamux would probably not fix it. 256 ping flows: 232 succeeded, the rest Failed. Decision for the user: spec says insert step 6b (`tokio-yamux`); my recommendation is to find the phone-side connect stall first (cellular path, the 1 s steps) and revisit yamux after.
  - D-A3 passed: every USB flow reached the phone flow server from 127.0.0.1 (11 flows seen), so D9 holds. The temporary log is removed; the phone was rebuilt and reinstalled clean.
  - D-A4 passed: after an unplug and replug the relay logged `Waiting: no device is connected`, then `Connected to the app`, same process, and SOCKS curls returned 200 straight away. Recovery time was not timed exactly (the log has no timestamps); the relay polls every second.
- Step 5: `relay/src/socks.rs`, hand-written RFC 1928 (no new crate): CONNECT, UDP ASSOCIATE, `--socks [ADDR]`; `Addr` gained `Hash`; 18 relay tests. The idle-timeout test uses a 300 ms real timeout, not `time::pause` (paused time would skip socket waits).
- Step 4: relay split into `device.rs`, `wireguard.rs`, `flow.rs`; 7 relay tests, `make test` and clippy clean.
- Step 3: phone flow server (`src/flow.rs`, `Service`, `flow_start`, rlib), 15 tests, `make` ok.
- Step 2: shared flow codec (`frame/src/flow.rs`, `FLOW_PORT`), 8 tests.
- Step 1: spec written (`serverless-spec.md`). Details in `serverless-log.md`.
