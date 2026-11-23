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
- All work through step 10 is merged into `master` and pushed to `origin`,
  the user's fork (TreyHam27/LightningRouter). `upstream` is the original
  author's repo (jkcoxson), read-only: open no PR there without asking.
  New work branches from `origin/master` in a fresh worktree.
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
missed: one usbmuxd connection per flow (D1, gate passed in step 6a, at most 32
usbmuxd connects in flight); fake-IP DNS on the Mac, names resolved on the phone at connect
(D4); no tun2proxy or socks5-impl because of GPL (D5); root work happens
before the runtime starts, then privileges drop, and teardown is the utun
fd plus the SCDynamicStore session closing (D8, confirmed in 7b, no fallback);
stage B is IPv4-only, and the IPv6 key stays to stop IPv6 leaking (D6, M8); the flow server binds
loopback only (D9).

## Tracks (merged into `master` via step 9m done)

- app (step 11 except C2): `Tunnel.Start`, mode picker, status text.
- core (7a, 7b): `relay/src/system/setup.rs`, `stub.rs`, `examples/system_spike.rs`. The original runbook is in git history at commit `8c1016d`: `docs/specs/serverless-track-core.md`.
- net (8, 9): `fakedns.rs`, `stack.rs`; deps `ipstack`, `tun-rs`, `system-configuration`.
- Merged state: `make test` all green (21 + 16 + 45 relay), clippy clean in all three crates, `make` ok.
- Open item from the app track: C1 (simulator run) not done; C2 (device: picker persists, disabled while on, WireGuard still works) needs a device.

## Next step

Tracks are merged. In order:

1. Step 9m (merge tracks) done.
2. Steps 6 and 6a (stage A on a device, D1 gate) done; the gate passes.
3. Step 7b (privileged spike runs) done: D8 confirmed, IPv4-only.
4. **Step 10: wire `--system` end to end, then M1 to M8 on a device. Opus
   medium. Needs the user and the phone over USB**, with every other
   thread on this Mac idle. Then C2 on a device, then steps 12 and 13.

### How privileged runs work (rules for step 10)

- **The user types the sudo password into their own Terminal.** They declined
  `request_secret` for it. So the agent writes a wrapper script, and the user
  runs it. It starts with `sudo -v`, and after that uses `sudo -n`.
- **Every run is time-boxed and ends by itself.** While the relay holds the
  default route, this Mac's internet goes through the phone, or nowhere if
  the stack is broken. So the script runs its checks locally, writes them to
  a results file, has a watchdog that `kill -9`s the relay after a fixed
  time, and has an EXIT trap that removes any `LightningRouter-*` keys left
  behind. The agent reads the file once the network is back.
- **Do the shortest teardown run first,** and diff it against a baseline
  snapshot (`netstat -rn` without W routes, `scutil --dns`, `ifconfig -l`).
- **Start the relay in the background through** `perl -e
  '$SIG{INT}="DEFAULT"; exec @ARGV' sudo -n …`. A script's `&` jobs ignore
  SIGINT otherwise.
- **For M1, check a system-resolver lookup** (`dscacheutil -q host`), not only
  `dig @198.18.0.2`. In 7b, mDNSResponder kept using the old servers for a
  few seconds; the real stack answers port 53 at any address, which should
  cover that.
- **Manual recovery:** `sudo pkill -9 -f <binary>`; if `scutil <<< 'list
  State:/Network/Service/LightningRouter.*'` lists keys, run `sudo scutil`
  and `remove <key>` for each; or reboot.

## Commands

```sh
make test          # all crates
make               # xcframework (after changing src/ or frame/)
cd relay && cargo run --release
```

## Finished steps

- Step 7b: M1, M2, M3, M7 pass with the stub stack; temporary keys go on SIGKILL and `OverridePrimary` works for a State-only service, so D8 needs no fallback. M8: IPv6 is neither captured nor leaked, so stage B is IPv4-only and keeps the IPv6 key. Details in the log.
- Step 6a: the D1 gate **passes** under reworded criteria (D1 in the spec): ping flows p50 about 2.5 ms, and 256 at once all succeed. The step 6 stalls came from the phone's VPN (1 s steps) and a carrier cap of about 318 TCP connections; a burst also overflowed macOS usbmuxd's accept queue, fixed by `USBMUXD_CONNECTS` (32 in flight) in `relay/src/device.rs`. No yamux (6b dropped). Numbers in the log.
- Step 6: stage A on an iPhone 14 over USB: D-A1, D-A3, D-A4 passed; D-A2 handed to 6a. Details in the log.
- Step 5: `relay/src/socks.rs`, hand-written RFC 1928 (no new crate): CONNECT, UDP ASSOCIATE, `--socks [ADDR]`; `Addr` gained `Hash`; 18 relay tests. The idle-timeout test uses a 300 ms real timeout, not `time::pause` (paused time would skip socket waits).
- Step 4: relay split into `device.rs`, `wireguard.rs`, `flow.rs`; 7 relay tests, `make test` and clippy clean.
- Step 3: phone flow server (`src/flow.rs`, `Service`, `flow_start`, rlib), 15 tests, `make` ok.
- Step 2: shared flow codec (`frame/src/flow.rs`, `FLOW_PORT`), 8 tests.
- Step 1: spec written (`serverless-spec.md`). Details in `serverless-log.md`.
