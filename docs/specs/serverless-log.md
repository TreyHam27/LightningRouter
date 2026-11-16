# Serverless mode: log

Detail from finished steps. Open only to look something up.

## Step 1: spec (Opus)

- **Crate research, as of 2026-11-15** (crates.io, docs.rs, downloaded
  sources):
  - ipstack 1.0.1 (Apache-2.0)
  - tun-rs 2.8.18 (Apache-2.0)
  - netstack-smoltcp 0.2.5 (fallback stack)
  - tun2proxy 0.8.7 (MIT, but it depends on socks5-impl 0.10.3, which is
    GPL-3.0-or-later)
  - fast-socks5 1.0.0 (MIT)
  - system-configuration 0.8.0 (no temporary-value wrapper; `-sys` 0.6.0
    declares `SCDynamicStoreAddTemporaryValue`)
  - tokio-yamux 0.3.20
  - idevice 0.1.68 is the newest; the relay uses 0.1.63.
- **tun2proxy's macOS `--setup`:** `tproxy-config` replaces the default route
  and per-service DNS and restores them only on a clean exit, so it was
  rejected.
- **utun teardown:** in xnu, `utun_ctl_disconnect` detaches the interface
  when its control socket closes, and `if_rtdel` deletes the routes that
  point at it. Not yet confirmed on a live Mac (M3 in step 7).
- **`SCDynamicStore.h` (macOS SDK):** temporary values are "removed
  automatically when the session is closed". The behaviour on SIGKILL is
  unverified (M3).
- **usbmuxd:** no documented limit on concurrent connections and no
  published latency, hence the D1 gate in step 6.

## Step 6: stage A on a device (2026-11-15)

- Step 6: stage A on an iPhone 14 over USB, run 2026-11-15. Signing: the user's own team via the gitignored `LightningRouter/Signing.xcconfig` (bundle ID `com.treyhamilton.LightningRouter`). `relay/examples/flood.rs` and a README section added.
  - D-A1 passed, run differently from the spec: the Mac's Wi-Fi stayed on, since turning it off would cut the agent's own connection. `curl --socks5-hostname 127.0.0.1:1080 https://ifconfig.me` returned 146.70.202.190 (AS9009 M247, a hosting provider, so the phone may have a VPN or Private Relay on, unchecked); a direct curl returned 73.233.211.106 (Comcast). The addresses differ, so traffic leaves through the phone. Safari was not tried.
  - D-A2: the D1 gate **fails as written**. One TCP flow opens in about 65 ms; a ping flow (usbmuxd connect plus phone accept only) in 1.7 ms (8 flows: 3.4 ms p50; 32: 10 ms p50). TCP flows opened at once stall in about 1 s steps (1, 3, 5, 7 s, like connect retries): 8 flows p50 1.07 s, 16 p50 1.1 s, 256 gave 75 to 86 successes with p50 1.4 to 3.3 s and p99 about 7.2 s (statuses Failed and TimedOut). The stall is the same for 1.1.1.1:443, 8.8.8.8:53 and example.com:80, so it comes from the phone's outbound connects, not usbmuxd, and yamux would probably not fix it. 256 ping flows: 232 succeeded, the rest Failed. Decision for the user: spec says insert step 6b (`tokio-yamux`); my recommendation is to find the phone-side connect stall first (cellular path, the 1 s steps) and revisit yamux after.
  - D-A3 passed: every USB flow reached the phone flow server from 127.0.0.1 (11 flows seen), so D9 holds. The temporary log is removed; the phone was rebuilt and reinstalled clean.
  - D-A4 passed: after an unplug and replug the relay logged `Waiting: no device is connected`, then `Connected to the app`, same process, and SOCKS curls returned 200 straight away. Recovery time was not timed exactly (the log has no timestamps); the relay polls every second.

## Step 6a: the D1 gate (2026-11-15)

iPhone 14 over USB, app in Serverless mode, `relay/examples/flood.rs` (now
takes comma-separated targets used round robin, `FLOOD_GAP_MS` to space
starts, prints successes per block of 32 and the last usbmuxd failure).

- **VPN or Private Relay was on** during step 6 (the user confirmed, then turned it
  off). With it off, the 1 s steps disappeared. Cellular, all at once to
  1.1.1.1:443: 1 flow 50 to 74 ms, 16: p50 84 ms, 32: 86 ms, 64: 86 ms,
  128: 132 ms, all succeeded. A name target (one.one.one.one, 64 flows) gave
  p50 248 ms, all succeeded.
- **Carrier cap on concurrent TCP connections, about 318.** Cellular, 400
  flows 2 ms apart: the first 318 succeeded, every later one TimedOut (the
  phone's 10 s connect limit); blocks of 32 were full up to block 9, then 0.
  Runs right after a big run got 62 or 1 successes, because closed flows kept
  counting toward the cap (gone within 60 s; 128 flows succeeded 5 s after a
  62-success run). It follows the count, not the target: 8 different hosts
  behaved the same. **On Wi-Fi** (same phone, same build): 400 and 600 flows
  2 ms apart all succeeded, p50 about 28 ms. So the cap is the carrier's
  network, not iOS, the phone's flow server or the transport.
- **Mac usbmuxd accept queue.** 256 flows started at once (ping or TCP,
  cellular or Wi-Fi) lost 24 to 120 with `Failed`, and the cache's
  reason was `usbmuxd is unreachable (device socket io failed)`, which is the
  connect to the Mac's own usbmuxd socket. Flows started 1 ms apart never
  failed (256 and 512 pings). Fix: `USBMUXD_CONNECTS`, a 32-permit semaphore
  around the usbmuxd connect handshake. After the fix: 256 pings at once
  succeeded twice (p50 125 to 149 ms, queueing included), and 512 TCP flows at once
  on Wi-Fi all succeeded (p50 341 ms, queued).
- **Phone side ruled out:** no `Busy` reply in any run (the flow cap was
  never reached), and no `Failed` from the phone (EMFILE would show as
  `Failed` from `connect_tcp`). The phone's 128-entry listen backlog never
  overflowed once the Mac queued its connects.
- **Transport cost, steady state:** ping flows 100 ms apart p50 2.5 to 2.6
  ms; TCP to 1.1.1.1:443 on Wi-Fi p50 26 ms. The first flow after an idle
  pause pays about 120 ms (seen in single-flow runs), probably the USB link
  waking up.
- **Outcome:** D1 gate reworded to measure the transport (ping flows) and
  passed; no yamux. Relay `cargo test` 45 passed, clippy clean.
