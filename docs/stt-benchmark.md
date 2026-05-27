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

## Results (2026-05-27)

Latency = wall clock for `whisper-cli` (includes ~125 ms model load). Clips are longer than a
typical ~5 s PTT clip, so real-world latency is lower. WER is qualitative (no exact reference):
the mixed clip targets *"apriamo VSCode … Next.js … deploy su Vercel … Elasticsearch … query …
scaffolding … parecchio effort … fiducioso"*.

| Model | Quant | it-pure 34s | mixed 31s | it-fast 13s | Code-switch quality |
|-------|-------|-------------|-----------|-------------|---------------------|
| tiny  | —     | 0.30 s | 0.31 s | 0.20 s | ❌ poor — "Appliamo o vuoi secod", "subversse", trailing "Ciao!" hallucination |
| small | —     | 0.77 s | 0.65 s | 0.39 s | ⚠️ ok — got effort/query/fiducioso, but "ovs code"/"versel", no tech-term casing |
| medium | q5_0 | 1.31 s | 1.27 s | 0.69 s | ✅ strong — VSCode/Next.js/Vercel/elasticsearch; most complete IT; mixed tail mangled |
| **large-v3-turbo** | **q5_0** | **0.82 s** | **0.86 s** | **0.52 s** | ✅ strong — Next.js/Vercel/Elasticsearch; **faster than medium**; "WSCode", dropped a quiet tail clause |

## Decision

**`large-v3-turbo-q5_0`** (primary), **`medium-q5_0`** (CPU/quality fallback). Set in
`config/default.toml`.

Rationale: large-v3-turbo is **both faster** (fewer decoder layers than medium) **and** accurate
on IT/EN code-switching, getting tech-term casing right (Next.js, Vercel, Elasticsearch). All
candidates sit well inside the 1.5 s budget for ~5 s clips.

### Caveat — input gain

The recordings came in ~20–25 dB too quiet (mean ≈ −48 dB, peak ≈ −27 dB) and were loudnorm'd to
≈ −18 dB mean / −2 dB peak for this benchmark. The remaining errors cluster at **quiet sentence
tails** (dropped clauses, "fiducioso" → "fissime"), consistent with low SNR amplified by
normalization. The mic is a USB PCM2902 codec already at PipeWire volume 1.00 (hardware-limited),
so **Step 3 must apply configurable digital gain / normalization** to the captured buffer before
sending it to whisper-server. A re-record at proper gain would give a cleaner WER but does not
change the model choice.
