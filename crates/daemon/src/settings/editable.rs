//! The editable settings: every config value the settings screen changes, declared
//! once in [`SETTINGS`] with its `[table] key`, its options (labels and hints), how
//! to read it from a [`Config`], and how to write a chosen value into the user's
//! TOML *text* so comments and layout survive (`toml_edit`).
//!
//! [`sections`] turns the table into the form's sections; [`save`] is the one
//! write path: apply every setting to the document, parse the result as a
//! [`Config`], [`Config::validate`] it, and only then replace the file. A form
//! value a setting can't parse is an error, never a silent default.

use std::path::Path;

use toml_edit::{DocumentMut, Item, Value};

use super::form::{Body, Choice, Form, Section, SectionKey};
use super::sources::{self, Source};
use crate::config::{self, Config, ConfigChoice, InjectMode};
use crate::engine::Engine;

/// One editable setting.
struct Setting {
    /// The form section that edits it.
    key: SectionKey,
    title: &'static str,
    /// One-line explanation shown above the options.
    hint: &'static str,
    /// Its `[table]` in `config.toml`.
    table: &'static str,
    /// Its key in that table.
    name: &'static str,
    options: Options,
    /// The current value in a loaded config, as the form holds it.
    read: fn(&Config) -> String,
    /// A form value as the TOML value to store, or why it isn't one.
    write: fn(&str) -> Result<Value, String>,
}

/// Where a setting's options come from.
enum Options {
    /// A fixed list.
    Fixed(fn() -> Vec<Choice>),
    /// The `PipeWire` capture sources present right now.
    Microphones,
}

/// The settings, in screen order.
const SETTINGS: &[Setting] = &[
    Setting {
        key: SectionKey::Microphone,
        title: "Microphone",
        hint: "Where dictation records from. The meter follows the highlighted one.",
        table: "audio",
        name: "source",
        options: Options::Microphones,
        read: |cfg| cfg.audio.source.clone(),
        write: |v| Ok(v.into()),
    },
    Setting {
        key: SectionKey::Engine,
        title: "Engine",
        hint: "The speech-to-text model. Switching needs it installed: whispy-daemon setup.",
        table: "stt",
        name: "engine",
        options: Options::Fixed(|| {
            keywords(|engine| match engine {
                Engine::Parakeet => (
                    "Parakeet",
                    "fastest, punctuates well, no text on silence · 25 European languages",
                ),
                Engine::Whisper => (
                    "Whisper",
                    "~100 languages · vocabulary as a decoding prompt",
                ),
            })
        }),
        read: |cfg| cfg.stt.engine.as_str().into(),
        write: keyword::<Engine>,
    },
    Setting {
        key: SectionKey::Language,
        title: "Language",
        hint: "Automatic detects the language of each dictation, so you can switch freely.",
        table: "stt",
        name: "language",
        options: Options::Fixed(|| {
            listed(&[
                ("Automatic", "auto", "detected per dictation"),
                ("Italiano", "it", ""),
                ("English", "en", ""),
                ("Español", "es", ""),
                ("Français", "fr", ""),
                ("Deutsch", "de", ""),
                ("Português", "pt", ""),
            ])
        }),
        read: |cfg| cfg.stt.language.clone(),
        write: |v| Ok(v.into()),
    },
    Setting {
        key: SectionKey::Typing,
        title: "Typing",
        hint: "How the transcript reaches the focused window.",
        table: "injection",
        name: "mode",
        options: Options::Fixed(|| {
            keywords(|mode| match mode {
                InjectMode::Type => ("Type", "wtype types it; works in terminals"),
                InjectMode::Paste => ("Paste", "clipboard + Ctrl+V; needs ydotoold"),
            })
        }),
        read: |cfg| cfg.injection.mode.as_str().into(),
        write: keyword::<InjectMode>,
    },
    Setting {
        key: SectionKey::AutoStop,
        title: "Auto-stop",
        hint: "After you stop talking, stop recording and transcribe after this much silence.",
        table: "audio",
        name: "silence_timeout_ms",
        options: Options::Fixed(|| {
            listed(&[
                ("Off", "0", "stop only with the key"),
                ("1 s", "1000", ""),
                ("1.5 s", "1500", ""),
                ("2 s", "2000", ""),
                ("3 s", "3000", ""),
                ("5 s", "5000", ""),
            ])
        }),
        read: |cfg| cfg.audio.silence_timeout_ms.to_string(),
        write: |v| {
            let ms: u64 = v
                .parse()
                .map_err(|e| format!("{v:?} is not a duration in ms ({e})"))?;
            i64::try_from(ms)
                .map(Value::from)
                .map_err(|e| e.to_string())
        },
    },
];

/// The options of a [`ConfigChoice`], in its `ALL` order, labeled by `describe`
/// (label, detail).
fn keywords<T: ConfigChoice>(describe: fn(T) -> (&'static str, &'static str)) -> Vec<Choice> {
    T::ALL
        .iter()
        .map(|&choice| {
            let (label, detail) = describe(choice);
            Choice::new(label, choice.as_str(), detail)
        })
        .collect()
}

/// Options given as (label, value, detail).
fn listed(options: &[(&str, &str, &str)]) -> Vec<Choice> {
    options
        .iter()
        .map(|&(label, value, detail)| Choice::new(label, value, detail))
        .collect()
}

/// Write a [`ConfigChoice`] keyword, rejecting anything config loading would.
fn keyword<T: ConfigChoice>(value: &str) -> Result<Value, String> {
    T::parse(value)
        .map(|choice| choice.as_str().into())
        .ok_or_else(|| {
            let known: Vec<&str> = T::ALL.iter().map(|c| c.as_str()).collect();
            format!("unknown value {value:?} (expected one of {known:?})")
        })
}

/// The screen's setting sections, each with `cfg`'s current value chosen. The
/// Microphone options are the capture `sources` (and the `default_source` name).
pub fn sections(cfg: &Config, sources: &[Source], default_source: Option<&str>) -> Vec<Section> {
    SETTINGS
        .iter()
        .map(|setting| {
            let current = (setting.read)(cfg);
            let (choices, chosen) = match setting.options {
                Options::Microphones => {
                    sources::microphone_choices(sources, default_source, &current)
                }
                Options::Fixed(options) => radio(options(), &current),
            };
            Section {
                key: setting.key,
                title: setting.title,
                hint: setting.hint,
                body: Body::Radio { choices, chosen },
            }
        })
        .collect()
}

/// Fixed `choices` with `current` chosen; a current value outside them is kept as
/// an extra option so opening the screen never changes the config by itself.
fn radio(mut choices: Vec<Choice>, current: &str) -> (Vec<Choice>, usize) {
    if !choices.iter().any(|c| c.value == current) {
        choices.push(Choice::new(current, current, "current"));
    }
    let chosen = choices.iter().position(|c| c.value == current).unwrap_or(0);
    (choices, chosen)
}

/// Write the `form`'s chosen values into the config at `path` (or into the
/// built-in defaults when it doesn't exist yet). The edited config must parse and
/// pass [`Config::validate`], as the daemon checks at start, before the file is
/// replaced (atomically, tmp + rename): saving a missing engine binary or model
/// would leave whispy down after the restart.
pub fn save(path: &Path, form: &Form) -> Result<(), String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            config::default_config_toml().to_string()
        }
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let out = apply(&text, form)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, out)
        .and_then(|()| std::fs::rename(&tmp, path))
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// The config TOML `text` with every setting set to the `form`'s choice, keeping
/// everything else (comments, order, other keys) as it was. Errors when a value
/// doesn't parse or the result isn't a valid daemon config.
fn apply(text: &str, form: &Form) -> Result<String, String> {
    let mut doc: DocumentMut = text.parse().map_err(|e| format!("config.toml: {e}"))?;
    for setting in SETTINGS {
        let place = format!("{}.{}", setting.table, setting.name);
        let chosen = form
            .value(setting.key)
            .ok_or_else(|| format!("{place}: nothing chosen"))?;
        let value = (setting.write)(chosen).map_err(|e| format!("{place}: {e}"))?;
        set(&mut doc, setting.table, setting.name, value);
    }
    let out = doc.to_string();
    let cfg: Config = toml::from_str(&out).map_err(|e| format!("edited config is invalid: {e}"))?;
    cfg.validate()?;
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// Throwaway engine binaries and models, so a config can pass `validate`.
    struct Files {
        dir: PathBuf,
    }

    impl Files {
        fn new(test: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("whispy-editable-{test}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            for f in ["whisper-server", "model.bin", "whispy-stt", "parakeet.gguf"] {
                std::fs::write(dir.join(f), b"").unwrap();
            }
            Self { dir }
        }

        /// The embedded defaults with both engines' files pointed at these.
        fn base_text(&self) -> String {
            let mut doc: DocumentMut = config::default_config_toml().parse().unwrap();
            for (key, file) in [
                ("server_bin", "whisper-server"),
                ("model_path", "model.bin"),
                ("parakeet_bin", "whispy-stt"),
                ("parakeet_model_path", "parakeet.gguf"),
            ] {
                let path = self.dir.join(file).to_string_lossy().into_owned();
                set(&mut doc, "stt", key, path.into());
            }
            doc.to_string()
        }
    }

    impl Drop for Files {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    fn sources() -> Vec<Source> {
        ["podmic-mono", "kuroko-mic-aec"]
            .map(|name| Source {
                name: name.into(),
                description: name.into(),
            })
            .to_vec()
    }

    fn parse(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    fn form_for(cfg: &Config) -> Form {
        Form::new(sections(cfg, &sources(), Some("podmic-mono")))
    }

    fn setting(key: SectionKey) -> &'static Setting {
        SETTINGS.iter().find(|s| s.key == key).unwrap()
    }

    /// Replace the options of the `key` section with the single `value`.
    fn force(form: &mut Form, key: SectionKey, value: &str) {
        let section = form.sections.iter_mut().find(|s| s.key == key).unwrap();
        section.body = Body::Radio {
            choices: vec![Choice::new(value, value, "")],
            chosen: 0,
        };
    }

    #[test]
    fn every_option_round_trips_and_validates() {
        let files = Files::new("round-trip");
        let base = files.base_text();
        let cfg = parse(&base);
        for setting in SETTINGS {
            let form = form_for(&cfg);
            let section = form.sections.iter().find(|s| s.key == setting.key).unwrap();
            let Body::Radio { choices, .. } = &section.body else {
                panic!("{} is a radio", setting.title);
            };
            assert!(choices.len() > 1, "{} offers options", setting.title);
            for choice in choices {
                // config → form: a config holding this option shows it chosen…
                let mut doc: DocumentMut = base.parse().unwrap();
                let value = (setting.write)(&choice.value).unwrap();
                set(&mut doc, setting.table, setting.name, value);
                let with = parse(&doc.to_string());
                assert_eq!((setting.read)(&with), choice.value);
                let form = form_for(&with);
                assert_eq!(form.value(setting.key), Some(choice.value.as_str()));
                // …and form → document → parse gives it back, validated.
                let out = apply(&base, &form)
                    .unwrap_or_else(|e| panic!("{} = {:?}: {e}", setting.name, choice.value));
                assert_eq!((setting.read)(&parse(&out)), choice.value);
            }
        }
    }

    #[test]
    fn enum_options_come_from_their_keywords() {
        let cfg = parse(config::default_config_toml());
        let form = form_for(&cfg);
        let values = |key| {
            let section = form.sections.iter().find(|s| s.key == key).unwrap();
            let Body::Radio { choices, .. } = &section.body else {
                panic!("radio");
            };
            choices.iter().map(|c| c.value.clone()).collect::<Vec<_>>()
        };
        assert_eq!(values(SectionKey::Engine), ["parakeet", "whisper"]);
        assert_eq!(values(SectionKey::Typing), ["type", "paste"]);
    }

    #[test]
    fn an_unparseable_form_value_is_an_error() {
        let files = Files::new("invalid");
        let base = files.base_text();
        for (key, bad) in [
            (SectionKey::AutoStop, "soon"),
            (SectionKey::AutoStop, "-1"),
            (SectionKey::Engine, "vosk"),
            (SectionKey::Typing, "Paste"),
        ] {
            let mut form = form_for(&parse(&base));
            force(&mut form, key, bad);
            let err = apply(&base, &form).unwrap_err();
            let s = setting(key);
            assert!(err.contains(&format!("{}.{}", s.table, s.name)), "{err}");
            assert!(err.contains(bad), "{err}");
        }
    }

    #[test]
    fn an_invalid_config_is_not_saved() {
        let files = Files::new("not-saved");
        let path = files.dir.join("config.toml");
        let text = files.base_text().replace("model.bin", "missing.bin");
        std::fs::write(&path, &text).unwrap();
        let mut form = form_for(&parse(&text));
        force(&mut form, SectionKey::Engine, "whisper");
        let err = save(&path, &form).unwrap_err();
        assert!(err.contains("model file not found"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text, "untouched");
    }

    #[test]
    fn save_keeps_comments_and_changes_only_the_edited_value() {
        let files = Files::new("save");
        let path = files.dir.join("config.toml");
        let text = files
            .base_text()
            .replace("language = \"auto\"", "language = \"auto\"   # I speak two");
        assert!(text.contains("# I speak two"));
        std::fs::write(&path, &text).unwrap();
        let mut form = form_for(&parse(&text));
        force(&mut form, SectionKey::Language, "it");
        save(&path, &form).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("# whisper's input format"), "comments kept");
        assert_eq!(
            saved,
            text.replace("language = \"auto\"", "language = \"it\"")
        );
    }

    #[test]
    fn set_keeps_comments_and_trailing_comments() {
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
        let files = Files::new("missing-key");
        let text = files.base_text().replace("source = \"\"\n", "");
        assert!(!text.contains("source ="));
        let mut form = form_for(&parse(&text));
        force(&mut form, SectionKey::Microphone, "podmic-mono");
        let out = apply(&text, &form).unwrap();
        assert!(out.contains("source = \"podmic-mono\""));
    }

    #[test]
    fn rejects_text_that_is_not_toml() {
        let form = form_for(&parse(config::default_config_toml()));
        apply("[stt", &form).unwrap_err();
    }

    #[test]
    fn an_unknown_current_value_stays_chosen() {
        let mut cfg = parse(config::default_config_toml());
        cfg.stt.language = "ja".into();
        cfg.audio.silence_timeout_ms = 2500;
        let form = form_for(&cfg);
        assert_eq!(form.value(SectionKey::Language), Some("ja"));
        assert_eq!(form.value(SectionKey::AutoStop), Some("2500"));
    }
}
