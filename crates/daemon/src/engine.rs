//! The speech-to-text engine (`stt.engine`) and everything that depends on it.
//!
//! Both engines run as an STT server child behind the same `/inference` HTTP API
//! (`stt.rs` talks to either), so what differs is gathered here: which server
//! binary runs and with which extra arguments, which model it loads and where
//! `setup` downloads it from, which `setup` step installs it, and how the custom
//! vocabulary reaches it. Callers ask the engine instead of branching on it.

use std::fmt;
use std::path::PathBuf;

use serde::Deserialize;

use crate::config::{ConfigChoice, Stt, expand};

/// The STT engine. Configs written before `stt.engine` existed are [`Engine::Whisper`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    /// whisper.cpp's whisper-server (built per machine by `setup whisper`).
    #[default]
    Whisper,
    /// NVIDIA Parakeet via whispy-stt on transcribe.cpp.
    Parakeet,
}

/// How the custom vocabulary (`dictionary.vocabulary`) reaches the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VocabStrategy {
    /// No vocabulary configured.
    Off,
    /// Bias decoding with this prompt (whisper's `prompt` field).
    Prompt(String),
    /// Fix the transcript afterwards with fuzzy matching (`vocab::apply`).
    Correct,
}

/// Where `setup model` gets an engine's model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSource {
    /// Human-readable name for setup output.
    pub label: String,
    pub url: String,
    /// Expected SHA-256 of the file, when pinned.
    pub sha256: Option<&'static str>,
}

/// The `whispy-stt` binary's file name.
pub const STT_BIN: &str = "whispy-stt";

/// Parakeet model: Handy's GGUF conversion, pinned to a revision, with its
/// SHA-256 (both from Handy's model catalog).
const PARAKEET_MODEL_URL: &str = "https://huggingface.co/handy-computer/parakeet-ultra-gguf/resolve/39eeb55181f0d354fd934f06e92fd8d5037fed8e/parakeet-ultra-Q8_0.gguf";
const PARAKEET_MODEL_SHA256: &str =
    "007a59761e9258779f189df396b50d12b83bb5b79e66df4b955c230d2f2a0a59";

impl ConfigChoice for Engine {
    const ALL: &'static [Self] = &[Self::Parakeet, Self::Whisper];

    fn as_str(self) -> &'static str {
        match self {
            Self::Whisper => "whisper",
            Self::Parakeet => "parakeet",
        }
    }
}

impl Engine {
    /// The STT server program's name, for logs and messages.
    pub fn server_name(self) -> &'static str {
        match self {
            Self::Whisper => "whisper-server",
            Self::Parakeet => STT_BIN,
        }
    }

    /// The `whispy-daemon setup` step that installs this engine's server.
    pub fn setup_step(self) -> &'static str {
        match self {
            Self::Whisper => "whisper",
            Self::Parakeet => "stt",
        }
    }

    /// The server binary the daemon runs.
    pub fn server_bin(self, stt: &Stt) -> PathBuf {
        match self {
            Self::Whisper => stt.whisper_server_bin(),
            Self::Parakeet => stt.parakeet_bin_path(),
        }
    }

    /// The model file the server loads.
    pub fn model_file(self, stt: &Stt) -> PathBuf {
        match self {
            Self::Whisper => expand(&stt.model_path),
            Self::Parakeet => expand(&stt.parakeet_model_path),
        }
    }

    /// Arguments beyond the shared `-m MODEL -l LANG --host H --port P`.
    /// whispy-stt picks its compute backend at runtime; whisper-server's is fixed
    /// when `setup whisper` builds it.
    pub fn extra_server_args(self, stt: &Stt) -> Vec<String> {
        match self {
            Self::Whisper => Vec::new(),
            Self::Parakeet => vec!["--backend".to_string(), stt.backend.as_str().to_string()],
        }
    }

    /// Where `setup model` downloads the model from.
    pub fn model_source(self, stt: &Stt) -> ModelSource {
        match self {
            Self::Whisper => ModelSource {
                label: stt.model.clone(),
                url: format!(
                    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{}.bin",
                    stt.model
                ),
                sha256: None,
            },
            Self::Parakeet => ModelSource {
                label: "parakeet-ultra (Q8_0)".to_string(),
                url: PARAKEET_MODEL_URL.to_string(),
                sha256: Some(PARAKEET_MODEL_SHA256),
            },
        }
    }

    /// How `words` (the custom vocabulary) reach this engine: whisper takes them
    /// as a decoding prompt; Parakeet can't, so its transcripts get corrected.
    pub fn vocabulary(self, words: &[String]) -> VocabStrategy {
        if words.is_empty() {
            return VocabStrategy::Off;
        }
        match self {
            Self::Whisper => VocabStrategy::Prompt(words.join(", ")),
            Self::Parakeet => VocabStrategy::Correct,
        }
    }
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Holder {
        #[serde(default)]
        engine: Engine,
    }

    fn parse_toml(text: &str) -> Result<Engine, String> {
        toml::from_str::<Holder>(text)
            .map(|h| h.engine)
            .map_err(|e| e.to_string())
    }

    #[test]
    fn deserializes_config_values_and_defaults_to_whisper() {
        assert_eq!(parse_toml("engine = \"parakeet\""), Ok(Engine::Parakeet));
        assert_eq!(parse_toml("engine = \"whisper\""), Ok(Engine::Whisper));
        assert_eq!(parse_toml(""), Ok(Engine::Whisper), "old configs");
        let err = parse_toml("engine = \"vosk\"").unwrap_err();
        assert!(err.contains("vosk"), "{err}");
    }

    #[test]
    fn config_values_round_trip() {
        for &e in Engine::ALL {
            assert_eq!(Engine::parse(e.as_str()), Some(e));
            assert_eq!(e.to_string(), e.as_str());
        }
        assert_eq!(Engine::parse("Parakeet"), None);
    }

    #[test]
    fn vocabulary_is_a_prompt_for_whisper_and_a_correction_for_parakeet() {
        let words = vec!["Hyprland".to_string(), "Omarchy".to_string()];
        assert_eq!(
            Engine::Whisper.vocabulary(&words),
            VocabStrategy::Prompt("Hyprland, Omarchy".into())
        );
        assert_eq!(Engine::Parakeet.vocabulary(&words), VocabStrategy::Correct);
        assert_eq!(Engine::Whisper.vocabulary(&[]), VocabStrategy::Off);
        assert_eq!(Engine::Parakeet.vocabulary(&[]), VocabStrategy::Off);
    }

    #[test]
    fn parakeet_model_is_pinned_and_checksummed() {
        let stt: Stt =
            toml::from_str::<crate::config::Config>(crate::config::default_config_toml())
                .expect("defaults parse")
                .stt;
        let src = Engine::Parakeet.model_source(&stt);
        assert!(
            src.url.contains("/resolve/39eeb551"),
            "pinned revision: {}",
            src.url
        );
        assert_eq!(src.sha256.map(str::len), Some(64));
        assert_eq!(Engine::Whisper.model_source(&stt).sha256, None);
        assert_eq!(
            Engine::Parakeet.extra_server_args(&stt),
            ["--backend", "auto"]
        );
        assert_eq!(
            Engine::Whisper.extra_server_args(&stt),
            Vec::<String>::new()
        );
    }
}
