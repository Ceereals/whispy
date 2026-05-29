//! `whispy-daemon setup`: one-shot bootstrap for a fresh install.
//!
//! Brings the machine from "binaries installed" to "ready to dictate": checks the
//! runtime/build tools, builds whisper.cpp with the configured compute backend
//! (Vulkan, CPU, or auto-detected — see `stt.backend`), downloads the
//! ggml model to the path the config expects, grants ydotool uinput access, seeds
//! the user config, installs+enables the systemd user unit, and (opt-in) drops the
//! Quickshell pill module in place. Each step is idempotent — already-done work is
//! detected and skipped — so it is safe to re-run.

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
    /// Check that the installed system is actually ready to dictate.
    Verify,
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
        Some(SetupStep::Verify) => {
            verify(cfg);
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

    println!("  runtime:");
    for (cmd, note) in required_tools(server, &cfg.injection.mode) {
        report(cmd, note, have(cmd));
    }

    // The graphical pill is layer-shell only (Hyprland/KDE/Sway). On X11 we fall
    // back to desktop notifications, so flag that here instead of implying a gap.
    match server {
        DisplayServer::Wayland => {
            println!("  pill overlay: Quickshell (layer-shell); install with `setup --quickshell`")
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
    report(
        "libopenblas",
        "faster CPU inference (optional, backend = cpu)",
        has_lib("libopenblas"),
    );
}

fn report(name: &str, note: &str, ok: bool) {
    let mark = if ok { "✓" } else { "✗" };
    println!("    [{mark}] {name:<12} {note}");
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

    if !repo.join("CMakeLists.txt").exists() {
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
    } else {
        println!("  using existing source at {}", repo.display());
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
    let total: Option<u64> = resp.header("Content-Length").and_then(|s| s.parse().ok());

    // Download to a temp file, then rename, so an interrupted run never leaves a
    // truncated model that looks valid to the daemon.
    let tmp = dest.with_extension("part");
    let mut out = File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let mut reader = resp.into_reader();
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
    let user = std::env::var("USER").map_err(|_| "USER not set".to_string())?;
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

    // If systemd already knows a ydotoold unit (e.g. a packaged /usr unit), keep it.
    let known = Command::new("systemctl")
        .args(["--user", "cat", "ydotoold.service"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !known {
        let dir = config::expand("~/.config/systemd/user");
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let unit = dir.join("ydotoold.service");
        fs::write(&unit, ydotoold_unit_contents())
            .map_err(|e| format!("{}: {e}", unit.display()))?;
        println!("  wrote {}", unit.display());
        exec(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
    } else {
        println!("  using the ydotoold unit already known to systemd");
    }

    // Best-effort: a headless/SSH session may have no user manager.
    if exec(Command::new("systemctl").args(["--user", "enable", "--now", "ydotoold.service"]))
        .is_err()
    {
        println!("  could not enable ydotoold now — start it from your session with:");
        println!("    systemctl --user enable --now ydotoold");
    } else {
        println!("  enabled and started ydotoold");
    }
    Ok(())
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
    // If a unit is already visible to systemd (e.g. a packaged /usr unit), keep it.
    let known = Command::new("systemctl")
        .args(["--user", "cat", "whispy-daemon.service"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !known {
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let dir = config::expand("~/.config/systemd/user");
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let unit = dir.join("whispy-daemon.service");
        fs::write(&unit, unit_contents(&exe)).map_err(|e| format!("{}: {e}", unit.display()))?;
        println!("  wrote {}", unit.display());
        exec(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
    } else {
        println!("  using the unit already known to systemd");
    }

    // Best-effort: a headless/SSH session may have no user manager.
    if exec(Command::new("systemctl").args(["--user", "enable", "--now", "whispy-daemon.service"]))
        .is_err()
    {
        println!("  could not enable the service now — start it from your session with:");
        println!("    systemctl --user enable --now whispy-daemon");
    } else {
        println!("  enabled and started whispy-daemon");
    }
    Ok(())
}

fn unit_contents(exe: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=whispy dictation daemon\n\
         Documentation=https://github.com/Ceereals/whispy\n\
         After=pipewire.service ydotoold.service\n\
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
    let known = Command::new("systemctl")
        .args(["--user", "cat", "whispy-pill.service"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !known {
        let dir = config::expand("~/.config/systemd/user");
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let unit = dir.join("whispy-pill.service");
        fs::write(&unit, pill_unit_contents()).map_err(|e| format!("{}: {e}", unit.display()))?;
        println!("  wrote {}", unit.display());
        exec(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
    } else {
        println!("  using the whispy-pill unit already known to systemd");
    }

    if exec(Command::new("systemctl").args(["--user", "enable", "--now", "whispy-pill.service"]))
        .is_err()
    {
        println!("  could not enable the pill now — start it from your session with:");
        println!("    systemctl --user enable --now whispy-pill");
    } else {
        println!("  enabled and started whispy-pill");
    }
    Ok(())
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
    serde_json::from_str::<whispy_common::Resp>(line.trim())
        .map(|r| r.ok)
        .unwrap_or(false)
}

/// Is the `ydotoold` user service active?
fn ydotoold_active() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "ydotoold.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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
    let name = format!("{:?}", cmd.get_program());
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
        .map(|s| s.success())
        .unwrap_or(false)
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

/// Is a shared library matching `needle` registered with the dynamic linker?
/// Used to decide which whisper.cpp backend (Vulkan, OpenBLAS) the host can build.
fn has_lib(needle: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("ldconfig -p | grep -q {needle}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn print_next_steps(server: DisplayServer) {
    println!("\n==> done. Add push-to-talk keybinds:");
    match server {
        DisplayServer::Wayland => print_wayland_keybinds(),
        DisplayServer::X11 => print_x11_keybinds(),
    }
    println!("\nThen check it: whispy-client ping");
}

/// Hyprland (and other layer-shell compositors) keybind hints.
fn print_wayland_keybinds() {
    println!(
        "\n  Hyprland (~/.config/hypr/hyprland.conf), press-and-hold:\n\
         \n\
         bindd = SUPER, Space, Start dictation, exec, whispy-client start\n\
         bindr = SUPER, Space, Stop dictation,  exec, whispy-client stop\n\
         bind  = SUPER SHIFT, Space, Cancel,    exec, whispy-client cancel\n\
         \n\
         Other compositors (Sway, etc.) can bind the same three commands, or use\n\
         a single toggle: `whispy-client toggle`."
    );
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
            unit.contains("After=pipewire.service ydotoold.service"),
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
