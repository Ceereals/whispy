//! `PipeWire` capture sources for the Microphone section, via `pactl -f json`.
//! Parsing and choice-building are pure; [`list`] runs the tools.

use std::process::Command;
use std::time::Duration;

use super::form::Choice;
use crate::proc;

/// How long one `pactl` call may take.
const PACTL_TIMEOUT: Duration = Duration::from_secs(3);

/// One capture source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// Node name (what `audio.source` stores and `pw-record --target` takes).
    pub name: String,
    /// Human-readable description.
    pub description: String,
}

/// The capture sources and the name of the current default source. Best-effort:
/// a failing `pactl` yields an empty list (the "system default" choice remains).
pub fn list() -> (Vec<Source>, Option<String>) {
    let sources = proc::output(
        Command::new("pactl").args(["-f", "json", "list", "sources"]),
        PACTL_TIMEOUT,
    )
    .ok()
    .filter(|o| o.status.success())
    .map(|o| parse_sources(&String::from_utf8_lossy(&o.stdout)))
    .unwrap_or_default();
    let default = proc::output(
        Command::new("pactl").arg("get-default-source"),
        PACTL_TIMEOUT,
    )
    .ok()
    .filter(|o| o.status.success())
    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    .filter(|s| !s.is_empty());
    (sources, default)
}

/// Parse `pactl -f json list sources`, dropping sink monitors (they capture what
/// plays, not a microphone).
pub fn parse_sources(json: &str) -> Vec<Source> {
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str(json) else {
        return Vec::new();
    };
    items
        .iter()
        .filter(|s| s["properties"]["device.class"] != "monitor")
        .filter_map(|s| {
            let name = s["name"].as_str()?.to_string();
            let description = s["description"].as_str().unwrap_or(&name).to_string();
            Some(Source { name, description })
        })
        .collect()
}

/// The Microphone choices: "System default" first, then each source. A configured
/// source that isn't present (unplugged) is kept as a choice so it isn't lost.
/// Returns the choices and the index of `current`.
pub fn microphone_choices(
    sources: &[Source],
    default: Option<&str>,
    current: &str,
) -> (Vec<Choice>, usize) {
    let default_detail = default
        .map(|d| {
            let desc = sources
                .iter()
                .find(|s| s.name == d)
                .map_or(d, |s| s.description.as_str());
            format!("now: {desc}")
        })
        .unwrap_or_default();
    let mut choices = vec![Choice::new("System default", "", default_detail)];
    choices.extend(
        sources
            .iter()
            .map(|s| Choice::new(s.description.clone(), s.name.clone(), s.name.clone())),
    );
    if !current.is_empty() && !sources.iter().any(|s| s.name == current) {
        choices.push(Choice::new(current, current, "not connected"));
    }
    let chosen = choices.iter().position(|c| c.value == current).unwrap_or(0);
    (choices, chosen)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACTL: &str = r#"[
      {"name":"kuroko-mic-aec","description":"Rode PodMic (senza eco)","properties":{"media.class":"Audio/Source"}},
      {"name":"podmic-mono","description":"Rode PodMic","properties":{}},
      {"name":"alsa_output.hdmi.monitor","description":"Monitor of HDMI","properties":{"device.class":"monitor"}}
    ]"#;

    #[test]
    fn parses_sources_and_skips_monitors() {
        let s = parse_sources(PACTL);
        assert_eq!(
            s.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["kuroko-mic-aec", "podmic-mono"]
        );
        assert_eq!(s[1].description, "Rode PodMic");
        assert_eq!(parse_sources("not json"), Vec::new());
    }

    #[test]
    fn choices_start_with_the_system_default() {
        let sources = parse_sources(PACTL);
        let (choices, chosen) = microphone_choices(&sources, Some("kuroko-mic-aec"), "");
        assert_eq!(chosen, 0);
        assert_eq!(choices[0].value, "");
        assert_eq!(choices[0].detail, "now: Rode PodMic (senza eco)");
        assert_eq!(choices.len(), 3);
    }

    #[test]
    fn configured_source_is_chosen() {
        let sources = parse_sources(PACTL);
        let (choices, chosen) = microphone_choices(&sources, None, "podmic-mono");
        assert_eq!(choices[chosen].value, "podmic-mono");
    }

    #[test]
    fn unplugged_configured_source_is_kept() {
        let sources = parse_sources(PACTL);
        let (choices, chosen) = microphone_choices(&sources, None, "usb-headset");
        assert_eq!(choices[chosen].value, "usb-headset");
        assert_eq!(choices[chosen].detail, "not connected");
    }
}
