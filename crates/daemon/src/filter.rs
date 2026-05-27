//! Hallucination filter.
//!
//! Runs in order:
//! 1. confidence thresholds (`no_speech_prob` / `avg_logprob`),
//! 2. punctuation/whitespace-only rejection,
//! 3. exact + fuzzy match (`strsim`) against the hallucination blacklist.
//! The drop reason is surfaced via [`DropReason::kind`] for the state.json
//! `error_kind` field.

use std::path::Path;

use serde::Deserialize;

use crate::config::Filter;

/// Blacklist of known whisper hallucination phrases.
///
/// Phrases are stored already normalized (trimmed + lowercased) so matching
/// against a normalized candidate is a direct comparison.
pub struct Hallucinations {
    phrases: Vec<String>,
}

/// On-disk shape of a hallucinations TOML file (`phrases = [ ... ]`).
#[derive(Debug, Deserialize)]
struct HallucinationsFile {
    #[serde(default)]
    phrases: Vec<String>,
}

impl Hallucinations {
    /// Load from a TOML file of the form `phrases = [ ... ]`.
    pub fn load(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let parsed: HallucinationsFile = toml::from_str(&text)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        let count = parsed.phrases.len();
        tracing::debug!(count, path = %path.display(), "loaded hallucination blacklist");
        Ok(Self::from_phrases(parsed.phrases))
    }

    /// Build from an explicit list (used in tests).
    pub fn from_phrases(phrases: Vec<String>) -> Self {
        let phrases = phrases.into_iter().map(|p| normalize(&p)).collect();
        Self { phrases }
    }
}

/// Why a transcript was rejected by the filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// Model confidence fell below the configured thresholds.
    LowConfidence,
    /// Nothing remained after trimming whitespace and punctuation.
    Empty,
    /// Matched (exactly or fuzzily) a known hallucination phrase.
    Hallucination,
}

impl DropReason {
    /// Stable snake_case identifier for the state.json `error_kind` field.
    pub fn kind(&self) -> &'static str {
        match self {
            DropReason::LowConfidence => "low_confidence",
            DropReason::Empty => "empty",
            DropReason::Hallucination => "hallucination",
        }
    }
}

/// Outcome of running the filter on a transcript.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Keep the transcript (trimmed, original case).
    Accept(String),
    /// Reject the transcript for the given reason.
    Drop(DropReason),
}

/// Confidence metrics reported by the STT backend for a clip.
#[derive(Debug, Clone, Copy)]
pub struct Metrics {
    pub avg_logprob: f32,
    pub no_speech_prob: f32,
}

/// Run the hallucination filter on a transcript.
///
/// Checks are applied in order: low confidence, then empty/punctuation-only,
/// then the hallucination blacklist. An accepted transcript is returned trimmed
/// but with its original casing intact.
pub fn evaluate(
    text: &str,
    metrics: Metrics,
    cfg: &Filter,
    blacklist: &Hallucinations,
) -> Decision {
    // 1. Low confidence.
    if metrics.no_speech_prob > cfg.no_speech_prob_max || metrics.avg_logprob < cfg.avg_logprob_min
    {
        tracing::debug!(
            no_speech_prob = metrics.no_speech_prob,
            avg_logprob = metrics.avg_logprob,
            "dropping low-confidence transcript"
        );
        return Decision::Drop(DropReason::LowConfidence);
    }

    let trimmed = text.trim();

    // 2. Empty / punctuation-only.
    let has_content = trimmed
        .chars()
        .any(|c| !c.is_whitespace() && !is_punctuation(c));
    if !has_content {
        tracing::debug!("dropping empty/punctuation-only transcript");
        return Decision::Drop(DropReason::Empty);
    }

    // 3. Hallucination blacklist.
    let candidate = normalize(trimmed);
    for phrase in &blacklist.phrases {
        if candidate == *phrase {
            tracing::debug!(phrase = %phrase, "dropping exact hallucination match");
            return Decision::Drop(DropReason::Hallucination);
        }
        let similarity = strsim::normalized_levenshtein(&candidate, phrase);
        if similarity >= cfg.fuzzy_ratio {
            tracing::debug!(phrase = %phrase, similarity, "dropping fuzzy hallucination match");
            return Decision::Drop(DropReason::Hallucination);
        }
    }

    // 4. Accept the trimmed, original-case text.
    Decision::Accept(trimmed.to_string())
}

/// Trim and lowercase a phrase for case-insensitive matching.
fn normalize(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Whether `c` is punctuation that should be ignored when deciding emptiness.
/// Covers both ASCII punctuation and Unicode punctuation/symbol categories.
fn is_punctuation(c: char) -> bool {
    c.is_ascii_punctuation() || (!c.is_alphanumeric() && !c.is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Filter {
        Filter {
            no_speech_prob_max: 0.6,
            avg_logprob_min: -1.0,
            fuzzy_ratio: 0.85,
            hallucinations_path: String::new(),
        }
    }

    fn good_metrics() -> Metrics {
        Metrics { avg_logprob: -0.3, no_speech_prob: 0.1 }
    }

    fn blacklist() -> Hallucinations {
        Hallucinations::from_phrases(vec![
            "Grazie per aver guardato".to_string(),
            "Thanks for watching".to_string(),
        ])
    }

    #[test]
    fn drop_reason_kind_is_stable() {
        assert_eq!(DropReason::LowConfidence.kind(), "low_confidence");
        assert_eq!(DropReason::Empty.kind(), "empty");
        assert_eq!(DropReason::Hallucination.kind(), "hallucination");
    }

    #[test]
    fn exact_hallucination_is_dropped() {
        let d = evaluate("Grazie per aver guardato", good_metrics(), &cfg(), &blacklist());
        assert_eq!(d, Decision::Drop(DropReason::Hallucination));
    }

    #[test]
    fn near_miss_punctuation_is_dropped() {
        let d = evaluate("grazie per aver guardato!", good_metrics(), &cfg(), &blacklist());
        assert_eq!(d, Decision::Drop(DropReason::Hallucination));
    }

    #[test]
    fn near_miss_typo_is_dropped() {
        let d = evaluate("Grazie per aver guardatoo", good_metrics(), &cfg(), &blacklist());
        assert_eq!(d, Decision::Drop(DropReason::Hallucination));
    }

    #[test]
    fn legit_sentence_is_accepted_unchanged() {
        let text = "apriamo VSCode e creiamo un nuovo file in Next.js";
        let d = evaluate(text, good_metrics(), &cfg(), &blacklist());
        assert_eq!(d, Decision::Accept(text.to_string()));
    }

    #[test]
    fn accept_trims_surrounding_whitespace() {
        let d = evaluate("  ciao mondo  ", good_metrics(), &cfg(), &blacklist());
        assert_eq!(d, Decision::Accept("ciao mondo".to_string()));
    }

    #[test]
    fn high_no_speech_prob_is_low_confidence() {
        let metrics = Metrics { avg_logprob: -0.3, no_speech_prob: 0.9 };
        let d = evaluate("apriamo VSCode", metrics, &cfg(), &blacklist());
        assert_eq!(d, Decision::Drop(DropReason::LowConfidence));
    }

    #[test]
    fn low_avg_logprob_is_low_confidence() {
        let metrics = Metrics { avg_logprob: -1.5, no_speech_prob: 0.1 };
        let d = evaluate("apriamo VSCode", metrics, &cfg(), &blacklist());
        assert_eq!(d, Decision::Drop(DropReason::LowConfidence));
    }

    #[test]
    fn empty_and_punctuation_only_are_dropped() {
        for text in ["", "  ", "...", " . , ! "] {
            let d = evaluate(text, good_metrics(), &cfg(), &blacklist());
            assert_eq!(d, Decision::Drop(DropReason::Empty), "input {text:?}");
        }
    }

    #[test]
    fn confidence_check_precedes_emptiness() {
        // Empty text with bad metrics is dropped as LowConfidence (order matters).
        let metrics = Metrics { avg_logprob: -2.0, no_speech_prob: 0.1 };
        let d = evaluate("", metrics, &cfg(), &blacklist());
        assert_eq!(d, Decision::Drop(DropReason::LowConfidence));
    }
}
