//! `whispy-daemon stats`: summarize `transcripts.jsonl` so users can tune the
//! filter thresholds (`fuzzy_ratio`, confidence bounds) without hand-parsing the log.
//!
//! The daemon appends one [`Record`] per clip to the [`TranscriptLog`] — accepted
//! or dropped, with the drop reason and confidence signals. This reads it back
//! (both generations) and prints how many clips were accepted vs dropped, broken
//! down by drop reason.

use std::process::ExitCode;

use crate::transcripts::{Record, TranscriptLog};

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

/// Fold transcript records into a [`Summary`].
pub fn summarize(records: impl IntoIterator<Item = Record>) -> Summary {
    let mut s = Summary::default();
    let mut logprob_sum = 0.0f64;
    for rec in records {
        s.total += 1;
        if rec.accepted {
            s.accepted += 1;
            logprob_sum += f64::from(rec.avg_logprob);
        } else {
            let reason = rec.drop_reason.unwrap_or_else(|| "unknown".to_string());
            *s.dropped.entry(reason).or_insert(0) += 1;
        }
    }
    if s.accepted > 0 {
        #[expect(
            clippy::cast_possible_truncation,
            reason = "mean log-prob is a small negative number; f32 is plenty"
        )]
        let mean = (logprob_sum / s.accepted as f64) as f32;
        s.mean_accepted_logprob = mean;
    }
    s
}

/// Read the daemon's transcript log (both generations) and print the summary.
pub fn run() -> ExitCode {
    let log = TranscriptLog::open();
    let path = log.path();
    let Some(records) = log.read() else {
        eprintln!(
            "whispy-daemon stats: no transcript log at {}",
            path.display()
        );
        return ExitCode::FAILURE;
    };
    let s = summarize(records);

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

    fn rec(accepted: bool, drop_reason: Option<&str>, avg_logprob: f32) -> Record {
        Record {
            accepted,
            drop_reason: drop_reason.map(String::from),
            avg_logprob,
            ..Record::default()
        }
    }

    #[test]
    fn counts_accepted_and_groups_drop_reasons() {
        let s = summarize([
            rec(true, None, -0.2),
            rec(true, None, -0.4),
            rec(false, Some("low_confidence"), 0.0),
            rec(false, Some("hallucination"), 0.0),
            rec(false, Some("low_confidence"), 0.0),
        ]);
        assert_eq!(s.total, 5);
        assert_eq!(s.accepted, 2);
        assert_eq!(s.dropped.get("low_confidence"), Some(&2));
        assert_eq!(s.dropped.get("hallucination"), Some(&1));
        assert!((s.mean_accepted_logprob - (-0.3)).abs() < 1e-6);
    }

    #[test]
    fn dropped_without_reason_falls_back_to_unknown() {
        let s = summarize([rec(false, None, 0.0)]);
        assert_eq!(s.dropped.get("unknown"), Some(&1));
    }

    #[test]
    fn empty_input_is_all_zero() {
        assert_eq!(summarize([]), Summary::default());
    }

    #[test]
    fn summarizes_a_log_across_both_generations() {
        let dir = std::env::temp_dir().join(format!("whispy-stats-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let log = TranscriptLog::in_dir(&dir);
        std::fs::write(
            crate::config::rotated(log.path()),
            "{\"accepted\":true,\"drop_reason\":null,\"avg_logprob\":-0.1}\nnot json\n",
        )
        .unwrap();
        log.append(&rec(false, Some("empty"), 0.0)).unwrap();

        let s = summarize(log.read().unwrap());
        assert_eq!(s.total, 2);
        assert_eq!(s.accepted, 1);
        assert_eq!(s.dropped.get("empty"), Some(&1));
        std::fs::remove_dir_all(&dir).ok();
    }
}
