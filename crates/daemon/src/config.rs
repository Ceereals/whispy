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
    #[serde(default)]
    pub dictionary: Dictionary,
    #[serde(default)]
    pub snippets: Snippets,
    #[serde(default)]
    pub llm: Llm,
    #[serde(default)]
    pub workflow: Vec<Workflow>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Stt {
    pub host: String,
    pub port: u16,
    pub model: String,
    pub model_path: String,
    pub language: String,
    pub server_bin: String,
    /// Hard cap on a single `/inference` request. Without it a hung whisper-server
    /// (e.g. a GPU/Vulkan lockup) would strand the daemon in `transcribing` forever.
    #[serde(default = "default_stt_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_stt_timeout_secs() -> u64 {
    30
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
    /// Trailing-silence auto-stop: once speech has been heard, this many
    /// milliseconds of continuous silence stops capture and transcribes what was
    /// said. `0` disables it (stop only on the explicit command or `too_long`).
    #[serde(default = "default_silence_timeout_ms")]
    pub silence_timeout_ms: u64,
    /// RMS (normalized to `[0, 1]`) at or below which an RMS window counts as
    /// silence for the trailing-silence auto-stop. Tune to your mic/gain.
    #[serde(default = "default_silence_rms_threshold")]
    pub silence_rms_threshold: f32,
}

fn default_gain() -> f32 {
    1.0
}

fn default_silence_timeout_ms() -> u64 {
    2000
}

fn default_silence_rms_threshold() -> f32 {
    0.0015
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

    /// Consecutive silent RMS windows that trigger the trailing-silence auto-stop,
    /// or `None` when the feature is disabled (`silence_timeout_ms == 0`).
    pub fn silence_windows(&self) -> Option<usize> {
        if self.silence_timeout_ms == 0 {
            return None;
        }
        let window_ms = self.rms_interval_ms.max(1);
        Some((self.silence_timeout_ms.div_ceil(window_ms) as usize).max(1))
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

/// Custom vocabulary (biases the STT prompt) and literal corrections (post-fix).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Dictionary {
    /// Words/names joined into the whisper prompt to bias recognition.
    #[serde(default)]
    pub vocabulary: Vec<String>,
    /// Case-insensitive whole-word replacements applied to accepted transcripts.
    #[serde(default)]
    pub corrections: std::collections::BTreeMap<String, String>,
}

/// Text-expansion shortcuts (trigger -> replacement, with placeholders).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Snippets {
    /// Trigger phrase -> replacement. Replacement may contain `{{DATE}}`,
    /// `{{TIME}}`, `{{CLIPBOARD}}` placeholders.
    #[serde(default)]
    pub entries: std::collections::BTreeMap<String, String>,
}

/// OpenAI-compatible chat endpoint used by AI workflows (local ollama by default).
#[derive(Debug, Clone, Deserialize)]
pub struct Llm {
    #[serde(default = "default_llm_base_url")]
    pub base_url: String,
    /// Model name. Empty disables LLM workflows.
    #[serde(default)]
    pub model: String,
    /// Bearer token for cloud providers; empty for local.
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_llm_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for Llm {
    fn default() -> Self {
        Self {
            base_url: default_llm_base_url(),
            model: String::new(),
            api_key: String::new(),
            timeout_secs: default_llm_timeout_secs(),
        }
    }
}

fn default_llm_base_url() -> String {
    "http://127.0.0.1:11434/v1".to_string()
}

fn default_llm_timeout_secs() -> u64 {
    20
}

/// One AI workflow: a system prompt plus the app window-classes it auto-applies to.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct Workflow {
    pub name: String,
    #[serde(default)]
    pub prompt: String,
    /// Focused-window classes that auto-select this workflow (empty = manual only).
    #[serde(default)]
    pub apps: Vec<String>,
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

    /// Validate the loaded config before the daemon commits to starting. Catches
    /// the misconfigurations that would otherwise fail opaquely at first dictation
    /// (missing model, bad injection mode, out-of-range thresholds).
    pub fn validate(&self) -> Result<(), String> {
        let bin = self.stt.server_bin_path();
        if !bin.exists() {
            return Err(format!(
                "whisper-server not found at {} — run `whispy-daemon setup whisper`",
                bin.display()
            ));
        }
        let model = self.stt.model_file();
        if !model.exists() {
            return Err(format!(
                "model file not found at {} — run `whispy-daemon setup model`",
                model.display()
            ));
        }

        match self.injection.mode.as_str() {
            "paste" => {
                if self.injection.paste_keys.trim().is_empty() {
                    return Err("injection.paste_keys must be set when mode = \"paste\"".into());
                }
            }
            "type" => {}
            other => {
                return Err(format!(
                    "injection.mode must be \"paste\" or \"type\", got {other:?}"
                ));
            }
        }

        if self.audio.gain <= 0.0 || !self.audio.gain.is_finite() {
            return Err(format!("audio.gain must be > 0, got {}", self.audio.gain));
        }
        if !(0.0..=1.0).contains(&self.filter.fuzzy_ratio) {
            return Err(format!(
                "filter.fuzzy_ratio must be in [0.0, 1.0], got {}",
                self.filter.fuzzy_ratio
            ));
        }
        if self.audio.silence_rms_threshold < 0.0 {
            return Err(format!(
                "audio.silence_rms_threshold must be >= 0, got {}",
                self.audio.silence_rms_threshold
            ));
        }

        Ok(())
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
        assert!(cfg.workflow.is_empty());
        assert_eq!(cfg.llm.base_url, "http://127.0.0.1:11434/v1");
    }

    #[test]
    fn silence_windows_respects_timeout_and_interval() {
        let mut audio = Config::load(None).unwrap().audio;
        audio.rms_interval_ms = 80;

        audio.silence_timeout_ms = 0;
        assert_eq!(audio.silence_windows(), None, "0 disables auto-stop");

        audio.silence_timeout_ms = 2000;
        assert_eq!(
            audio.silence_windows(),
            Some(25),
            "2000ms / 80ms = 25 windows"
        );

        // Rounds up so a sub-window timeout still arms at least one window.
        audio.silence_timeout_ms = 50;
        assert_eq!(audio.silence_windows(), Some(1));
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

    #[test]
    fn expand_resolves_leading_tilde() {
        // Read (don't mutate) HOME so this is safe under parallel test execution.
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(expand("~/x/y"), PathBuf::from(home).join("x/y"));
        }
        // Absolute and bare paths pass through untouched.
        assert_eq!(
            expand("/etc/whispy.toml"),
            PathBuf::from("/etc/whispy.toml")
        );
        assert_eq!(expand("relative/path"), PathBuf::from("relative/path"));
        // A `~` not followed by `/` is left alone.
        assert_eq!(expand("~tilde"), PathBuf::from("~tilde"));
    }

    /// A config that passes `validate()`: defaults with `server_bin`/`model_path`
    /// pointed at real temp files (existence is one of the checks).
    fn valid_config() -> (Config, tempfiles::Guard) {
        let guard = tempfiles::two();
        let mut cfg = Config::load(None).unwrap();
        cfg.stt.server_bin = guard.bin.to_string_lossy().into_owned();
        cfg.stt.model_path = guard.model.to_string_lossy().into_owned();
        (cfg, guard)
    }

    #[test]
    fn validate_accepts_a_good_config() {
        let (cfg, _g) = valid_config();
        assert!(cfg.validate().is_ok(), "{:?}", cfg.validate());
    }

    #[test]
    fn validate_rejects_bad_fields() {
        let (mut cfg, _g) = valid_config();
        cfg.injection.mode = "nope".into();
        assert!(cfg.validate().is_err());

        let (mut cfg, _g) = valid_config();
        cfg.injection.mode = "paste".into();
        cfg.injection.paste_keys = "  ".into();
        assert!(cfg.validate().is_err());

        let (mut cfg, _g) = valid_config();
        cfg.audio.gain = 0.0;
        assert!(cfg.validate().is_err());

        let (mut cfg, _g) = valid_config();
        cfg.filter.fuzzy_ratio = 1.5;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_reports_missing_model() {
        let (mut cfg, _g) = valid_config();
        cfg.stt.model_path = "/nonexistent/model.bin".into();
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("model file not found"), "{err}");
    }

    /// Tiny helper to create two throwaway files for the path-existence checks.
    mod tempfiles {
        use std::path::PathBuf;

        pub struct Guard {
            pub bin: PathBuf,
            pub model: PathBuf,
            dir: PathBuf,
        }

        impl Drop for Guard {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.dir).ok();
            }
        }

        pub fn two() -> Guard {
            let dir = std::env::temp_dir().join(format!(
                "whispy-cfg-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let bin = dir.join("whisper-server");
            let model = dir.join("model.bin");
            std::fs::write(&bin, b"").unwrap();
            std::fs::write(&model, b"").unwrap();
            Guard { bin, model, dir }
        }
    }
}
