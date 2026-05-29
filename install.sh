#!/usr/bin/env bash
# whispy installer.
#
# On Arch-like systems this routes to the AUR package (via an AUR helper, or a
# local `makepkg -si`). Everywhere else it builds from source with cargo and
# installs the two binaries under a prefix.
#
# After install, run `whispy-daemon setup` to bootstrap whisper.cpp + the model.
#
# It then chains straight into `whispy-daemon setup` so a fresh machine goes from
# nothing to ready-to-dictate in one command.
#
# Env:
#   WHISPY_FORCE_SOURCE=1   build from this checkout with cargo even on Arch
#   WHISPY_NO_SETUP=1       install the binaries only; don't run `setup`
#   PREFIX=~/.local         install prefix for the source path (default ~/.local)
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Absolute path to the installed whispy-daemon, so `setup` runs even when the
# install prefix (e.g. ~/.local/bin) isn't on PATH in this shell yet. Set by the
# install path that ran; falls back to the PATH lookup for the AUR case.
whispy_daemon_bin="whispy-daemon"

is_arch() {
  [ -f /etc/arch-release ] && return 0
  grep -qiE '^(ID|ID_LIKE)=.*\barch\b' /etc/os-release 2>/dev/null
}

install_from_source() {
  command -v cargo >/dev/null || {
    echo "error: cargo not found — install Rust (https://rustup.rs)" >&2
    exit 1
  }
  local prefix="${PREFIX:-$HOME/.local}"
  echo "Building from source (cargo build --release)..."
  ( cd "$repo_root" && cargo build --release --locked )
  install -Dm755 "$repo_root/target/release/whispy-daemon" "$prefix/bin/whispy-daemon"
  install -Dm755 "$repo_root/target/release/whispy-client" "$prefix/bin/whispy-client"
  echo "Installed whispy-daemon and whispy-client to $prefix/bin"
  whispy_daemon_bin="$prefix/bin/whispy-daemon"
  case ":$PATH:" in
    *":$prefix/bin:"*) ;;
    *) echo "note: add $prefix/bin to your PATH" ;;
  esac
}

install_on_arch() {
  if helper="$(command -v paru || command -v yay)"; then
    echo "Arch detected — installing the 'whispy' AUR package via $(basename "$helper")..."
    "$helper" -S whispy && return 0
    echo "AUR helper install failed; falling back to local makepkg." >&2
  fi
  command -v makepkg >/dev/null || {
    echo "error: makepkg not found — install base-devel" >&2
    exit 1
  }
  echo "Building the AUR package locally (makepkg -si)..."
  echo "(this fetches the published v\$pkgver release; for a local-checkout build"
  echo " instead, re-run with WHISPY_FORCE_SOURCE=1)"
  ( cd "$repo_root/packaging/aur" && makepkg -si )
}

if is_arch && [ "${WHISPY_FORCE_SOURCE:-0}" != "1" ]; then
  install_on_arch
else
  install_from_source
fi

if [ "${WHISPY_NO_SETUP:-0}" = "1" ]; then
  cat <<EOF

Binaries installed. Finish setup when ready (builds whisper.cpp + downloads the
model + grants ydotool access + enables the services):

    $whispy_daemon_bin setup

EOF
  exit 0
fi

echo
echo "==> Bootstrapping the runtime ($whispy_daemon_bin setup)..."
echo "    (whisper.cpp build + model download + ydotool + services; re-runnable)"
echo "    Skip this next time with WHISPY_NO_SETUP=1."
echo
"$whispy_daemon_bin" setup
