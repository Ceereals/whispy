# Hyprland Setup

End-to-end setup for whispy on Hyprland, including [Omarchy](https://omarchy.org).

## Quick start

After installing the binaries (AUR, `./install.sh`, or `cargo build`), one command
does everything below — build whisper.cpp (Vulkan or CPU, per `stt.backend`), download the model, grant
ydotool access, seed the config, and enable the services:

```sh
whispy-daemon setup --quickshell
```

Then add the [keybinds](#5-keybinds) it prints (reprint them any time with
`whispy-daemon setup keybinds`). On Omarchy, see [Omarchy](#omarchy) for the
specifics. The sections below document each step for when you want to do it by
hand or debug a failure.

## 1. Prerequisites

- whisper.cpp built (Vulkan or CPU, per `stt.backend`) and a model downloaded (see `docs/stt-benchmark.md`).
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
whispy-daemon setup systemd       # installs + enables the ydotoold user unit
```

`ydotool` needs `ydotoold` running and `/dev/uinput` accessible — see the script output.
To skip ydotool entirely, set `injection.mode = "type"` in `~/.config/whispy/config.toml`:
text is typed with `wtype` instead (no uinput, no clipboard).

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

The default keys are the ones Omarchy reserves for dictation, and they're free on
a plain Hyprland too:

| Keys | Action |
|------|--------|
| **F9** (hold) | push-to-talk: record while held, transcribe on release |
| **SUPER+CTRL+X** | toggle: press to start, press again to stop |
| **SUPER+CTRL+SHIFT+X** | cancel the current recording (nothing is typed) |

Hyprland now reads a Lua config (`~/.config/hypr/hyprland.lua`) in place of the
classic hyprlang `hyprland.conf`; use the block that matches the file you have.
`whispy-daemon setup keybinds` detects it and prints the right one.

**Lua config** (`~/.config/hypr/hyprland.lua`):

```lua
hl.bind("SUPER + CTRL + X", hl.dsp.exec_cmd("whispy-client toggle"), { description = "Toggle dictation" })
hl.bind("F9", hl.dsp.exec_cmd("whispy-client start"), { description = "Start dictation" })
hl.bind("F9", hl.dsp.exec_cmd("whispy-client stop"), { description = "Stop dictation", release = true })
hl.bind("SUPER + CTRL + SHIFT + X", hl.dsp.exec_cmd("whispy-client cancel"), { description = "Cancel dictation" })
```

**hyprlang config** (`~/.config/hypr/hyprland.conf`):

```ini
bindd = SUPER CTRL, X, Toggle dictation, exec, whispy-client toggle
bindd = , F9, Start dictation, exec, whispy-client start
bindr = , F9, exec, whispy-client stop
bindd = SUPER CTRL SHIFT, X, Cancel dictation, exec, whispy-client cancel
```

Validate after saving with `hyprctl reload && hyprctl configerrors`.

Don't bind a bare `Escape` to cancel: a non-transparent Hyprland bind swallows the
key, so Escape would stop reaching every app.

If the release bind lags (measure it early — > ~100 ms is noticeable), drop the
push-to-talk pair and keep just the toggle.

## Omarchy

whispy runs on [Omarchy](https://omarchy.org) 4 out of the box; the only things
that differ from a plain Hyprland are where the binds go and which keys are taken.

**Install.** Omarchy ships `yay`, so the AUR package is one command:

```sh
yay -S whispy
whispy-daemon setup
```

**Keybinds.** Omarchy 4 configures Hyprland in Lua and loads
`~/.config/hypr/bindings.lua` after its defaults. Add:

```lua
-- Omarchy binds these to its own Voxtype dictation when Voxtype is installed;
-- unbind first so whispy is the only thing on the keys.
hl.unbind("SUPER + CTRL + X")
hl.unbind("F9")
o.bind("SUPER + CTRL + X", "Toggle dictation", "whispy-client toggle")
o.bind("F9", "Start dictation (push-to-talk)", "whispy-client start")
o.bind("F9", "Stop dictation (push-to-talk)", "whispy-client stop", { release = true })
o.bind("SUPER + CTRL + SHIFT + X", "Cancel dictation", "whispy-client cancel")
```

Hyprland reloads on save; check with `hyprctl configerrors`. The descriptions show
up in Omarchy's keybindings menu (`omarchy menu keybindings --print`).

> **Don't use SUPER+SPACE.** Omarchy 4 binds SUPER+SPACE (Omarchy menu),
> SUPER+ALT+SPACE (apps), SUPER+SHIFT+SPACE (top bar) and SUPER+CTRL+SPACE
> (backgrounds). Older whispy docs suggested those keys; move existing binds to the
> ones above.

On Omarchy 3.x (hyprlang config) the same binds go in `~/.config/hypr/bindings.conf`:

```ini
unbind = SUPER CTRL, X
unbind = , F9
bindd  = SUPER CTRL, X, Toggle dictation, exec, whispy-client toggle
bindd  = , F9, Start dictation, exec, whispy-client start
bindr  = , F9, exec, whispy-client stop
bindd  = SUPER CTRL SHIFT, X, Cancel dictation, exec, whispy-client cancel
```

**Voxtype.** If you enabled Omarchy's built-in dictation, the unbinds above are
enough for whispy to take over the keys, but Voxtype still keeps its own model
loaded. Remove it to free the memory: `omarchy-voxtype-remove`.

**Status UI.** Omarchy's own dictation icon (the "Dictation" entry of
`omarchy.indicators`) follows Voxtype only, and its click opens Voxtype's config;
the first `setup` hides it. whispy brings two status UIs of its own; use either or
both:

- **Bar widget.** On Omarchy 4, `setup` installs whispy as an Omarchy bar widget
  plugin (`whispy.dictation`, in `~/.config/omarchy/plugins/`) and puts it where
  Omarchy's dictation icon was, right after `omarchy.indicators` in the center.
  Re-run just this step with `whispy-daemon setup omarchy`.
  It follows `state.json` live: a mic, pulsing red while recording, an hourglass
  while transcribing, a check or mic-off when done. Click opens the settings TUI
  (`whispy-daemon settings`, floating; a second click focuses it) or, while
  recording, stops; right click toggles dictation; middle click cancels. Manage
  it like any Omarchy widget:

  ```sh
  omarchy bar move whispy.dictation --section left   # or center / right
  omarchy bar set whispy.dictation hideWhenIdle true --json   # show only while dictating
  omarchy plugin disable whispy.dictation             # take it off the bar
  omarchy bar put whispy.dictation                    # put it back
  ```

  Re-running `setup` updates the widget's files but never moves it, or re-adds it
  after you disable it.
- **Pill.** `setup` also runs the floating pill as the `whispy-pill` user service,
  a separate Quickshell instance that doesn't touch `omarchy-shell`. It sits at the
  bottom of the screen, clear of the bar. Keep only the bar icon with
  `setup --no-pill`.

**Injection.** Omarchy ships `wl-clipboard` and `wtype` but not `ydotool` (the AUR
package pulls it in). If you'd rather skip the uinput/`input`-group step, set
`injection.mode = "type"` to type through `wtype`, which works with no extra setup.

## 6. Pill UI

The Quickshell pill lives in `ui/quickshell/` and watches
`$XDG_RUNTIME_DIR/whispy/state.json`. See `ui/quickshell/README.md`.

## Troubleshooting

- `whispy-client` errors with connection refused → the daemon isn't running.
- Nothing pastes → check `ydotoold` is running and uinput perms (step 3).
- A bind does nothing → `hyprctl configerrors` for syntax errors, then
  `hyprctl binds | grep -B2 -A4 whispy` to confirm it registered and that nothing
  else is on the same key.
- Transcripts look wrong / clip quiet tails → raise `[audio].gain` in config; the
  daemon also peak-normalizes each clip. See `docs/stt-benchmark.md`.
- Inspect rejected transcriptions in `~/.local/state/whispy/transcripts.jsonl`.
