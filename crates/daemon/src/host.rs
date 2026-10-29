//! Host tools: what dictation and the STT builds need from the machine, and
//! whether this host has it.
//!
//! One table of requirements, one probe, one verdict. `setup doctor`, the daemon's
//! startup warning, `setup whisper`/`setup stt`, and the X11 clipboard choice all
//! read from here, so they agree by construction (e.g. an X11 host with only
//! `xsel` satisfies the clipboard requirement everywhere, not just in injection).
//!
//! A requirement may have accepted alternatives (`xclip | xsel`), listed in order
//! of preference; its [`Check`] names the one that satisfied it. Probing goes
//! through the small [`Probe`] trait: [`System`] looks commands up on `PATH`
//! in-process, libraries via `ldconfig -p` (bounded by a timeout), and headers
//! under the system include dirs; tests use a fake.

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::display::DisplayServer;

/// How long `ldconfig -p` may take before a library probe gives up (reports absent).
const LDCONFIG_TIMEOUT: Duration = Duration::from_secs(5);

/// The system include dirs a header probe searches.
const INCLUDE_DIRS: [&str; 2] = ["/usr/include", "/usr/local/include"];

/// The shared library that marks a usable Vulkan loader.
const VULKAN_LOADER: &str = "libvulkan";

/// Asks the host whether something is installed.
pub trait Probe {
    /// Is `name` an executable on `PATH` (or, if it contains `/`, at that path)?
    fn command(&self, name: &str) -> bool;
    /// Is a shared library whose name contains `needle` known to the dynamic linker?
    fn library(&self, needle: &str) -> bool;
    /// Is `header` (relative, e.g. `vulkan/vulkan.h`) under a system include dir?
    fn header(&self, header: &str) -> bool;
}

/// The real host: the production [`Probe`].
#[derive(Debug, Clone, Copy)]
pub struct System;

impl Probe for System {
    fn command(&self, name: &str) -> bool {
        std::env::var_os("PATH").is_some_and(|path| on_path(name, &path))
    }

    fn library(&self, needle: &str) -> bool {
        match crate::proc::output(Command::new("ldconfig").arg("-p"), LDCONFIG_TIMEOUT) {
            Ok(out) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).contains(needle)
            }
            _ => false,
        }
    }

    fn header(&self, header: &str) -> bool {
        INCLUDE_DIRS
            .iter()
            .any(|dir| Path::new(dir).join(header).is_file())
    }
}

/// The verdict on one requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Check {
    /// The alternative that satisfied the requirement, or the preferred one when
    /// none did (so a "missing" report points at the default to install).
    pub name: &'static str,
    /// What it is for, shown next to the name.
    pub note: &'static str,
    /// Whether any accepted alternative is installed.
    pub ok: bool,
}

/// The runtime tools dictation needs for `server` and the injection mode
/// (`type_mode` for `injection.mode = "type"`), each checked against `probe`.
///
/// - Always: `pw-record` (capture), `ydotool` (paste keystroke, both servers),
///   `notify-send` (error/status notifications).
/// - Wayland: `wl-copy`/`wl-paste` for clipboard, plus `wtype` only in type mode.
/// - X11: a clipboard tool (`xclip`, else `xsel`) and `xdotool` (typing + window class).
pub fn dictation(probe: &impl Probe, server: DisplayServer, type_mode: bool) -> Vec<Check> {
    let mut needs = vec![&PW_RECORD, &YDOTOOL, &NOTIFY_SEND];
    match server {
        DisplayServer::Wayland => {
            needs.extend([&WL_COPY, &WL_PASTE]);
            if type_mode {
                needs.push(&WTYPE);
            }
        }
        DisplayServer::X11 => needs.extend([&X11_CLIPBOARD, &XDOTOOL]),
    }
    needs.into_iter().map(|r| r.check(probe)).collect()
}

/// The X11 clipboard tool to drive: `xclip` preferred, `xsel` fallback, `None`
/// when neither is installed. The same verdict [`dictation`] reports on X11.
pub fn x11_clipboard(probe: &impl Probe) -> Option<&'static str> {
    let check = X11_CLIPBOARD.check(probe);
    check.ok.then_some(check.name)
}

/// Resolve the configured `stt.backend` to a concrete backend for a build:
/// `"auto"` picks Vulkan when its loader is present, else CPU; the explicit values
/// pass through unchanged. Unknown strings are validated away at config load, but
/// resolve like `"auto"` here so `setup` can never build for one.
pub fn resolve_backend(probe: &impl Probe, configured: &str) -> &'static str {
    match configured {
        "vulkan" => "vulkan",
        "cpu" => "cpu",
        _ if probe.library(VULKAN_LOADER) => "vulkan",
        _ => "cpu",
    }
}

/// What a Vulkan build (whisper.cpp's or whispy-stt's ggml backend) needs, as found
/// on this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VulkanBuild {
    /// The Vulkan loader (`libvulkan`) is installed.
    pub loader: bool,
    /// The build-time dependencies, named by their Arch package (the AUR package
    /// depends on all of them); `note` says what each provides.
    pub deps: Vec<Check>,
}

/// Check the Vulkan build prerequisites on `probe`.
pub fn vulkan_build(probe: &impl Probe) -> VulkanBuild {
    VulkanBuild {
        loader: probe.library(VULKAN_LOADER),
        deps: VULKAN_BUILD_DEPS.iter().map(|r| r.check(probe)).collect(),
    }
}

impl VulkanBuild {
    /// `Ok` when the Vulkan build of `target` (e.g. `"whisper.cpp"`) can run, else
    /// an error naming what to install. Fails fast with a clear message where cmake
    /// would otherwise die deep in ggml's configure without naming the problem.
    pub fn ensure(&self, target: &str) -> Result<(), String> {
        const CPU_HINT: &str =
            "Or set `stt.backend = \"cpu\"` in config.toml for a CPU-only build.";
        if !self.loader {
            return Err(format!(
                "the Vulkan loader (libvulkan) is missing — the Vulkan build of {target} needs it.\n  \
                 Install it (Arch: `vulkan-icd-loader` + your GPU's ICD, e.g. `vulkan-radeon`), then re-run.\n  \
                 {CPU_HINT}"
            ));
        }
        let missing: Vec<&Check> = self.deps.iter().filter(|d| !d.ok).collect();
        if missing.is_empty() {
            return Ok(());
        }
        let what: Vec<&str> = missing.iter().map(|d| d.note).collect();
        let packages: Vec<&str> = missing.iter().map(|d| d.name).collect();
        Err(format!(
            "the Vulkan build of {target} needs: {}.\n  \
             Install them (Arch: `sudo pacman -S --needed {}`), then re-run.\n  \
             {CPU_HINT}",
            what.join(", "),
            packages.join(" ")
        ))
    }
}

// --- the requirement table ----------------------------------------------------

/// One thing the host must provide, satisfied by any of its alternatives.
struct Requirement {
    /// Accepted alternatives, most preferred first. Never empty.
    any_of: &'static [Alternative],
    /// What it is for.
    note: &'static str,
}

/// One way to satisfy a [`Requirement`]: a display name and how to probe for it.
struct Alternative {
    name: &'static str,
    need: Need,
}

/// What to probe for.
#[derive(Clone, Copy)]
enum Need {
    Command(&'static str),
    Header(&'static str),
}

impl Requirement {
    fn check(&self, probe: &impl Probe) -> Check {
        let found = self.any_of.iter().find(|alt| match alt.need {
            Need::Command(cmd) => probe.command(cmd),
            Need::Header(header) => probe.header(header),
        });
        let preferred = self.any_of.first().map_or("", |alt| alt.name);
        Check {
            name: found.map_or(preferred, |alt| alt.name),
            note: self.note,
            ok: found.is_some(),
        }
    }
}

/// An alternative that is a command of the same name.
const fn cmd(name: &'static str) -> Alternative {
    Alternative {
        name,
        need: Need::Command(name),
    }
}

static PW_RECORD: Requirement = Requirement {
    any_of: &[cmd("pw-record")],
    note: "PipeWire audio capture",
};
static YDOTOOL: Requirement = Requirement {
    any_of: &[cmd("ydotool")],
    note: "paste keystroke injection (both servers)",
};
static NOTIFY_SEND: Requirement = Requirement {
    any_of: &[cmd("notify-send")],
    note: "error/status notifications (libnotify)",
};
static WL_COPY: Requirement = Requirement {
    any_of: &[cmd("wl-copy")],
    note: "clipboard write (wl-clipboard)",
};
static WL_PASTE: Requirement = Requirement {
    any_of: &[cmd("wl-paste")],
    note: "clipboard read (wl-clipboard)",
};
static WTYPE: Requirement = Requirement {
    any_of: &[cmd("wtype")],
    note: "direct typing (injection.mode = type)",
};
static X11_CLIPBOARD: Requirement = Requirement {
    any_of: &[cmd("xclip"), cmd("xsel")],
    note: "clipboard (xclip preferred, or install xsel)",
};
static XDOTOOL: Requirement = Requirement {
    any_of: &[cmd("xdotool")],
    note: "typing + active-window class",
};

/// What ggml's Vulkan `CMake` requires besides the loader:
/// `find_package(Vulkan COMPONENTS glslc)` and `find_package(SPIRV-Headers CONFIG)`,
/// plus cmake itself. Names are Arch packages; notes say what each provides.
static VULKAN_BUILD_DEPS: [Requirement; 4] = [
    Requirement {
        any_of: &[cmd("cmake")],
        note: "cmake",
    },
    Requirement {
        any_of: &[Alternative {
            name: "shaderc",
            need: Need::Command("glslc"),
        }],
        note: "the glslc shader compiler",
    },
    Requirement {
        any_of: &[Alternative {
            name: "vulkan-headers",
            need: Need::Header("vulkan/vulkan.h"),
        }],
        note: "Vulkan headers",
    },
    Requirement {
        any_of: &[Alternative {
            name: "spirv-headers",
            need: Need::Header("spirv/unified1/spirv.hpp"),
        }],
        note: "SPIR-V headers",
    },
];

// --- PATH lookup ----------------------------------------------------------------

/// Is `name` an executable file in one of the `path` (`PATH`-style) dirs? A name
/// containing `/` is checked as a path instead, like the shell's `command -v`.
/// In-process: no `sh -c "command -v"` subprocess to wedge or to inject into.
fn on_path(name: &str, path: &OsStr) -> bool {
    if name.is_empty() {
        return false;
    }
    if name.contains('/') {
        return is_executable(Path::new(name));
    }
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .any(|dir| is_executable(&dir.join(name)))
}

/// Is `path` a regular file (following symlinks) with an execute bit set?
fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;

    use super::*;

    /// A host that has exactly the listed commands, libraries, and headers.
    #[derive(Default)]
    struct Fake {
        commands: HashSet<&'static str>,
        libraries: HashSet<&'static str>,
        headers: HashSet<&'static str>,
    }

    impl Fake {
        fn with_commands(commands: &[&'static str]) -> Self {
            Self {
                commands: commands.iter().copied().collect(),
                ..Self::default()
            }
        }
    }

    impl Probe for Fake {
        fn command(&self, name: &str) -> bool {
            self.commands.contains(name)
        }
        fn library(&self, needle: &str) -> bool {
            self.libraries.contains(needle)
        }
        fn header(&self, header: &str) -> bool {
            self.headers.contains(header)
        }
    }

    /// The names dictation checks on an empty host, i.e. each requirement's
    /// preferred alternative.
    fn needed(server: DisplayServer, type_mode: bool) -> Vec<&'static str> {
        dictation(&Fake::default(), server, type_mode)
            .into_iter()
            .map(|c| c.name)
            .collect()
    }

    #[test]
    fn dictation_requirement_sets_per_server_and_mode() {
        use DisplayServer::{Wayland, X11};
        let always = ["pw-record", "ydotool", "notify-send"];
        let cases: [(DisplayServer, bool, &[&str]); 4] = [
            (Wayland, false, &["wl-copy", "wl-paste"]),
            (Wayland, true, &["wl-copy", "wl-paste", "wtype"]),
            (X11, false, &["xclip", "xdotool"]),
            (X11, true, &["xclip", "xdotool"]),
        ];
        for (server, type_mode, specific) in cases {
            let expected: Vec<&str> = always.iter().chain(specific).copied().collect();
            assert_eq!(
                needed(server, type_mode),
                expected,
                "{server:?}/{type_mode}"
            );
        }
    }

    #[test]
    fn an_empty_host_misses_everything_a_full_one_nothing() {
        for server in [DisplayServer::Wayland, DisplayServer::X11] {
            assert!(
                dictation(&Fake::default(), server, true)
                    .iter()
                    .all(|c| !c.ok)
            );
            let all = Fake::with_commands(&needed(server, true));
            assert!(dictation(&all, server, true).iter().all(|c| c.ok));
        }
    }

    #[test]
    fn xsel_only_x11_host_is_satisfied_by_xsel_everywhere() {
        // Regression: doctor used to report xclip missing on a host the daemon
        // accepts. Doctor, the startup warning, and the clipboard choice now all
        // read this one verdict.
        let host = Fake::with_commands(&["pw-record", "ydotool", "notify-send", "xsel", "xdotool"]);
        let checks = dictation(&host, DisplayServer::X11, false);
        assert!(checks.iter().all(|c| c.ok), "{checks:?}");
        let clipboard = checks
            .iter()
            .find(|c| c.note.starts_with("clipboard"))
            .expect("clipboard requirement");
        assert_eq!(clipboard.name, "xsel");
        assert_eq!(x11_clipboard(&host), Some("xsel"));
    }

    #[test]
    fn x11_clipboard_prefers_xclip_then_xsel_then_none() {
        let cases: [(&[&'static str], Option<&str>); 4] = [
            (&["xclip", "xsel"], Some("xclip")),
            (&["xclip"], Some("xclip")),
            (&["xsel"], Some("xsel")),
            (&[], None),
        ];
        for (commands, expected) in cases {
            assert_eq!(x11_clipboard(&Fake::with_commands(commands)), expected);
        }
        // A missing clipboard is reported by its preferred tool.
        let checks = dictation(&Fake::default(), DisplayServer::X11, false);
        assert!(checks.iter().any(|c| c.name == "xclip" && !c.ok));
    }

    fn missing_packages(host: &Fake) -> Vec<&'static str> {
        vulkan_build(host)
            .deps
            .into_iter()
            .filter(|d| !d.ok)
            .map(|d| d.name)
            .collect()
    }

    #[test]
    fn vulkan_build_deps_name_only_what_is_absent() {
        let mut host = Fake::default();
        assert_eq!(
            missing_packages(&host),
            ["cmake", "shaderc", "vulkan-headers", "spirv-headers"]
        );

        // The case from the AUR report: tools present, both header sets absent.
        host.commands.extend(["cmake", "glslc"]);
        assert_eq!(missing_packages(&host), ["vulkan-headers", "spirv-headers"]);

        host.headers
            .extend(["vulkan/vulkan.h", "spirv/unified1/spirv.hpp"]);
        assert_eq!(missing_packages(&host), Vec::<&str>::new());

        // glslc is probed as a command and reported as its package.
        host.commands.remove("glslc");
        assert_eq!(missing_packages(&host), ["shaderc"]);
    }

    #[test]
    fn vulkan_build_ensure_names_the_loader_then_the_packages() {
        let mut host = Fake::default();
        let err = vulkan_build(&host).ensure("whisper.cpp").unwrap_err();
        assert!(
            err.contains("Vulkan loader (libvulkan) is missing"),
            "{err}"
        );
        assert!(err.contains("Vulkan build of whisper.cpp"), "{err}");

        host.libraries.insert("libvulkan");
        host.commands.extend(["cmake", "glslc"]);
        let err = vulkan_build(&host).ensure("whispy-stt").unwrap_err();
        assert!(err.contains("Vulkan build of whispy-stt needs"), "{err}");
        assert!(err.contains("Vulkan headers, SPIR-V headers"), "{err}");
        assert!(
            err.contains("pacman -S --needed vulkan-headers spirv-headers`"),
            "{err}"
        );
        assert!(err.contains("stt.backend = \"cpu\""), "{err}");

        host.headers
            .extend(["vulkan/vulkan.h", "spirv/unified1/spirv.hpp"]);
        assert_eq!(vulkan_build(&host).ensure("whispy-stt"), Ok(()));
    }

    #[test]
    fn resolve_backend_auto_follows_the_vulkan_loader() {
        let without = Fake::default();
        let mut with = Fake::default();
        with.libraries.insert("libvulkan");
        let cases = [
            ("vulkan", &without, "vulkan"),
            ("cpu", &with, "cpu"),
            ("auto", &with, "vulkan"),
            ("auto", &without, "cpu"),
            // Unknown strings (rejected by config validation) resolve like auto.
            ("rocm", &without, "cpu"),
        ];
        for (configured, host, expected) in cases {
            assert_eq!(resolve_backend(host, configured), expected, "{configured}");
        }
    }

    #[test]
    fn on_path_finds_only_executable_files() {
        let dir = std::env::temp_dir().join(format!("whispy-host-{}", std::process::id()));
        let bin = dir.join("bin");
        fs::create_dir_all(bin.join("subdir")).unwrap();
        let exe = bin.join("tool");
        fs::write(&exe, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(bin.join("plain"), b"data").unwrap();
        fs::set_permissions(bin.join("plain"), fs::Permissions::from_mode(0o644)).unwrap();

        let path = std::env::join_paths([dir.join("nope"), bin]).unwrap();
        assert!(on_path("tool", &path), "executable in a later PATH dir");
        assert!(!on_path("plain", &path), "not executable");
        assert!(!on_path("subdir", &path), "a directory is not a command");
        assert!(!on_path("missing", &path));
        assert!(!on_path("", &path));
        // A name with a slash is a path, not a PATH lookup.
        assert!(on_path(exe.to_str().unwrap(), OsStr::new("")));
        assert!(!on_path("tool", OsStr::new("")), "empty PATH");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn system_probe_reads_the_real_path() {
        assert!(System.command("sh"), "every host has a POSIX shell");
        assert!(!System.command("whispy-no-such-tool"));
    }
}
