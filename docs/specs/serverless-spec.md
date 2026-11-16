# Serverless mode: spec

Serverless mode lets the Mac use the phone's mobile data with no WireGuard
server. The relay on the Mac turns the Mac's connections into requests, and
the app on the phone opens each one with an ordinary socket. WireGuard relay
mode stays as it is.

Read `CLAUDE.md` for the current architecture. Decisions 1 to 5 in
`serverless-progress.md` still apply. This file adds to them and does not
repeat them.

## Overview

```
Stage A (no root)
  browser / macOS SOCKS setting ──TCP──> relay SOCKS5 (127.0.0.1:1080)
Stage B (root)
  any app ──IP packets──> utun ──> relay userspace stack (ipstack)
                                    │ fake-IP DNS answered on the Mac
Both stages
  relay ── one usbmuxd connection per flow, device port 51821 ──> app
  app ── TCP connect / connected UDP socket, names resolved on the phone ──> internet
```

The phone has one new job, the **flow server**: it reads a request ("TCP to
host:port" or "UDP to host:port"), opens the socket, replies with a status,
and then carries bytes. Everything else is on the Mac: SOCKS5, the IP stack,
DNS answers, and the system setup.

## Decisions on the open questions

### D1. One usbmuxd connection per flow, not a multiplexed stream

- **Per-flow backpressure:** each flow gets usbmux's own per-connection flow
  control.
- **No head-of-line blocking:** a stalled download cannot block a DNS reply
  or a keystroke.
- **Small phone code:** the phone stays one task per accepted connection,
  the same shape as `accept_loop` today.
- **No new dependency:** there is no mux crate to adopt.
- **Cost, measured (step 6a):** a ping flow (usbmuxd connect plus the
  phone's accept, no outbound connect) opens in about 2.5 ms p50, and 512
  held flows all succeed. No usbmuxd connection limit was hit. macOS
  usbmuxd's accept queue does overflow on a burst ("device socket io
  failed"), so the relay allows at most 32 usbmuxd connects in flight
  (`USBMUXD_CONNECTS` in `relay/src/device.rs`). Later connects wait
  instead of failing.
- **Gate (reworded in step 6a, passed):** the gate measures the transport,
  not the network beyond the phone. With ping flows, p50 open latency must
  be at most 20 ms, and 256 flows opened at once must all succeed. Otherwise
  add a multiplexed transport with `tokio-yamux` (0.3.20, MIT) behind the
  same `open_flow` function. The flow protocol below stays the same either
  way, because yamux streams would carry it unchanged. TCP flows to real
  hosts are recorded, but they do not decide the gate: their latency is the
  path's round trip, and their limits are the network's (below).
- **Network limits that yamux would not change:** on the user's cellular
  carrier, about 318 outbound TCP connections can exist at once. Flows past
  that time out, and closed flows still count toward the cap for under a
  minute. On Wi-Fi, 600 succeeded. A VPN on the phone added connect stalls
  in 1 s steps, even at 8 flows.

### D2. The phone speaks a small custom flow protocol, not SOCKS5

- **SOCKS5 can't carry UDP here:** its UDP ASSOCIATE needs a UDP path
  between client and server, and usbmuxd carries only TCP. A SOCKS5 server
  on the phone would therefore still need a custom UDP extension.
- **The flow protocol is simpler:** it covers TCP and UDP the same way, and
  it reuses the existing frame codec for datagrams.
- **SOCKS5 lives on the Mac:** in the relay, where it can be tested on the
  host.

### D3. UDP: one flow per destination, framed with the existing codec

- **Mapping:** one Mac-side UDP 5-tuple, or one (SOCKS association,
  destination) pair, maps to one phone flow. On the phone that flow is a
  connected UDP socket.
- **Framing:** datagrams cross as the existing `u16` LE length frames,
  payload only, because the flow is already bound to a destination.
- **Timeout:** the relay closes a UDP flow after 60 s with no datagram in
  either direction. The phone needs no timer, because the relay closing the
  usbmuxd connection ends the flow there.
- **DNS:** it does not ride UDP flows at all, except for forwarded
  non-address queries (D4).

### D4. DNS: fake-IP on the Mac, names resolved on the phone at connect time

This is the approach tun2proxy, Clash and sing-box use as "virtual DNS".

- **A queries:** the relay answers at once with an address from a pool in
  198.18.0.0/15 (RFC 2544 benchmark space) and remembers which name the
  address stands for. The answer has a TTL of 30 s.
- **Fake-IP connections:** a TCP or UDP flow to a pool address is sent to
  the phone **by name**. The phone resolves the name with `getaddrinfo` on
  cellular and picks IPv4 or IPv6 itself. This satisfies decision 3 at no
  extra round trip, and it works on IPv6-only carriers (NAT64), because the
  phone's resolver synthesises the addresses.
- **AAAA and HTTPS/SVCB (type 65, 64):** answered NOERROR with no records,
  so Mac apps use the fake IPv4 address. HTTPS records are also suppressed
  because their `ipv4hint` would leak real addresses past the mapping.
- **PTR for a pool address:** returns the mapped name, or NXDOMAIN.
- **All other types (MX, TXT, SRV, ...):** forwarded unchanged as a UDP flow
  through the phone to an upstream resolver. `--dns-upstream` sets it, and
  the default is `1.1.1.1:53`. The phone resolves even literal addresses with
  `getaddrinfo`, so this also works over NAT64.
- **Pool:** at most 65 536 entries, least recently used first out. An
  address with an open flow is never recycled.
- **Known limits:** a name that does not exist still gets an address, and
  the failure shows up at connect time as a reset. `ping name` gets a pool
  address and no reply (see D7).
- **Where the queries come from:** a query to UDP or TCP port 53 at **any**
  destination is answered locally, so apps with a hard-coded resolver
  (8.8.8.8) get fake addresses too.
- **Stage A:** fake-IP is not needed, because SOCKS clients send names.

### D5. IP stack: `ipstack` on a `tun-rs` utun

- **Crates:**
  - `ipstack` 1.0.1 (2026-07, Apache-2.0) takes any `AsyncRead + AsyncWrite`
    device and yields tokio TCP streams and UDP flows with their
    destinations. tun2proxy runs it on macOS with `packet_information(true)`.
  - `tun-rs` 2.8.18 (2026-11, Apache-2.0) creates the macOS utun with tokio
    support.
  - `hickory-proto` (MIT/Apache) builds and parses DNS messages.
  - `etherparse` (MIT/Apache) builds packets in tests.
- **Rejected:**
  - `tun2proxy` 0.8.7 would do most of stage B, but it depends on
    `socks5-impl`, which is GPL-3.0-or-later since 0.9, and this repo has no
    licence decided yet. Its macOS `--setup` also edits the default route and
    the per-service DNS, and restores them only on a clean exit, which breaks
    decision 5.
  - `netstack-smoltcp` 0.2.5 is a viable fallback with the same shape. It is
    younger and less used, so it stays the fallback if ipstack misbehaves.
- **First-use check:** each build step re-checks the API it uses against
  docs.rs before writing code.

### D6. IPv6

- **Stage B captures both families:** the utun gets IPv4 `198.18.0.1` and
  IPv6 `fd6c:7274::1/64` ("lr").
- **Most traffic is IPv4 anyway:** with fake-IP, apps that resolve names use
  IPv4 pool addresses.
- **IPv6 literals:** they reach the phone as literals. If the phone has no
  IPv6 route, its connect fails, the relay resets the flow, and happy
  eyeballs on the Mac falls back.
- **IPv6 capture is best effort:** if step 7 finds that macOS will not route
  IPv6 through the service, stage B ships IPv4-only and says so. With no
  other network, IPv6 then has no route and fails fast, which is harmless.
- **The phone always resolves, even literals:** it passes every host string,
  literals included, through `getaddrinfo`. That is how iOS synthesises
  NAT64 addresses for IPv4 literals.

### D7. ICMP

- **Dropped in v1.** An iOS app can open `SOCK_DGRAM`/`IPPROTO_ICMP` sockets
  (Apple's SimplePing sample does), so echo could be added later as a third
  flow kind.
- **No PMTU problem:** TCP ends at the Mac's stack, so ICMP errors such as
  path MTU discovery are not needed across the link.
- **No fake replies:** the relay never answers pings itself, because that
  would report a reachability that does not exist.

### D8. Root, and how it is scoped

`sudo relay --system` does all privileged work in a synchronous phase before
the Tokio runtime starts. Then it drops to `SUDO_UID`/`SUDO_GID`
(`setgroups`, `setgid`, `setuid`) and checks that `setuid(0)` now fails. The
privileged work is:

1. Raise `RLIMIT_NOFILE`.
2. Create the utun with `tun-rs` and set its addresses and an MTU of 1500.
3. Open an `SCDynamicStore` session and publish this run's network service,
   as described below.

After the drop, the relay keeps two things: the utun fd and the
SCDynamicStore session. **Teardown needs no privilege, because it is only
those two things closing.** It runs the same way on a clean exit, on Ctrl-C,
on a panic (`panic = "abort"` in the relay's release profile) and on
SIGKILL:

- **The utun fd closes:** xnu's `utun_ctl_disconnect` detaches the
  interface, and `if_rtdel` deletes every route through it.
- **The session closes:** configd removes values added with
  `SCDynamicStoreAddTemporaryValue` when the session that added them closes.

**The network service:**

- **Keys:** temporary values only, under a service ID that is new for each
  run: `State:/Network/Service/LightningRouter-<pid>/{IPv4,IPv6,DNS}`.
  - `IPv4` holds `Addresses`, `SubnetMasks`, `Router = 198.18.0.1`,
    `InterfaceName = utunN` and `OverridePrimary = 1`.
  - `IPv6` holds the same with IPv6 values.
  - `DNS` holds `ServerAddresses = [198.18.0.2]`.
- **What it does:** with `OverridePrimary`, configd's IPMonitor makes this
  service primary. It installs the default route through the utun and makes
  this the default resolver. When the keys vanish, it recomputes and puts
  the previous primary back.
- **Why it is crash-safe:**
  - The relay never edits another service's keys.
  - It never edits `Setup:` keys (those persist).
  - It never runs `networksetup`.
  - It never deletes a route it did not create.

The binding uses the `system-configuration` crate (0.8.0, mullvad,
MIT/Apache). The high-level wrapper has no temporary-value call, so
`SCDynamicStoreAddTemporaryValue` is called through
`system-configuration-sys` in one small `unsafe` function.

Two things are **unverified** and are verified first, in step 7:

- whether temporary values are removed when the process is SIGKILLed;
- whether IPMonitor honours `OverridePrimary` for a State-only service.

The fallbacks, in order:

1. If the default route is not installed, add `0.0.0.0/1` and `128.0.0.0/1`
   (and `::/1`, `8000::/1`) with `-interface utunN`. They die with the
   interface.
2. If temporary keys survive SIGKILL, the startup of every run removes any
   `State:/Network/Service/LightningRouter-*` keys whose pid is not alive.
   Only this program's keys are touched. Startup also prints
   `sudo relay --restore`, which does the same and exits.

There are other things stage B never does:

- **No blackholing switch:** while the phone is unreachable, the system stays
  captured and new flows are reset at once, not left hanging. The user quits
  the relay to get the previous network back. Withdrawing the service would
  need root again, which the relay no longer holds.
- **No routing loops:** usbmuxd over USB is a local socket. For a
  network-attached device, the LAN subnet route is more specific than the
  service's default route. The relay opens no other sockets.

### D9. Who can use the flow server (security)

- **The risk:** a flow server reachable from a Wi-Fi network would turn the
  phone into an open proxy for that network.
- **Bind:** the flow server listens on loopback only, `127.0.0.1` and `::1`.
  The phone's usbmuxd delivers USB connections from loopback.
- **Peer check:** the server also rejects any accepted peer that is not a
  loopback address.
- **Device check:** step 6 confirms on a device that USB flows arrive on
  loopback and records which family they use.
- **Network-attached devices:** consequently, serverless mode works over USB
  only in v1. The relay says so if the only device is on the network.
- **Out of scope:** the WireGuard port 51820 keeps its current
  all-interfaces bind. Its exposure is limited because it forwards only to
  the configured endpoint.
- **The relay's SOCKS listener:** it binds `127.0.0.1:1080` by default. A
  non-loopback `--socks` address is accepted only with a printed warning
  that it is an open proxy for that network.

### D10. Limits and resources

- **iOS open-file limit:** the default soft `RLIMIT_NOFILE` is 256, and each
  phone flow uses two fds. `flow_start` therefore raises the soft limit to
  `min(hard, 8192)`.
- **Flow cap:** the phone allows `(limit - 64) / 2` concurrent flows. A
  connection over the cap gets status `BUSY` and is closed.
- **Relay:** it raises its own soft limit the same way. Stage A uses two fds
  per flow; stage B uses one.
- **Phone timeouts:**
  - The request header must arrive within 5 s, or the connection is closed.
  - TCP connects try the resolved addresses in `getaddrinfo` order. The
    whole attempt is limited to 10 s, after which the status is `TIMED_OUT`.

### D11. Device discovery on the relay

- **Background task:** every second, the task lists usbmuxd devices and
  picks one, preferring USB, as today. It then sends a `PING` flow to it.
- **Cache:** the result is cached as "ready: device id", or "waiting:
  reason". The reason is printed once per change, as the relay does today.
- **Opening a flow:** `open_flow` uses the cached device id. If the connect
  fails, it marks the cache stale and the flow fails at once (no retry on
  the hot path).
- **`LR_DEVICE_ADDR=host:port`:** this environment variable bypasses usbmuxd
  and connects over plain TCP. It is used by tests and by the iOS simulator,
  which shares the Mac's network stack, so the app's flow server is on the
  Mac's loopback.

### D12. Testing without a device

- **The root crate becomes `staticlib` + `rlib`:** the relay can then
  dev-depend on it and run the real phone flow server in-process. Stage A
  and B tests are therefore end to end:
  relay → flow server → loopback echo target.
- **Phone tests:** they stay plain `#[test]` on `RUNTIME`.
- **Stage B stack tests:** they feed `ipstack` hand-built packets over a
  `tokio::io::duplex`, built with `etherparse`, and need no root.
- **System setup:** it can only be checked as root on a real Mac. Those
  checks are a manual checklist (M1 to M8) that step 7 and step 10 run with
  the user, and the commands are written out.

## Wire protocol: the flow port

- **Constant:** `FLOW_PORT: u16 = 51821`, in `frame/src/lib.rs`, next to the
  other two ports.
- **Codec:** in a new module `frame/src/flow.rs`, used by both ends.
- **Integers:** little-endian, like the frame header.

Each usbmuxd connection to `FLOW_PORT` carries exactly one flow.

### Request (relay to phone)

| Field | Size | Value |
|---|---|---|
| version | 1 | `1` |
| kind | 1 | `0` PING, `1` TCP, `2` UDP |
| atyp | 1 | `1` IPv4, `3` domain, `4` IPv6. Absent for PING. |
| addr | 4, 1+n, or 16 | IPv4 bytes. Domain: a length `n` (1 to 255) then `n` bytes of ASCII, not NUL-terminated. IPv6 bytes. Absent for PING. |
| port | 2 | destination port, LE. Absent for PING. |

### Reply (phone to relay)

A single status byte:

| Status | Name | Meaning | Stage A SOCKS5 reply |
|---|---|---|---|
| 0 | OK | flow open | `0x00` |
| 1 | FAILED | anything else | `0x01` |
| 2 | REFUSED | connection refused | `0x05` |
| 3 | UNREACHABLE | network or host unreachable | `0x03` |
| 4 | NO_SUCH_NAME | the name did not resolve | `0x04` |
| 5 | TIMED_OUT | the connect timed out | `0x04` |
| 6 | UNSUPPORTED | unknown version or kind, or a bad atyp | `0x07` |
| 7 | BUSY | over the flow cap | `0x01` |

- **Any status but OK:** the phone closes the connection after the byte.
- **PING:** always gets OK, and then the connection closes.

### After OK

- **TCP:** raw bytes in both directions.
  - **Half-close:** EOF from either side becomes `shutdown(Write)` on the
    other socket, and the flow ends when both directions are done (the
    semantics of `tokio::io::copy_bidirectional`).
  - **Reset:** an error on either socket closes both.
- **UDP:** frames (`FrameReader`/`FrameWriter`) in both directions.
  - **End:** a zero-length frame or EOF from the relay ends the flow, and
    the phone closes its UDP socket.
  - **Phone side:** the phone never ends a UDP flow on its own, except on a
    socket error.

### Decoder contract

- **`read_request`:** returns `Unsupported` for an unknown version, kind or
  atyp, so the phone can reply status 6. It returns an I/O error for a
  truncated header.
- **Domains:** a domain of length 0, or one that is not ASCII, is
  `Unsupported`.
- **Cancel safety:** neither read is cancel-safe. The 5 s header deadline
  wraps the whole `read_request` and drops the connection on expiry, which
  is safe because nothing else uses the stream.

## Stage A: SOCKS5 on the relay (no root)

### Relay CLI

Modes are exclusive. Arguments are parsed by hand, because there are four
flags.

```
relay                          WireGuard mode, unchanged
relay --socks [ADDR]           stage A, ADDR defaults to 127.0.0.1:1080
sudo relay --system [--dns-upstream ADDR]   stage B
sudo relay --restore           remove stale service keys, then exit (only if step 7 needs it)
```

### Relay modules

`main.rs` becomes thin:

| Module | Contents |
|---|---|
| `device.rs` | discovery, the cache, `LR_DEVICE_ADDR` (D11) |
| `wireguard.rs` | today's `bridge`, `uplink` and `downlink`, moved unchanged with their tests |
| `flow.rs` | `open_flow(kind, target) -> Result<Stream, Status>`, plus the TCP splice and the UDP frame pump |
| `socks.rs` | the SOCKS5 server |

### SOCKS5 server

- **Supported:** no-auth only, the CONNECT command, and UDP ASSOCIATE.
- **Rejected:** BIND gets `0x07`. A client offering no "no auth" method gets
  `0xFF`.
- **Implementation:** use `fast-socks5` 1.0.0 (MIT) server-side if its
  protocol API hands back the parsed command and target before replying, so
  that the reply can carry the phone's status. Check this on docs.rs at step
  time. If it can't, write a minimal server against RFC 1928, about 150
  lines, with the codec unit-tested.
- **UDP ASSOCIATE:**
  - **Socket:** the relay binds a UDP socket on the same IP as the SOCKS
    listener, port 0, and replies with that address.
  - **Datagrams:** each datagram's SOCKS UDP header gives the destination.
    Each new destination opens one phone UDP flow. Replies go back with the
    SOCKS UDP header naming the destination they came from.
  - **Fragments:** datagrams with `FRAG != 0` are dropped.
  - **Lifetime:** the association ends when its TCP control connection
    closes, and that closes all its flows. Each flow also has the 60 s idle
    timeout.

### Phone (`src/flow.rs` and FFI)

- **`ProxyHandle`:** it is generalised over the session the accept loop
  runs:

  ```rust
  enum Service { Relay(Target), Flows(FlowLimits) }
  ```

  Everything else (start, stats, stop, `JoinSet` abort) is shared.
- **New FFI:**

  ```c
  bool flow_start(ProxyHandle **proxy)
  ```

  It binds `FLOW_PORT` on loopback (D9), raises the fd limit (D10), and
  returns a handle that `proxy_stats` and `proxy_stop` accept unchanged.
- **Stats:** `connections` counts open flows (pings excluded), and the byte
  counters count TCP and UDP payload.
- **Testable listener:** `pub fn serve_flows(listener) -> ProxyHandle` (or
  similar) lets relay tests start it on an ephemeral loopback port.

## Stage B: whole system (root)

### Relay modules

| Module | Contents |
|---|---|
| `system/setup.rs` | **risky core, Opus.** The privileged phase, the service dictionaries, temporary keys, the privilege drop, and `--restore` if needed. The dictionary builders are pure functions so they can be unit-tested. |
| `system/fakedns.rs` | the pure DNS responder |
| `system/stack.rs` | the ipstack glue |

`system/fakedns.rs` takes `query bytes` and returns one of:

- `Answer(bytes)`
- `Forward`
- `Drop`

It also owns the pool:

- `lookup_name(ip) -> Option<Name>`
- pin and unpin, for addresses with open flows.

`system/stack.rs` runs this loop:

- **The loop:** accept from `ipstack`, then dispatch:
  - **TCP or UDP to port 53:** handled by fakedns. TCP uses the 2-byte
    length prefix. A `Forward` sends the query to the upstream resolver over
    a phone UDP flow.
  - **TCP to a pool address:** opens a TCP flow by name. With no mapping,
    the client gets a reset.
  - **TCP to any other address:** opens a TCP flow by IP.
  - **UDP:** follows the same name-or-IP rule, with the 60 s idle timeout.
  - **Anything else:** dropped.
- **A failed open:** the TCP stream is dropped at once, so the client sees a
  reset or close and not a hang.

### Addresses

| Address | Role |
|---|---|
| `198.18.0.1` | the utun's local address and the service `Router` |
| `198.18.0.2` | the DNS server |
| `198.18.0.3` to `198.19.255.254` | the fake-IP pool, capped at 65 536 live entries |

Running another tool that uses 198.18.0.0/15 (Clash, Surge) at the same time
is not supported, and the relay refuses to start if a route to that range
already exists.

## Stage C: the app

- **Mode picker:** a picker in `ContentView`, "WireGuard relay" or
  "Serverless", persisted with `@AppStorage("tunnelMode")` like the existing
  settings. It is disabled while the tunnel is on, like the target fields.
- **WireGuard fields:** the host and port fields show in WireGuard mode only.
- **`Tunnel`:** `start(host:port:)` becomes `start(mode:)`, with
  `Mode.wireGuard(host:port:)` and `Mode.serverless`. Serverless calls
  `flow_start`.
- **Status:** in serverless mode it reads "Serverless: N connections" with
  the up and down bytes, or "Waiting for the Mac" when N is 0. The text is
  honest about what can be seen: zero open flows looks the same as no relay.
- **Background service:** unchanged.

## Test cases

Write these by name. Automated tests use real loopback sockets, as today.

### Shared codec (`frame/src/flow.rs`)

- `request_wire_layout_is_exact`: a TCP request to `example.com:443` encodes
  to the exact bytes given in the table above.
- `request_round_trips_every_kind_and_address`: PING, TCP and UDP requests
  with IPv4, IPv6 and domain targets.
- `request_reassembles_a_byte_at_a_time`: over `duplex(1)`.
- `unknown_version_kind_or_atyp_is_unsupported`: each one yields
  `Unsupported`, not a panic or an I/O error.
- `empty_or_non_ascii_domain_is_unsupported`.
- `domain_over_255_bytes_is_rejected_on_encode`.
- `truncated_request_is_an_io_error`.
- `status_round_trips_every_value`, and an unknown status byte decodes as
  `FAILED`.

### Phone flow server (`src/flow.rs`)

- `tcp_flow_reaches_target_by_ip_and_by_name`: an echo target on loopback,
  reached as `127.0.0.1` and as `localhost`.
- `tcp_half_close_carries_the_remaining_reply`: the relay shuts down its
  write side. The target sees EOF and then writes a final reply, which still
  arrives.
- `refused_target_replies_refused`.
- `unresolvable_name_replies_no_such_name`: uses `nothing.invalid`
  (RFC 6761).
- `unreachable_or_slow_target_fails_within_the_timeout`: the test sets the
  connect timeout to 200 ms and targets `192.0.2.1`, and the status is
  `TIMED_OUT` or `UNREACHABLE`.
- `udp_flow_relays_datagrams_both_ways`.
- `udp_flow_ends_on_an_empty_frame`: the target socket stops receiving after
  the flow ends.
- `ping_replies_ok_and_closes`.
- `unsupported_request_replies_unsupported`.
- `silent_relay_is_dropped_after_the_header_deadline`: the deadline is set
  short in the test.
- `flow_cap_replies_busy`: the cap is 1, the first flow is held open, and the
  second one gets `BUSY`.
- `stats_count_open_flows_and_bytes`: pings are not counted.
- `stop_closes_open_flows_and_frees_the_port`.
- `flow_listener_binds_loopback_only`: checks the bound addresses. If the
  host has a non-loopback IPv4 address, connecting to it is refused.
- `ffi_flow_start_rejects_null_and_stops_cleanly`.

### Relay, stage A (`relay/src/{device,flow,socks}.rs`)

These run end to end against the real flow server, through the rlib
dev-dependency and `LR_DEVICE_ADDR` or an injected address.

- `socks_connect_by_domain_reaches_target`.
- `socks_connect_by_ipv4_and_ipv6_reaches_target`.
- `phone_status_maps_to_socks_reply`: refused gives `0x05`, no such name
  gives `0x04`, and an unsupported command gives `0x07`.
- `socks_rejects_bind_and_auth_only_clients`.
- `socks_udp_associate_reaches_two_destinations`: two echo targets, two
  phone flows, and each reply is labelled with the destination it came from.
- `udp_association_ends_with_its_control_connection`: the phone's open flow
  count returns to 0.
- `idle_udp_flow_closes_after_timeout`: uses `tokio::time::pause`.
- `no_device_fails_flows_fast`: with no device, the SOCKS reply is `0x01` in
  under 1 s.
- `socks_listens_on_loopback_by_default`.
- `device_cache_reports_reason_once_per_change`: a pure state-machine test.
- The existing WireGuard tests move with `wireguard.rs` and still pass.

### Stage A, on a device (manual, step 6)

- **D-A1:** Safari with the macOS SOCKS proxy set to `127.0.0.1:1080`, over
  USB with Wi-Fi off on the Mac. Pages load, and `curl --socks5-hostname
  127.0.0.1:1080 https://ifconfig.me` shows the carrier's IP.
- **D-A2:** `relay/examples/flood.rs` opens N concurrent flows and records
  p50 and p99 open latency and how many succeed. 256 ping flows at once
  are the D1 gate. TCP flows to real hosts are recorded alongside, as
  network figures.
- **D-A3:** log the peer address that the flow server sees for USB flows,
  then remove the log. This is the D9 confirmation.
- **D-A4:** unplugging and replugging recovers within 2 s, with no relay
  restart.

### Stage B, fakedns (pure, `system/fakedns.rs`)

- `a_query_gets_a_pool_address_with_ttl_30`.
- `same_name_gets_the_same_address_case_insensitively`.
- `aaaa_https_and_svcb_get_empty_noerror`.
- `other_types_are_forwarded`: MX, TXT and SRV.
- `ptr_for_pool_address_returns_its_name_else_nxdomain`.
- `reply_keeps_id_question_and_rd_flag`.
- `malformed_query_is_dropped`.
- `pool_recycles_least_recently_used_when_full`: a 4-entry pool in the test.
- `pinned_address_is_not_recycled`.
- `dns_server_and_router_addresses_are_never_handed_out`.

### Stage B, stack (`system/stack.rs`, crafted packets, no root)

- `syn_to_pool_address_opens_tcp_flow_by_name`.
- `syn_to_real_address_opens_tcp_flow_by_ip`.
- `tcp_payload_and_fin_cross_both_ways`.
- `refused_flow_resets_the_client`.
- `syn_to_unmapped_pool_address_resets`.
- `udp_to_port_53_anywhere_is_answered_locally`.
- `udp_to_pool_address_opens_udp_flow_by_name`.
- `forwarded_dns_query_goes_to_upstream_via_udp_flow`.
- `icmp_is_dropped_and_the_stack_keeps_working`.

### Stage B, setup (unit tests, `system/setup.rs`)

- `service_dictionaries_have_the_expected_keys`: IPv4, IPv6 and DNS,
  including `OverridePrimary`.
- `service_id_includes_pid`.
- `privilege_drop_needs_sudo_ids`: missing or zero `SUDO_UID` refuses to run
  instead of staying root.
- `refuses_to_start_when_198_18_is_already_routed`: route-table parsing is a
  pure function.

### Stage B, on a real Mac (manual, root; steps 7 and 10)

Save `netstat -rn` and `scutil --dns` before each check, and compare.

- **M1:** start. `scutil --dns` resolver #1 is `198.18.0.2`, the default
  route goes through `utunN`, and `ps -o user= -p <pid>` is the sudo user.
- **M2:** Ctrl-C. Routes and DNS are byte-identical to the saved copies,
  ignoring the order of the cached entries.
- **M3:** `kill -9`. Same as M2, within 5 s.
- **M4:** with Wi-Fi on, `curl https://ifconfig.me` shows the carrier IP,
  and the LAN router's admin page still loads.
- **M5:** with Wi-Fi off and no other network, browsing, `dig MX gmail.com`
  (forwarded) and a QUIC site (`curl --http3` if available) all work.
- **M6:** the phone is unplugged while running. New connections fail in
  under 1 s, and after replugging they work again.
- **M7:** start again straight after M3. There is no stale service, and
  `scutil` shows exactly one `LightningRouter-*` key set.
- **M8:** IPv6. Note whether IPv6 traffic is captured, then apply D6.

### Stage C, app

- **Compile check:** the simulator `xcodebuild` command from `CLAUDE.md`.
- **C1 (simulator, manual):** with the app running serverless in the
  simulator and `LR_DEVICE_ADDR=127.0.0.1:51821 relay --socks`, `curl
  --socks5-hostname` works, and the status counts the connection.
- **C2 (device):** the mode picker persists across launches and is disabled
  while on. WireGuard mode still works against a real endpoint.

## Build steps

Each step is one area, with a budget of about 40 tool calls. At the end of
each step:

- `make test` and `cargo clippy --all-targets` pass in each crate it
  touched;
- `CLAUDE.md` is updated for the area;
- the progress file is updated;
- the work is committed;
- the next step is handed off.

**Model rule:** the risky core is Opus. Wiring and tests from the listed
cases are Sonnet.

| Step | Area | Model | Needs the user |
|---|---|---|---|
| 2 | Shared codec: `frame/src/flow.rs` and `FLOW_PORT`, with its tests | Sonnet medium | no |
| 3 | Phone flow server: `src/flow.rs`, `Service` generalisation, `flow_start` FFI, fd limit, loopback bind, `rlib`, with its tests; run `make` | Sonnet medium | no |
| 4 | Relay restructure: split `main.rs` into `device.rs` (D11, `LR_DEVICE_ADDR`), `wireguard.rs` (moved) and `flow.rs` (`open_flow`, splice, UDP pump), plus the CLI; tests `device_cache_*` and moved WireGuard tests | Sonnet medium | no |
| 5 | SOCKS5: `socks.rs` (fast-socks5 or minimal), with all stage A relay tests | Sonnet medium | no |
| 6 | Stage A on a device: D-A1 to D-A4, `examples/flood.rs`, decide the D1 gate, README section | Sonnet medium | yes: device, Wi-Fi off |
| 7 | **Stage B spike, risky core:** `system/setup.rs` privileged phase, temporary keys, `OverridePrimary`, privilege drop, setup unit tests. Run M1, M2, M3, M7 and M8 with a stub stack that only answers DNS, and settle D8's fallbacks | Opus high | yes: sudo (via `request_secret`) |
| 8 | `system/fakedns.rs` and its pool, with the fakedns tests | Sonnet medium | no |
| 9 | `system/stack.rs` ipstack glue, with the stack tests | Opus medium | no |
| 10 | Wire `--system` end to end, then run the full M1 to M8 on a device | Opus medium | yes: sudo, device |
| 11 | Stage C: `Tunnel` mode, picker, status text, compile check, C1 | Sonnet medium | C2 on a device |
| 12 | Docs pass (README, `CLAUDE.md`) and a review of the branch (`mattpocock-skills:code-review`) | Sonnet medium | no |
| 13 | Ship. Ask first: `origin` is upstream, so the user decides where it goes | Sonnet medium | yes |

If D1's gate fails in step 6, insert step 6b (Opus medium): a `tokio-yamux`
transport behind `open_flow`, with the phone accepting both forms on
`FLOW_PORT`.
