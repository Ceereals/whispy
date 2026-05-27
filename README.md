# whispy

System-wide push-to-talk dictation for **Hyprland** (Wayland), inspired by TypeWhisper.
Hold a hotkey, speak, release — the transcript is injected into the focused field. A
Quickshell pill shows live state.

> **Status:** v0.1.0 — functional. See [`docs/spike-fork-vs-build.md`](docs/spike-fork-vs-build.md)
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
packaging/aur/  PKGBUILD (whispy) + whispy-git/PKGBUILD
ui/quickshell/  Quickshell pill overlay (reads state.json)
docs/           spike, benchmark, hyprland setup
```

## Install

**Arch / CachyOS (AUR):**

```sh
paru -S whispy        # or: yay -S whispy
```

**Any distro (script):** clones nothing — run it from the repo. On Arch it routes
to the AUR package; elsewhere it builds from source into `~/.local/bin`.

```sh
./install.sh
```

**From source:**

```sh
cargo build --release --locked
install -Dm755 target/release/whispy-{daemon,client} -t ~/.local/bin
```

## Setup

After install, bootstrap the runtime once. This **builds whisper.cpp (Vulkan)**,
**downloads the model**, grants ydotool uinput access, seeds `~/.config/whispy`,
and enables the systemd user service:

```sh
whispy-daemon setup                 # add --quickshell for the pill overlay
```

Granular steps are available too: `whispy-daemon setup doctor|whisper|model|ydotool|systemd|quickshell`.
`setup` prints the Hyprland keybinds to add at the end — see
[`docs/hyprland-setup.md`](docs/hyprland-setup.md) for details.

## Requirements

- Hyprland / Wayland, PipeWire (`pw-record`), `wl-clipboard`, `ydotool`, `libnotify`.
- A Vulkan-capable GPU (developed on RX 9070 XT / RDNA4); `vulkan-icd-loader`.
- Build tools for `setup whisper`: `git`, `cmake`, a C/C++ compiler.
- Optional: [Quickshell](https://quickshell.org) (Qt 6.5+) for the pill overlay.

## Roadmap

- [x] Spike: build vs fork → full custom (Rust)
- [x] Step 0 — scaffold cargo workspace
- [x] Step 1 — build whisper.cpp (Vulkan) + models + benchmark → large-v3-turbo-q5_0
- [x] Step 2 — daemon skeleton (socket, whisper supervise, state, systemd)
- [x] Step 3 — audio capture (pw-record, RMS, gain/normalize)
- [x] Step 4 — inference + hallucination filter (+ transcripts.jsonl)
- [x] Step 5 — injection (wl-copy + ydotool; needs ydotool setup)
- [x] Step 6 — thin client (start/stop/cancel/toggle) + Hyprland binds doc
- [x] Step 7 — pill UI integration (module in `ui/quickshell/`)
- [x] Step 8 — QoL (cancel-on-Escape bind, notify-send on hard errors)
