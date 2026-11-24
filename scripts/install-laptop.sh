#!/usr/bin/env bash
# Build and install the hyprvr daemon (vrd) for the current user on Arch/Omarchy.
#
#   scripts/install-laptop.sh [--no-deps] [--no-firewall] [--no-service] [--tailscale]
#
# Interactive: pacman and ufw steps ask for your sudo password.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$HOME/.local/bin"
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
DEPS=1 FIREWALL=1 SERVICE=1 TAILSCALE=0
for a in "$@"; do
  case "$a" in
    --no-deps) DEPS=0 ;;
    --no-firewall) FIREWALL=0 ;;
    --no-service) SERVICE=0 ;;
    --tailscale) TAILSCALE=1 ;;
    -h|--help) sed -n '2,6p' "$0"; exit 0 ;;
    *) echo "unknown option: $a" >&2; exit 1 ;;
  esac
done

say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }

if [[ $EUID -eq 0 ]]; then
  echo "Run this as your normal user (it uses sudo where needed)." >&2
  exit 1
fi

# 1. System packages ------------------------------------------------------------
if (( DEPS )); then
  if command -v pacman >/dev/null; then
    PKGS=(base-devel clang pkgconf ffmpeg libva intel-media-driver libva-utils
          wayland libpulse libnotify)
    # pactl/parec come from libpulse; the server side is pipewire-pulse (or pulseaudio).
    if ! pacman -Qq pulseaudio >/dev/null 2>&1; then PKGS+=(pipewire-pulse); fi
    missing=()
    for p in "${PKGS[@]}"; do pacman -Qq "$p" >/dev/null 2>&1 || missing+=("$p"); done
    if ((${#missing[@]})); then
      say "Installing packages: ${missing[*]}"
      sudo pacman -S --needed "${missing[@]}"
    else
      say "System packages already installed"
    fi
  else
    warn "pacman not found; install ffmpeg (dev headers), libva, clang, pkg-config, libpulse and libnotify yourself"
  fi
fi

# 2. Rust toolchain (user level) ------------------------------------------------
if ! command -v cargo >/dev/null; then
  if [[ -x "$HOME/.cargo/bin/cargo" ]]; then
    export PATH="$HOME/.cargo/bin:$PATH"
  else
    say "Installing Rust with rustup (user level, ~/.cargo)"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    export PATH="$HOME/.cargo/bin:$PATH"
  fi
fi
# The code uses Rust 2024 edition features (let chains); make sure stable is recent.
if command -v rustup >/dev/null; then rustup update stable --no-self-update >/dev/null 2>&1 || true; fi
say "Using $(cargo --version)"

# 3. Build ----------------------------------------------------------------------
say "Building (release)"
cargo build --release --manifest-path "$REPO/Cargo.toml" -p vrd -p hyprvr-probe
mkdir -p "$BIN"
install -m755 "$REPO/target/release/vrd" "$BIN/vrd"
install -m755 "$REPO/target/release/hyprvr-probe" "$BIN/hyprvr-probe"
say "Installed $BIN/vrd and $BIN/hyprvr-probe"
case ":$PATH:" in *":$BIN:"*) ;; *) warn "$BIN is not on PATH" ;; esac

# 4. Firewall -------------------------------------------------------------------
if (( FIREWALL )) && command -v ufw >/dev/null; then
  NETS=(10.0.0.0/8 172.16.0.0/12 192.168.0.0/16)
  (( TAILSCALE )) && NETS+=(100.64.0.0/10)
  say "Opening UDP 47800 (QUIC) and 47801 (discovery) for private networks: ${NETS[*]}"
  for n in "${NETS[@]}"; do
    for port in 47800 47801; do
      sudo ufw allow proto udp from "$n" to any port "$port" comment hyprvr >/dev/null
    done
  done
  # Link-local IPv6 (fe80::/10) for v6-only LANs.
  sudo ufw allow proto udp from fe80::/10 to any port 47800 comment hyprvr >/dev/null 2>&1 || true
  sudo ufw allow proto udp from fe80::/10 to any port 47801 comment hyprvr >/dev/null 2>&1 || true
elif (( FIREWALL )); then
  say "ufw not installed; skipping firewall rules"
fi

# 5. systemd user service -------------------------------------------------------
if (( SERVICE )); then
  mkdir -p "$UNIT_DIR"
  install -m644 "$REPO/packaging/systemd/vrd.service" "$UNIT_DIR/vrd.service"
  systemctl --user daemon-reload
  systemctl --user enable vrd.service
  if systemctl --user is-active --quiet graphical-session.target; then
    systemctl --user restart vrd.service
    sleep 1
    systemctl --user --no-pager --lines=5 status vrd.service || true
  else
    say "graphical-session.target is not active here; vrd will start with your next Hyprland (uwsm) session"
  fi
fi

# 6. Bar snippet (printed only; nothing is edited) ------------------------------
cat <<'SNIP'

==> Bar integration (not installed automatically)

`vrd waybar` (same as `vrd status --follow --json`) prints one JSON line per change:
  {"text": "VR 2 · 3.1M", "tooltip": "...", "class": "connected|grace|idle|pairing|stopped"}

Waybar:
  "custom/vr": {
    "exec": "vrd waybar",
    "return-type": "json",
    "on-click": "vrd pin",
    "on-click-right": "vrd audio"
  }

Quickshell (QML), e.g. inside your bar:
  import Quickshell.Io
  Process {
    id: vrd
    command: ["vrd", "waybar"]
    running: true
    stdout: SplitParser { onRead: line => vrStatus.text = JSON.parse(line).text }
  }
  Text { id: vrStatus; MouseArea { anchors.fill: parent; onClicked: Quickshell.execDetached(["vrd", "pin"]) } }

Next steps:
  vrd status          # daemon state
  vrd pin             # show pairing PIN (also sent as a notification)
  Config: ~/.config/hyprvr/config.toml   Environments: ~/.config/hyprvr/environments/*.jpg|png
SNIP
