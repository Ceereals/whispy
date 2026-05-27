# whispy

System-wide push-to-talk dictation for **Hyprland** (Wayland), inspired by TypeWhisper.
Hold a hotkey, speak, release — the transcript is injected into the focused field. A
Quickshell pill shows live state.

> **Status:** early scaffold. See [`docs/spike-fork-vs-build.md`](docs/spike-fork-vs-build.md)
> for the build-vs-fork decision and the roadmap below for progress.

## Architecture

```
Hyprland keybind ──► whispy-client ──(unix socket)──► whispy-daemon ──► whisper-server (Vulkan)
                                                          │
                                                          ├─► state.json ──► Quickshell pill
                                                          └─► wl-copy + ydotool ──► focused app
```

- **whispy-daemon** — resident service: supervises a `whisper-server` child (model stays in
  RAM), captures audio (PipeWire), filters hallucinations, injects text, publishes state.
- **whispy-client** — tiny binary called from Hyprland binds (`start`/`stop`/`cancel`).
- **whisper-server** — whisper.cpp built with the Vulkan backend (runs on AMD RDNA4, no ROCm).
- **pill UI** — separate Quickshell layer overlay reading `state.json` (separate handoff).

## Layout

```
crates/common   shared protocol + state types (serde)
crates/daemon   the daemon (audio, stt, filter, inject, state, server, whisper)
crates/client   the thin client
config/         default.toml, hallucinations.toml
systemd/        whispy-daemon.service (user unit)
scripts/        benchmark.sh, setup-ydotool.sh
docs/           spike, benchmark, hyprland setup
```

## Build

```sh
cargo build --release
cargo test
```

Binaries land in `target/release/{whispy-daemon,whispy-client}` (or install with
`cargo install --path crates/daemon` and `--path crates/client`).

## Requirements

- Hyprland / Wayland, PipeWire (`pw-record`), `wl-clipboard`, `ydotool`.
- whisper.cpp built with `-DGGML_VULKAN=1` (see Step 1 / `docs/stt-benchmark.md`).
- A Vulkan-capable GPU (developed on RX 9070 XT / RDNA4).

## Roadmap

- [x] Spike: build vs fork → full custom (Rust)
- [x] Step 0 — scaffold cargo workspace
- [ ] Step 1 — build whisper.cpp (Vulkan) + models + benchmark
- [ ] Step 2 — daemon skeleton (socket, whisper supervise, state, systemd)
- [ ] Step 3 — audio capture
- [ ] Step 4 — inference + hallucination filter
- [ ] Step 5 — injection
- [ ] Step 6 — thin client + Hyprland binds
- [ ] Step 7 — pill UI integration (separate handoff)
- [ ] Step 8 — QoL (cancel-on-Escape, notifications, chime)
