# Serverless, track net: status

Branch `feat/serverless-net` (from `feat/serverless`). Steps 8 and 9 of
`serverless-spec.md` (build steps). This track ends after step 9.

## Track rules (so the branches merge cleanly)

- Touch only this track's files: `relay/src/system/fakedns.rs`,
  `relay/src/system/stack.rs`, this file. `relay/src/system/mod.rs` holds only
  `pub mod` lines. In `relay/src/main.rs` only the `mod system;` line.
- Leave `docs/specs/serverless-progress.md` and `CLAUDE.md` alone.
- Run nothing with sudo and nothing that changes this Mac's routes, DNS or
  interfaces. Leave the iOS simulator to the app track.
- Push nothing and open no PR.
- Step 9 commits and stops; no further handoff.

Other tracks: main line `feat/serverless` (step 5, socks, CLI),
`feat/serverless-core` (`system/setup.rs`), `feat/serverless-app` (`LightningRouter/`).

## Current state

Track finished.

- Step 8 done: `relay/src/system/fakedns.rs`, all 10 spec tests pass.
- Step 9 done: `relay/src/system/stack.rs`, all 9 spec "Stage B, stack" tests
  pass (plus `fake_range_is_198_18_slash_15`). Whole relay: 38 tests, clippy clean.

## Decisions made (step 8)

- No new dependency: DNS parsing is hand-written (question only).
- API: `FakeDns::new()`, `with_capacity(n)`, `respond(&mut self, &[u8]) ->
  Outcome::{Answer, Forward, Drop}`, `lookup_name`, `pin`/`unpin` (counted).
  Constants `ROUTER_ADDR`, `DNS_ADDR`, `TTL`, `POOL_CAPACITY`.
- Pool full and all pinned: SERVFAIL. Non-IN class / other types: `Forward`.
  Malformed: `Drop`. PTR for anything not mapped: NXDOMAIN.
- Still `#![allow(dead_code)]`: only `ROUTER_ADDR`/`DNS_ADDR` are unused until setup.

## Decisions made (step 9)

- API: `stack::run(device, cache, Options { upstream, packet_information, mtu })
  -> io::Error`. `device` is any `AsyncRead + AsyncWrite` that carries one IP
  packet per read and per write (the utun). Dropping the future aborts all flows.
  `Options::default()`: upstream `1.1.1.1:53`, no packet info, MTU 1500.
  `UDP_IDLE` (60 s) is ipstack's `udp_timeout`, which counts both directions.
- Fake range is 198.18.0.0/15: TCP to an unmapped address there (including
  .0.1 and .0.2 on ports other than 53) is refused, and UDP is dropped. A mapped
  address is pinned for the life of its flow.
- Port 53 at any address, TCP (length prefix) or UDP: fakedns. Each `Forward`
  opens its own phone UDP flow to the upstream, takes the first reply, and gives
  up after 5 s. UDP forwards run alongside local answers.
- Refusal: ipstack SYN-ACKs before handing over a stream, so a failed open
  sends FIN (if established, at most 1 s), then drops the stream. Anything the
  client sends after that gets a RST from ipstack. The tests check for the RST.
- Stack tests use a packet device built on mpsc channels, not
  `tokio::io::duplex`: a byte duplex can merge two packets into one read.
- ipstack 1.0.1 quirks:
  1. **No TCP half-close.** A client FIN with nothing in flight gets an
     immediate FIN back, so a reply sent after the client shut down its write
     side is lost (`nc -N`, some RPC). The FIN test closes server-first.
     Fixing it means patching ipstack or switching to netstack-smoltcp (D5 fallback).
  2. **A parked shutdown never wakes.** `poll_shutdown` that finds unacked
     data is never woken by the ACK. `Closing` in stack.rs re-polls it every 20 ms.
  3. **Dropping a TCP stream calls `block_in_place`.** `run` needs a
     multi-thread runtime.

## What the merge needs to know

- New deps (relay): `ipstack = "1.0.1"` (Apache-2.0, brings etherparse 0.20,
  ahash); dev-dep `etherparse = "0.20"` (pinned to ipstack's version); tokio
  feature `rt-multi-thread` added. `Cargo.lock` also moved tokio to 1.54 and
  bumped libc, mio and socket2.
- **`main.rs` runs `current_thread`.** The `--system` CLI step must start a
  multi-thread runtime for system mode, or the first TCP stream drop panics.
  Build it by hand after the privileged phase (D8), not with `#[tokio::main]`.
- With `tun-rs`, pass `packet_information` to match how the utun is opened.
  tun2proxy uses `true` on macOS.
- `stack.rs` is `#![allow(dead_code)]` until the CLI calls `run`.
- Merge conflict spots: `relay/src/main.rs` (`mod system;` line) and
  `relay/src/system/mod.rs` (core adds `pub mod setup;`, this track `fakedns`, `stack`).
- `CLAUDE.md` should later say: `system/fakedns.rs` is the pure DNS responder and
  fake-IP pool (D4); `system/stack.rs` is the ipstack glue (`run`), and needs a
  multi-thread runtime. Also list the three ipstack quirks above.
- The repo is not rustfmt-clean: `cargo fmt` rewrites socks.rs and fakedns.rs.
  stack.rs is formatted, and the other files were left as they were.

## Commands

```sh
cd relay && cargo test system::   # this track's tests
cd relay && cargo clippy --all-targets
```
