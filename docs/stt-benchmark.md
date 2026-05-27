# STT Benchmark

> Filled in during **Step 1**. Run `scripts/benchmark.sh` after building whisper.cpp
> (Vulkan) and recording the three voice samples.

## Method

- Backend: whisper.cpp Vulkan (`whisper-cli` / `whisper-server`) on RX 9070 XT (gfx1201).
- Samples (16 kHz mono): IT pure 30 s, IT+EN mixed 30 s, IT fast 15 s.
- Metrics: inference latency (wall clock) per model; WER assessed manually against a reference.

## Results

| Model | Quant | Latency (IT 30s) | Latency (mixed 30s) | Latency (fast 15s) | WER (manual) | Notes |
|-------|-------|------------------|---------------------|--------------------|--------------|-------|
| tiny  |       | _TBD_ | _TBD_ | _TBD_ | _TBD_ | |
| small |       | _TBD_ | _TBD_ | _TBD_ | _TBD_ | |
| medium | q5_0 | _TBD_ | _TBD_ | _TBD_ | _TBD_ | CPU fallback candidate |
| large-v3-turbo | q5_0 | _TBD_ | _TBD_ | _TBD_ | _TBD_ | primary candidate |

## Decision

_TBD — record the chosen model in `config/default.toml` (`[stt].model` / `model_path`)._
