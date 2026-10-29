# Glossary

Terms used in whispy's code, config, and docs.

**Dictation session** — one push-to-talk dictation from `start` to the
success/error flash: recording, then (after `stop`, trailing-silence auto-stop,
or the max-duration cap) transcription, the hallucination filter, and delivery
(post-processing + injection) into the window focused at `start`. Only one
recording is live at a time, but an earlier clip may still be transcribing.
`cancel` discards the recording and any clip not yet being injected. See `crates/daemon/src/app.rs`.

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
