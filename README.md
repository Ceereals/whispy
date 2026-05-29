<div align="center">

# whispy

**Push-to-talk dictation for Hyprland.** Hold a key, speak, release — your words land in the focused field.

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![AUR](https://img.shields.io/aur/version/whispy?label=AUR)](https://aur.archlinux.org/packages/whispy)
[![CI](https://github.com/Ceereals/whispy/actions/workflows/ci.yml/badge.svg)](https://github.com/Ceereals/whispy/actions/workflows/ci.yml)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org)

[Quickstart](#quickstart) · [Install](#install) · [Setup](#setup) · [How it works](#how-it-works) · [Configuration](#configuration) · [Requirements](#requirements)

<!-- Drop a real capture here: record the pill + a dictation into an app, save as docs/demo.gif -->
![whispy demo](docs/demo.gif)

</div>

Local, fast, Wayland-native dictation: a resident daemon keeps a
[whisper.cpp](https://github.com/ggerganov/whisper.cpp) model in RAM, a thin client is fired
from your Hyprland binds, and a [Quickshell](https://quickshell.org) pill shows live state.
Speech never leaves your machine. Hyprland gets the full experience (pill overlay); dictation
also runs on **X11/Xorg and generic Wayland** (GNOME, KDE, Sway, XFCE, i3) — see
[Non-Hyprland](#non-hyprland-x11-and-generic-wayland).

## Quickstart

```sh
paru -S whispy            # install (Arch/CachyOS)
whispy-daemon setup       # build whisper.cpp, fetch model, enable service
```

Then add a toggle bind to Hyprland (press to start, press again to stop):

```ini
bind = SUPER ALT, Space, exec, whispy-client toggle
bind = , Escape, exec, whispy-client cancel
```

Reload Hyprland, press **SUPER+ALT+Space**, speak, press again — the text is typed into
your focused field. See [`docs/hyprland-setup.md`](docs/hyprland-setup.md) for the full bind set.

## Install

**Any distro (script)** — one command, nothing to do after. On Arch it routes to the AUR
package; elsewhere it builds from source into `~/.local/bin`. Either way it then runs
`whispy-daemon setup` for you (whisper.cpp build, model download, ydotool + services):

```sh
./install.sh                         # add WHISPY_NO_SETUP=1 to install binaries only
```

**Arch / CachyOS (AUR):**

```sh
paru -S whispy            # or: yay -S whispy
whispy-daemon setup       # finish the one-time bootstrap
```

**From source:**

```sh
cargo build --release --locked
install -Dm755 target/release/whispy-{daemon,client} -t ~/.local/bin
whispy-daemon setup
```

## Setup

`whispy-daemon setup` is the one-time bootstrap (idempotent — safe to re-run). It **builds
whisper.cpp** (Vulkan or CPU, per `stt.backend` — auto-detected by default), **downloads the
model**, grants ydotool uinput access, seeds
`~/.config/whispy`, enables the `whispy-daemon` **and** `ydotoold` user services, and — if
Quickshell is present — runs the pill as its own `whispy-pill` service (no `shell.qml` edit),
then **verifies** the running system:

```sh
whispy-daemon setup                 # --quickshell forces the pill; --no-pill installs the
                                    # module but skips the service (embed PillPanel yourself)
```

Run any step on its own with:

```sh
whispy-daemon setup doctor|whisper|model|ydotool|systemd|quickshell|verify
```

The only manual step left is the keybinds — `setup` prints them at the end (Hyprland binds, see
[`docs/hyprland-setup.md`](docs/hyprland-setup.md); X11/GNOME equivalents under
[Non-Hyprland](#non-hyprland-x11-and-generic-wayland)). Re-run `whispy-daemon setup verify` any
time to check that the model, daemon, `ydotoold`, and whisper-server are all up.

## How it works

```
Hyprland keybind ──► whispy-client ──(unix socket)──► whispy-daemon ──► whisper-server (Vulkan/CPU)
                                                          │
                                                          ├─► state.json ──► Quickshell pill
                                                          └─► inject ──► focused app
```

- **whispy-daemon** — resident service: supervises a `whisper-server` child (model stays
  in RAM), captures audio (PipeWire), filters hallucinations, injects text, publishes state.
- **whispy-client** — tiny binary called from Hyprland binds (`start` / `stop` / `cancel` / `toggle`).
- **whisper-server** — whisper.cpp built with the backend `stt.backend` picks: Vulkan (developed on AMD RDNA4, no ROCm) or CPU (OpenBLAS-accelerated when available).
- **pill UI** — Quickshell layer overlay reading the state file ([`ui/quickshell/`](ui/quickshell/)).

**Two injection modes** (`injection.mode`):

| Mode | How | Best for |
|------|-----|----------|
| `paste` *(default)* | `wl-copy` + `ydotool` Ctrl+V, with clipboard save/restore | GUI fields; preserves accents |
| `type` | types directly via `wtype` (virtual keyboard) | terminals too; touches no clipboard |

State & IPC paths (everything is namespaced `whispy`):

- socket — `$XDG_RUNTIME_DIR/whispy/whispy.sock`
- state — `$XDG_RUNTIME_DIR/whispy/state.json` (atomic writes, ≤20 Hz) ← the pill reads this
- logs — `$XDG_STATE_HOME/whispy/{daemon.log,whisper-server.log}` (JSON lines)

## Configuration

Defaults are baked into the daemon (see [`config/default.toml`](config/default.toml)). Override
any of them by copying the file to `$XDG_CONFIG_HOME/whispy/config.toml`. Common knobs: the
model and `whisper-server` paths (`[stt]`), the per-request `stt.timeout_secs`, the hallucination
filter thresholds (`[filter]`), and `injection.mode` to switch between paste and type.

To tune the filter, run **`whispy-daemon stats`** — it summarizes the transcript log
(`$XDG_STATE_HOME/whispy/transcripts.jsonl`) as accepted vs dropped clips, broken down by drop
reason, so you can see whether `fuzzy_ratio` / confidence bounds are too aggressive.

## Requirements

- PipeWire (`pw-record`), `libnotify`, `ydotool` (paste mode — works on both Wayland and X11)
- **Wayland**: `wl-clipboard` (paste mode) and/or `wtype` (type mode)
- **X11/Xorg** (e.g. XFCE, i3, Cinnamon, GNOME-on-Xorg): `xdotool` + `xclip` **or** `xsel`
- For the Vulkan backend: a Vulkan-capable GPU (developed on RX 9070 XT / RDNA4) + `vulkan-icd-loader`. Not needed for the CPU backend.
- Build tools for `setup whisper`: `git`, `cmake`, a C/C++ compiler
- Optional: `openblas` for ~3-4× faster CPU inference; [Quickshell](https://quickshell.org) (Qt 6.5+) for the pill overlay (Wayland layer-shell only)

Run `whispy-daemon setup doctor` to see exactly which tools your session needs — it detects
the display server and lists the per-backend dependencies.

### Non-Hyprland: X11 and generic Wayland

Dictation runs everywhere; only the on-screen pill is Hyprland/layer-shell specific.

- **Display server** — `injection.backend` (`[injection]` in config) selects the injection path:
  `auto` (default) detects `WAYLAND_DISPLAY` then `DISPLAY`; force it with `"wayland"` or `"x11"`.
  On X11, injection uses `xdotool type` / `xclip`/`xsel` instead of `wtype` / `wl-clipboard`.
- **Status UI** — the Quickshell pill needs `wlr-layer-shell`, which X11 and GNOME/Mutter-Wayland
  don't provide, so it's skipped there. Instead whispy fires desktop **notifications** on
  success/error (`ui.notify = "auto"` notifies on X11, `"on"` always, `"off"` never).
  `state.json` is still written, so you can wire your own tray/status indicator.
- **Keybinds** — there are no Hyprland binds off Hyprland. Map your push-to-talk key to
  `whispy-client toggle` (and `whispy-client cancel`) through your environment:
  - **XFCE**: Settings → Keyboard → Application Shortcuts
  - **X11 WMs** (i3/bspwm/...): `xbindkeys`, `sxhkd`, or the WM's own bind syntax
  - **GNOME**: Settings → Keyboard → Custom Shortcuts
- **Window-class auto-workflow** — uses `xdotool` on X11 and `hyprctl`/`swaymsg` on Wayland;
  on GNOME/KDE-Wayland (no universal protocol) it's skipped. Manual `--workflow NAME` works everywhere.

> **Platform notes.** `setup whisper` builds whisper.cpp with the backend `stt.backend` selects:
> `auto` (default) builds Vulkan when its loader is present, else a CPU build (OpenBLAS-accelerated
> when `libopenblas` is installed) — so GPU-less machines work out of the box. Force it with
> `stt.backend = "vulkan"` or `"cpu"`. NVIDIA (CUDA) users should build whisper.cpp themselves and
> point `stt.server_bin` at it.

<details>
<summary><b>Project layout</b></summary>

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

See [`docs/spike-fork-vs-build.md`](docs/spike-fork-vs-build.md) for the build-vs-fork
decision and [`docs/stt-benchmark.md`](docs/stt-benchmark.md) for model benchmarks.

</details>

<details>
<summary><b>Roadmap</b></summary>

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

</details>

## License

[MIT](LICENSE) © Riccardo Romoli
