# Lightning Router

Use your device as a router through USB

## Vision

Your device has mobile data. Your Mac does not.
Why not use your device's mobile data on your Mac?

Right now, this program relays a Wireguard connection
from your Mac to a Wireguard server.

Eventually, this app will have a userspace TCP stack
to proxy connections as a NAT.

## Building/Running

Build the Rust half of the app from the repo root
(`make targets` installs the Rust targets it needs, once)

```sh
make targets
make
```

Build the app with Xcode and install on your device.
Run `make` again whenever the Rust code changes.

Run the Mac relay

```sh
cd relay
cargo run --release
```

In Wireguard, set the target address to ``127.0.0.1:3400``
(or the IP of your Mac from another device).

In the iOS app, set the target to your Wireguard endpoint
(an IP address or a hostname), and enable.

The relay and the app can be started in either order. The relay waits
for the device and the app, and reconnects if either goes away. The app
shows whether the relay is connected and how much it has carried.

Profit.

## Serverless mode (SOCKS5, experimental)

Serverless mode needs no WireGuard endpoint. The phone opens each
connection itself, so traffic leaves through the phone's cellular data.

1. In the app, choose **Serverless** and turn the tunnel on.
2. Run the relay with a SOCKS5 proxy on `127.0.0.1:1080`:

   ```sh
   cd relay
   cargo run --release -- --socks
   ```

3. Point an app or the macOS proxy settings at `127.0.0.1:1080`. To check:

   ```sh
   curl --socks5-hostname 127.0.0.1:1080 https://ifconfig.me
   ```

   Use `--socks5-hostname` so names are resolved on the phone.

The relay waits for the phone and reconnects after an unplug and replug,
with no restart. Whole-system mode (routing everything, with no per-app
proxy settings) is not wired up yet.

Measured on an iPhone 14 over USB: a single flow opens in about 65 ms.
Many TCP flows opened at once are slow, because each open waits on the
phone's outbound connect: 16 at once take about 1 s each, and only about a
third of 256 succeed. Run `cargo run --release --example flood [N]
[host:port|ping]` in `relay/` to measure it on your own phone.

## Tests

```sh
make test
```

## Speed

But is it fast? *Oh yeah* it's fast. I get roughly 200mbps,
or about 2/3 of my internet connection speed.
Not too bad for overhead.

## License

Not sure yet
