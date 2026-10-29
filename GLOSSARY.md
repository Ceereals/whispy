# Glossary

Terms used in whispy's code, config, and docs.

**Desktop** — the display server as the daemon sees it: the focused window's
class, the clipboard, and typing into the focused window. One adapter per
display server (Wayland, X11), chosen from `injection.backend` and the session
environment. See `crates/daemon/src/desktop.rs`.

**Engine** — the speech-to-text engine chosen by `stt.engine`: `whisper`
(whisper.cpp's whisper-server) or `parakeet` (NVIDIA Parakeet via `whispy-stt` on
transcribe.cpp). The daemon runs it as a supervised STT server behind the same
`/inference` API; the engine owns what differs between them — its server binary
and arguments, its model and where `setup` downloads it, the `setup` step that
installs it, and its vocabulary strategy. See `crates/daemon/src/engine.rs`.

**Vocabulary correction** — the fuzzy, after-the-fact rewrite of a transcript
against `dictionary.vocabulary`, for engines that can't take the vocabulary as a
decoding prompt (Parakeet). Misheard spellings of a custom word ("Hyperland",
"Omarki") are replaced by it; ordinary words that merely sound alike are left
alone. See `crates/daemon/src/vocab.rs`.
