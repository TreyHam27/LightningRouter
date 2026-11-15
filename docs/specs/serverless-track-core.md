# Serverless track core: status

Branch `feat/serverless-core` (from `feat/serverless` at `9657647`). This
track is spec step 7, split into 7a (code, done) and 7b (privileged manual
runs, waiting for the user). The track ends here; nothing hands off.

## Current state

- **Step 7a done.**
  - `relay/src/system/setup.rs`: `prepare()` does D8's privileged phase.
  - `relay/src/system/stub.rs`: the stub stack that answers only DNS.
  - `relay/examples/system_spike.rs`: runs `prepare()` and then the stub.
    It is what step 7b runs.
- Tests: 25 relay tests pass, 7 of them new. The four setup tests named in
  the spec are there, plus three stub tests. `cargo clippy --all-targets`
  is clean in `relay/`.
- Checked without root: the spike, run without sudo, prints
  `--system needs root: run it with sudo`, exits 1 and changes nothing.
- **Step 7b not run.** Nothing here has run as root yet. Whether
  temporary keys survive SIGKILL and whether `OverridePrimary` works for a
  State-only service are both still unverified.

## Decisions made in 7a

1. **Order inside `prepare()`.** These checks change nothing: euid is 0,
   `SUDO_UID` and `SUDO_GID` are set and non-zero, and nothing in
   `netstat -rn -f inet` lies inside 198.18.0.0/15. Then, in order: raise
   the fd limit to `min(hard, 8192)`, create the utun, publish IPv4, IPv6
   and DNS as temporary keys (DNS last), and drop privileges. On any error
   after the utun exists, `prepare()` returns early, and dropping the
   locals is the teardown.
2. **The privilege drop.** It runs `setgroups([gid])`, `setgid`, `setuid`,
   then requires that `setuid(0)` and `seteuid(0)` both fail and that
   uid, euid and egid match. If the drop fails, `prepare()` returns `Err`
   and the caller exits. It never carries on as root.
3. **Range check.** Only routes inside 198.18.0.0/15 (prefix 15 or longer)
   count. Wider routes such as `default`, `128.0/1` or `198/8` do not.
   netstat's classful short forms are handled: `198.18` means /16, and
   `198.18.0` means /24.
4. **The utun.** tun-rs `DeviceBuilder` with IPv4 `198.18.0.1/15`, IPv6
   `fd6c:7274::1/64`, MTU 1500, `.enable(true)`, `packet_information(false)`
   and `associate_route(true)`. `enable` is explicit because tun-rs leaves
   the enable state unset by default. `associate_route` adds the /15 and
   /64 routes through the utun, and those routes die with it.
5. **The dictionaries are plain Rust (`Dict` of `Value`)** and convert to
   CF in `to_cf`, so they test without configd. IPv6 uses `PrefixLength`
   where IPv4 uses `SubnetMasks`. Both set `OverridePrimary = 1`.
6. **FFI the crates lack.** `SCDynamicStoreAddTemporaryValue` comes from
   `system-configuration-sys` 0.6. `SCError` and `SCErrorString` are not
   bound there, so `setup.rs` declares them itself, linked against the
   SystemConfiguration framework.
7. **No signal handler.** Ctrl-C uses the default SIGINT action, so M2 and
   M3 both test the "no cleanup code" teardown. Step 10 may add a handler
   for a clean log line, but it must not run cleanup that the teardown
   depends on.
8. **`setup.rs` and `stub.rs` use no `crate::` items**, so the example can
   pull them in with `#[path]` (the relay is a bin crate with no lib).
   Keep it that way, or give the relay a lib target in step 10.
9. **The stub is not fakedns.** It answers IPv4 UDP sent to
   `198.18.0.2:53`: an A/IN query gets `198.18.0.3` with TTL 0, any other
   query gets an empty NOERROR. Every other packet is dropped and counted
   on stderr. While the spike runs, the Mac has no working network.

## Step 7b runbook (user present, other threads idle)

**Warning:** while the spike runs, all traffic goes into a utun that drops
it. Every agent on this Mac loses network. Wi-Fi can stay on; M4 to M6 are
step 10's.

Use `request_secret` with `delivery: "file"` for the sudo password. Run
from `relay/`:

```sh
cargo build --release --example system_spike
S=/tmp/lr-7b; mkdir -p $S
snap() { netstat -rn > $S/routes.$1; scutil --dns > $S/dns.$1;
         scutil <<< 'list State:/Network/Service/LightningRouter.*' > $S/keys.$1; }
same() { diff <(grep -v ' W' $S/routes.before | sort) <(grep -v ' W' $S/routes.$1 | sort);
         diff <(sort $S/dns.before) <(sort $S/dns.$1); cat $S/keys.$1; }
start() { sudo -S -p '' target/release/examples/system_spike < "$PWFILE" > $S/spike.log 2>&1 &
          sleep 2; PID=$(awk '/^pid/{print $2; exit}' $S/spike.log); cat $S/spike.log; }
snap before
```

`grep -v ' W'` drops cloned and cached host routes, which come and go on
their own. Check anything else that differs by hand.

| Run | Commands | Must show |
|---|---|---|
| M1 start | `start`, then `scutil --dns \| sed -n '/resolver #1/,/^$/p'`, `route -n get default`, `scutil <<< 'show State:/Network/Global/IPv4'`, `ps -o user= -p $PID`, `snap m1; cat $S/keys.m1`, `dig @198.18.0.2 example.com +short`, `dscacheutil -q host -a name example.com` | resolver #1 `nameserver[0] : 198.18.0.2`; default `interface: utunN`; `PrimaryService : LightningRouter-$PID`; ps shows the sudo user, not root; exactly 3 keys (IPv4, IPv6, DNS); both lookups give `198.18.0.3` |
| M2 Ctrl-C | `kill -INT $PID; sleep 1; snap m2; same m2` | no diff lines; keys list is empty; `ifconfig -l` has no extra utun |
| M3 kill -9 | `start; kill -9 $PID; sleep 5; snap m3; same m3` | as M2. The `kill` needs no sudo, which is itself a check on the drop |
| M7 restart | straight after M3: `start; snap m7; cat $S/keys.m7; kill -INT $PID` | exactly 3 keys, all for the new `$PID` |
| M8 IPv6 | during an M1 run: `route -n get -inet6 default`, `scutil <<< 'show State:/Network/Global/IPv6'`, `ping6 -c2 2606:4700:4700::1111` (stub's dropped count rises) | record whether the IPv6 default goes through utunN |

If anything gets stuck (it should not, since nothing persistent is
written): `sudo scutil` then `remove State:/Network/Service/LightningRouter-<pid>/IPv4`
(and `/IPv6`, `/DNS`). Rebooting also clears everything.

**Which fallback applies to each failure (D8, D6):**

- **M1: the default route is not through utunN, or `PrimaryService` is
  some other service.** IPMonitor ignores `OverridePrimary` on a
  State-only service. Use fallback 1: while still root in `prepare()`, add
  `0.0.0.0/1` and `128.0.0.0/1` with `-interface utunN`, via the routing
  socket or `/sbin/route`. Those routes die with the utun, so teardown is
  unchanged. Resolver #1 then stays the Wi-Fi resolver, but its packets
  route into the utun. The real stack answers port 53 at any address, so
  DNS still works. Only the stub would miss it, because it answers only
  198.18.0.2.
- **M1: resolver #1 is not 198.18.0.2, but the default route is ours.**
  No fallback needed, for the same reason as above. Record it.
- **M1: `ps` shows root.** A bug in `drop_privileges`, with no fallback.
  Fix it before anything else.
- **M1: the keys are missing after the drop.** configd tied the values to
  the session's credentials. No D8 fallback covers this, so stop and
  redesign on Opus. One option is to keep a tiny root child that holds the
  session.
- **M3: the keys are still listed after `kill -9`.** Use fallback 2: at
  startup, remove `State:/Network/Service/LightningRouter-<pid>/*` for
  every pid that is no longer alive, and add `relay --restore`, which does
  only that. Write the pid filter as a pure function with a unit test.
- **M2 or M3: routes or the utun linger.** Not expected, because xnu
  deletes them on fd close. Capture `netstat -rn` and `ifconfig` and stop.
- **M7: more than 3 keys.** Use fallback 2, as for M3.
- **M8: IPv6 is not captured.** Apply D6: ship stage B as IPv4-only and
  say so. Then decide whether to drop the IPv6 key or to add `::/1` and
  `8000::/1` with `-interface utunN` (fallback 1 for IPv6). Without one of
  the two, IPv6 traffic leaks over Wi-Fi.

Record the results here and in the main progress file when the tracks
merge.

## What the merge needs to know

- **New relay dependencies:** `tun-rs 2.8` (feature `async_tokio`),
  `system-configuration 0.8`, `system-configuration-sys 0.6`,
  `core-foundation 0.10` and `libc 0.2`. Track net probably adds tun-rs
  and ipstack too. If `relay/Cargo.lock` conflicts, take either side and
  rerun `cargo build` to regenerate it.
- **`relay/Cargo.toml`** gains `[profile.release] panic = "abort"`, as D8
  requires.
- **`relay/src/system/mod.rs`:** keep the union of the `pub mod` lines
  (`setup`, `stub`, plus net's `fakedns`, `stack`).
- **`relay/src/main.rs`:** this track adds
  `#[allow(dead_code)] mod system;`. Keep one copy. Step 10 removes the
  `allow` when it wires `--system`.
- **Step 10:** call `setup::prepare()` before building the runtime, then
  `AsyncDevice::new(prepared.device)` inside it. Keep `prepared.store`
  alive for the life of the process. `stub.rs` can then go, or stay for
  diagnostics.
- **What `CLAUDE.md` should say later, in its Relay section:** `system/`
  (setup is the root phase, and the teardown is the fd plus the session),
  that `examples/system_spike.rs` needs sudo, and that `setup.rs` and
  `stub.rs` must stay free of `crate::` items while the example includes
  them with `#[path]`.
