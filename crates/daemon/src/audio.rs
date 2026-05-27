//! PipeWire audio capture via `pw-record`, with RMS metering.
//!
//! Implemented in Step 3: spawn `pw-record --rate 16000 --channels 1 --format s16 -`,
//! read PCM frames, publish RMS every `audio.rms_interval_ms`, enforce clip
//! min/max durations, and return the captured buffer on stop.
