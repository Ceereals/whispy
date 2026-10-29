//! The `/inference` wire contract between whispy-daemon and its STT server.
//!
//! whispy-daemon POSTs one clip to the STT server's [`PATH`] as a
//! `multipart/form-data` body ([`build_multipart`]): a few text fields plus a
//! 16 kHz mono WAV `file`. The server answers with a [`Response`], the subset of
//! whisper-server's `verbose_json` both sides agree on. whispy-stt implements the
//! server side; whisper.cpp's whisper-server, which we don't control, speaks the
//! same contract, so the field names and part layout here must stay compatible
//! with it.
//!
//! Pure data and one pure encoder, so both ends test against the same bytes.

use serde::{Deserialize, Serialize};

/// HTTP path of the transcription endpoint.
pub const PATH: &str = "/inference";

/// Multipart boundary whispy-daemon uses for the request body.
pub const BOUNDARY: &str = "----whispyFormBoundary7MA4YWxkTrZu0gW";

/// Sample rate of the WAV clip: what every model expects and the daemon always sends.
pub const SAMPLE_RATE: u32 = 16_000;

/// The `response_format` value asking for the [`Response`] shape.
pub const VERBOSE_JSON: &str = "verbose_json";

/// Decoding temperature the daemon sends (greedy, deterministic).
const TEMPERATURE: &str = "0.0";

/// Form field names of the request.
pub mod field {
    /// Desired reply format; always [`super::VERBOSE_JSON`].
    pub const RESPONSE_FORMAT: &str = "response_format";
    /// Spoken language: an ISO code, or `auto` to detect it.
    pub const LANGUAGE: &str = "language";
    /// Decoding temperature.
    pub const TEMPERATURE: &str = "temperature";
    /// Optional decoding prompt (whisper's vocabulary bias).
    pub const PROMPT: &str = "prompt";
    /// The WAV clip.
    pub const FILE: &str = "file";
}

/// The text fields of one `/inference` request (the WAV travels separately).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request<'a> {
    /// Value of the `language` field (`auto` or an ISO code).
    pub language: &'a str,
    /// Value of the `prompt` field; the part is omitted when `None`.
    pub prompt: Option<&'a str>,
}

/// The reply: the subset of `verbose_json` the daemon reads and whispy-stt writes.
///
/// Every field defaults when missing, so any whisper-server-style reply parses.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// The whole transcript (whisper-server joins segments with newlines).
    #[serde(default)]
    pub text: String,
    /// Language the model detected, if any (whisper names it in English: `"italian"`).
    #[serde(default)]
    pub detected_language: Option<String>,
    /// Per-segment text and confidence.
    #[serde(default)]
    pub segments: Vec<Segment>,
}

/// One transcription segment.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    /// This segment's text.
    #[serde(default)]
    pub text: String,
    /// Mean natural-log token probability (0.0 = certain).
    #[serde(default)]
    pub avg_logprob: f32,
    /// Probability the segment is silence rather than speech.
    #[serde(default)]
    pub no_speech_prob: f32,
}

/// Encode the `multipart/form-data` request body: the text fields
/// (`response_format`, `language`, `temperature`, optional `prompt`) followed by
/// the raw `wav` bytes as the `file` part, delimited by `boundary`.
///
/// The `Content-Type` header must name the same boundary:
/// `multipart/form-data; boundary={boundary}`.
pub fn build_multipart(boundary: &str, request: &Request<'_>, wav: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(wav.len() + 512);

    let mut text_field = |name: &str, value: &str| {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    };

    text_field(field::RESPONSE_FORMAT, VERBOSE_JSON);
    text_field(field::LANGUAGE, request.language);
    text_field(field::TEMPERATURE, TEMPERATURE);
    if let Some(prompt) = request.prompt {
        text_field(field::PROMPT, prompt);
    }

    // File part: the WAV bytes go in raw, not as UTF-8.
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"{}\"; filename=\"clip.wav\"\r\n",
            field::FILE
        )
        .as_bytes(),
    );
    body.extend_from_slice(b"Content-Type: audio/wav\r\n\r\n");
    body.extend_from_slice(wav);
    body.extend_from_slice(b"\r\n");

    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn multipart_has_whisper_servers_layout() {
        let body = build_multipart(
            "b",
            &Request {
                language: "it",
                prompt: None,
            },
            b"WAV",
        );
        let expected = "--b\r\nContent-Disposition: form-data; name=\"response_format\"\r\n\r\nverbose_json\r\n\
             --b\r\nContent-Disposition: form-data; name=\"language\"\r\n\r\nit\r\n\
             --b\r\nContent-Disposition: form-data; name=\"temperature\"\r\n\r\n0.0\r\n\
             --b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"clip.wav\"\r\n\
             Content-Type: audio/wav\r\n\r\nWAV\r\n\
             --b--\r\n";
        assert_eq!(String::from_utf8(body).unwrap(), expected);
    }

    #[test]
    fn multipart_includes_prompt_only_when_present() {
        let request = Request {
            language: "auto",
            prompt: Some("Hyprland, Quickshell"),
        };
        let s = String::from_utf8(build_multipart(BOUNDARY, &request, b"")).unwrap();
        assert!(s.contains(
            "name=\"prompt\"\r\n\r\nHyprland, Quickshell\r\n------whispyFormBoundary7MA4YWxkTrZu0gW\r\n"
        ));
        assert!(s.ends_with(&format!("--{BOUNDARY}--\r\n")));
    }

    #[test]
    fn response_serializes_to_the_verbose_json_keys() {
        let resp = Response {
            text: "Ciao".into(),
            detected_language: Some("italian".into()),
            segments: vec![Segment {
                text: "Ciao".into(),
                avg_logprob: -0.5,
                no_speech_prob: 0.0,
            }],
        };
        assert_eq!(
            serde_json::to_value(&resp).unwrap(),
            json!({
                "text": "Ciao",
                "detected_language": "italian",
                "segments": [{ "text": "Ciao", "avg_logprob": -0.5, "no_speech_prob": 0.0 }],
            })
        );
        // No language and no segments still keep every key (null / empty list).
        assert_eq!(
            serde_json::to_value(Response::default()).unwrap(),
            json!({ "text": "", "detected_language": null, "segments": [] })
        );
    }

    #[test]
    fn deserializes_whisper_server_verbose_json() {
        let raw = r#"{
            "task": "transcribe",
            "language": "italian",
            "duration": 1.5,
            "text": " ciao\n mondo",
            "detected_language": "italian",
            "detected_language_probability": 0.98,
            "segments": [
                {"id": 0, "text": " ciao", "start": 0.0, "end": 0.7,
                 "tokens": [1, 2], "temperature": 0.0,
                 "avg_logprob": -0.25, "no_speech_prob": 0.01},
                {"id": 1, "text": " mondo"}
            ]
        }"#;
        let resp: Response = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.text, " ciao\n mondo");
        assert_eq!(resp.detected_language.as_deref(), Some("italian"));
        assert_eq!(
            resp.segments,
            [
                Segment {
                    text: " ciao".into(),
                    avg_logprob: -0.25,
                    no_speech_prob: 0.01,
                },
                Segment {
                    text: " mondo".into(),
                    ..Segment::default()
                },
            ]
        );
    }

    #[test]
    fn missing_fields_default() {
        let resp: Response = serde_json::from_str("{}").unwrap();
        assert_eq!(resp, Response::default());
        let resp: Response = serde_json::from_str(r#"{"text":"x","segments":[{}]}"#).unwrap();
        assert_eq!(resp.segments, [Segment::default()]);
    }
}
