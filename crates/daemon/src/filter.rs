//! Hallucination filter.
//!
//! Implemented in Step 4:
//! 1. confidence thresholds (`no_speech_prob` / `avg_logprob`),
//! 2. exact + fuzzy match (`strsim`) against the hallucination blacklist,
//! 3. punctuation/whitespace-only rejection.
//! Every transcript (accepted or dropped, with reason) is logged to
//! `transcripts.jsonl` for calibration.

#[cfg(test)]
mod tests {
    #[test]
    fn placeholder() {
        // Deterministic filter unit tests land in Step 4.
    }
}
