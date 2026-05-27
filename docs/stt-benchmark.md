# STT Benchmark

> Latency/WER table is filled after recording the three voice samples. The
> **environment validation** below is done.

## Method

- Backend: whisper.cpp Vulkan (`whisper-cli` / `whisper-server`) on RX 9070 XT (gfx1201).
- Samples (16 kHz mono): IT pure 30 s, IT+EN mixed 30 s, IT fast 15 s.
- Metrics: inference latency (wall clock) per model; WER assessed manually against a reference.

## Environment validation (2026-05-27)

- whisper.cpp `6dcdd65` built with `-DGGML_VULKAN=1`; `whisper-cli` + `whisper-server` at
  `~/.local/share/whisper.cpp/build/bin/`.
- **Vulkan runs on RDNA4** — device 0 = `AMD Radeon RX 9070 XT (RADV GFX1201)`, matrix cores
  `KHR_coopmat`. No CPU fallback needed (handoff risk #1 cleared). The `radv is not a
  conformant Vulkan implementation` line is RADV's benign "not Khronos-certified" notice.
- Sanity baseline: `large-v3-turbo-q5_0` on the 11 s JFK sample → ~670–880 ms total (encode
  ~145 ms/run). Well inside the 1.5 s budget for ~5 s clips.
- **whisper-server `verbose_json` fields** (drives the Step 4 filter):
  - per-segment: `avg_logprob` ✓, `no_speech_prob` ✓, `temperature`, `tokens`,
    `words[].probability`.
  - **`compression_ratio` is NOT emitted** → compute it in `filter.rs` from the text.
  - top-level bonus: `detected_language`, `detected_language_probability`,
    `language_probabilities` — useful for IT/EN code-switching diagnostics.
  - Filter 1 (`no_speech_prob > 0.6` or `avg_logprob < -1.0`) is fully supported as-is.

## Results

| Model | Quant | Latency (IT 30s) | Latency (mixed 30s) | Latency (fast 15s) | WER (manual) | Notes |
|-------|-------|------------------|---------------------|--------------------|--------------|-------|
| tiny  |       | _TBD_ | _TBD_ | _TBD_ | _TBD_ | |
| small |       | _TBD_ | _TBD_ | _TBD_ | _TBD_ | |
| medium | q5_0 | _TBD_ | _TBD_ | _TBD_ | _TBD_ | CPU fallback candidate |
| large-v3-turbo | q5_0 | _TBD_ | _TBD_ | _TBD_ | _TBD_ | primary candidate |

## Decision

_TBD — record the chosen model in `config/default.toml` (`[stt].model` / `model_path`)._
