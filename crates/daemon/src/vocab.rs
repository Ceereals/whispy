//! Fuzzy custom-vocabulary correction for engines that can't take a decoding
//! prompt (Parakeet ignores whisper's `prompt` bias).
//!
//! After transcription, every 1–3 word window is compared against
//! `dictionary.vocabulary`: words are reduced to lowercase alphanumeric keys,
//! scored by normalized Levenshtein distance, and phonetic matches (same Soundex
//! code) get a ×0.3 boost. The closest match under the threshold replaces the
//! window, keeping its surrounding punctuation and the first word's casing. So
//! "Hyperland" → "Hyprland" and "Charge B" → "`ChargeBee`".
//!
//! Ported from Handy's `apply_custom_words` (`src/audio_toolkit/text.rs`,
//! MIT, © 2025 CJ Pais), with an inline Soundex instead of the `natural` crate.

/// Default acceptance threshold (lower = stricter), as in Handy.
pub const DEFAULT_THRESHOLD: f64 = 0.18;

/// Longest candidate key considered (longer windows can't be a custom word).
const MAX_KEY_CHARS: usize = 50;

/// One normalized key for a custom word.
struct Key {
    word: usize,
    key: String,
}

/// Correct `text` against `words`. Returns `text` unchanged when `words` is empty.
pub fn apply(text: &str, words: &[String], threshold: f64) -> String {
    if words.is_empty() {
        return text.to_string();
    }
    let keys: Vec<Key> = words
        .iter()
        .enumerate()
        .flat_map(|(i, w)| keys_for(w, i))
        .collect();
    let tokens: Vec<&str> = text.split_whitespace().collect();
    let mut out: Vec<String> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        // Try windows of 3, 2, then 1 words and keep the closest match.
        let mut best: Option<(usize, &str, f64)> = None;
        for n in (1..=3).rev() {
            let Some(window) = tokens.get(i..i + n) else {
                continue;
            };
            // Don't merge across punctuation: "Charge B, che" stops at "B,".
            if window[..n - 1]
                .iter()
                .any(|w| !split_punctuation(w).1.is_empty())
            {
                continue;
            }
            let candidate: String = window.iter().map(|w| match_key(w)).collect();
            if let Some((word, score)) = best_match(&candidate, words, &keys, threshold)
                && best.is_none_or(|(_, _, s)| score < s)
            {
                best = Some((n, word, score));
            }
        }
        if let Some((n, word, _)) = best {
            let (prefix, _) = split_punctuation(tokens[i]);
            let (_, suffix) = split_punctuation(tokens[i + n - 1]);
            out.push(format!("{prefix}{}{suffix}", keep_case(tokens[i], word)));
            i += n;
        } else {
            out.push(tokens[i].to_string());
            i += 1;
        }
    }
    out.join(" ")
}

/// Lowercase alphanumeric key: "Charge-B." → "chargeb".
fn match_key(word: &str) -> String {
    word.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Keys for one custom word (plus an "&" → "and" variant). Non-ASCII words are
/// skipped: the tokenization and Soundex below only make sense for ASCII.
fn keys_for(word: &str, index: usize) -> Vec<Key> {
    let primary = match_key(word);
    let mut keys = Vec::new();
    if is_ascii_key(&primary) {
        keys.push(Key {
            word: index,
            key: primary.clone(),
        });
    }
    if word.contains('&') {
        let expanded = match_key(&word.replace('&', " and "));
        if is_ascii_key(&expanded) && expanded != primary {
            keys.push(Key {
                word: index,
                key: expanded,
            });
        }
    }
    keys
}

fn is_ascii_key(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric())
}

/// The custom word closest to `candidate` under `threshold`, with its score.
fn best_match<'a>(
    candidate: &str,
    words: &'a [String],
    keys: &[Key],
    threshold: f64,
) -> Option<(&'a str, f64)> {
    if !is_ascii_key(candidate) || candidate.len() > MAX_KEY_CHARS {
        return None;
    }
    let mut best: Option<(&str, f64)> = None;
    for k in keys {
        let (a, b) = (candidate.len(), k.key.len());
        let max_len = a.max(b) as f64;
        // At most 25% (and at least 2 chars) length difference, so a 3-word
        // window can't swallow a much shorter custom word.
        if a.abs_diff(b) as f64 > (max_len * 0.25).max(2.0) {
            continue;
        }
        let distance = strsim::levenshtein(candidate, &k.key) as f64 / max_len;
        let phonetic = soundex(candidate).is_some_and(|c| Some(c) == soundex(&k.key));
        let score = if phonetic { distance * 0.3 } else { distance };
        if score < threshold && best.is_none_or(|(_, s)| score < s) {
            best = Some((words[k.word].as_str(), score));
        }
    }
    best
}

/// American Soundex code ("Robert" → "R163") of an ASCII-letters word; `None`
/// for anything else (digits get no phonetic boost).
fn soundex(word: &str) -> Option<[u8; 4]> {
    if word.is_empty() || !word.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let digit = |c: u8| match c {
        b'b' | b'f' | b'p' | b'v' => b'1',
        b'c' | b'g' | b'j' | b'k' | b'q' | b's' | b'x' | b'z' => b'2',
        b'd' | b't' => b'3',
        b'l' => b'4',
        b'm' | b'n' => b'5',
        b'r' => b'6',
        _ => b'0', // vowels, y, h, w
    };
    let bytes: Vec<u8> = word.bytes().map(|b| b.to_ascii_lowercase()).collect();
    let mut code = [b'0'; 4];
    code[0] = bytes[0].to_ascii_uppercase();
    let mut last = digit(bytes[0]);
    let mut len = 1;
    for &b in &bytes[1..] {
        if len == 4 {
            break;
        }
        let d = digit(b);
        if d != b'0' && d != last {
            code[len] = d;
            len += 1;
        }
        // h and w don't separate equal codes; vowels do.
        if b != b'h' && b != b'w' {
            last = d;
        }
    }
    Some(code)
}

/// Apply the original word's casing pattern (ALL CAPS / Capitalized) to `replacement`.
fn keep_case(original: &str, replacement: &str) -> String {
    let letters = || original.chars().filter(|c| c.is_alphabetic());
    if letters().next().is_some()
        && letters().all(char::is_uppercase)
        && original.chars().count() > 1
    {
        return replacement.to_uppercase();
    }
    if original.chars().next().is_some_and(char::is_uppercase) {
        let mut chars = replacement.chars();
        return chars
            .next()
            .map(|first| first.to_uppercase().chain(chars).collect())
            .unwrap_or_default();
    }
    replacement.to_string()
}

/// Split leading/trailing non-alphanumerics off a token: "(Hyperland," →
/// ("(", ","). Works on char boundaries, so multibyte punctuation is safe.
fn split_punctuation(word: &str) -> (&str, &str) {
    let start = word
        .char_indices()
        .find(|(_, c)| c.is_alphanumeric())
        .map_or(word.len(), |(i, _)| i);
    let end = word
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_alphanumeric())
        .map_or(0, |(i, c)| i + c.len_utf8());
    if start >= end {
        return (word, "");
    }
    (&word[..start], &word[end..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn soundex_matches_the_reference_codes() {
        let code = |w| soundex(w).map(|c| String::from_utf8(c.to_vec()).unwrap());
        assert_eq!(code("robert").as_deref(), Some("R163"));
        assert_eq!(code("rupert").as_deref(), Some("R163"));
        assert_eq!(code("ashcraft").as_deref(), Some("A261"));
        assert_eq!(code("tymczak").as_deref(), Some("T522"));
        assert_eq!(code("pfister").as_deref(), Some("P236"));
        assert_eq!(code("lee").as_deref(), Some("L000"));
        assert_eq!(code("x11"), None);
    }

    #[test]
    fn corrects_the_benchmark_misspellings() {
        let v = words(&["Hyprland", "Omarchy", "whispy"]);
        assert_eq!(
            apply(
                "Ho aggiornato Hyperland e adesso la barra",
                &v,
                DEFAULT_THRESHOLD
            ),
            "Ho aggiornato Hyprland e adesso la barra"
        );
        assert_eq!(
            apply("la barra di Omarkey.", &v, DEFAULT_THRESHOLD),
            "la barra di Omarchy."
        );
        let p = words(&["Parakeet"]);
        assert_eq!(
            apply("conviene usare Parakeit oppure", &p, DEFAULT_THRESHOLD),
            "conviene usare Parakeet oppure"
        );
    }

    #[test]
    fn merges_split_words_but_not_across_punctuation() {
        let v = words(&["ChargeBee"]);
        assert_eq!(
            apply("use Charge B today", &v, DEFAULT_THRESHOLD),
            "use ChargeBee today"
        );
        assert_eq!(
            apply("Charge B, che", &v, DEFAULT_THRESHOLD),
            "ChargeBee, che"
        );
    }

    #[test]
    fn keeps_punctuation_and_case() {
        let v = words(&["Hyprland"]);
        assert_eq!(apply("(hyperland,", &v, DEFAULT_THRESHOLD), "(Hyprland,");
        assert_eq!(apply("HYPERLAND!", &v, DEFAULT_THRESHOLD), "HYPRLAND!");
    }

    #[test]
    fn leaves_ordinary_words_alone() {
        let v = words(&["Hyprland", "Omarchy", "whispy", "commit", "push"]);
        let text = "Ricordami di fare il commit e il push prima di chiudere il portatile.";
        assert_eq!(apply(text, &v, DEFAULT_THRESHOLD), text);
        let text = "Oggi provo la dettatura vocale per vedere se riconosce bene quello che dico.";
        assert_eq!(apply(text, &v, DEFAULT_THRESHOLD), text);
        assert_eq!(apply("anything", &[], DEFAULT_THRESHOLD), "anything");
    }

    #[test]
    fn non_ascii_words_and_tokens_are_safe() {
        let v = words(&["Città", "Hyprland"]);
        assert_eq!(
            apply("la città è «bella»", &v, DEFAULT_THRESHOLD),
            "la città è «bella»"
        );
        assert_eq!(split_punctuation("«bella»"), ("«", "»"));
        assert_eq!(split_punctuation("..."), ("...", ""));
    }
}
