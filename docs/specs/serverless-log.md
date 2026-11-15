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
