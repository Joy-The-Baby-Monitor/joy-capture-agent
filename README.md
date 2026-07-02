# Joy — Capture Agent

**Joy**, an open-source baby monitor. The Capture Agent runs on a
Raspberry Pi, captures audio and video, classifies signals into structured
*events*, and streams events plus live media to authorized clients over WebRTC.
It is remotely configurable, onboards onto a new WiFi network from the phone
over BLE, and is extensible in Python.

Written in Rust, it targets the Pi (production) but also builds and runs on
macOS (development), with a `--simulate` mode so the pipeline can be exercised
with no camera or microphone attached.

> Status: roadmap step 1 (skeleton) in progress. The capture HAL is in place —
> `VideoSource`/`AudioSource` traits, simulate backends, and hardware backends
> (nokhwa camera, cpal microphone) — with a probe mode in `joy-agentd`:
>
> ```sh
> cargo run -p joy-agentd -- --simulate   # no hardware needed
> cargo run -p joy-agentd                 # real camera + microphone
> ```

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

Requires a recent stable Rust toolchain (edition 2024, Rust 1.91+).

## Roadmap

1. **Skeleton** — video + audio capture on Mac and Pi; `--simulate` mode.
2. **Local live view** — WebRTC peer + signaling; a LAN client sees live A/V.
3. **Event bus + analysis** — motion and sound publishing `core.*` events.
4. **Control plane + settings** — capabilities, get/set config, subscriptions.
5. **Provisioning + local auth** — BLE onboarding and the device trust model.
6. **Extension host** — PyO3 contract, manifest, one example extension.
7. **Remote + notifications + hardening** — the additive cloud layer.

