//! `whispy-daemon setup`: one-shot bootstrap for a fresh install.
//!
//! Brings the machine from "binaries installed" to "ready to dictate": checks the
//! runtime/build tools, builds whisper.cpp with the configured compute backend
//! (Vulkan, CPU, or auto-detected — see `stt.backend`), downloads the
//! ggml model to the path the config expects, grants ydotool uinput access, seeds
//! the user config, installs+enables the systemd user unit, and (opt-in) drops the
//! Quickshell pill module in place, then prints keybinds in the syntax of the
//! user's compositor config (Omarchy, Hyprland Lua or hyprlang, X11 binders).
//! Each step is idempotent — already-done work is detected and skipped — so it is
//! safe to re-run.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use crate::config::{self, Config};
use crate::display::{self, Backend, DisplayServer};

#[derive(clap::Args, Debug)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent clap --flags, not a state machine"
)]
pub struct SetupArgs {
    /// Run a single step instead of the full bootstrap.
    #[command(subcommand)]
    step: Option<SetupStep>,
    /// Also install the Quickshell pill module to ~/.config/quickshell/Whispy.
    #[arg(long)]
    quickshell: bool,
    /// Skip the ydotool / uinput permission step (needs sudo).
    #[arg(long)]
    no_ydotool: bool,
    /// Don't install or enable the systemd user service.
    #[arg(long)]
    no_systemd: bool,
    /// Install the pill module but don't run it as a standalone service (use when
    /// you embed `PillPanel {}` in your own Quickshell shell instead).
    #[arg(long)]
    no_pill: bool,
}

#[derive(clap::Subcommand, Debug)]
enum SetupStep {
    /// Check that the required runtime and build tools are present.
    Doctor,
    /// Clone and build whisper.cpp with the configured `stt.backend`.
    Whisper,
    /// Download the ggml model to the configured path.
    Model,
    /// Grant uinput access for ydotool (sudo; log out/in afterwards).
    Ydotool,
    /// Install config files and the systemd user service.
    Systemd,
    /// Install the Quickshell pill module.
    Quickshell,
    /// Install the Omarchy 4 bar widget (dictation state in Omarchy's own bar).
    Omarchy,
    /// Check that the installed system is actually ready to dictate.
    Verify,
    /// Print the push-to-talk keybinds for your compositor config.
    Keybinds,
}

/// Entry point dispatched from `main` when the `setup` subcommand is given.
pub fn run(cfg: &Config, args: SetupArgs) -> ExitCode {
    let result = match args.step {
        Some(SetupStep::Doctor) => {
            doctor(cfg);
            Ok(())
        }
        Some(SetupStep::Whisper) => build_whisper(cfg),
        Some(SetupStep::Model) => download_model(cfg),
        Some(SetupStep::Ydotool) => setup_ydotool(),
        Some(SetupStep::Systemd) => install_config(cfg).and_then(|()| install_systemd()),
        Some(SetupStep::Quickshell) => install_quickshell(!args.no_pill),
        Some(SetupStep::Omarchy) => install_omarchy_widget(),
        Some(SetupStep::Verify) => {
            verify(cfg);
            Ok(())
        }
        Some(SetupStep::Keybinds) => {
            print_keybinds(cfg);
            Ok(())
        }
        None => full(cfg, &args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("\nsetup: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The full bootstrap, in dependency order.
fn full(cfg: &Config, args: &SetupArgs) -> Result<(), String> {
    doctor(cfg);
    build_whisper(cfg)?;
    download_model(cfg)?;
    if args.no_ydotool {
        println!("\n==> ydotool: skipped (--no-ydotool)");
    } else {
        setup_ydotool()?;
    }
    install_config(cfg)?;
    if args.no_systemd {
        println!("\n==> systemd: skipped (--no-systemd)");
    } else {
        install_ydotoold()?;
        install_systemd()?;
    }
    // The Quickshell pill needs wlr-layer-shell, which X11 doesn't provide — skip
    // it there and rely on desktop notifications (ui.notify) instead.
    let server = display::resolve_from_env(Backend::parse(&cfg.injection.backend))
        .unwrap_or(DisplayServer::Wayland);
    if server == DisplayServer::X11 {
        println!(
            "\n==> Quickshell pill: skipped on X11 (layer-shell only); \
             desktop notifications are used instead (ui.notify)"
        );
    } else if args.quickshell || have("quickshell") {
        // Install the pill on request, or automatically when Quickshell is present —
        // no point shipping the module to a machine that can't render it.
        install_quickshell(!args.no_pill)?;
    }
    if server == DisplayServer::Wayland && HyprConfig::detect() == HyprConfig::OmarchyLua {
        install_omarchy_widget()?;
    }
    verify(cfg);
    print_next_steps(server);
    Ok(())
}

// --- doctor -----------------------------------------------------------------

fn doctor(cfg: &Config) {
    println!("==> doctor: checking tools");

    // Resolve the display server the same way the daemon does at startup, so the
    // tool list matches what dictation will actually drive.
    let server =
        display::resolve_from_env(Backend::parse(&cfg.injection.backend)).unwrap_or_else(|| {
            println!("  (could not detect display server; assuming Wayland)");
            DisplayServer::Wayland
        });
    println!("  display server: {}", server_label(server));
    if server == DisplayServer::Wayland && have("hyprctl") {
        println!("  hyprland config: {}", HyprConfig::detect().label());
    }

    println!("  runtime:");
    for (cmd, note) in required_tools(server, &cfg.injection.mode) {
        report(cmd, note, have(cmd));
    }

    // The graphical pill is layer-shell only (Hyprland/KDE/Sway). On X11 we fall
    // back to desktop notifications, so flag that here instead of implying a gap.
    match server {
        DisplayServer::Wayland => {
            println!("  pill overlay: Quickshell (layer-shell); install with `setup --quickshell`");
        }
        DisplayServer::X11 => println!(
            "  pill overlay: layer-shell only — not available on X11; \
             desktop notifications are used instead (see ui.notify)"
        ),
    }

    println!("  build (needed for `setup whisper`):");
    for (cmd, note) in [
        ("git", "clone whisper.cpp"),
        ("cmake", "configure the build"),
        ("make", "build driver"),
        ("cc", "C/C++ compiler"),
    ] {
        report(cmd, note, have(cmd));
    }
    println!("  backend (for `setup whisper`; one of these, per stt.backend):");
    report(
        "libvulkan",
        "Vulkan GPU backend (backend = vulkan/auto)",
        has_vulkan(),
    );
    let missing = missing_vulkan_build_deps(have, header_installed);
    // cmake is already listed under "build" above.
    for dep in VULKAN_BUILD_DEPS.iter().filter(|d| d.package != "cmake") {
        report(
            dep.package,
            &format!("{} (Vulkan build)", dep.what),
            !missing.iter().any(|m| m.package == dep.package),
        );
    }
    report(
        "libopenblas",
        "faster CPU inference (optional, backend = cpu)",
        has_lib("libopenblas"),
    );
}

fn report(name: &str, note: &str, ok: bool) {
    let mark = if ok { "✓" } else { "✗" };
    println!("    [{mark}] {name:<14} {note}");
}

/// A human label for the resolved display server.
fn server_label(server: DisplayServer) -> &'static str {
    match server {
        DisplayServer::Wayland => "Wayland",
        DisplayServer::X11 => "X11",
    }
}

/// The runtime tools dictation needs for `server` and injection `mode`, as
/// `(command, note)` pairs. Pure (no PATH probing) so `doctor` and
/// `main::warn_missing_tools` can share one source of truth and stay in sync.
///
/// - Always: `pw-record` (capture), `ydotool` (paste keystroke, both servers),
///   `notify-send` (error/status notifications).
/// - Wayland: `wl-copy`/`wl-paste` for clipboard, plus `wtype` only in `type` mode.
/// - X11: a clipboard tool (`xclip` *or* `xsel`) and `xdotool` (typing + window class).
///
/// On X11 the clipboard requirement is "either xclip or xsel"; we list `xclip`
/// (the preferred one) so a single missing-tool warning points at the default.
pub fn required_tools(server: DisplayServer, mode: &str) -> Vec<(&'static str, &'static str)> {
    let type_mode = mode.eq_ignore_ascii_case("type");
    let mut tools: Vec<(&'static str, &'static str)> = vec![
        ("pw-record", "PipeWire audio capture"),
        ("ydotool", "paste keystroke injection (both servers)"),
        ("notify-send", "error/status notifications (libnotify)"),
    ];
    match server {
        DisplayServer::Wayland => {
            tools.push(("wl-copy", "clipboard write (wl-clipboard)"));
            tools.push(("wl-paste", "clipboard read (wl-clipboard)"));
            if type_mode {
                tools.push(("wtype", "direct typing (injection.mode = type)"));
            }
        }
        DisplayServer::X11 => {
            tools.push(("xclip", "clipboard (xclip preferred, or install xsel)"));
            tools.push(("xdotool", "typing + active-window class"));
        }
    }
    tools
}

// --- whisper.cpp ------------------------------------------------------------

fn build_whisper(cfg: &Config) -> Result<(), String> {
    let backend = resolve_backend(&cfg.stt.backend);
    println!("\n==> whisper.cpp ({backend})");
    let bin = cfg.stt.server_bin_path();
    if bin.exists() {
        println!("  already built: {}", bin.display());
        return Ok(());
    }

    // Fail fast with a clear message: without the Vulkan loader the cmake configure
    // below dies in a wall of output that doesn't name the real problem. (Only the
    // Vulkan backend needs it — the CPU backend builds with no GPU libraries.)
    if backend == "vulkan" && !has_vulkan() {
        return Err(
            "the Vulkan loader (libvulkan) is missing — the Vulkan whisper.cpp build needs it.\n  \
             Install it (Arch: `vulkan-icd-loader` + your GPU's ICD, e.g. `vulkan-radeon`), then re-run.\n  \
             Or set `stt.backend = \"cpu\"` in config.toml to build a CPU-only whisper-server instead."
            .to_string(),
        );
    }
    // The Vulkan build also needs a shader compiler and two header sets; without
    // them cmake fails deep in ggml's configure. Name the packages up front.
    if backend == "vulkan" {
        let missing = missing_vulkan_build_deps(have, header_installed);
        if !missing.is_empty() {
            let packages: Vec<&str> = missing.iter().map(|d| d.package).collect();
            let what: Vec<&str> = missing.iter().map(|d| d.what).collect();
            return Err(format!(
                "the Vulkan whisper.cpp build is missing: {}.\n  \
                 Install them (Arch: `sudo pacman -S --needed {}`), then re-run.\n  \
                 Or set `stt.backend = \"cpu\"` in config.toml to build a CPU-only whisper-server instead.",
                what.join(", "),
                packages.join(" ")
            ));
        }
    }

    // Default layout: <root>/build/bin/whisper-server. Derive <root> and <build>.
    let build_dir = bin
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("cannot derive build dir from {}", bin.display()))?;
    let repo = build_dir
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("cannot derive source dir from {}", build_dir.display()))?;

    if repo.join("CMakeLists.txt").exists() {
        println!("  using existing source at {}", repo.display());
    } else {
        if let Some(parent) = repo.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        println!("  cloning whisper.cpp into {}", repo.display());
        exec(Command::new("git").args([
            "clone",
            "--depth=1",
            "https://github.com/ggml-org/whisper.cpp",
            &repo.to_string_lossy(),
        ]))?;
    }

    // Base configure args, then backend-specific GGML flags.
    let mut configure = vec![
        "-B".to_string(),
        build_dir.to_string_lossy().into_owned(),
        "-S".to_string(),
        repo.to_string_lossy().into_owned(),
        "-DCMAKE_BUILD_TYPE=Release".to_string(),
    ];
    match backend {
        "vulkan" => configure.push("-DGGML_VULKAN=ON".to_string()),
        // CPU backend: OpenBLAS gives ~3-4x faster inference when it's installed.
        _ => {
            if has_lib("libopenblas") {
                println!("  OpenBLAS found — enabling the BLAS backend (faster CPU inference)");
                configure.push("-DGGML_BLAS=ON".to_string());
                configure.push("-DGGML_BLAS_VENDOR=OpenBLAS".to_string());
            } else {
                println!("  CPU-only build (install `openblas` for ~3-4x faster inference)");
            }
        }
    }
    println!("  configuring (cmake)");
    exec(Command::new("cmake").args(&configure))?;

    println!("  building whisper-server (this takes a while)");
    exec(Command::new("cmake").args([
        "--build",
        &build_dir.to_string_lossy(),
        "--config",
        "Release",
        "--target",
        "whisper-server",
        "-j",
    ]))?;

    if !bin.exists() {
        return Err(format!(
            "build finished but {} is missing — check the cmake output above",
            bin.display()
        ));
    }
    println!("  built: {}", bin.display());
    Ok(())
}

// --- model download ---------------------------------------------------------

fn download_model(cfg: &Config) -> Result<(), String> {
    println!("\n==> model: {}", cfg.stt.model);
    let dest = cfg.stt.model_file();
    if dest.exists() {
        println!("  already present: {}", dest.display());
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let url = format!(
        "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{}.bin",
        cfg.stt.model
    );
    println!("  downloading {url}");

    let resp = ureq::get(&url)
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let total: Option<u64> = resp
        .headers()
        .get("Content-Length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok());

    // Download to a temp file, then rename, so an interrupted run never leaves a
    // truncated model that looks valid to the daemon.
    let tmp = dest.with_extension("part");
    let mut out = File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    // Unlike `read_to_*`, the body reader has no size cap — the model is ~0.5 GB.
    let mut reader = resp.into_body().into_reader();
    let mut buf = vec![0u8; 1 << 16];
    let mut done: u64 = 0;
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])
            .map_err(|e| format!("write: {e}"))?;
        done += n as u64;
        print_progress(done, total);
    }
    out.flush().map_err(|e| format!("flush: {e}"))?;
    println!();
    fs::rename(&tmp, &dest).map_err(|e| format!("rename: {e}"))?;
    println!("  saved: {}", dest.display());
    Ok(())
}

fn print_progress(done: u64, total: Option<u64>) {
    let mb = |b: u64| b as f64 / 1_048_576.0;
    match total {
        Some(t) if t > 0 => {
            let pct = done as f64 / t as f64 * 100.0;
            eprint!("\r  {:.0}% ({:.0}/{:.0} MiB)   ", pct, mb(done), mb(t));
        }
        _ => eprint!("\r  {:.0} MiB   ", mb(done)),
    }
}

// --- ydotool ----------------------------------------------------------------

fn setup_ydotool() -> Result<(), String> {
    println!("\n==> ydotool: uinput access (uses sudo)");
    if !have("ydotool") {
        println!("  note: ydotool is not installed yet — install it, then re-run this step.");
    }
    let user = std::env::var("USER").map_err(|e| format!("USER: {e}"))?;
    const RULE_PATH: &str = "/etc/udev/rules.d/80-uinput.rules";
    const RULE: &str =
        r#"KERNEL=="uinput", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput""#;

    exec(Command::new("sudo").args(["groupadd", "-f", "input"]))?;
    exec(Command::new("sudo").args(["usermod", "-aG", "input", &user]))?;
    println!("  installing udev rule at {RULE_PATH}");
    exec(
        Command::new("sh")
            .arg("-c")
            .arg(format!("echo '{RULE}' | sudo tee {RULE_PATH} >/dev/null")),
    )?;
    exec(Command::new("sudo").args(["udevadm", "control", "--reload-rules"]))?;
    exec(Command::new("sudo").args(["udevadm", "trigger"]))?;
    println!("  done. Log out/in (or run `newgrp input`) for the 'input' group to apply.");
    Ok(())
}

/// Install and enable a `ydotoold` systemd user unit so paste-mode injection works
/// without a manual step. `ydotoold` owns the uinput socket the daemon's
/// `ydotool key` calls talk to; without it running, paste silently no-ops.
fn install_ydotoold() -> Result<(), String> {
    println!("\n==> ydotoold user service");
    if !have("ydotool") {
        println!("  note: ydotool is not installed yet — install it, then re-run `setup`.");
        return Ok(());
    }

    install_user_unit("ydotoold", ydotoold_unit_contents())
}

fn ydotoold_unit_contents() -> &'static str {
    "[Unit]\n\
     Description=ydotoold (uinput daemon for whispy paste injection)\n\
     Documentation=https://github.com/Ceereals/whispy\n\
     PartOf=graphical-session.target\n\
     \n\
     [Service]\n\
     Type=simple\n\
     ExecStart=ydotoold\n\
     Restart=on-failure\n\
     RestartSec=2\n\
     \n\
     [Install]\n\
     WantedBy=graphical-session.target\n"
}

// --- config + systemd -------------------------------------------------------

fn install_config(cfg: &Config) -> Result<(), String> {
    println!("\n==> config");
    let dir = config::user_config_dir().ok_or("cannot resolve config dir (HOME unset)")?;
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;

    seed(&dir.join("config.toml"), config::default_config_toml())?;
    // Seed the blacklist where the default config points, so user edits take effect.
    seed(
        &cfg.filter.hallucinations_file(),
        config::default_hallucinations_toml(),
    )?;
    Ok(())
}

/// Write `contents` to `path` only if it does not already exist (never clobber edits).
fn seed(path: &Path, contents: &str) -> Result<(), String> {
    if path.exists() {
        println!("  kept existing {}", path.display());
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    fs::write(path, contents).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("  wrote {}", path.display());
    Ok(())
}

fn install_systemd() -> Result<(), String> {
    println!("\n==> systemd user service");
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    install_user_unit("whispy-daemon", &unit_contents(&exe))
}

/// Install `<name>.service` as a user unit, unless systemd already knows one (e.g.
/// a packaged /usr unit, which is kept), then enable and start it. Enabling is
/// best-effort: a headless/SSH session may have no user manager, so a failure
/// prints the command to run from the graphical session instead.
fn install_user_unit(name: &str, contents: &str) -> Result<(), String> {
    let service = format!("{name}.service");
    let known = Command::new("systemctl")
        .args(["--user", "cat", &service])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());

    if known {
        println!("  using the {name} unit already known to systemd");
    } else {
        let dir = config::expand("~/.config/systemd/user");
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let unit = dir.join(&service);
        fs::write(&unit, contents).map_err(|e| format!("{}: {e}", unit.display()))?;
        println!("  wrote {}", unit.display());
        exec(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
    }

    if exec(Command::new("systemctl").args(["--user", "enable", "--now", &service])).is_err() {
        println!("  could not enable {name} now — start it from your session with:");
        println!("    systemctl --user enable --now {name}");
    } else {
        println!("  enabled and started {name}");
    }
    Ok(())
}

fn unit_contents(exe: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=whispy dictation daemon\n\
         Documentation=https://github.com/Ceereals/whispy\n\
         After=graphical-session.target pipewire.service ydotoold.service\n\
         Wants=ydotoold.service\n\
         PartOf=graphical-session.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={}\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=graphical-session.target\n",
        exe.display()
    )
}

// --- quickshell -------------------------------------------------------------

fn install_quickshell(run_service: bool) -> Result<(), String> {
    println!("\n==> Quickshell pill module");
    let src =
        quickshell_source().ok_or("could not find the Quickshell module source (ui/quickshell)")?;
    let dest = config::expand("~/.config/quickshell/Whispy");
    fs::create_dir_all(&dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    for name in ["Tokens.qml", "Pill.qml", "PillPanel.qml", "qmldir"] {
        let from = src.join(name);
        let to = dest.join(name);
        fs::copy(&from, &to).map_err(|e| format!("{} -> {}: {e}", from.display(), to.display()))?;
    }
    println!("  installed module to {}", dest.display());

    // Seed the standalone shell config (never clobber user edits) so the pill can
    // run as its own Quickshell instance with no change to the user's main shell.qml.
    let shell = config::expand("~/.config/quickshell/whispy/shell.qml");
    seed(
        &shell,
        &fs::read_to_string(src.join("shell.qml"))
            .map_err(|e| format!("{}: {e}", src.join("shell.qml").display()))?,
    )?;

    if run_service {
        install_pill_service()?;
    } else {
        println!("  --no-pill: skipped the standalone pill service");
        println!("  embed it yourself: `import Whispy` + `PillPanel {{}}` in your shell.qml");
    }
    Ok(())
}

/// Install and enable the standalone pill service (`quickshell -c whispy`), so the
/// overlay runs without the user editing their own Quickshell shell.
fn install_pill_service() -> Result<(), String> {
    install_user_unit("whispy-pill", pill_unit_contents())
}

fn pill_unit_contents() -> &'static str {
    // QML2_IMPORT_PATH points the QML engine at ~/.config/quickshell so `import Whispy`
    // resolves the module dir — Quickshell does not add the config root to the import
    // path, so without this `quickshell -c whispy` dies with "module not installed".
    "[Unit]\n\
     Description=whispy dictation pill (standalone Quickshell overlay)\n\
     Documentation=https://github.com/Ceereals/whispy\n\
     After=graphical-session.target\n\
     PartOf=graphical-session.target\n\
     \n\
     [Service]\n\
     Type=simple\n\
     Environment=QML2_IMPORT_PATH=%h/.config/quickshell\n\
     ExecStart=quickshell -c whispy\n\
     Restart=on-failure\n\
     RestartSec=2\n\
     \n\
     [Install]\n\
     WantedBy=graphical-session.target\n"
}

/// Locate the bundled Quickshell QML sources across install layouts.
fn quickshell_source() -> Option<PathBuf> {
    let mut candidates = vec![
        PathBuf::from("/usr/share/whispy/quickshell"),
        PathBuf::from("/usr/local/share/whispy/quickshell"),
        PathBuf::from("ui/quickshell"),
    ];
    // ../share/whispy/quickshell relative to the binary (e.g. /usr/bin -> /usr/share).
    if let Ok(exe) = std::env::current_exe()
        && let Some(prefix) = exe.parent().and_then(Path::parent)
    {
        candidates.push(prefix.join("share/whispy/quickshell"));
    }
    candidates.into_iter().find(|p| p.join("qmldir").exists())
}

// --- omarchy bar widget ------------------------------------------------------

/// Plugin id of the Omarchy bar widget (third-party ids can't use `omarchy.*`).
const OMARCHY_WIDGET_ID: &str = "whispy.dictation";

/// The widget's plugin files (`ui/omarchy/`), embedded so every install method
/// (AUR, install.sh, cargo) can drop them in place without packaging changes.
const OMARCHY_WIDGET_FILES: [(&str, &str); 2] = [
    (
        "manifest.json",
        include_str!("../../../ui/omarchy/manifest.json"),
    ),
    ("whispy.qml", include_str!("../../../ui/omarchy/whispy.qml")),
];

/// Install whispy as an Omarchy 4 bar widget plugin and, the first time, put it on
/// the bar.
///
/// The files are rewritten on every run so upgrades reach them (Omarchy reloads
/// plugin code on save). Only a first install places the widget, through
/// `omarchy-bar put`, Omarchy's call for unattended installers: it enables the
/// plugin and never moves a widget that is already on the bar. Later runs leave
/// the layout alone, so a widget the user disabled stays disabled.
fn install_omarchy_widget() -> Result<(), String> {
    println!("\n==> Omarchy bar widget");
    let dir = omarchy_widget_dir();
    let first_install = !dir.exists();
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for (name, contents) in OMARCHY_WIDGET_FILES {
        let path = dir.join(name);
        fs::write(&path, contents).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    println!("  installed {}", dir.display());

    let id = OMARCHY_WIDGET_ID;
    if !first_install {
        println!("  updated in place; its spot on the bar (if any) is unchanged");
        return Ok(());
    }
    if !have("omarchy-shell") || !have("omarchy-bar") {
        println!("  add it to the bar from your Omarchy session: omarchy bar put {id}");
        return Ok(());
    }
    // Rescan first so the shell knows the new plugin before `put` asks for it.
    // Best-effort: the shell may not be running (e.g. setup over SSH).
    let _ = Command::new("omarchy-shell")
        .args(["shell", "rescanPlugins"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    // Where Omarchy's own (Voxtype) dictation indicator sits: right after
    // omarchy.indicators in the center. Fall back to the manifest's default
    // section when that widget isn't on the bar.
    let placed =
        exec(Command::new("omarchy-bar").args(["put", id, "--after", "omarchy.indicators"]))
            .or_else(|_| exec(Command::new("omarchy-bar").args(["put", id])));
    if placed.is_ok() {
        println!(
            "  move it with `omarchy bar move {id} --section left|center|right`;\n  \
             remove it with `omarchy plugin disable {id}`"
        );
    } else {
        println!("  could not add it now — from your Omarchy session run: omarchy bar put {id}");
    }
    hide_omarchy_dictation_indicator();
    if have("voxtype") {
        println!(
            "  Voxtype is still installed and keeps its own model loaded; stop it with\n    \
             systemctl --user disable --now voxtype   (or remove it: omarchy-voxtype-remove)"
        );
    }
    Ok(())
}

/// Omarchy's `omarchy.indicators` bar widget ships a "Dictation" indicator wired
/// to Voxtype (its status, and a click opens Voxtype's config). Next to whispy's
/// widget it shows a second, wrong dictation state, so on first install drop it
/// from the widget's `items` in `shell.json`. Best-effort: any failure only warns.
///
/// This edits `shell.json` directly because `omarchy bar set … --json` mangles
/// array values (the shell IPC splits them at commas). Omarchy hot-reloads it.
fn hide_omarchy_dictation_indicator() {
    let path = config::expand("~/.config/omarchy/shell.json");
    let Ok(text) = fs::read_to_string(&path) else {
        return;
    };
    let Ok(mut shell) = serde_json::from_str::<serde_json::Value>(&text) else {
        println!(
            "  could not parse {}; leaving Omarchy's indicators alone",
            path.display()
        );
        return;
    };
    if !without_dictation_indicator(&mut shell, &omarchy_indicator_ids()) {
        return;
    }
    let Ok(out) = serde_json::to_string_pretty(&shell) else {
        return;
    };
    let tmp = path.with_extension("json.whispy-tmp");
    match fs::write(&tmp, format!("{out}\n")).and_then(|()| fs::rename(&tmp, &path)) {
        Ok(()) => println!(
            "  hid Omarchy's Voxtype \"Dictation\" indicator (re-enable it in the indicators widget settings)"
        ),
        Err(e) => println!("  could not update {}: {e}", path.display()),
    }
}

/// Indicator ids `omarchy.indicators` shows when its `items` setting is empty,
/// read from Omarchy's widget manifest (falls back to the Omarchy 4.0 list).
fn omarchy_indicator_ids() -> Vec<String> {
    let root = std::env::var_os("OMARCHY_PATH")
        .map_or_else(|| PathBuf::from("/usr/share/omarchy"), PathBuf::from);
    let manifest = root.join("shell/plugins/bar/widgets/Indicators.manifest.json");
    let from_manifest = fs::read_to_string(manifest)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .map(|m| indicator_ids_from_manifest(&m))
        .unwrap_or_default();
    if from_manifest.is_empty() {
        [
            "Dictation",
            "ScreenRecording",
            "Reminder",
            "NightLight",
            "Dnd",
            "StayAwake",
        ]
        .map(String::from)
        .to_vec()
    } else {
        from_manifest
    }
}

/// The `items` option values from the indicators widget manifest's schema.
fn indicator_ids_from_manifest(manifest: &serde_json::Value) -> Vec<String> {
    manifest["barWidget"]["schema"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|field| field["key"] == "items")
        .filter_map(|field| field["options"].as_array())
        .flatten()
        .filter_map(|opt| opt["value"].as_str().map(String::from))
        .collect()
}

/// Remove "Dictation" from every `omarchy.indicators` widget in `shell`. An empty
/// or missing `items` means "all indicators", so it becomes `all` minus Dictation.
/// Returns whether anything changed.
fn without_dictation_indicator(shell: &mut serde_json::Value, all: &[String]) -> bool {
    use serde_json::Value;
    match shell {
        Value::Object(map) => {
            let mut changed = false;
            if map.get("id").and_then(Value::as_str) == Some("omarchy.indicators") {
                let current: Vec<String> = map
                    .get("items")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .filter(|items: &Vec<String>| !items.is_empty())
                    .unwrap_or_else(|| all.to_vec());
                if current.iter().any(|i| i == "Dictation") {
                    let kept: Vec<Value> = current
                        .into_iter()
                        .filter(|i| i != "Dictation")
                        .map(Value::String)
                        .collect();
                    map.insert("items".into(), Value::Array(kept));
                    changed = true;
                }
            }
            for v in map.values_mut() {
                changed |= without_dictation_indicator(v, all);
            }
            changed
        }
        Value::Array(items) => items
            .iter_mut()
            .fold(false, |acc, v| without_dictation_indicator(v, all) | acc),
        _ => false,
    }
}

/// Where Omarchy discovers third-party plugins. Omarchy hardcodes
/// `$HOME/.config/omarchy/plugins` (it ignores `$XDG_CONFIG_HOME`), so this does too.
fn omarchy_widget_dir() -> PathBuf {
    config::expand("~/.config/omarchy/plugins").join(OMARCHY_WIDGET_ID)
}

/// `$XDG_CONFIG_HOME`, defaulting to `~/.config`.
fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| config::expand("~/.config"))
}

// --- verify -----------------------------------------------------------------

/// Check the *running* system, not just tools on PATH: are the model and binary in
/// place, is the daemon answering, is ydotoold up, is whisper-server listening?
/// Prints a ✓/✗ table so the user knows whether setup actually worked.
fn verify(cfg: &Config) {
    println!("\n==> verify");

    report(
        "model",
        &cfg.stt.model_file().display().to_string(),
        cfg.stt.model_file().exists(),
    );
    report(
        "whisper-server",
        &cfg.stt.server_bin_path().display().to_string(),
        cfg.stt.server_bin_path().exists(),
    );

    let daemon_ok = daemon_responds(cfg);
    report("daemon", "responds to ping on the IPC socket", daemon_ok);

    // ydotoold only matters for paste mode; note it but don't imply failure in type mode.
    let paste_mode = cfg.injection.mode == "paste";
    if paste_mode {
        report(
            "ydotoold",
            "active (needed for paste-mode injection)",
            ydotoold_active(),
        );
    } else {
        println!(
            "    [-] {:<12} not needed (injection.mode = \"type\")",
            "ydotoold"
        );
    }

    report(
        "whisper http",
        &format!("listening on {}:{}", cfg.stt.host, cfg.stt.port),
        port_open(&cfg.stt.host, cfg.stt.port),
    );
}

/// Connect to the IPC socket and confirm the daemon answers a `ping`.
fn daemon_responds(cfg: &Config) -> bool {
    let path = cfg.ipc.socket_path();
    let Ok(mut stream) = UnixStream::connect(&path) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    if stream.write_all(b"{\"cmd\":\"ping\"}\n").is_err() {
        return false;
    }
    let mut line = String::new();
    if BufReader::new(&stream).read_line(&mut line).is_err() {
        return false;
    }
    serde_json::from_str::<whispy_common::Resp>(line.trim()).is_ok_and(|r| r.ok)
}

/// Is the `ydotoold` user service active?
fn ydotoold_active() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "ydotoold.service"])
        .status()
        .is_ok_and(|s| s.success())
}

/// Can we open a TCP connection to `host:port` (i.e. whisper-server is listening)?
fn port_open(host: &str, port: u16) -> bool {
    use std::net::ToSocketAddrs;
    let Ok(mut addrs) = (host, port).to_socket_addrs() else {
        return false;
    };
    addrs.any(|addr| TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok())
}

// --- helpers ----------------------------------------------------------------

/// Run a command, streaming its output to the terminal; error on non-zero exit.
fn exec(cmd: &mut Command) -> Result<(), String> {
    let name = cmd.get_program().display().to_string();
    let status = cmd
        .status()
        .map_err(|e| format!("failed to run {name}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{name} exited with {status}"))
    }
}

/// Is `cmd` on PATH?
pub(crate) fn have(cmd: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {cmd}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Resolve the configured `stt.backend` to a concrete backend for the build:
/// `"auto"` picks Vulkan when its loader is present, else CPU; the explicit values
/// pass through unchanged. Unknown strings are validated away at config load, but
/// fall back to CPU here so `setup whisper` can never panic on one.
fn resolve_backend(configured: &str) -> &'static str {
    match configured {
        "vulkan" => "vulkan",
        "cpu" => "cpu",
        _ => {
            if has_vulkan() {
                "vulkan"
            } else {
                "cpu"
            }
        }
    }
}

/// Is the Vulkan loader present (so the Vulkan whisper.cpp build can run)?
fn has_vulkan() -> bool {
    has_lib("libvulkan")
}

/// A build-time dependency of whisper.cpp's Vulkan backend, named by its Arch
/// package (the AUR package depends on all of them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BuildDep {
    package: &'static str,
    what: &'static str,
    probe: Probe,
}

/// How to tell whether a [`BuildDep`] is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    /// A command on PATH.
    Command(&'static str),
    /// A header under a system include dir.
    Header(&'static str),
}

/// What `ggml`'s Vulkan `CMake` requires: `find_package(Vulkan COMPONENTS glslc)`
/// and `find_package(SPIRV-Headers CONFIG)`, plus cmake itself.
const VULKAN_BUILD_DEPS: [BuildDep; 4] = [
    BuildDep {
        package: "cmake",
        what: "cmake",
        probe: Probe::Command("cmake"),
    },
    BuildDep {
        package: "shaderc",
        what: "the glslc shader compiler",
        probe: Probe::Command("glslc"),
    },
    BuildDep {
        package: "vulkan-headers",
        what: "Vulkan headers",
        probe: Probe::Header("vulkan/vulkan.h"),
    },
    BuildDep {
        package: "spirv-headers",
        what: "SPIR-V headers",
        probe: Probe::Header("spirv/unified1/spirv.hpp"),
    },
];

/// The Vulkan build deps that are missing, given probes for commands and headers.
/// Pure (probes injected) so the decision is unit-testable.
fn missing_vulkan_build_deps(
    has_command: impl Fn(&str) -> bool,
    has_header: impl Fn(&str) -> bool,
) -> Vec<BuildDep> {
    VULKAN_BUILD_DEPS
        .into_iter()
        .filter(|dep| match dep.probe {
            Probe::Command(cmd) => !has_command(cmd),
            Probe::Header(header) => !has_header(header),
        })
        .collect()
}

/// Is `header` installed under a system include dir?
fn header_installed(header: &str) -> bool {
    ["/usr/include", "/usr/local/include"]
        .iter()
        .any(|dir| Path::new(dir).join(header).is_file())
}

/// Is a shared library matching `needle` registered with the dynamic linker?
/// Used to decide which whisper.cpp backend (Vulkan, `OpenBLAS`) the host can build.
fn has_lib(needle: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("ldconfig -p | grep -q {needle}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn print_next_steps(server: DisplayServer) {
    println!("\n==> done. Add push-to-talk keybinds:");
    print_server_keybinds(server);
    println!("\nThen check it: whispy-client ping");
}

/// `setup keybinds`: just the keybind hints, for the display server in use.
fn print_keybinds(cfg: &Config) {
    let server = display::resolve_from_env(Backend::parse(&cfg.injection.backend))
        .unwrap_or(DisplayServer::Wayland);
    println!("==> push-to-talk keybinds");
    print_server_keybinds(server);
}

fn print_server_keybinds(server: DisplayServer) {
    match server {
        DisplayServer::Wayland => print_wayland_keybinds(),
        DisplayServer::X11 => print_x11_keybinds(),
    }
}

/// Hyprland (and other layer-shell compositors) keybind hints, in the syntax of
/// the config the user actually runs.
fn print_wayland_keybinds() {
    let hypr = HyprConfig::detect();
    println!("\n  {}:\n\n{}", hypr.label(), wayland_keybinds(hypr));
    println!(
        "\n  Other compositors (Sway, etc.) can bind the same commands, or use just\n  \
         a single toggle: `whispy-client toggle`."
    );
}

/// Which Hyprland config the user runs, so `setup` prints binds they can paste
/// as-is. Hyprland now reads a Lua config (`hyprland.lua`) in place of hyprlang
/// (`hyprland.conf`), and Omarchy layers its own `o.bind` helper and a dedicated
/// user bindings file on top of each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HyprConfig {
    /// Omarchy 4+: Lua config; user binds go in `~/.config/hypr/bindings.lua`.
    OmarchyLua,
    /// Omarchy 3.x: hyprlang config; user binds go in `~/.config/hypr/bindings.conf`.
    OmarchyConf,
    /// Plain Hyprland with a Lua config (`~/.config/hypr/hyprland.lua`).
    Lua,
    /// Plain Hyprland with the classic hyprlang config (`hyprland.conf`).
    Conf,
}

impl HyprConfig {
    /// Pick the flavor from the two probed facts (pure, for tests).
    fn pick(omarchy: bool, lua: bool) -> Self {
        match (omarchy, lua) {
            (true, true) => Self::OmarchyLua,
            (true, false) => Self::OmarchyConf,
            (false, true) => Self::Lua,
            (false, false) => Self::Conf,
        }
    }

    /// Probe the machine: Omarchy is detected from `OMARCHY_PATH` or its install
    /// dir (`/usr/share/omarchy` on 4.x, `~/.local/share/omarchy` on 3.x); a Lua
    /// config from `$XDG_CONFIG_HOME/hypr/hyprland.lua`.
    fn detect() -> Self {
        let omarchy = std::env::var_os("OMARCHY_PATH").is_some_and(|p| !p.is_empty())
            || Path::new("/usr/share/omarchy").is_dir()
            || config::expand("~/.local/share/omarchy").is_dir();
        let lua = config_home().join("hypr/hyprland.lua").is_file();
        Self::pick(omarchy, lua)
    }

    /// Where the binds go, as a heading for the printed snippet.
    fn label(self) -> &'static str {
        match self {
            Self::OmarchyLua => "Omarchy 4 (~/.config/hypr/bindings.lua)",
            Self::OmarchyConf => "Omarchy 3 (~/.config/hypr/bindings.conf)",
            Self::Lua => "Hyprland Lua config (~/.config/hypr/hyprland.lua)",
            Self::Conf => "Hyprland (~/.config/hypr/hyprland.conf)",
        }
    }
}

/// The keybind snippet for `hypr`: SUPER+CTRL+X toggles, F9 is push-to-talk
/// (hold to record, release to transcribe), SUPER+CTRL+SHIFT+X cancels.
///
/// These are Omarchy's own dictation keys, which it binds only while its bundled
/// dictation backend (Voxtype) is installed — so they're free on a stock Omarchy
/// and the leading unbinds make whispy win if Voxtype is present. They also stay
/// clear of SUPER+SPACE and its ALT/SHIFT/CTRL variants, which Omarchy uses for
/// its menus and bar.
fn wayland_keybinds(hypr: HyprConfig) -> &'static str {
    match hypr {
        HyprConfig::OmarchyLua => {
            "hl.unbind(\"SUPER + CTRL + X\")  -- Omarchy's Voxtype dictation, if installed\n\
             hl.unbind(\"F9\")\n\
             o.bind(\"SUPER + CTRL + X\", \"Toggle dictation\", \"whispy-client toggle\")\n\
             o.bind(\"F9\", \"Start dictation (push-to-talk)\", \"whispy-client start\")\n\
             o.bind(\"F9\", \"Stop dictation (push-to-talk)\", \"whispy-client stop\", { release = true })\n\
             o.bind(\"SUPER + CTRL + SHIFT + X\", \"Cancel dictation\", \"whispy-client cancel\")"
        }
        HyprConfig::OmarchyConf => {
            "unbind = SUPER CTRL, X\n\
             unbind = , F9\n\
             bindd  = SUPER CTRL, X, Toggle dictation, exec, whispy-client toggle\n\
             bindd  = , F9, Start dictation, exec, whispy-client start\n\
             bindr  = , F9, exec, whispy-client stop\n\
             bindd  = SUPER CTRL SHIFT, X, Cancel dictation, exec, whispy-client cancel"
        }
        HyprConfig::Lua => {
            "hl.bind(\"SUPER + CTRL + X\", hl.dsp.exec_cmd(\"whispy-client toggle\"), { description = \"Toggle dictation\" })\n\
             hl.bind(\"F9\", hl.dsp.exec_cmd(\"whispy-client start\"), { description = \"Start dictation\" })\n\
             hl.bind(\"F9\", hl.dsp.exec_cmd(\"whispy-client stop\"), { description = \"Stop dictation\", release = true })\n\
             hl.bind(\"SUPER + CTRL + SHIFT + X\", hl.dsp.exec_cmd(\"whispy-client cancel\"), { description = \"Cancel dictation\" })"
        }
        HyprConfig::Conf => {
            "bindd = SUPER CTRL, X, Toggle dictation, exec, whispy-client toggle\n\
             bindd = , F9, Start dictation, exec, whispy-client start\n\
             bindr = , F9, exec, whispy-client stop\n\
             bindd = SUPER CTRL SHIFT, X, Cancel dictation, exec, whispy-client cancel"
        }
    }
}

/// X11 (sxhkd / xbindkeys) and GNOME custom-shortcut keybind hints.
fn print_x11_keybinds() {
    println!(
        "\n  sxhkd (~/.config/sxhkd/sxhkdrc), toggle on a single key:\n\
         \n\
         super + space\n\
             whispy-client toggle\n\
         \n\
         xbindkeys (~/.xbindkeysrc):\n\
         \n\
         \"whispy-client toggle\"\n\
             super + space\n\
         \n\
         GNOME (Settings > Keyboard > Custom Shortcuts): add a shortcut with\n\
         command `whispy-client toggle` bound to Super+Space.\n\
         \n\
         (Press-and-hold start/stop needs a binder that fires on key release, e.g.\n\
         Hyprland's `bindr`; sxhkd/xbindkeys/GNOME fire on press, so use `toggle`.)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whispy_unit_depends_on_ydotoold() {
        let unit = unit_contents(Path::new("/usr/bin/whispy-daemon"));
        assert!(unit.contains("Wants=ydotoold.service"), "{unit}");
        assert!(
            unit.contains("After=graphical-session.target pipewire.service ydotoold.service"),
            "{unit}"
        );
        assert!(unit.contains("ExecStart=/usr/bin/whispy-daemon"), "{unit}");
    }

    #[test]
    fn ydotoold_unit_is_a_valid_user_service() {
        let unit = ydotoold_unit_contents();
        assert!(unit.contains("ExecStart=ydotoold"), "{unit}");
        assert!(unit.contains("[Install]"), "{unit}");
        assert!(unit.contains("WantedBy=graphical-session.target"), "{unit}");
    }

    /// The command names from `required_tools`, for set-membership assertions.
    fn tool_names(server: DisplayServer, mode: &str) -> Vec<&'static str> {
        required_tools(server, mode)
            .into_iter()
            .map(|(cmd, _)| cmd)
            .collect()
    }

    #[test]
    fn required_tools_always_includes_capture_paste_notify() {
        for server in [DisplayServer::Wayland, DisplayServer::X11] {
            for mode in ["paste", "type"] {
                let names = tool_names(server, mode);
                for always in ["pw-record", "ydotool", "notify-send"] {
                    assert!(
                        names.contains(&always),
                        "{always} missing for {server:?}/{mode}"
                    );
                }
            }
        }
    }

    #[test]
    fn required_tools_wayland_lists_wl_clipboard_and_wtype_only_in_type_mode() {
        let paste = tool_names(DisplayServer::Wayland, "paste");
        assert!(paste.contains(&"wl-copy"));
        assert!(paste.contains(&"wl-paste"));
        assert!(!paste.contains(&"wtype"), "paste mode needs no wtype");

        let typed = tool_names(DisplayServer::Wayland, "type");
        assert!(typed.contains(&"wtype"), "type mode needs wtype");
        // No X11 tools leak into the Wayland list.
        assert!(!typed.contains(&"xdotool"));
        assert!(!typed.contains(&"xclip"));
    }

    #[test]
    fn required_tools_x11_lists_clipboard_and_xdotool_in_both_modes() {
        for mode in ["paste", "type"] {
            let names = tool_names(DisplayServer::X11, mode);
            assert!(names.contains(&"xclip"), "x11 clipboard for {mode}");
            assert!(names.contains(&"xdotool"), "x11 typing/class for {mode}");
            // No Wayland tools leak into the X11 list.
            assert!(!names.contains(&"wl-copy"));
            assert!(!names.contains(&"wtype"));
        }
    }

    #[test]
    fn omarchy_widget_manifest_is_consistent() {
        // The embedded manifest must name our id and point at the embedded QML,
        // or Omarchy's registry silently drops the plugin.
        let manifest: serde_json::Value =
            serde_json::from_str(OMARCHY_WIDGET_FILES[0].1).expect("manifest.json parses");
        assert_eq!(manifest["schemaVersion"], 1);
        assert_eq!(manifest["id"], OMARCHY_WIDGET_ID);
        assert!(
            !OMARCHY_WIDGET_ID.starts_with("omarchy."),
            "reserved namespace"
        );
        assert_eq!(manifest["kinds"][0], "bar-widget");
        let entry = manifest["entryPoints"]["barWidget"]
            .as_str()
            .expect("entry point");
        let qml = OMARCHY_WIDGET_FILES
            .iter()
            .find(|(name, _)| *name == entry)
            .expect("entry point file is embedded");
        assert!(qml.1.contains("function triggerPress(button)"));
        assert!(qml.1.contains("whispy-client toggle"));
        assert_eq!(
            omarchy_widget_dir().file_name().and_then(|n| n.to_str()),
            Some(OMARCHY_WIDGET_ID)
        );
    }

    #[test]
    fn missing_vulkan_build_deps_names_only_what_is_absent() {
        let all = missing_vulkan_build_deps(|_| false, |_| false);
        let packages: Vec<&str> = all.iter().map(|d| d.package).collect();
        assert_eq!(
            packages,
            ["cmake", "shaderc", "vulkan-headers", "spirv-headers"]
        );

        assert_eq!(missing_vulkan_build_deps(|_| true, |_| true), []);

        // The case from the AUR report: tools present, both header sets absent.
        let headers_only = missing_vulkan_build_deps(|_| true, |_| false);
        let packages: Vec<&str> = headers_only.iter().map(|d| d.package).collect();
        assert_eq!(packages, ["vulkan-headers", "spirv-headers"]);

        // glslc is probed as a command, not a library.
        let no_glslc = missing_vulkan_build_deps(|cmd| cmd != "glslc", |_| true);
        assert_eq!(no_glslc.len(), 1);
        assert_eq!(no_glslc[0].package, "shaderc");
    }

    #[test]
    fn hypr_config_pick_covers_every_combination() {
        assert_eq!(HyprConfig::pick(true, true), HyprConfig::OmarchyLua);
        assert_eq!(HyprConfig::pick(true, false), HyprConfig::OmarchyConf);
        assert_eq!(HyprConfig::pick(false, true), HyprConfig::Lua);
        assert_eq!(HyprConfig::pick(false, false), HyprConfig::Conf);
    }

    #[test]
    fn keybinds_use_the_syntax_of_each_config() {
        let omarchy = wayland_keybinds(HyprConfig::OmarchyLua);
        assert!(omarchy.contains("o.bind(\"SUPER + CTRL + X\""), "{omarchy}");
        assert!(omarchy.contains("{ release = true }"), "{omarchy}");

        let lua = wayland_keybinds(HyprConfig::Lua);
        // Plain Hyprland has no Omarchy `o` helper table.
        assert!(!lua.contains("o.bind"), "{lua}");
        assert!(
            lua.contains("hl.dsp.exec_cmd(\"whispy-client stop\")"),
            "{lua}"
        );
        assert!(lua.contains("release = true"), "{lua}");

        for conf in [HyprConfig::OmarchyConf, HyprConfig::Conf] {
            let binds = wayland_keybinds(conf);
            let stop = binds
                .lines()
                .find(|l| l.contains("whispy-client stop"))
                .expect("stop bind present");
            assert!(stop.starts_with("bindr"), "{binds}");
            assert!(!binds.contains("hl."), "{binds}");
        }
    }

    fn all_indicators() -> Vec<String> {
        ["Dictation", "ScreenRecording", "Dnd"]
            .map(String::from)
            .to_vec()
    }

    #[test]
    fn empty_indicator_items_become_all_but_dictation() {
        let mut shell = serde_json::json!({ "bar": { "layout": { "center": [
            { "id": "omarchy.indicators" },
            { "id": "omarchy.clock" }
        ] } } });
        assert!(without_dictation_indicator(&mut shell, &all_indicators()));
        assert_eq!(
            shell["bar"]["layout"]["center"][0]["items"],
            serde_json::json!(["ScreenRecording", "Dnd"])
        );
        assert!(shell["bar"]["layout"]["center"][1].get("items").is_none());
    }

    #[test]
    fn explicit_indicator_items_keep_the_users_choice_minus_dictation() {
        let mut shell = serde_json::json!({ "layout": { "right": [
            { "id": "omarchy.indicators", "items": ["Dnd", "Dictation"] }
        ] } });
        assert!(without_dictation_indicator(&mut shell, &all_indicators()));
        assert_eq!(
            shell["layout"]["right"][0]["items"],
            serde_json::json!(["Dnd"])
        );
    }

    #[test]
    fn no_change_without_a_dictation_indicator() {
        let mut shell = serde_json::json!({ "layout": { "center": [
            { "id": "omarchy.indicators", "items": ["Dnd"] }
        ] } });
        assert!(!without_dictation_indicator(&mut shell, &all_indicators()));
        let mut no_widget =
            serde_json::json!({ "layout": { "center": [{ "id": "omarchy.clock" }] } });
        assert!(!without_dictation_indicator(
            &mut no_widget,
            &all_indicators()
        ));
    }

    #[test]
    fn indicator_ids_come_from_the_manifest_items_options() {
        let manifest = serde_json::json!({ "barWidget": { "schema": [
            { "key": "items", "options": [{ "value": "Dictation" }, { "value": "Dnd" }] },
            { "key": "alwaysShow", "type": "boolean" }
        ] } });
        assert_eq!(indicator_ids_from_manifest(&manifest), ["Dictation", "Dnd"]);
        assert_eq!(
            indicator_ids_from_manifest(&serde_json::json!({})),
            Vec::<String>::new()
        );
    }

    #[test]
    fn omarchy_keybinds_override_voxtype_before_binding() {
        // Omarchy binds SUPER+CTRL+X and F9 to Voxtype when it's installed; a second
        // bind on the same key would fire both, so the unbinds must come first.
        for hypr in [HyprConfig::OmarchyLua, HyprConfig::OmarchyConf] {
            let binds = wayland_keybinds(hypr);
            let unbind = binds.find("unbind").expect("unbind present");
            let bind = binds.find("whispy-client").expect("bind present");
            assert!(unbind < bind, "{binds}");
        }
    }

    #[test]
    fn keybinds_avoid_omarchy_super_space_menus() {
        // SUPER+SPACE (+ALT/SHIFT/CTRL) open Omarchy's menus and bar toggles.
        for hypr in [
            HyprConfig::OmarchyLua,
            HyprConfig::OmarchyConf,
            HyprConfig::Lua,
            HyprConfig::Conf,
        ] {
            let binds = wayland_keybinds(hypr).to_ascii_uppercase();
            assert!(!binds.contains("SPACE"), "{binds}");
        }
    }

    #[test]
    fn resolve_backend_passes_through_explicit_values() {
        assert_eq!(resolve_backend("vulkan"), "vulkan");
        assert_eq!(resolve_backend("cpu"), "cpu");
    }

    #[test]
    fn resolve_backend_auto_picks_a_concrete_backend() {
        // "auto" depends on host Vulkan presence, but must always resolve to one
        // of the two concrete backends so the build can never proceed on "auto".
        assert!(matches!(resolve_backend("auto"), "vulkan" | "cpu"));
        // An unknown string (shouldn't reach here past config validation) is safe.
        assert!(matches!(resolve_backend("rocm"), "vulkan" | "cpu"));
    }
}
