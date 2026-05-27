//! Daemon configuration: parsed from TOML, with the defaults baked into the binary.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Built-in defaults, embedded so the daemon runs without an installed config file.
const DEFAULT_TOML: &str = include_str!("../../../config/default.toml");

/// The hallucination blacklist baked into the binary (runtime fallback + `setup` seed).
const DEFAULT_HALLUCINATIONS_TOML: &str = include_str!("../../../config/hallucinations.toml");

/// The embedded `default.toml`, exposed so `setup` can seed a user config file.
pub fn default_config_toml() -> &'static str {
    DEFAULT_TOML
}

/// The embedded `hallucinations.toml`, exposed so `setup` can seed a user copy.
pub fn default_hallucinations_toml() -> &'static str {
    DEFAULT_HALLUCINATIONS_TOML
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub stt: Stt,
    pub audio: Audio,
    pub filter: Filter,
    pub injection: Injection,
    pub ipc: Ipc,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Stt {
    pub host: String,
    pub port: u16,
    pub model: String,
    pub model_path: String,
    pub language: String,
    pub server_bin: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Audio {
    pub rate: u32,
    pub channels: u16,
    pub max_clip_secs: f64,
    pub min_clip_ms: u64,
    pub rms_interval_ms: u64,
    /// Linear gain applied to captured samples (saturating). The mic is quiet
    /// (see docs/stt-benchmark.md); raise this for a more responsive meter.
    #[serde(default = "default_gain")]
    pub gain: f32,
}

fn default_gain() -> f32 {
    1.0
}

impl Audio {
    /// Total samples at which capture auto-stops (`error: too_long`).
    pub fn max_samples(&self) -> usize {
        (self.rate as f64 * self.max_clip_secs) as usize
    }

    /// Below this sample count a clip is discarded silently.
    pub fn min_samples(&self) -> usize {
        (self.rate as u64 * self.min_clip_ms / 1000) as usize
    }

    /// Number of samples per RMS reporting window.
    pub fn rms_window(&self) -> usize {
        ((self.rate as u64 * self.rms_interval_ms / 1000) as usize).max(1)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Filter {
    pub no_speech_prob_max: f32,
    pub avg_logprob_min: f32,
    pub fuzzy_ratio: f64,
    pub hallucinations_path: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Injection {
    /// "paste" = clipboard + ydotool Ctrl+V (preserves accents; GUI apps only).
    /// "type"  = wtype types the transcript directly (works in terminals too).
    #[serde(default = "default_mode")]
    pub mode: String,
    pub restore_clipboard_delay_ms: u64,
    pub paste_keys: String,
    /// Delay between ydotool key events. Without it the Ctrl+V events fire in one
    /// instant and apps miss the held modifier, so the paste silently no-ops.
    #[serde(default = "default_key_delay")]
    pub key_delay_ms: u64,
}

fn default_key_delay() -> u64 {
    25
}

fn default_mode() -> String {
    "paste".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct Ipc {
    pub socket_path: String,
    pub state_path: String,
    pub state_max_hz: u32,
}

impl Config {
    /// Load config from `explicit` if given, else `$XDG_CONFIG_HOME/whispy/config.toml`,
    /// else the built-in defaults.
    pub fn load(explicit: Option<&Path>) -> Result<Self, String> {
        let text = if let Some(path) = explicit {
            std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?
        } else if let Some(path) = user_config_path().filter(|p| p.exists()) {
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?
        } else {
            DEFAULT_TOML.to_string()
        };
        toml::from_str(&text).map_err(|e| e.to_string())
    }
}

impl Stt {
    /// Resolved path to the whisper-server binary.
    pub fn server_bin_path(&self) -> PathBuf {
        expand(&self.server_bin)
    }

    /// Resolved path to the ggml model file.
    pub fn model_file(&self) -> PathBuf {
        expand(&self.model_path)
    }
}

impl Filter {
    /// Resolved path to the hallucination blacklist (expands a leading `~/`).
    pub fn hallucinations_file(&self) -> PathBuf {
        expand(&self.hallucinations_path)
    }
}

impl Ipc {
    /// Resolved Unix socket path (config value or `$XDG_RUNTIME_DIR/whispy/whispy.sock`).
    pub fn socket_path(&self) -> PathBuf {
        if self.socket_path.is_empty() {
            runtime_dir().join("whispy").join("whispy.sock")
        } else {
            expand(&self.socket_path)
        }
    }

    /// Resolved state.json path (config value or `$XDG_RUNTIME_DIR/whispy/state.json`).
    pub fn state_path(&self) -> PathBuf {
        if self.state_path.is_empty() {
            runtime_dir().join("whispy").join("state.json")
        } else {
            expand(&self.state_path)
        }
    }
}

/// `$XDG_CONFIG_HOME/whispy` (default `~/.config/whispy`): user config + blacklist.
pub fn user_config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("whispy"))
}

pub fn user_config_path() -> Option<PathBuf> {
    user_config_dir().map(|d| d.join("config.toml"))
}

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// `$XDG_STATE_HOME/whispy` (default `~/.local/state/whispy`): logs and transcripts.
pub fn state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("whispy")
}

/// Expand a leading `~/` to `$HOME`.
pub(crate) fn expand(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_defaults_parse() {
        let cfg: Config = toml::from_str(DEFAULT_TOML).expect("default.toml must parse");
        assert_eq!(cfg.audio.rate, 16000);
        assert_eq!(cfg.audio.channels, 1);
        assert!(cfg.filter.fuzzy_ratio > 0.0);
    }

    #[test]
    fn socket_path_falls_back_to_runtime_dir() {
        let ipc = Ipc {
            socket_path: String::new(),
            state_path: String::new(),
            state_max_hz: 20,
        };
        assert!(ipc.socket_path().ends_with("whispy.sock"));
        assert!(ipc.state_path().ends_with("state.json"));
    }
}
