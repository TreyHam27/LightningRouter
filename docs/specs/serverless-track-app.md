# Serverless track: app (step 11)

Branch `feat/serverless-app`. Touches only `LightningRouter/`.

## Current state

- `Tunnel.swift`: `Tunnel.Mode` (`wireGuard`, `serverless`, persisted by raw
  value) and `Tunnel.Start` (`.wireGuard(host:port:)`, `.serverless`).
  `start(mode:)` replaces `start(host:port:)`; serverless calls `flow_start`.
  `Status.relaying` now carries `connections`. `Tunnel.mode` records which
  mode is running, for the status text.
- `ContentView.swift`: segmented picker (`@AppStorage("tunnelMode")`),
  disabled while the tunnel is on; host/port fields show in WireGuard mode
  only. Status reads "Serverless: N connection(s) · ↓ … ↑ …", or "Waiting for
  the Mac" at 0.
- Compile check passes (`make`, then the simulator `xcodebuild` from CLAUDE.md,
  BUILD SUCCEEDED). The Rust side is untouched.

## Decisions

- `Start` is separate from `Mode` because only WireGuard carries a target.
- The picker is segmented (two options, always visible).

## Outstanding

- **C1 not run.** It needs the app started in the simulator with the toggle
  tapped (no UI-automation tool here) plus `LR_DEVICE_ADDR=127.0.0.1:<FLOW_PORT>
  relay --socks` and `curl --socks5-hostname`. The simulator shares the host
  loopback, so the flow server's loopback bind is reachable. Spec text says
  port 51821; check it against `FLOW_PORT` in `frame/src/lib.rs`. Nothing was
  installed on a simulator, so nothing to uninstall.
- **C2 (device)**: picker persists across launches and is disabled while on;
  WireGuard mode still works against a real endpoint.

## For CLAUDE.md later

- App section: `Tunnel.start(mode:)` takes `Tunnel.Start`; serverless calls
  `flow_start`; `ContentView` has a mode picker (`tunnelMode`).
- Remove the progress file's "Gap" note: `Tunnel.swift` now calls `flow_start`.
