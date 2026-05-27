//! whisper-server HTTP client.
//!
//! Implemented in Step 4: POST the captured WAV to `/inference` with
//! `response_format=verbose_json`, returning the transcript plus per-segment
//! `avg_logprob`, `no_speech_prob`, and `compression_ratio` for the filter.
