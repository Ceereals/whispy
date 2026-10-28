//! Reading the editable settings out of a [`Config`] and writing them back into the
//! user's TOML *text*, so comments and layout survive (`toml_edit`). [`apply`] is
//! pure; [`save`] adds the atomic file write.

use std::path::Path;

use toml_edit::{DocumentMut, Item, Value};

use crate::config::Config;
use crate::engine::Engine;

/// The settings the TUI edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Values {
    /// `audio.source` (empty = system default source).
    pub source: String,
    /// `stt.language` (`auto` or an ISO code).
    pub language: String,
    /// `injection.mode` (`type` or `paste`).
    pub mode: String,
    /// `audio.silence_timeout_ms` (0 = off).
    pub silence_timeout_ms: u64,
    /// `stt.engine`.
    pub engine: Engine,
}

impl Values {
    /// The current values of a loaded config.
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            source: cfg.audio.source.clone(),
            language: cfg.stt.language.clone(),
            mode: cfg.injection.mode.clone(),
            silence_timeout_ms: cfg.audio.silence_timeout_ms,
            engine: cfg.stt.engine,
        }
    }
}

/// Write `values` into the config TOML `text`, keeping everything else (comments,
/// order, other keys) as it was. The result is re-parsed as a [`Config`], so a
/// value that would break the daemon's config is an error instead of a file.
pub fn apply(text: &str, values: &Values) -> Result<String, String> {
    let mut doc: DocumentMut = text.parse().map_err(|e| format!("config.toml: {e}"))?;
    set(&mut doc, "audio", "source", values.source.as_str().into());
    set(&mut doc, "stt", "language", values.language.as_str().into());
    set(&mut doc, "injection", "mode", values.mode.as_str().into());
    set(&mut doc, "stt", "engine", values.engine.as_str().into());
    let timeout = i64::try_from(values.silence_timeout_ms).map_err(|e| e.to_string())?;
    set(&mut doc, "audio", "silence_timeout_ms", timeout.into());
    let out = doc.to_string();
    toml::from_str::<Config>(&out).map_err(|e| format!("edited config is invalid: {e}"))?;
    Ok(out)
}

/// Set `table.key`, keeping the old value's decoration (its spacing and trailing
/// `# comment`) when the key already exists.
fn set(doc: &mut DocumentMut, table: &str, key: &str, mut new: Value) {
    let item = &mut doc[table][key];
    if let Some(old) = item.as_value() {
        *new.decor_mut() = old.decor().clone();
    }
    *item = Item::Value(new);
}

/// Apply `values` to the config at `path` (or to the built-in defaults when it
/// doesn't exist yet) and write it atomically (tmp + rename).
pub fn save(path: &Path, values: &Values) -> Result<(), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            crate::config::default_config_toml().to_string()
        }
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let out = apply(&text, values)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, out)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> &'static str {
        crate::config::default_config_toml()
    }

    fn values() -> Values {
        Values {
            source: "podmic-mono".into(),
            language: "it".into(),
            mode: "paste".into(),
            silence_timeout_ms: 1500,
            engine: Engine::Parakeet,
        }
    }

    #[test]
    fn round_trips_through_the_daemon_config() {
        let out = apply(defaults(), &values()).unwrap();
        let cfg: Config = toml::from_str(&out).unwrap();
        assert_eq!(Values::from_config(&cfg), values());
    }

    #[test]
    fn keeps_comments_and_trailing_comments() {
        // `apply` validates a full config, so check the decor handling on `set`.
        let text = "# my config\n[stt]\nlanguage = \"auto\"   # I speak two languages\n";
        let mut doc: DocumentMut = text.parse().unwrap();
        set(&mut doc, "stt", "language", "it".into());
        assert_eq!(
            doc.to_string(),
            "# my config\n[stt]\nlanguage = \"it\"   # I speak two languages\n"
        );
    }

    #[test]
    fn adds_a_missing_key_to_an_existing_table() {
        // Configs written before `audio.source` existed don't have the key.
        let text = defaults().replace("source = \"\"\n", "");
        assert!(!text.contains("source ="));
        let out = apply(&text, &values()).unwrap();
        assert!(out.contains("source = \"podmic-mono\""));
    }

    #[test]
    fn rejects_text_that_is_not_toml() {
        apply("[stt", &values()).unwrap_err();
    }

    #[test]
    fn save_creates_the_file_from_defaults_and_preserves_it_after() {
        let dir = std::env::temp_dir().join(format!("whispy-store-{}", std::process::id()));
        let path = dir.join("config.toml");
        std::fs::remove_file(&path).ok();
        save(&path, &values()).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        assert!(
            first.contains("# whisper's input format"),
            "default comments kept"
        );
        let mut v = values();
        v.language = "en".into();
        save(&path, &v).unwrap();
        let second = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            second,
            first.replace("language = \"it\"", "language = \"en\"")
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
