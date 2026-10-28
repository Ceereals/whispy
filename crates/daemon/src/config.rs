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
    pub ui: Ui,
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
    /// whisper.cpp compute backend that `setup whisper` builds whisper-server with:
    /// `"auto"` (Vulkan when the loader is present, else CPU), `"vulkan"`, or `"cpu"`.
    /// Only consulted at build time; an installed binary keeps whatever it was built with.
    #[serde(default = "default_backend")]
    pub backend: String,
    /// Hard cap on a single `/inference` request. Without it a hung whisper-server
    /// (e.g. a GPU/Vulkan lockup) would strand the daemon in `transcribing` forever.
    #[serde(default = "default_stt_timeout_secs")]
    pub timeout_secs: u64,
    /// Speech-to-text engine the daemon runs as its STT server: `"parakeet"`
    /// (`whispy-stt` on transcribe.cpp) or `"whisper"` (whisper.cpp's
    /// whisper-server). Configs written before this option keep `"whisper"`.
    #[serde(default = "default_engine")]
    pub engine: String,
    /// Parakeet GGUF model (`engine = "parakeet"`).
    #[serde(default = "default_parakeet_model_path")]
    pub parakeet_model_path: String,
    /// `whispy-stt` binary (`engine = "parakeet"`). Empty: the one installed next
    /// to `whispy-daemon`, else the first on `PATH`.
    #[serde(default)]
    pub parakeet_bin: String,
}

fn default_engine() -> String {
    "whisper".to_string()
}

fn default_parakeet_model_path() -> String {
    "~/.local/share/whispy/models/parakeet-ultra-Q8_0.gguf".to_string()
}

fn default_backend() -> String {
    "auto".to_string()
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
    /// `PipeWire` source (node name) to capture from, passed to `pw-record --target`.
    /// Empty uses the session's default source.
    #[serde(default)]
    pub source: String,
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
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "float-to-int `as` saturates (negative -> 0), which is the intended clamp"
    )]
    pub fn max_samples(&self) -> usize {
        (f64::from(self.rate) * self.max_clip_secs) as usize
    }

    /// Below this sample count a clip is discarded silently.
    pub fn min_samples(&self) -> usize {
        (u64::from(self.rate) * self.min_clip_ms / 1000) as usize
    }

    /// Number of samples per RMS reporting window.
    pub fn rms_window(&self) -> usize {
        ((u64::from(self.rate) * self.rms_interval_ms / 1000) as usize).max(1)
    }

    /// Consecutive silent RMS windows that trigger the trailing-silence auto-stop,
    /// or `None` when the feature is disabled (`silence_timeout_ms == 0`).
    #[expect(
        clippy::cast_possible_truncation,
        reason = "Linux targets are 64-bit; window counts fit usize"
    )]
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
    /// "type"  = wtype/xdotool types the transcript directly (works in terminals too).
    #[serde(default = "default_mode")]
    pub mode: String,
    /// Display server / injection backend: "auto" (detect via `WAYLAND_DISPLAY`/`DISPLAY`),
    /// "wayland" (wl-copy/wl-paste/wtype), or "x11" (xclip/xsel/xdotool). ydotool
    /// delivers the paste keystroke on both.
    #[serde(default = "default_inject_backend")]
    pub backend: String,
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

fn default_inject_backend() -> String {
    "auto".to_string()
}

/// UI / status-feedback options.
#[derive(Debug, Clone, Deserialize)]
pub struct Ui {
    /// Desktop notifications on transient state transitions (`success`/`error`),
    /// for sessions without the Quickshell pill:
    ///   "auto" = notify only on X11 (where the layer-shell pill can't run),
    ///   "on"   = always notify, "off" = never.
    #[serde(default = "default_notify")]
    pub notify: String,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            notify: default_notify(),
        }
    }
}

fn default_notify() -> String {
    "auto".to_string()
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
        match self.stt.engine.as_str() {
            "parakeet" | "whisper" => {}
            other => {
                return Err(format!(
                    "stt.engine must be \"parakeet\" or \"whisper\", got {other:?}"
                ));
            }
        }
        let bin = self.stt.server_bin_path();
        if !bin.exists() {
            let step = if self.stt.is_parakeet() {
                "stt"
            } else {
                "whisper"
            };
            return Err(format!(
                "{} not found at {} — run `whispy-daemon setup {step}`",
                self.stt.server_name(),
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

        match self.stt.backend.as_str() {
            "auto" | "vulkan" | "cpu" => {}
            other => {
                return Err(format!(
                    "stt.backend must be \"auto\", \"vulkan\", or \"cpu\", got {other:?}"
                ));
            }
        }

        // The clip goes to whisper as a WAV that is always labeled 16 kHz mono
        // (whisper's native input); any other capture format would be misread.
        if self.audio.rate != 16000 || self.audio.channels != 1 {
            return Err(format!(
                "audio.rate must be 16000 and audio.channels 1 (whisper's input format), got {} Hz / {} ch",
                self.audio.rate, self.audio.channels
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

        match self.injection.backend.as_str() {
            "auto" | "wayland" | "x11" => {}
            other => {
                return Err(format!(
                    "injection.backend must be \"auto\", \"wayland\", or \"x11\", got {other:?}"
                ));
            }
        }

        match self.ui.notify.as_str() {
            "auto" | "on" | "off" => {}
            other => {
                return Err(format!(
                    "ui.notify must be \"auto\", \"on\", or \"off\", got {other:?}"
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
    /// Whether the configured engine is Parakeet (`whispy-stt`).
    pub fn is_parakeet(&self) -> bool {
        self.engine == "parakeet"
    }

    /// The STT server program's name, for logs and messages.
    pub fn server_name(&self) -> &'static str {
        if self.is_parakeet() {
            "whispy-stt"
        } else {
            "whisper-server"
        }
    }

    /// The STT server binary the daemon runs for the configured engine.
    pub fn server_bin_path(&self) -> PathBuf {
        if self.is_parakeet() {
            self.parakeet_bin_path()
        } else {
            self.whisper_server_bin()
        }
    }

    /// The model file for the configured engine.
    pub fn model_file(&self) -> PathBuf {
        if self.is_parakeet() {
            expand(&self.parakeet_model_path)
        } else {
            expand(&self.model_path)
        }
    }

    /// whisper.cpp's whisper-server (built by `setup whisper`), whatever the engine.
    pub fn whisper_server_bin(&self) -> PathBuf {
        expand(&self.server_bin)
    }

    /// `whispy-stt`: `parakeet_bin` if set, else next to this executable, else on
    /// `PATH`, else `~/.cargo/bin/whispy-stt` (where `setup stt` installs it).
    pub fn parakeet_bin_path(&self) -> PathBuf {
        if !self.parakeet_bin.is_empty() {
            return expand(&self.parakeet_bin);
        }
        let sibling = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join(STT_BIN)));
        let on_path: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|p| {
                std::env::split_paths(&p)
                    .map(|dir| dir.join(STT_BIN))
                    .collect()
            })
            .unwrap_or_default();
        first_existing(sibling.into_iter().chain(on_path))
            .unwrap_or_else(|| expand("~/.cargo/bin").join(STT_BIN))
    }
}

/// The Parakeet STT server binary's file name.
pub const STT_BIN: &str = "whispy-stt";

/// The first candidate path that exists.
fn first_existing(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(|p| p.is_file())
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
    std::env::var_os("XDG_RUNTIME_DIR").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from)
}

/// `$XDG_STATE_HOME/whispy` (default `~/.local/state/whispy`): logs and transcripts.
pub fn state_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("whispy")
}

/// Size caps for the files the daemon appends to in [`state_dir`]. Each keeps one
/// rotated generation (`<name>.1`), so disk use stays under twice the cap.
pub const DAEMON_LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;
pub const WHISPER_LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;
pub const TRANSCRIPTS_MAX_BYTES: u64 = 5 * 1024 * 1024;

/// The previous generation of a rotated file: `<path>.1`.
pub fn rotated(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".1");
    PathBuf::from(name)
}

/// If `path` has grown past `max_bytes`, move it to `<path>.1` (replacing any older
/// generation) so the next write starts a fresh file. Best-effort: a missing
/// file is not an error, and the caller just keeps appending if the rename fails.
pub fn rotate_if_larger(path: &Path, max_bytes: u64) -> std::io::Result<()> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() > max_bytes => std::fs::rename(path, rotated(path)),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
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
    fn rotate_if_larger_moves_only_oversized_files() {
        let dir = std::env::temp_dir().join(format!("whispy-rotate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("transcripts.jsonl");

        // Missing file: nothing to do.
        rotate_if_larger(&log, 4).expect("missing file is fine");

        // At or under the cap: left alone.
        std::fs::write(&log, b"1234").unwrap();
        rotate_if_larger(&log, 4).unwrap();
        assert!(log.exists());
        assert!(!rotated(&log).exists());

        // Over the cap: moved to .1, replacing an older generation.
        std::fs::write(rotated(&log), b"old").unwrap();
        std::fs::write(&log, b"12345").unwrap();
        rotate_if_larger(&log, 4).unwrap();
        assert!(!log.exists());
        assert_eq!(std::fs::read(rotated(&log)).unwrap(), b"12345");
        assert_eq!(rotated(&log), dir.join("transcripts.jsonl.1"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn configs_without_an_engine_stay_on_whisper() {
        // A config.toml written before `stt.engine` existed keeps its whisper setup.
        let old: String = DEFAULT_TOML
            .lines()
            .filter(|l| !l.trim_start().starts_with("engine"))
            .collect::<Vec<_>>()
            .join("\n");
        let cfg: Config = toml::from_str(&old).expect("old config parses");
        assert_eq!(cfg.stt.engine, "whisper");
        assert_eq!(cfg.stt.server_name(), "whisper-server");
        assert_eq!(cfg.stt.model_file(), expand(&cfg.stt.model_path));
        assert_eq!(cfg.stt.server_bin_path(), cfg.stt.whisper_server_bin());
    }

    #[test]
    fn parakeet_engine_uses_its_own_model_and_server() {
        let mut cfg: Config = toml::from_str(DEFAULT_TOML).expect("defaults parse");
        cfg.stt.engine = "parakeet".into();
        cfg.stt.parakeet_bin = "/opt/whispy/whispy-stt".into();
        assert!(cfg.stt.is_parakeet());
        assert_eq!(cfg.stt.server_name(), "whispy-stt");
        assert_eq!(
            cfg.stt.server_bin_path(),
            PathBuf::from("/opt/whispy/whispy-stt")
        );
        assert_eq!(cfg.stt.model_file(), expand(&cfg.stt.parakeet_model_path));
        assert_eq!(
            cfg.stt.model_file().extension().and_then(|e| e.to_str()),
            Some("gguf")
        );
    }

    #[test]
    fn unknown_engine_is_rejected() {
        let mut cfg: Config = toml::from_str(DEFAULT_TOML).expect("defaults parse");
        cfg.stt.engine = "vosk".into();
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("stt.engine"), "{err}");
    }

    #[test]
    fn first_existing_skips_missing_candidates() {
        let here = std::env::current_exe().expect("test binary path");
        assert_eq!(
            first_existing([PathBuf::from("/nonexistent/whispy-stt"), here.clone()]),
            Some(here)
        );
        assert_eq!(first_existing([PathBuf::from("/nonexistent")]), None);
    }

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
        // The embedded defaults, not Config::load(None): that would read the
        // developer's own ~/.config/whispy/config.toml.
        let mut cfg: Config = toml::from_str(DEFAULT_TOML).unwrap();
        // These checks exercise the whisper paths; parakeet's are covered below.
        cfg.stt.engine = "whisper".into();
        cfg.stt.server_bin = guard.bin.to_string_lossy().into_owned();
        cfg.stt.model_path = guard.model.to_string_lossy().into_owned();
        (cfg, guard)
    }

    #[test]
    fn validate_checks_the_parakeet_engine_files() {
        let (mut cfg, g) = valid_config();
        cfg.stt.engine = "parakeet".into();
        cfg.stt.parakeet_bin = g.bin.to_string_lossy().into_owned();
        cfg.stt.parakeet_model_path = g.model.to_string_lossy().into_owned();
        assert!(cfg.validate().is_ok(), "{:?}", cfg.validate());
        cfg.stt.parakeet_model_path = "/nonexistent/parakeet.gguf".into();
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("model file not found"), "{err}");
        cfg.stt.parakeet_bin = "/nonexistent/whispy-stt".into();
        let err = cfg.validate().unwrap_err();
        assert!(
            err.contains("whispy-stt not found") && err.contains("setup stt"),
            "{err}"
        );
    }

    #[test]
    fn validate_accepts_a_good_config() {
        let (cfg, _g) = valid_config();
        assert!(cfg.validate().is_ok(), "{:?}", cfg.validate());
    }

    #[test]
    fn validate_rejects_bad_fields() {
        let (mut cfg, _g) = valid_config();
        cfg.audio.rate = 48000;
        assert!(cfg.validate().is_err());

        let (mut cfg, _g) = valid_config();
        cfg.audio.channels = 2;
        assert!(cfg.validate().is_err());

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

        let (mut cfg, _g) = valid_config();
        cfg.stt.backend = "rocm".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_accepts_each_known_backend() {
        for backend in ["auto", "vulkan", "cpu"] {
            let (mut cfg, _g) = valid_config();
            cfg.stt.backend = backend.into();
            assert!(cfg.validate().is_ok(), "{backend} should be valid");
        }
    }

    #[test]
    fn validate_accepts_each_known_injection_backend() {
        for backend in ["auto", "wayland", "x11"] {
            let (mut cfg, _g) = valid_config();
            cfg.injection.backend = backend.into();
            assert!(cfg.validate().is_ok(), "{backend} should be valid");
        }
        let (mut cfg, _g) = valid_config();
        cfg.injection.backend = "mir".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_accepts_each_known_notify_mode() {
        for mode in ["auto", "on", "off"] {
            let (mut cfg, _g) = valid_config();
            cfg.ui.notify = mode.into();
            assert!(cfg.validate().is_ok(), "{mode} should be valid");
        }
        let (mut cfg, _g) = valid_config();
        cfg.ui.notify = "popup".into();
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
