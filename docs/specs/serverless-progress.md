# Serverless mode: progress

Goal: tether the Mac through the phone with only the Mac and the phone. Today
the phone forwards WireGuard UDP to an endpoint the user must already have
(a VPN provider or their own server). Serverless mode removes that
requirement: the phone opens the Mac's connections itself.

Read `CLAUDE.md` first for the current architecture and commands.

## Current state

- Step 1 (spec) not started. No serverless code exists.
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

## Proposed staging (step 1 confirms or changes it)

- **Stage A, no root:** the phone exposes a per-flow proxy (SOCKS5 is the
  obvious candidate) over usbmuxd; the relay publishes it on a localhost
  port. Usable at once from a browser or the macOS SOCKS setting, and it is
  the phone-side foundation for stage B.
- **Stage B, whole system:** the relay creates a `utun` interface (needs
  root), runs a userspace stack, and turns captured TCP and UDP flows into
  stage A requests. Adds route and DNS setup and teardown.
- **Stage C, app:** mode picker and status in the SwiftUI app.

Open questions for the spec: one usbmuxd connection per flow versus one
multiplexed stream; how UDP flows (DNS, QUIC) are carried and timed out;
which stack crate; how root is obtained and scoped; what happens to
IPv6 and to ICMP; how each stage is tested without a device (the iOS
simulator shares the Mac's network stack, so the app's listener is
reachable on loopback there).

## Next step

**Step 1: write the spec. Model: Opus. No product code.**

Produce `docs/specs/serverless-spec.md`:

- the design for each stage, answering the open questions above, with the
  reason for each choice;
- the wire protocol between relay and phone, exact enough to implement;
- the test cases for each stage, written out by name and intent;
- a step list for the build, each step one area, about 40 tool calls, with
  its model per the user's table (risky core: Opus; wiring, tests from
  listed cases, UI that copies the existing pattern: Sonnet).

Done when: every open question has a decision, every stage has named test
cases, and this file's "Next step" names build step 2. Then commit and hand
off as the user's global instructions describe.

## Commands

```sh
make test          # all crates
make               # xcframework (after changing src/ or frame/)
cd relay && cargo run --release
```

## Finished steps

(none yet; details move to `serverless-log.md`)
