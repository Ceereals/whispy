# Whispy — Quickshell dictation pill

A small overlay pill that floats on top of every screen during dictation. It
renders the state the `whispy-daemon` publishes — no I/O of its own beyond
watching the state file. (Visual spec authored in the design tool; this module
mirrors it.)

## Files

| File | Purpose |
|---|---|
| `Tokens.qml` | Singleton with every color, size, and timing token. |
| `Pill.qml`   | The morphing pill component — visual only, no I/O. |
| `PillPanel.qml` | Layer-shell `PanelWindow` + `FileView` state watcher + state machine. |
| `qmldir` | Module registration as `Whispy`. |
| `shell.qml.example` | Drop-in integration. |

## Installation

```bash
# From this directory (ui/quickshell/ in the repo):

# 1. Copy the module into your Quickshell config
mkdir -p ~/.config/quickshell/Whispy
cp Tokens.qml Pill.qml PillPanel.qml qmldir ~/.config/quickshell/Whispy/

# 2. From your shell.qml
```

```qml
import Quickshell
import Whispy

ShellRoot {
    PillPanel {}
}
```

Run with `quickshell -p ~/.config/quickshell/shell.qml`.

## Requirements

- Quickshell with `Quickshell.Wayland`, `Quickshell.Io` modules
- Qt 6.5+ (uses `QtQuick.Effects.MultiEffect`, `QtQuick.Shapes` `dashOffset`)
- Wayland compositor with wlr-layer-shell (Hyprland, Sway, river…)
- Font: Inter (or any sans-serif fallback — see `Tokens.fontFamily`)

## State file contract

The pill watches a JSON file (default `$XDG_RUNTIME_DIR/whispy/state.json` —
exactly what `whispy-daemon` writes). The daemon writes it atomically (tmp +
rename) at up to 20 Hz during recording.

```json
{
  "state": "idle | recording | transcribing | success | error",
  "rms": 0.42,
  "error_kind": "low_confidence",
  "error_message": "Couldn't make that out clearly",
  "timestamp": 1734267384.123
}
```

- **state**: drives all visuals.
- **rms**: 0..1, drives the waveform when `state === "recording"`. Smoothed internally with `Tokens.rmsSmoothing` per update.
- **error_kind**: present in the daemon's JSON (kept for schema stability); the pill ignores it and renders `error_message`.
- **error_message**: shown verbatim in the error pill. Width auto-fits (180–360 px), then ellipses.
- **timestamp**: epoch float (fractional seconds). If older than `Tokens.staleThresholdMs` (5 s), the pill forces back to idle.

If the file is missing, malformed, or empty: pill stays idle, parse errors logged silently.

## State machine

| In | Triggers | Behavior |
|---|---|---|
| `idle` | invisible | Pill hidden (or 6×6 dot, see "Idle dot" tweak in the design reference). |
| `recording` | `state="recording"` | 134×48 (240 if label), mic + 16-bar waveform driven by RMS, 1 Hz ambient glow. |
| `transcribing` | `state="transcribing"` | 60×48 (156 if label), indigo spinner rotating 900 ms/loop. |
| `success` | `state="success"` | 56×48 (96 if label), check stroke draws over 220 ms. Auto-hides after 600 ms. |
| `error` | `state="error"` | Width auto-fits message (180–360 px), shake on entry, amber tint. Auto-hides after 1200 ms. |

Transition timings:

| From → To | Duration | Easing |
|---|---|---|
| idle → recording | 200 ms | OutCubic (fade + slide-up) |
| recording → transcribing | 250 ms | InOutQuad (width morph + cross-fade) |
| transcribing → success | 200 ms | OutQuad |
| transcribing → error | 200 ms | OutQuad + shake |
| success → idle | 250 ms | InCubic (fade + slide-down) |
| error → idle | 300 ms | InCubic |
| recording → idle (cancel) | 200 ms | InQuad |

Everything is the **same `Item`** that morphs `width` — never two pills.

## Public API of `Pill.qml`

| Property | Type | Default | Notes |
|---|---|---|---|
| `dictationState` | string | `"idle"` | one of the 5 states |
| `rms` | real | `0.0` | 0..1, drives waveform |
| `errorMessage` | string | `""` | shown during error |
| `showLabel` | bool | `false` | "Listening…" / "Transcribing…" / "Done" |
| `showGlow` | bool | `true` | 1 Hz ambient pulse during recording |

`PillPanel.qml` adds `statePath` (default `$XDG_RUNTIME_DIR/whispy/state.json`).
Edit `Tokens.qml` for everything else.

## Edge cases handled

1. **state.json missing at startup** → idle, no console errors.
2. **JSON parse error** → silent `console.warn`, last valid state retained.
3. **Stale state** (timestamp > 5 s old) → forced idle every 1 s tick.
4. **Rapid transitions** (rec → trans → error < 500 ms) → no frame skipping; behaviors interrupt cleanly.
5. **Recording > 30 s** → no special handling; bars keep drawing.
6. **Constant `rms = 0`** → bars sit at the 18 % floor; ambient glow still pulses.
7. **Quickshell reload mid-recording** → no crash; FileView re-mounts.
8. **Multi-monitor** → one `PanelWindow` per screen via `Variants { model: Quickshell.screens }`. Each instance is independent so the pill effectively appears on every screen at once. If you want it only on the focused output, replace the `model` with a single screen reference (e.g. `[focusedScreen]`).

## Test plan (manual QA)

After installing, verify each scenario:

1. **Idle** — pill not visible anywhere on any screen.
2. **Recording, real audio** — write `{state:"recording", rms:<live>}` at 20 Hz; bars track RMS, glow pulses.
3. **Recording, silence** — `rms` stuck at 0; bars sit at floor, glow still pulses → "alive" signal.
4. **Transcribing** — width morphs smoothly from recording size; spinner spins.
5. **Success** — check draws in; pill auto-hides ~600 ms later.
6. **Error (long message)** — width grows up to 360 px, then ellipses; shake on entry; auto-hides ~1200 ms later.
7. **Cancel** — write `state:"idle"` while recording; pill fades + slides down with no success/error beat.
8. **Stale** — stop writing the file. After 5 s, pill returns to idle on its own.
9. **Click-through** — pill renders above a fullscreen browser/Electron app; clicking through it lands in the underlying app. **Verify focus is not stolen** — type in your terminal while the pill is on screen; characters still go to the terminal.
10. **Reload** — `quickshell -p` again mid-recording; no crash, pill reappears on next state update.

Drive states by hand without the daemon:

```bash
d="$XDG_RUNTIME_DIR/whispy"; mkdir -p "$d"
printf '{"state":"recording","rms":0.4,"error_kind":null,"error_message":null,"timestamp":%s}\n' \
  "$(date +%s.%N)" > "$d/state.json"
```

## Customization

Edit `Tokens.qml`:

- `accentRecording` — swap coral for any hue.
- `marginBottom` — distance from screen bottom (default 24).
- `widthRecording`, `widthRecordingLbl` etc. — adjust pill sizes if your audience prefers more padding.
- `holdSuccessMs`, `holdErrorMs` — how long terminal states linger.
- `staleThresholdMs` — when to force idle.

For palette inheritance from a system theme (matugen / pywal): replace the hard-coded colors in `Tokens.qml` with bindings to your theme singleton.

## What this is NOT

- Not a daemon — the dictation engine (audio capture, Whisper, paste) is `whispy-daemon`. This module only **renders** state.
- Not a settings UI — no clickable buttons, no settings panel.
- Not skinnable at runtime — change `Tokens.qml` and reload.
