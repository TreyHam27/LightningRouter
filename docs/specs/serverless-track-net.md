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

- Step 8 done: `relay/src/system/fakedns.rs`, all 10 spec tests pass, clippy clean.
- Step 9 (`system/stack.rs`, Opus medium) not started.

## Decisions made (step 8)

- No new dependency: DNS parsing is hand-written (question only).
- API for the stack glue: `FakeDns::new()`, `with_capacity(n)` (tests),
  `respond(&mut self, &[u8]) -> Outcome::{Answer(Vec<u8>), Forward, Drop}`,
  `lookup_name(Ipv4Addr) -> Option<&str>`, `pin(ip) -> bool`, `unpin(ip)`.
  Constants `ROUTER_ADDR` (198.18.0.1), `DNS_ADDR` (198.18.0.2), `TTL`, `POOL_CAPACITY`.
- Pins are counted (several flows to one address): the stack calls `pin` when a
  flow to a pool address opens and `unpin` when it ends.
- Pool full and every entry pinned: the A query gets SERVFAIL (not a drop).
- Replies echo only header + question (no EDNS OPT); RA set, AA clear.
- Non-IN class and other types: `Forward`. Opcode != 0, QR set, qdcount != 1,
  compression pointer in the question, root name, non-graphic bytes: `Drop`.
- PTR for anything not a mapped pool address (including non-pool and
  malformed names): NXDOMAIN.
- The module is `#![allow(dead_code)]` until stack.rs uses it; remove then.

## What the merge needs to know

- No new Cargo dependencies from step 8.
- Merge conflict spots: `relay/src/main.rs` (`mod system;` line, next to
  `mod socks;`) and `relay/src/system/mod.rs` (core track adds `pub mod setup;`,
  this track `pub mod fakedns;` and `pub mod stack;`).
- `CLAUDE.md` should later say: `system/fakedns.rs` is the pure DNS responder and
  fake-IP pool (spec D4).

## Next: step 9

`system/stack.rs`, the ipstack glue with the stack tests from the spec section
"Stage B, stack". Check each crate's current docs before using `ipstack` /
`tun-rs` (progress file decision 4). Add the dependencies in `relay/Cargo.toml`
and list them under "What the merge needs to know".

## Commands

```sh
cd relay && cargo test system::   # this track's tests
cd relay && cargo clippy --all-targets
```
