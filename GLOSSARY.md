# Glossary

Terms used in whispy's code, config, and docs.

**Editable setting** — a config value the settings screen (`whispy-daemon
settings`) changes: microphone, engine, language, typing mode, auto-stop. Each is
declared once with its `[table] key`, its options, and how it is read from the
config and written back into `config.toml` (comments kept). See
`crates/daemon/src/settings/editable.rs`.

**Engine** — the speech-to-text engine chosen by `stt.engine`: `whisper`
(whisper.cpp's whisper-server) or `parakeet` (NVIDIA Parakeet via `whispy-stt` on
transcribe.cpp). The daemon runs it as a supervised STT server behind the same
`/inference` API; the engine owns what differs between them — its server binary
and arguments, its model and where `setup` downloads it, the `setup` step that
installs it, and its vocabulary strategy. See `crates/daemon/src/engine.rs`.

**Inference contract** — the `POST /inference` wire format between whispy-daemon
and its STT server: a `multipart/form-data` request (`response_format`,
`language`, `temperature`, optional `prompt`, and a 16 kHz mono WAV `file`) and
the `verbose_json` subset it gets back (`text`, `detected_language`,
`segments[].text/avg_logprob/no_speech_prob`). whispy-stt and whisper.cpp's
whisper-server both speak it. See `crates/common/src/inference.rs`.

**Transcript log** — `transcripts.jsonl` in the state dir: one record per
transcribed clip, accepted or dropped (with the drop reason and confidence
signals), kept for filter calibration and shown by `stats` and the settings
History tab. Size-capped with one rotated `.1` generation; readers see both.
See `crates/daemon/src/transcripts.rs`.

**Vocabulary correction** — the fuzzy, after-the-fact rewrite of a transcript
against `dictionary.vocabulary`, for engines that can't take the vocabulary as a
decoding prompt (Parakeet). Misheard spellings of a custom word ("Hyperland",
"Omarki") are replaced by it; ordinary words that merely sound alike are left
alone. See `crates/daemon/src/vocab.rs`.
