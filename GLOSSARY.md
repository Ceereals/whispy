# Glossary

Terms used in whispy's code, config, and docs.

**Engine** — the speech-to-text engine chosen by `stt.engine`: `whisper`
(whisper.cpp's whisper-server) or `parakeet` (NVIDIA Parakeet via `whispy-stt` on
transcribe.cpp). The daemon runs it as a supervised STT server behind the same
`/inference` API; the engine owns what differs between them — its server binary
and arguments, its model and where `setup` downloads it, the `setup` step that
installs it, and its vocabulary strategy. See `crates/daemon/src/engine.rs`.

**Host tools** — what whispy needs installed on the machine: the runtime tools
dictation drives for the display server and injection mode (some with accepted
alternatives, like `xclip` or `xsel` for the X11 clipboard) and the prerequisites
of a Vulkan STT build. One verdict on them is shared by `setup doctor`, the
daemon's startup warning, the `setup` builds, and injection. See
`crates/daemon/src/host.rs`.

**Vocabulary correction** — the fuzzy, after-the-fact rewrite of a transcript
against `dictionary.vocabulary`, for engines that can't take the vocabulary as a
decoding prompt (Parakeet). Misheard spellings of a custom word ("Hyperland",
"Omarki") are replaced by it; ordinary words that merely sound alike are left
alone. See `crates/daemon/src/vocab.rs`.
