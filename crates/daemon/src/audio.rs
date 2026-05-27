//! PipeWire audio capture via `pw-record`, with RMS metering.
//!
//! Implemented in Step 3: spawn `pw-record --rate 16000 --channels 1 --format s16 -`,
//! read PCM frames, publish RMS every `audio.rms_interval_ms`, enforce clip
//! min/max durations, and return the captured buffer on stop.
//!
//! NOTE (from the Step 1 benchmark): the mic (USB PCM2902) records ~20-25 dB too
//! quiet at PipeWire volume 1.00, which dropped quiet sentence tails. Apply a
//! configurable digital gain / peak-normalization to the buffer before STT.
//! See `docs/stt-benchmark.md` ("Caveat — input gain").
