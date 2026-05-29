# CLAUDE.md

Guidance for AI assistants working in this repository.

## What whispy is

whispy is **push-to-talk dictation for Hyprland (Wayland)**. Hold/toggle a key,
speak, release — the transcribed text is injected into the focused field. Speech
never leaves the machine: a resident daemon keeps a
[whisper.cpp](https://github.com/ggerganov/whisper.cpp) model in RAM, a thin
client is fired from Hyprland binds, and a [Quickshell](https://quickshell.org)
pill overlay shows live state.

Target platform is Linux + Wayland (Hyprland), developed on AMD RDNA4 with the
Vulkan backend (no ROCm). whisper-server's compute backend is selectable via
`stt.backend` (`auto` / `vulkan` / `cpu`): `auto` builds Vulkan when its loader is
present, else a CPU build (OpenBLAS-accelerated when `libopenblas` is installed),
so GPU-less machines work too. It is **not** cross-platform (Linux/Wayland only).

## Architecture

```
Hyprland keybind ──► whispy-client ──(unix socket)──► whispy-daemon ──► whisper-server (Vulkan/CPU)
                                                          │
                                                          ├─► state.json ──► Quickshell pill
                                                          └─► inject ──► focused app
```

- **whispy-daemon** — resident systemd user service. Supervises a `whisper-server`
  child (model stays in RAM), captures audio (PipeWire), filters hallucinations,
  runs the text pipeline (corrections → snippets → optional LLM workflow), injects
  text, and publishes state.
- **whispy-client** — tiny binary called from Hyprland binds. Sends one command and
  exits fast (no async runtime, no config parsing).
- **whisper-server** — whisper.cpp built with the backend `stt.backend` selects
  (Vulkan, or CPU/OpenBLAS). Built and managed per-machine by `whispy-daemon setup`;
  **not** shipped in releases.
- **pill UI** — Quickshell layer overlay that reads `state.json` (`ui/quickshell/`).

### Recording flow

`start` → open PipeWire capture → on `stop`, transcription runs off-thread (client
returns immediately, UI follows `state.json`) → hallucination filter →
`pipeline::process` (corrections, snippets, AI workflow) → inject → flash
`success`/`error` and revert to `idle`.

## Workspace layout

Cargo workspace (`resolver = "3"`, `edition = "2024"`, MSRV `rust-version = 1.85`).

| Crate | Path | Role |
|-------|------|------|
| `whispy-common` | `crates/common/` | Shared IPC protocol + state types (`Cmd`, `Resp`, `State`, `StateSnapshot`). Serde only, no logic. |
| `whispy-daemon` | `crates/daemon/` | The resident service and the `setup` bootstrap. All the real logic. |
| `whispy-client` | `crates/client/` | Thin sync client invoked from Hyprland binds. |

### Daemon modules (`crates/daemon/src/`)

| Module | Responsibility |
|--------|----------------|
| `main.rs` | CLI args (clap), config load, JSON logging init, signal handling, wires everything, runs the socket server. Also routes the `setup` subcommand. |
| `app.rs` | `App` — maps socket commands to the capture→transcribe→filter→inject flow; owns the active capture and per-recording `StartCtx`. |
| `server.rs` | Unix-socket server: line-based JSON (`Cmd` in, `Resp` out), polls a shutdown flag for clean SIGTERM exit. |
| `audio.rs` | PipeWire capture via `pw-record` (raw s16le mono), RMS metering, gain, clip limits, trailing-silence auto-stop, peak normalization. |
| `whisper.rs` | Spawns and supervises the `whisper-server` child; waits for its port; a monitor thread in `main` polls `is_alive` and `restart`s it if it dies; kills it on shutdown. |
| `stt.rs` | HTTP client to whisper-server's `/inference` (multipart WAV, `verbose_json`); parses transcript + confidence signals. |
| `filter.rs` | Hallucination filter: confidence thresholds → punctuation-only rejection → exact + fuzzy (`strsim`) blacklist match. |
| `pipeline.rs` | Post-transcription text pipeline: corrections → snippets → AI workflow selection + LLM transform. Never fails (falls back to raw text). |
| `llm.rs` | Minimal OpenAI-compatible chat client (local ollama by default) for AI workflows. |
| `inject.rs` | Text injection: `paste` mode (`wl-copy` + `ydotool` Ctrl+V, clipboard save/restore) or `type` mode (`wtype`). |
| `state.rs` | `StatePublisher` writes `state.json` atomically (tmp + rename); `Status` keeps in-memory + on-disk snapshot in sync and flashes transient states. |
| `config.rs` | TOML config; built-in defaults embedded via `include_str!`; `Config::validate` rejects bad config at startup. |
| `setup.rs` | `whispy-daemon setup` — idempotent one-shot bootstrap (build whisper.cpp, fetch model, ydotool perms, seed config, install systemd unit, opt-in Quickshell). |
| `stats.rs` | `whispy-daemon stats` — read-only summary of `transcripts.jsonl` (accepted vs dropped by reason) for filter tuning. |

## IPC protocol

The client talks to the daemon over a Unix socket at
`$XDG_RUNTIME_DIR/whispy/whispy.sock` using a **line-based JSON** protocol: one
`Cmd` per line in, one `Resp` per line out. Types live in `crates/common/src/lib.rs`.

- Commands (`#[serde(tag = "cmd", rename_all = "lowercase")]`): `start` (optional
  `workflow`), `stop`, `cancel`, `status`, `ping`. `toggle` is client-side only
  (it queries `status`, then sends `start` or `stop`).
- `StateSnapshot` (also written to `state.json` for the UI) keeps `error_kind` /
  `error_message` always present (null when absent) so the UI JSON schema is stable.

**When changing the protocol, update `whispy-common` first** — both binaries depend
on it, and the QML pill reads the `state.json` shape.

## Configuration

- Authoritative defaults: `config/default.toml`, embedded in the binary via
  `include_str!`. The hallucination blacklist is `config/hallucinations.toml`.
- Runtime override: `$XDG_CONFIG_HOME/whispy/config.toml` (or `--config FILE`).
- Tilde paths (`~/...`) in config are expanded by the daemon.
- Logs: JSON lines to `$XDG_STATE_HOME/whispy/daemon.log`; level via `RUST_LOG`
  (default `info`). whisper-server output goes to `whisper-server.log` in the state dir.

If you add a config field, update both the struct in `config.rs` and
`config/default.toml` (with an explanatory comment), and keep `#[serde(default)]`
on optional sections so existing user configs keep parsing.

## Development workflow

```sh
cargo build --workspace --locked          # build both binaries
cargo test --workspace --locked           # run tests
cargo fmt --all                           # format (CI checks --check)
cargo clippy --workspace --all-targets -- -D warnings   # CI treats warnings as errors

cargo run -p whispy-daemon                # run the daemon (needs whisper-server + model)
cargo run -p whispy-client -- status      # talk to a running daemon
```

CI (`.github/workflows/ci.yml`) runs, in order, on every push to `main` and every PR:
`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo build --workspace --locked`, `cargo test --workspace --locked`. **Match this
locally before pushing.** `--locked` means `Cargo.lock` must be committed and current.

Releases (`.github/workflows/release.yml`) fire on `v*` tags: build, package the two
binaries (not whisper-server/model), publish a GitHub release, and push to the AUR
(`packaging/aur/PKGBUILD`).

## Conventions

- **Edition 2024**, MSRV 1.85 — don't use features newer than that.
- Every module opens with a `//!` doc comment explaining its role; public items get
  `///` docs. Match this density.
- The daemon is **threaded, not async** — plain `std::thread`, `Mutex`, `Arc`,
  `AtomicBool`. Don't introduce tokio/async-std.
- HTTP uses `ureq` (sync). JSON via `serde`/`serde_json`. CLI via `clap` derive. Config
  via `toml`. Logging via `tracing` + `tracing-subscriber` (JSON). Keep the dependency
  set lean; new deps go in `[workspace.dependencies]` and are referenced with
  `{ workspace = true }`.
- Tests are inline `#[cfg(test)] mod tests` in the same file. Favor pure, testable
  helpers (e.g. `pick_workflow`, `pick_wayland_socket`) factored out of side-effecting code.
- External tools are invoked as subprocesses: `pw-record`, `wl-copy`/`wl-paste`,
  `ydotool`, `wtype`, `whisper-server`. Best-effort steps log via `tracing::warn!` and
  continue; only the critical step is a hard error.
- Commit messages follow Conventional Commits (`feat:`, `fix:`, `docs:`, `refactor:`,
  `style:`, `ci:`, optional scope like `fix(ci):`).

## Where things live

- `ui/quickshell/` — QML pill overlay (`Pill.qml`, `PillPanel.qml`, `Tokens.qml`); reads `state.json`.
- `systemd/whispy-daemon.service` — user unit (installed by `setup`).
- `packaging/aur/` — AUR `PKGBUILD`s (`whispy` release + `whispy-git`).
- `scripts/` — `benchmark.sh` (STT latency per model), `setup-ydotool.sh`.
- `docs/` — `hyprland-setup.md`, `stt-benchmark.md`, `spike-fork-vs-build.md`.
- `install.sh` — distro-aware installer (AUR on Arch, build-from-source elsewhere).
- `.agents/skills/` (symlinked as `.claude/`) — bundled agent skills.

## Gotchas

- A systemd user service started before the compositor may lack `WAYLAND_DISPLAY`;
  `main.rs` discovers the lowest-numbered `wayland-N` socket so injection tools work.
- `state.json` is written atomically (tmp + rename) — never write it in place; the UI
  polls it and must never see a half-written file.
- whisper-server is launched/supervised by the daemon and respawned in-process by a monitor
  thread (`main.rs`) if it dies. Don't shell out to start it elsewhere. STT requests are
  bounded by `stt.timeout_secs` so a hung server surfaces an error instead of stalling.
- The mic records quiet (~20–25 dB low); audio is peak-normalized before transcription
  and the first ~50 ms are dropped to skip the codec stream-start transient. Keep this in
  mind when touching `audio.rs`.
