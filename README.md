# Joy — Capture Agent

**Joy**, an open-source baby monitor. The Capture Agent runs on a
Raspberry Pi, captures audio and video, classifies signals into structured
*events*, and streams events plus live media to authorized clients over WebRTC.
It is remotely configurable, onboards onto a new WiFi network from the phone
over BLE, and is extensible in Python.

Written in Rust, it targets the Pi (production) but also builds and runs on
macOS (development), with a `--simulate` mode so the pipeline can be exercised
with no camera or microphone attached.

> Status: roadmap step 2 (local live view) complete. The agent captures,
> encodes (H.264/Opus in software), and streams live A/V over WebRTC to any
> client that connects through the JSON-over-WebSocket signaling endpoint.
>
> ```sh
> # live view with the browser dev page (open http://localhost:8080)
> cargo run -p joy-agentd --features dev-ui -- --serve --simulate
> cargo run -p joy-agentd --features dev-ui -- --serve   # real camera + mic
>
> # capture probe (roadmap step 1 health check)
> cargo run -p joy-agentd -- --simulate   # no hardware needed
> cargo run -p joy-agentd                 # real camera + microphone
> ```
>
> The `dev-ui` feature embeds a single-file browser test client served at
> `/`; production builds omit the feature and the page is compiled out. The
> signaling protocol itself (`/signal`) is a permanent, versioned surface —
> see `joy-media::signaling` for the message contract.

## Workspace layout

A Cargo workspace keeps the boundaries honest and lets CI compile subsets.
`joy-core` is the root of the dependency graph — everything depends on it, and
nothing depends on the binary.

| Crate | Responsibility |
|---|---|
| `joy-core` | Event model, traits, pipeline plumbing, config types |
| `joy-capture` | `VideoSource`/`AudioSource` traits and platform backends |
| `joy-analysis` | Signal-processing analysis (motion, sound) |
| `joy-media` | Encoding / UVC passthrough, WebRTC peer management, signaling |
| `joy-control` | Control-plane RPC, capability negotiation, settings |
| `joy-provision` | BLE GATT onboarding state machine, NetworkManager |
| `joy-ext` | Extension host (PyO3) and extension contract |
| `joy-store` | Persistent config and encrypted-at-rest secrets |
| `joy-agentd` | Binary: wires everything together, supervisor, CLI/daemon |

First-party Python extensions live under [`extensions/`](extensions/).

## Building

```sh
cargo build --workspace
```

Requires a recent stable Rust toolchain (edition 2024, Rust 1.91+) and
**cmake** (`brew install cmake` / `apt install cmake`) — the `opus` crate
builds its bundled libopus with it. The H.264 encoder (openh264) is compiled
from vendored Cisco source by `cc` with no extra tooling.

> Licensing note: building openh264 from source means the binary does **not**
> carry Cisco's royalty coverage (that applies only to Cisco's prebuilt
> binaries). Fine for development and self-built kits; revisit before
> distributing prebuilt agent binaries.

Cross-compiling for the Pi (`aarch64-unknown-linux-gnu`) is expected to work
via [`cross`](https://github.com/cross-rs/cross) — both native deps build
under its Docker toolchains — but has not been exercised yet.

## Roadmap

1. **Skeleton** — video + audio capture on Mac and Pi; `--simulate` mode. ✅
2. **Local live view** — WebRTC peer + signaling; a LAN client sees live A/V. ✅
3. **Event bus + analysis** — motion and sound publishing `core.*` events.
4. **Control plane + settings** — capabilities, get/set config, subscriptions.
5. **Provisioning + local auth** — BLE onboarding and the device trust model.
6. **Extension host** — PyO3 contract, manifest, one example extension.
7. **Remote + notifications + hardening** — the additive cloud layer.

