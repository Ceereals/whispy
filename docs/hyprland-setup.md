# Hyprland Setup

End-to-end setup for whispy on Hyprland.

## Quick start

After installing the binaries (AUR, `./install.sh`, or `cargo build`), one command
does everything below — build whisper.cpp (Vulkan), download the model, grant
ydotool access, seed the config, and enable the service:

```sh
whispy-daemon setup --quickshell
```

Then add the [keybinds](#5-keybinds) it prints. The sections below document each
step for when you want to do it by hand or debug a failure.

## 1. Prerequisites

- whisper.cpp built with Vulkan and a model downloaded (see `docs/stt-benchmark.md`).
  Defaults expect `~/.local/share/whisper.cpp/build/bin/whisper-server` and
  `~/.local/share/whisper.cpp/models/ggml-large-v3-turbo-q5_0.bin`.
- `pw-record` (PipeWire), `wl-clipboard` (`wl-copy`/`wl-paste`), `ydotool`.

## 2. Install the binaries

```sh
cargo install --path crates/daemon
cargo install --path crates/client
# -> ~/.cargo/bin/{whispy-daemon,whispy-client}
```

## 3. ydotool (text injection)

Injection pastes via `ydotool` (Ctrl+V). Grant uinput access and run the daemon:

```sh
./scripts/setup-ydotool.sh        # udev rule + input group (log out/in after)
systemctl --user enable --now ydotool   # or run ydotoold from Hyprland autostart
```

`ydotool` needs `ydotoold` running and `/dev/uinput` accessible — see the script output.

## 4. Run the daemon

```sh
mkdir -p ~/.config/systemd/user
cp systemd/whispy-daemon.service ~/.config/systemd/user/
systemctl --user enable --now whispy-daemon
journalctl --user -u whispy-daemon -f     # or ~/.local/state/whispy/daemon.log
```

The daemon loads whisper-server at startup (model resident), so the first dictation
after boot is fast.

## 5. Keybinds

Push-to-talk (primary): distinct press/release events.

```
bindd = SUPER, Space, Start dictation, exec, whispy-client start
bindr = SUPER, Space, Stop dictation, exec, whispy-client stop
bind  = SUPER SHIFT, Space, Cancel dictation, exec, whispy-client cancel
```

Measure the release latency early; if `bindr` lags > ~100 ms, fall back to a toggle:

```
bind = SUPER, Space, Toggle dictation, exec, whispy-client toggle
```

Cancel-while-recording with Escape is a Step 8 nicety (submap or conditional bind).

## 6. Pill UI

The Quickshell pill lives in `ui/quickshell/` and watches
`$XDG_RUNTIME_DIR/whispy/state.json`. See `ui/quickshell/README.md`.

## Troubleshooting

- `whispy-client` errors with connection refused → the daemon isn't running.
- Nothing pastes → check `ydotoold` is running and uinput perms (step 3).
- Transcripts look wrong / clip quiet tails → raise `[audio].gain` in config; the
  daemon also peak-normalizes each clip. See `docs/stt-benchmark.md`.
- Inspect rejected transcriptions in `~/.local/state/whispy/transcripts.jsonl`.
