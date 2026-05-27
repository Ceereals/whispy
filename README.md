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
- **pill UI** — Quickshell layer overlay reading the state file (see [`ui/quickshell/`](ui/quickshell/)).

State/IPC paths (everything is namespaced `whispy`):
- socket: `$XDG_RUNTIME_DIR/whispy/whispy.sock`
- state: `$XDG_RUNTIME_DIR/whispy/state.json` (atomic writes, ≤20 Hz) ← the pill UI reads this
- logs: `$XDG_STATE_HOME/whispy/{daemon.log,whisper-server.log}` (JSON lines)

## Layout

```
crates/common   shared protocol + state types (serde)
crates/daemon   the daemon (audio, stt, filter, inject, state, server, whisper)
crates/client   the thin client
config/         default.toml, hallucinations.toml
systemd/        whispy-daemon.service (user unit)
scripts/        benchmark.sh, setup-ydotool.sh
ui/quickshell/  Quickshell pill overlay (reads state.json)
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
- [x] Step 1 — build whisper.cpp (Vulkan) + models + benchmark → large-v3-turbo-q5_0
- [x] Step 2 — daemon skeleton (socket, whisper supervise, state, systemd)
- [x] Step 3 — audio capture (pw-record, RMS, gain/normalize)
- [x] Step 4 — inference + hallucination filter (+ transcripts.jsonl)
- [x] Step 5 — injection (wl-copy + ydotool; needs ydotool setup)
- [x] Step 6 — thin client (start/stop/cancel/toggle) + Hyprland binds doc
- [ ] Step 7 — pill UI integration (module in `ui/quickshell/`; live QA pending)
- [ ] Step 8 — QoL (cancel-on-Escape, notifications, chime)
