//! `whispy-daemon setup`: one-shot bootstrap for a fresh install.
//!
//! Brings the machine from "binaries installed" to "ready to dictate": checks the
//! runtime/build tools, builds whisper.cpp with the Vulkan backend, downloads the
//! ggml model to the path the config expects, grants ydotool uinput access, seeds
//! the user config, installs+enables the systemd user unit, and (opt-in) drops the
//! Quickshell pill module in place. Each step is idempotent — already-done work is
//! detected and skipped — so it is safe to re-run.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use crate::config::{self, Config};

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
}

#[derive(clap::Subcommand, Debug)]
enum SetupStep {
    /// Check that the required runtime and build tools are present.
    Doctor,
    /// Clone and build whisper.cpp with the Vulkan backend.
    Whisper,
    /// Download the ggml model to the configured path.
    Model,
    /// Grant uinput access for ydotool (sudo; log out/in afterwards).
    Ydotool,
    /// Install config files and the systemd user service.
    Systemd,
    /// Install the Quickshell pill module.
    Quickshell,
}

/// Entry point dispatched from `main` when the `setup` subcommand is given.
pub fn run(cfg: &Config, args: SetupArgs) -> ExitCode {
    let result = match args.step {
        Some(SetupStep::Doctor) => {
            doctor();
            Ok(())
        }
        Some(SetupStep::Whisper) => build_whisper(cfg),
        Some(SetupStep::Model) => download_model(cfg),
        Some(SetupStep::Ydotool) => setup_ydotool(),
        Some(SetupStep::Systemd) => install_config(cfg).and_then(|()| install_systemd()),
        Some(SetupStep::Quickshell) => install_quickshell(),
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
    doctor();
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
        install_systemd()?;
    }
    if args.quickshell {
        install_quickshell()?;
    }
    print_next_steps();
    Ok(())
}

// --- doctor -----------------------------------------------------------------

fn doctor() {
    println!("==> doctor: checking tools");
    println!("  runtime:");
    for (cmd, note) in [
        ("pw-record", "PipeWire audio capture"),
        ("wl-copy", "clipboard write (wl-clipboard)"),
        ("wl-paste", "clipboard read (wl-clipboard)"),
        ("ydotool", "keystroke injection"),
        ("notify-send", "error notifications (libnotify)"),
    ] {
        report(cmd, note, have(cmd));
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
    report("libvulkan", "Vulkan loader", has_vulkan());
}

fn report(name: &str, note: &str, ok: bool) {
    let mark = if ok { "✓" } else { "✗" };
    println!("    [{mark}] {name:<12} {note}");
}

// --- whisper.cpp ------------------------------------------------------------

fn build_whisper(cfg: &Config) -> Result<(), String> {
    println!("\n==> whisper.cpp (Vulkan)");
    let bin = cfg.stt.server_bin_path();
    if bin.exists() {
        println!("  already built: {}", bin.display());
        return Ok(());
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

    println!("  configuring (cmake -DGGML_VULKAN=ON)");
    exec(Command::new("cmake").args([
        "-B",
        &build_dir.to_string_lossy(),
        "-S",
        &repo.to_string_lossy(),
        "-DGGML_VULKAN=ON",
        "-DCMAKE_BUILD_TYPE=Release",
    ]))?;

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
    println!("  done. Log out/in for the 'input' group, then start the ydotool daemon:");
    println!("    systemctl --user enable --now ydotool   # if your package ships a unit");
    Ok(())
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
         After=pipewire.service\n\
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

fn install_quickshell() -> Result<(), String> {
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
    println!("  installed to {}", dest.display());
    println!("  add `import Whispy` + `PillPanel {{}}` to your shell.qml");
    Ok(())
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
fn have(cmd: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {cmd}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Is the Vulkan loader present (so the Vulkan whisper.cpp build can run)?
fn has_vulkan() -> bool {
    Command::new("sh")
        .arg("-c")
        .arg("ldconfig -p | grep -q libvulkan")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn print_next_steps() {
    println!(
        "\n==> done. Add Hyprland keybinds (push-to-talk):\n\
         \n\
         bindd = SUPER, Space, Start dictation, exec, whispy-client start\n\
         bindr = SUPER, Space, Stop dictation,  exec, whispy-client stop\n\
         bind  = SUPER SHIFT, Space, Cancel,    exec, whispy-client cancel\n\
         \n\
         Then check it: whispy-client ping"
    );
}
