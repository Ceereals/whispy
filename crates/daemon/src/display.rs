//! Display-server detection: choose which injection/clipboard tools the daemon
//! drives (Wayland vs X11) from the configured `injection.backend` override and
//! the session environment.
//!
//! The decision lives behind a pure, env-free [`resolve`] so it can be unit-tested;
//! [`resolve_from_env`] is the thin shim that reads `WAYLAND_DISPLAY`/`DISPLAY`.

/// Which display server the daemon should drive its injection/clipboard tools for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayServer {
    /// Wayland: `wl-copy`/`wl-paste` clipboard, `wtype` typing.
    Wayland,
    /// X11: `xclip`/`xsel` clipboard, `xdotool` typing.
    X11,
}

/// The configured `injection.backend` preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Detect from the environment (`WAYLAND_DISPLAY` then `DISPLAY`).
    Auto,
    /// Force the Wayland backend.
    Wayland,
    /// Force the X11 backend.
    X11,
}

impl Backend {
    /// Parse the config string. Unknown values are rejected by `Config::validate`
    /// before we get here, so this falls back to [`Backend::Auto`] defensively.
    pub fn parse(s: &str) -> Self {
        match s {
            "wayland" => Backend::Wayland,
            "x11" => Backend::X11,
            _ => Backend::Auto,
        }
    }
}

/// Resolve the display server from the override and the (already-read) env values.
///
/// `Auto` precedence: `WAYLAND_DISPLAY` wins (a Wayland session also exports
/// `DISPLAY` for XWayland clients, so Wayland-first is correct), then `DISPLAY`.
/// Returns `None` when nothing is set, so the caller can warn and pick a default.
pub fn resolve(
    over: Backend,
    wayland_display: Option<&str>,
    x_display: Option<&str>,
) -> Option<DisplayServer> {
    match over {
        Backend::Wayland => Some(DisplayServer::Wayland),
        Backend::X11 => Some(DisplayServer::X11),
        Backend::Auto => {
            if wayland_display.is_some_and(|s| !s.is_empty()) {
                Some(DisplayServer::Wayland)
            } else if x_display.is_some_and(|s| !s.is_empty()) {
                Some(DisplayServer::X11)
            } else {
                None
            }
        }
    }
}

/// Read `WAYLAND_DISPLAY`/`DISPLAY` from the real environment and [`resolve`].
/// Untested side-effecting shim (mirrors `pick_wayland_socket` vs the env reader).
pub fn resolve_from_env(over: Backend) -> Option<DisplayServer> {
    let wl = std::env::var("WAYLAND_DISPLAY").ok();
    let x = std::env::var("DISPLAY").ok();
    resolve(over, wl.as_deref(), x.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_override_wins_regardless_of_env() {
        assert_eq!(
            resolve(Backend::Wayland, None, Some(":0")),
            Some(DisplayServer::Wayland)
        );
        assert_eq!(
            resolve(Backend::X11, Some("wayland-0"), None),
            Some(DisplayServer::X11)
        );
    }

    #[test]
    fn auto_prefers_wayland_over_x11() {
        assert_eq!(
            resolve(Backend::Auto, Some("wayland-1"), Some(":0")),
            Some(DisplayServer::Wayland)
        );
    }

    #[test]
    fn auto_falls_back_to_x11_then_none() {
        assert_eq!(
            resolve(Backend::Auto, None, Some(":0")),
            Some(DisplayServer::X11)
        );
        assert_eq!(resolve(Backend::Auto, None, None), None);
        // Empty strings count as unset.
        assert_eq!(resolve(Backend::Auto, Some(""), Some("")), None);
    }

    #[test]
    fn parse_maps_known_strings_else_auto() {
        assert_eq!(Backend::parse("wayland"), Backend::Wayland);
        assert_eq!(Backend::parse("x11"), Backend::X11);
        assert_eq!(Backend::parse("auto"), Backend::Auto);
        assert_eq!(Backend::parse("bogus"), Backend::Auto);
    }
}
