//! The transcript log: `transcripts.jsonl` in the state dir, one JSON [`Record`]
//! per transcribed clip (accepted or dropped by the filter).
//!
//! The daemon appends to it for offline calibration of the filter; `stats` and the
//! settings History tab read it back. This module owns everything about the file:
//! the record shape, its location, size-capped rotation (one `.1` generation, see
//! [`config::rotate_if_larger`]), appending, and reading across both generations.
//! Reading is tolerant: blank or malformed lines are skipped and missing fields
//! take their defaults, so logs written by older versions keep parsing.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

use crate::config;
use crate::stt::Transcription;

/// File name of the current generation inside the log's directory.
const FILE_NAME: &str = "transcripts.jsonl";

/// One line of `transcripts.jsonl`. The field names are the on-disk format.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Record {
    /// Unix time (seconds) the clip was logged.
    pub timestamp: f64,
    /// The transcript as the STT server returned it (before post-processing).
    pub text: String,
    /// Whether the filter accepted the transcript (it was then injected).
    pub accepted: bool,
    /// Why the filter dropped it (`filter::DropReason::kind`), when it did.
    pub drop_reason: Option<String>,
    /// Mean per-segment log-probability reported by the STT server.
    #[serde(deserialize_with = "null_as_zero")]
    pub avg_logprob: f32,
    /// Most silence-like segment's no-speech probability.
    #[serde(deserialize_with = "null_as_zero")]
    pub no_speech_prob: f32,
    /// Language the STT server detected, if it reported one.
    pub language: Option<String>,
}

impl Record {
    /// The record for transcription `t`, timestamped now; `drop_reason` is `None`
    /// when the filter accepted it.
    pub fn new(t: &Transcription, drop_reason: Option<&str>) -> Self {
        Self {
            timestamp: crate::state::now(),
            text: t.text.clone(),
            accepted: drop_reason.is_none(),
            drop_reason: drop_reason.map(String::from),
            avg_logprob: t.avg_logprob,
            no_speech_prob: t.no_speech_prob,
            language: t.language.clone(),
        }
    }
}

/// A `null` float (non-finite values serialize as `null`) reads as `0.0`
/// rather than discarding the whole line.
fn null_as_zero<'de, D: Deserializer<'de>>(d: D) -> Result<f32, D::Error> {
    Ok(Option::<f32>::deserialize(d)?.unwrap_or_default())
}

/// Handle on a transcript log: `transcripts.jsonl` plus its rotated
/// `transcripts.jsonl.1` in one directory. Cheap to build; holds no open file.
#[derive(Debug, Clone)]
pub struct TranscriptLog {
    path: PathBuf,
    max_bytes: u64,
}

impl TranscriptLog {
    /// The daemon's log, in [`config::state_dir`].
    pub fn open() -> Self {
        Self::in_dir(&config::state_dir())
    }

    /// The log in `dir`, capped at [`config::TRANSCRIPTS_MAX_BYTES`] per generation.
    pub fn in_dir(dir: &Path) -> Self {
        Self::capped(dir, config::TRANSCRIPTS_MAX_BYTES)
    }

    fn capped(dir: &Path, max_bytes: u64) -> Self {
        Self {
            path: dir.join(FILE_NAME),
            max_bytes,
        }
    }

    /// Path of the current generation (for messages).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append `record` as one line. The file holds dictated text, so it is capped:
    /// once past the cap it first rotates to `.1`, replacing the older generation.
    /// A failed rotation is logged and appending continues; the error returned is
    /// the write's.
    pub fn append(&self, record: &Record) -> std::io::Result<()> {
        if let Err(e) = config::rotate_if_larger(&self.path, self.max_bytes) {
            tracing::warn!(error = %e, "failed to rotate transcript log");
        }
        let line = serde_json::to_string(record)?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{line}")
    }

    /// Every readable record across both generations, oldest first, or `None`
    /// when neither file exists.
    pub fn read(&self) -> Option<Vec<Record>> {
        let files: Vec<std::fs::File> = [config::rotated(&self.path), self.path.clone()]
            .iter()
            .filter_map(|p| std::fs::File::open(p).ok())
            .collect();
        if files.is_empty() {
            return None;
        }
        let records = files
            .into_iter()
            .flat_map(|f| BufReader::new(f).split(b'\n').map_while(Result::ok))
            .filter_map(|line| parse(&line))
            .collect();
        Some(records)
    }

    /// The newest `n` records, newest first, spanning both generations (so the
    /// list stays full right after a rotation). Empty when there is no log.
    pub fn newest(&self, n: usize) -> Vec<Record> {
        let mut records = self.read().unwrap_or_default();
        records.reverse();
        records.truncate(n);
        records
    }
}

/// One log line, or `None` when it is blank or malformed.
fn parse(line: &[u8]) -> Option<Record> {
    let line = line.trim_ascii();
    if line.is_empty() {
        return None;
    }
    serde_json::from_slice(line).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty directory for one test.
    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("whispy-transcripts-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn record(text: &str) -> Record {
        Record {
            text: text.to_string(),
            accepted: true,
            ..Record::default()
        }
    }

    fn texts(records: &[Record]) -> Vec<&str> {
        records.iter().map(|r| r.text.as_str()).collect()
    }

    #[test]
    fn append_then_read_back() {
        let dir = temp_dir("roundtrip");
        let log = TranscriptLog::in_dir(&dir);
        assert_eq!(log.read(), None);
        assert_eq!(log.newest(5), Vec::new());

        let t = Transcription {
            text: "hello world".into(),
            avg_logprob: -0.25,
            no_speech_prob: 0.5,
            language: Some("en".into()),
        };
        let accepted = Record::new(&t, None);
        let dropped = Record::new(&t, Some("hallucination"));
        log.append(&accepted).unwrap();
        log.append(&dropped).unwrap();

        let read = log.read().unwrap();
        assert_eq!(read.len(), 2);
        for (got, want) in read.iter().zip([&accepted, &dropped]) {
            // serde_json's default float parsing may be off in the last ulp.
            assert!((got.timestamp - want.timestamp).abs() < 1e-3);
            let got = Record {
                timestamp: want.timestamp,
                ..got.clone()
            };
            assert_eq!(&got, want);
        }
        assert!(accepted.accepted && accepted.timestamp > 0.0);
        assert!(!dropped.accepted);
        assert_eq!(dropped.drop_reason.as_deref(), Some("hallucination"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn on_disk_field_names_are_stable() {
        let dir = temp_dir("fields");
        let log = TranscriptLog::in_dir(&dir);
        log.append(&record("hi")).unwrap();
        let line = std::fs::read_to_string(log.path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "accepted",
                "avg_logprob",
                "drop_reason",
                "language",
                "no_speech_prob",
                "text",
                "timestamp"
            ]
        );
        // Absent options are written as null, as before.
        assert!(v["drop_reason"].is_null() && v["language"].is_null());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rotation_then_reads_both_generations_oldest_first() {
        let dir = temp_dir("rotate");
        // Tiny cap: every append after the first rotates the previous line out.
        let log = TranscriptLog::capped(&dir, 10);
        for text in ["one", "two", "three"] {
            log.append(&record(text)).unwrap();
        }
        assert!(config::rotated(log.path()).exists());
        // "one" was replaced by the second rotation; one generation is kept.
        assert_eq!(texts(&log.read().unwrap()), ["two", "three"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn newest_spans_both_generations() {
        // Regression: History read only the current file, so right after a
        // rotation it showed just the lines written since.
        let dir = temp_dir("newest");
        let log = TranscriptLog::in_dir(&dir);
        std::fs::write(
            config::rotated(log.path()),
            "{\"text\":\"a\"}\n{\"text\":\"b\"}\n{\"text\":\"c\"}\n",
        )
        .unwrap();
        std::fs::write(log.path(), "{\"text\":\"d\"}\n").unwrap();

        assert_eq!(texts(&log.newest(3)), ["d", "c", "b"]);
        assert_eq!(texts(&log.newest(10)), ["d", "c", "b", "a"]);

        // Only the rotated generation left (nothing logged since rotating).
        std::fs::remove_file(log.path()).unwrap();
        assert_eq!(texts(&log.newest(2)), ["c", "b"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn old_and_partial_lines_parse_with_defaults() {
        let r = parse(br#"{"accepted":false,"drop_reason":"low_confidence"}"#).unwrap();
        assert!(!r.accepted);
        assert_eq!(r.drop_reason.as_deref(), Some("low_confidence"));
        assert_eq!(r.text, "");
        assert!(r.avg_logprob.abs() < f32::EPSILON);

        let r = parse(br#"{"text":"hi","avg_logprob":null,"extra":1}"#).unwrap();
        assert_eq!(r.text, "hi");
        assert!(!r.accepted);
        assert!(r.avg_logprob.abs() < f32::EPSILON);
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let dir = temp_dir("malformed");
        let log = TranscriptLog::in_dir(&dir);
        let mut raw = b"\n   \nnot json\n{\"text\":\"ok\"}\n{\"text\":\"\xff\"}\n".to_vec();
        raw.extend_from_slice(b"{\"accepted\":\"yes\"}\n{\"text\":\"last\"}");
        std::fs::write(log.path(), raw).unwrap();
        assert_eq!(texts(&log.read().unwrap()), ["ok", "last"]);
        std::fs::remove_dir_all(&dir).ok();
    }
}
