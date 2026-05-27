# Spike: Build vs Fork

**Date:** 2026-05-27 · **Time-box:** ~2-3h · **Decision:** **Full custom build** (Rust).

## Goal

Decide whether to fork an existing dictation tool or build from scratch. The default
recommendation from the handoff: fork **nerd-dictation** *if* its architecture allows
(a) swapping the STT backend for a custom whisper.cpp and (b) attaching an external UI
that reads daemon state; otherwise build custom.

## Non-negotiable requirements (from handoff)

1. **Persistent daemon** holding the model resident (no per-hotkey cold start).
2. STT = **whisper.cpp with the Vulkan backend** (RDNA4, no ROCm).
3. **External, machine-readable state** (file/socket) for a separate Quickshell pill UI.
4. **Wayland** text injection (wl-copy + ydotool).
5. Push-to-talk with distinct press/release.

## Candidates evaluated

| Candidate | Resident daemon | STT swappable → whisper.cpp | State readable by external UI | Stack | Verdict |
|-----------|-----------------|-----------------------------|-------------------------------|-------|---------|
| **nerd-dictation** | ❌ per-invocation ("no background processes") | ❌ tightly coupled to VOSK | ❌ only a cookie file as a trigger | Python | **Blocking** |
| **Handy STT** | ✓ | partial (bundled engines) | ❌ owns a gtk-layer-shell overlay; no parsable IPC | Rust/Tauri (GUI monolith) | Wrong stack |
| **OpenWhispr** | ✓ | uses whisper.cpp internally | ❌ monolithic Electron app | Electron/React | Wrong stack |
| **Whispering** | ✓ | pluggable providers | ❌ GUI app, no external state contract | Tauri/web | Wrong stack |
| **whisper_dictation** (LumenYoung) / **hyprwhspr** | ✓ daemon+client | no (python-whisper / faster-whisper) | ❌ | Python | Prior art, not a base |

### nerd-dictation (the default fork candidate) — details

- **Architecture:** spawns the recognizer per `begin`/`end` invocation. The docs state
  outright: *"As this relies on manual activation there are no background processes."*
  This is fundamentally incompatible with requirement #1 (resident model, no cold start).
- **STT backend:** hard-wired to the VOSK API. No pluggable backend interface — swapping in
  whisper.cpp means rewriting the recognition core.
- **State exposure:** none for a UI. The only IPC is a `--cookie FILE` that is *watched as a
  trigger* to begin/end; it does not publish `recording`/`transcribing` state.
- **Injection:** Wayland-capable (`ydotool`, `wtype`, `dotool` alongside the X11 `xdotool`).
  This is the **only** reusable piece — and it is trivial to reimplement (already a decided
  approach: wl-copy + ydotool Ctrl+V).
- **Config:** a Python user script (`nerd_dictation_process(text)`) for text post-processing.

→ nerd-dictation **fails both fork preconditions** (no backend swap path, no external state)
and breaks the resident-daemon requirement. Forking would mean replacing the daemon model,
the STT core, and adding a state channel — i.e. rewriting everything except the trivial,
already-decided injection layer.

### Other tools

- **Handy / OpenWhispr / Whispering** are GUI monoliths in the wrong stack (Tauri/Rust GUI,
  Electron). They bundle their own overlay and expose no external, parsable state contract
  for a separate Quickshell pill. The handoff also explicitly wants a small wrapper, not a
  desktop app to maintain.
- **whisper_dictation (LumenYoung)** and **hyprwhspr** are the closest prior art
  architecturally (background daemon + toggle client), useful as references, but they target
  python-whisper / faster-whisper (not whisper.cpp Vulkan) and KDE/X11, and expose no UI state.

## whisper.cpp Vulkan feasibility (verified)

- System has Vulkan 1.4 on `AMD Radeon RX 9070 XT (RADV GFX1201)` — RDNA4, gfx1201.
- Build: `cmake -B build -DGGML_VULKAN=1 && cmake --build build -j --config Release`.
- Vulkan is ~10× faster than the CPU backend; large-v3-turbo transcribed 80 min of audio in
  ~3 min on a far weaker Strix Halo APU, so 5 s clips on a 9070 XT sit comfortably inside the
  1.5 s budget.
- `whisper-server` keeps the model resident and, with `response_format=verbose_json`, returns
  per-segment `avg_logprob`, `no_speech_prob`, and `compression_ratio` — exactly the metrics
  the Step 4 hallucination filter needs.

## Decision and rationale

**Full custom build.** No candidate satisfies the fork preconditions: nerd-dictation (the
designated base) is per-invocation, VOSK-locked, and has no external state; the rest are
wrong-stack GUI monoliths. The only reusable concept — Wayland injection — is already a
decided, trivial implementation.

**Language: Rust** (deviation from the handoff's "Python single-file", authorized by the
user). The original rationale for Python was "mature audio/ML libraries", but inference now
lives entirely in the C++ `whisper-server`, so the daemon needs no ML libraries at all. Rust
gives a single static binary, sub-10 ms thin-client startup (better for hotkey latency than a
Python interpreter), and a stable RSS over long runs (success criterion #4).

**STT integration:** the daemon supervises a `whisper-server` child (Vulkan build) and talks
to it over HTTP on localhost, using `verbose_json` for the filter metrics. An in-process
binding (pywhispercpp) was rejected: fragile Vulkan builds on RDNA4 and incomplete metric
exposure.

The hallucination filter (Step 4) and the Quickshell pill UI (Step 7) remain custom regardless.
