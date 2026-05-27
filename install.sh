#!/usr/bin/env bash
# whispy installer.
#
# On Arch-like systems this routes to the AUR package (via an AUR helper, or a
# local `makepkg -si`). Everywhere else it builds from source with cargo and
# installs the two binaries under a prefix.
#
# After install, run `whispy-daemon setup` to bootstrap whisper.cpp + the model.
#
# Env:
#   WHISPY_FORCE_SOURCE=1   build from this checkout with cargo even on Arch
#   PREFIX=~/.local         install prefix for the source path (default ~/.local)
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

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

cat <<'EOF'

Next step — bootstrap the runtime (whisper.cpp build + model download + ydotool + service):

    whispy-daemon setup            # add --quickshell for the pill overlay

EOF
