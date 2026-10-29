//! Turning a transcript into the shared [`inference::Response`] (the
//! whisper-server `verbose_json` subset whispy-daemon parses), and decoding the
//! uploaded WAV. Pure, so the wire format is unit-tested.

use std::io::Cursor;

use whispy_common::inference::{self, SAMPLE_RATE, Segment};

/// What the engine produced, reduced to what the reply needs.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub text: String,
    /// ISO code the model detected, if any.
    pub language: Option<String>,
    /// Per-token probabilities (may be empty or contain NaN for "unknown").
    pub token_probs: Vec<f32>,
}

/// Build the `verbose_json`-shaped reply. Confidence is the mean natural log of
/// the token probabilities (whisper's `avg_logprob` scale). Parakeet has no
/// "no speech" head: it returns empty text on silence, so `no_speech_prob` is 0
/// and the daemon's empty-transcript filter does the dropping.
pub fn verbose_json(outcome: &Outcome, hinted_language: Option<&str>) -> inference::Response {
    let text = outcome.text.trim();
    let detected_language = outcome
        .language
        .as_deref()
        .or(hinted_language)
        .map(language_name);
    let segments = if text.is_empty() {
        Vec::new()
    } else {
        vec![Segment {
            text: text.to_string(),
            avg_logprob: avg_logprob(&outcome.token_probs),
            no_speech_prob: 0.0,
        }]
    };
    inference::Response {
        text: text.to_string(),
        detected_language,
        segments,
    }
}

/// Mean `ln(p)` over the finite, positive token probabilities (0.0 = certain,
/// also the value when none are known).
#[expect(
    clippy::cast_precision_loss,
    reason = "a token count far below f32's exact-integer range"
)]
pub fn avg_logprob(probs: &[f32]) -> f32 {
    let logs: Vec<f32> = probs
        .iter()
        .filter(|p| p.is_finite() && **p > 0.0)
        .map(|p| p.min(1.0).ln())
        .collect();
    if logs.is_empty() {
        return 0.0;
    }
    logs.iter().sum::<f32>() / logs.len() as f32
}

/// whisper reports languages by English name (`"italian"`); map the ISO codes
/// transcribe.cpp returns so logs and the transcript history stay consistent.
pub fn language_name(code: &str) -> String {
    let name = match code.to_ascii_lowercase().as_str() {
        "it" => "italian",
        "en" => "english",
        "es" => "spanish",
        "fr" => "french",
        "de" => "german",
        "pt" => "portuguese",
        "nl" => "dutch",
        "pl" => "polish",
        "ru" => "russian",
        "uk" => "ukrainian",
        "cs" => "czech",
        "sv" => "swedish",
        "da" => "danish",
        "fi" => "finnish",
        "el" => "greek",
        "hu" => "hungarian",
        "ro" => "romanian",
        _ => return code.to_string(),
    };
    name.to_string()
}

/// The language hint to pass the model: `None` for whisper's `auto`.
pub fn language_hint(field: Option<&str>) -> Option<String> {
    field
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.eq_ignore_ascii_case("auto"))
        .map(str::to_ascii_lowercase)
}

/// Decode a 16 kHz mono WAV (16-bit int or 32-bit float) into `[-1, 1]` samples.
pub fn decode_wav(bytes: &[u8]) -> Result<Vec<f32>, String> {
    let mut reader = hound::WavReader::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let spec = reader.spec();
    if spec.sample_rate != SAMPLE_RATE || spec.channels != 1 {
        return Err(format!(
            "expected {SAMPLE_RATE} Hz mono, got {} Hz × {} channels",
            spec.sample_rate, spec.channels
        ));
    }
    match (spec.sample_format, spec.bits_per_sample) {
        (hound::SampleFormat::Int, 16) => reader
            .samples::<i16>()
            .map(|s| s.map(|s| f32::from(s) / 32768.0).map_err(|e| e.to_string()))
            .collect(),
        (hound::SampleFormat::Float, 32) => reader
            .samples::<f32>()
            .map(|s| s.map_err(|e| e.to_string()))
            .collect(),
        (format, bits) => Err(format!(
            "unsupported WAV sample format {format:?}/{bits}-bit"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav_i16(samples: &[i16], rate: u32) -> Vec<u8> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut cursor = Cursor::new(Vec::new());
        let mut w = hound::WavWriter::new(&mut cursor, spec).unwrap();
        for &s in samples {
            w.write_sample(s).unwrap();
        }
        w.finalize().unwrap();
        cursor.into_inner()
    }

    #[test]
    fn reply_has_the_fields_the_daemon_reads() {
        let out = Outcome {
            text: " Ciao, come stai? ".into(),
            language: Some("it".into()),
            token_probs: vec![1.0, 0.5],
        };
        let r = verbose_json(&out, None);
        assert_eq!(r.text, "Ciao, come stai?");
        assert_eq!(r.detected_language.as_deref(), Some("italian"));
        let seg = &r.segments[0];
        assert_eq!(seg.text, "Ciao, come stai?");
        assert!((seg.avg_logprob - 0.5f32.ln() / 2.0).abs() < 1e-6);
        assert!(seg.no_speech_prob.abs() < f32::EPSILON);
    }

    #[test]
    fn silence_is_empty_text_with_no_segments() {
        let out = Outcome {
            text: "  ".into(),
            language: None,
            token_probs: vec![],
        };
        let r = verbose_json(&out, Some("it"));
        assert_eq!(r.text, "");
        assert_eq!(r.segments, []);
        assert_eq!(
            r.detected_language.as_deref(),
            Some("italian"),
            "falls back to the hint"
        );
    }

    #[test]
    fn avg_logprob_ignores_unknown_probabilities() {
        assert!(avg_logprob(&[]).abs() < f32::EPSILON);
        assert!(avg_logprob(&[f32::NAN, 0.0, 1.0]).abs() < f32::EPSILON);
        assert!(avg_logprob(&[0.25]) < -1.0);
    }

    #[test]
    fn language_hint_maps_auto_to_none() {
        assert_eq!(language_hint(Some("auto")), None);
        assert_eq!(language_hint(Some("")), None);
        assert_eq!(language_hint(None), None);
        assert_eq!(language_hint(Some("IT")).as_deref(), Some("it"));
        assert_eq!(language_name("xx"), "xx");
    }

    #[test]
    fn decodes_the_daemons_wav() {
        let pcm = decode_wav(&wav_i16(&[0, 16384, -32768], SAMPLE_RATE)).unwrap();
        assert_eq!(pcm, [0.0, 0.5, -1.0]);
        decode_wav(&wav_i16(&[0], 44_100)).unwrap_err();
        decode_wav(b"not a wav").unwrap_err();
    }
}
