#!/usr/bin/env bash
# Grant the current user uinput access so ydotool works without root (Step 5).
# Run once; uses sudo for the udev rule and group. Log out / back in afterwards.
set -euo pipefail

RULE=/etc/udev/rules.d/80-uinput.rules

if ! command -v ydotool >/dev/null; then
  echo "ydotool not installed. Install it first (e.g. 'sudo pacman -S ydotool')." >&2
fi

echo "Creating input group (if missing) and adding $USER..."
sudo groupadd -f input
sudo usermod -aG input "$USER"

echo "Installing udev rule at $RULE..."
echo 'KERNEL=="uinput", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"' \
  | sudo tee "$RULE" >/dev/null

echo "Reloading udev rules..."
sudo udevadm control --reload-rules
sudo udevadm trigger

cat <<'EOF'

Done. Next:
  - Log out and back in for the 'input' group to take effect.
  - Start the ydotool daemon (it provides /tmp/.ydotool_socket):
      systemctl --user enable --now ydotool    # if your package ships a unit
    or run 'ydotoold' manually / from your Hyprland autostart.
  - Verify: 'ydotool key 29:1 47:1 47:0 29:0' should paste into a focused field.
EOF
