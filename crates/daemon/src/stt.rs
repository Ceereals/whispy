//! whisper-server HTTP client.
//!
//! POSTs the captured audio to whisper-server's `/inference` endpoint as a
//! `multipart/form-data` WAV upload (`response_format=verbose_json`) and parses
//! the response into a [`Transcription`]: the transcript text plus the
//! per-segment confidence signals (`avg_logprob`, `no_speech_prob`) the filter
//! uses to reject hallucinations.

use std::fmt;
use std::io::Cursor;
use std::time::Duration;

use tracing::{debug, warn};

/// Multipart boundary marker for the `/inference` request body.
const BOUNDARY: &str = "----whispyFormBoundary7MA4YWxkTrZu0gW";

/// HTTP client for a running whisper-server instance.
pub struct SttClient {
    base_url: String,
    language: String,
    prompt: Option<String>,
    agent: ureq::Agent,
}

/// The result of transcribing one audio clip.
#[derive(Debug, Clone)]
pub struct Transcription {
    /// Transcript text with internal whitespace runs collapsed to single spaces.
    pub text: String,
    /// Mean of per-segment `avg_logprob` (0.0 if no segments).
    pub avg_logprob: f32,
    /// Max of per-segment `no_speech_prob`, i.e. the most "silence-like" segment (0.0 if none).
    pub no_speech_prob: f32,
    /// whisper's `detected_language`, if present.
    pub language: Option<String>,
}

/// Errors raised while talking to whisper-server.
#[derive(Debug)]
pub enum SttError {
    /// Transport failure or non-2xx HTTP status.
    Http(String),
    /// Response body was not valid `verbose_json`.
    Parse(String),
    /// WAV encoding of the input samples failed.
    Audio(String),
}

impl fmt::Display for SttError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SttError::Http(msg) => write!(f, "whisper-server HTTP error: {msg}"),
            SttError::Parse(msg) => write!(f, "failed to parse whisper-server response: {msg}"),
            SttError::Audio(msg) => write!(f, "failed to encode audio: {msg}"),
        }
    }
}

impl std::error::Error for SttError {}

/// The subset of whisper-server's `verbose_json` response we care about.
#[derive(serde::Deserialize)]
struct Response {
    #[serde(default)]
    text: String,
    #[serde(default)]
    segments: Vec<Segment>,
    #[serde(default)]
    detected_language: Option<String>,
}

/// One transcription segment; we only read the confidence fields.
#[derive(serde::Deserialize)]
struct Segment {
    #[serde(default)]
    avg_logprob: f32,
    #[serde(default)]
    no_speech_prob: f32,
}

impl SttClient {
    /// Build from config (base URL = `http://host:port`).
    pub fn new(cfg: &crate::config::Stt, prompt: Option<String>) -> Self {
        let prompt = prompt.filter(|p| !p.trim().is_empty());
        Self {
            base_url: format!("http://{}:{}", cfg.host, cfg.port),
            language: cfg.language.clone(),
            prompt,
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(cfg.timeout_secs))
                .build(),
        }
    }

    /// Transcribe 16 kHz mono signed-16-bit PCM. Encodes it to a WAV in memory and
    /// POSTs to whisper-server's `/inference` endpoint.
    pub fn transcribe(&self, samples: &[i16]) -> Result<Transcription, SttError> {
        let wav = encode_wav(samples)?;
        debug!(
            samples = samples.len(),
            wav_bytes = wav.len(),
            "encoded WAV for inference"
        );

        let body = build_multipart(&self.language, self.prompt.as_deref(), &wav);
        let url = format!("{}/inference", self.base_url);

        let resp = self
            .agent
            .post(&url)
            .set(
                "Content-Type",
                &format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .send_bytes(&body)
            .map_err(|e| match e {
                ureq::Error::Status(code, resp) => {
                    let detail = resp.into_string().unwrap_or_default();
                    SttError::Http(format!("{url} returned status {code}: {detail}"))
                }
                ureq::Error::Transport(t) => {
                    SttError::Http(format!("transport error contacting {url}: {t}"))
                }
            })?;

        let text = resp
            .into_string()
            .map_err(|e| SttError::Http(format!("failed to read response body: {e}")))?;

        let parsed: Response =
            serde_json::from_str(&text).map_err(|e| SttError::Parse(e.to_string()))?;

        Ok(aggregate(parsed))
    }
}

/// Encode signed-16-bit mono PCM at 16 kHz into an in-memory WAV file.
fn encode_wav(samples: &[i16]) -> Result<Vec<u8>, SttError> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer =
            hound::WavWriter::new(&mut cursor, spec).map_err(|e| SttError::Audio(e.to_string()))?;
        for &sample in samples {
            writer
                .write_sample(sample)
                .map_err(|e| SttError::Audio(e.to_string()))?;
        }
        writer
            .finalize()
            .map_err(|e| SttError::Audio(e.to_string()))?;
    }
    Ok(cursor.into_inner())
}

/// Assemble the `multipart/form-data` body whisper-server expects: the text
/// fields (`response_format`, `language`, `temperature`) and the WAV `file` part.
fn build_multipart(language: &str, prompt: Option<&str>, wav: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();

    let mut text_field = |name: &str, value: &str| {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    };

    text_field("response_format", "verbose_json");
    text_field("language", language);
    text_field("temperature", "0.0");
    if let Some(p) = prompt {
        text_field("prompt", p);
    }

    // File part: the WAV bytes go in raw, not as UTF-8.
    body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"clip.wav\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: audio/wav\r\n\r\n");
    body.extend_from_slice(wav);
    body.extend_from_slice(b"\r\n");

    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

/// Collapse the raw response into the aggregate confidence signals the filter uses.
fn aggregate(resp: Response) -> Transcription {
    let avg_logprob = if resp.segments.is_empty() {
        0.0
    } else {
        let sum: f32 = resp.segments.iter().map(|s| s.avg_logprob).sum();
        sum / resp.segments.len() as f32
    };

    let no_speech_prob = resp
        .segments
        .iter()
        .map(|s| s.no_speech_prob)
        .fold(0.0_f32, f32::max);

    if resp.segments.is_empty() {
        warn!("whisper-server returned no segments");
    }

    Transcription {
        text: normalize_whitespace(&resp.text),
        avg_logprob,
        no_speech_prob,
        language: resp.detected_language,
    }
}

/// Collapse internal whitespace runs into single spaces and trim the ends.
///
/// whisper-server's `verbose_json` `text` field joins per-segment text with
/// newlines, so a multi-segment clip arrives with embedded `\n`. Dictation
/// output should be a single flowing line, so we fold every whitespace run
/// (newlines, tabs, repeated spaces) down to one space.
fn normalize_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_riff_wav() {
        let samples = [0_i16, 1000, -1000, 32767, -32768];
        let wav = encode_wav(&samples).expect("WAV encoding must succeed");
        assert_eq!(&wav[0..4], b"RIFF", "WAV must start with RIFF magic");
        assert_eq!(&wav[8..12], b"WAVE", "RIFF type must be WAVE");
    }

    #[test]
    fn aggregates_empty_segments_to_zero() {
        let resp = Response {
            text: "  hello world  ".to_string(),
            segments: Vec::new(),
            detected_language: Some("en".to_string()),
        };
        let t = aggregate(resp);
        assert_eq!(t.text, "hello world");
        assert_eq!(t.avg_logprob, 0.0);
        assert_eq!(t.no_speech_prob, 0.0);
        assert_eq!(t.language.as_deref(), Some("en"));
    }

    #[test]
    fn collapses_internal_newlines_and_whitespace() {
        let resp = Response {
            text: "primo segmento\n secondo segmento\nterzo".to_string(),
            segments: vec![Segment {
                avg_logprob: -0.2,
                no_speech_prob: 0.1,
            }],
            detected_language: None,
        };
        let t = aggregate(resp);
        assert_eq!(t.text, "primo segmento secondo segmento terzo");
    }

    #[test]
    fn aggregates_mean_and_max() {
        let resp = Response {
            text: "x".to_string(),
            segments: vec![
                Segment {
                    avg_logprob: -0.2,
                    no_speech_prob: 0.1,
                },
                Segment {
                    avg_logprob: -0.4,
                    no_speech_prob: 0.9,
                },
            ],
            detected_language: None,
        };
        let t = aggregate(resp);
        assert!((t.avg_logprob - -0.3).abs() < 1e-6);
        assert!((t.no_speech_prob - 0.9).abs() < 1e-6);
    }

    #[test]
    fn multipart_contains_required_fields() {
        let body = build_multipart("auto", None, b"WAVDATA");
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("name=\"response_format\""));
        assert!(s.contains("verbose_json"));
        assert!(s.contains("name=\"language\""));
        assert!(s.contains("name=\"temperature\""));
        assert!(s.contains("filename=\"clip.wav\""));
        assert!(s.contains("Content-Type: audio/wav"));
        assert!(s.contains("WAVDATA"));
        assert!(s.ends_with(&format!("--{BOUNDARY}--\r\n")));
    }

    #[test]
    fn multipart_includes_prompt_when_present() {
        let body = build_multipart("auto", Some("Hyprland, Quickshell"), b"WAV");
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("name=\"prompt\""));
        assert!(s.contains("Hyprland, Quickshell"));
    }
}
