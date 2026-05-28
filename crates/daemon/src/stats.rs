//! `whispy-daemon stats`: summarize `transcripts.jsonl` so users can tune the
//! filter thresholds (`fuzzy_ratio`, confidence bounds) without hand-parsing the log.
//!
//! `App::log_transcript` (app.rs) appends one JSON record per clip — accepted or
//! dropped, with the drop reason and confidence signals. This reads that file and
//! prints how many clips were accepted vs dropped, broken down by drop reason.

use std::io::{BufRead, BufReader};
use std::process::ExitCode;

use crate::config;

/// One parsed line of `transcripts.jsonl` (only the fields we summarize).
#[derive(serde::Deserialize)]
struct Record {
    #[serde(default)]
    accepted: bool,
    #[serde(default)]
    drop_reason: Option<String>,
    #[serde(default)]
    avg_logprob: f32,
}

/// Aggregate counts over a run of records.
#[derive(Debug, Default, PartialEq)]
pub struct Summary {
    pub total: usize,
    pub accepted: usize,
    /// Drop reason -> count, e.g. `low_confidence`, `hallucination`, `empty`.
    pub dropped: std::collections::BTreeMap<String, usize>,
    /// Mean `avg_logprob` over accepted clips (0.0 if none accepted).
    pub mean_accepted_logprob: f32,
}

/// Fold a stream of JSONL lines into a [`Summary`]. Malformed lines are skipped.
pub fn summarize(lines: impl Iterator<Item = String>) -> Summary {
    let mut s = Summary::default();
    let mut logprob_sum = 0.0f64;
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<Record>(line) else {
            continue;
        };
        s.total += 1;
        if rec.accepted {
            s.accepted += 1;
            logprob_sum += rec.avg_logprob as f64;
        } else {
            let reason = rec.drop_reason.unwrap_or_else(|| "unknown".to_string());
            *s.dropped.entry(reason).or_insert(0) += 1;
        }
    }
    if s.accepted > 0 {
        s.mean_accepted_logprob = (logprob_sum / s.accepted as f64) as f32;
    }
    s
}

/// Read `transcripts.jsonl` from the state dir and print the summary.
pub fn run() -> ExitCode {
    let path = config::state_dir().join("transcripts.jsonl");
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "whispy-daemon stats: no transcript log at {} ({e})",
                path.display()
            );
            return ExitCode::FAILURE;
        }
    };

    let lines = BufReader::new(file).lines().map_while(Result::ok);
    let s = summarize(lines);

    println!("transcripts: {}  ({})", s.total, path.display());
    let pct = |n: usize| {
        if s.total == 0 {
            0.0
        } else {
            100.0 * n as f32 / s.total as f32
        }
    };
    println!("  accepted:   {:>5}  ({:.1}%)", s.accepted, pct(s.accepted));
    let dropped_total: usize = s.dropped.values().sum();
    println!(
        "  dropped:    {:>5}  ({:.1}%)",
        dropped_total,
        pct(dropped_total)
    );
    for (reason, count) in &s.dropped {
        println!("    {reason:<16} {count:>5}  ({:.1}%)", pct(*count));
    }
    if s.accepted > 0 {
        println!(
            "  mean avg_logprob (accepted): {:.3}",
            s.mean_accepted_logprob
        );
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(raw: &[&str]) -> Summary {
        summarize(raw.iter().map(|s| s.to_string()))
    }

    #[test]
    fn counts_accepted_and_groups_drop_reasons() {
        let s = lines(&[
            r#"{"accepted":true,"drop_reason":null,"avg_logprob":-0.2}"#,
            r#"{"accepted":true,"drop_reason":null,"avg_logprob":-0.4}"#,
            r#"{"accepted":false,"drop_reason":"low_confidence"}"#,
            r#"{"accepted":false,"drop_reason":"hallucination"}"#,
            r#"{"accepted":false,"drop_reason":"low_confidence"}"#,
        ]);
        assert_eq!(s.total, 5);
        assert_eq!(s.accepted, 2);
        assert_eq!(s.dropped.get("low_confidence"), Some(&2));
        assert_eq!(s.dropped.get("hallucination"), Some(&1));
        assert!((s.mean_accepted_logprob - (-0.3)).abs() < 1e-6);
    }

    #[test]
    fn skips_blank_and_malformed_lines() {
        let s = lines(&[
            "",
            "   ",
            "not json",
            r#"{"accepted":true,"drop_reason":null,"avg_logprob":-0.1}"#,
        ]);
        assert_eq!(s.total, 1);
        assert_eq!(s.accepted, 1);
    }

    #[test]
    fn dropped_without_reason_falls_back_to_unknown() {
        let s = lines(&[r#"{"accepted":false}"#]);
        assert_eq!(s.dropped.get("unknown"), Some(&1));
    }

    #[test]
    fn empty_input_is_all_zero() {
        assert_eq!(lines(&[]), Summary::default());
    }
}
