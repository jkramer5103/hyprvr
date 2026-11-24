#!/usr/bin/env bash
# One-command setup for hyprvr.
#
#   ./setup.sh            laptop daemon; plus the Quest app if a headset is plugged in (adb)
#   ./setup.sh laptop     only the laptop daemon (vrd)
#   ./setup.sh quest      only build + install the Quest app
#
# Extra options are passed to scripts/install-laptop.sh
# (--no-deps, --no-firewall, --no-service, --tailscale).
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
say() { printf '\n\033[1;32m### %s\033[0m\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

MODE=auto
case "${1:-}" in
  laptop|quest) MODE="$1"; shift ;;
  -h|--help) sed -n '2,9p' "$0"; exit 0 ;;
esac

export ANDROID_HOME="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-/opt/android/sdk}}"
ADB="$(command -v adb || echo "$ANDROID_HOME/platform-tools/adb")"
quest_connected() { "$ADB" get-state >/dev/null 2>&1; }

lan_ip() { ip -4 route get 1.1.1.1 2>/dev/null | awk '{for(i=1;i<NF;i++) if($i=="src"){print $(i+1); exit}}'; }

setup_laptop() {
  say "1/2  Laptop: building and installing the vrd daemon"
  "$REPO/scripts/install-laptop.sh" "$@"
}

setup_quest() {
  say "2/2  Quest: building and installing the headset app"
  [[ -d "$ANDROID_HOME/ndk" ]] || die "Android SDK + NDK not found at $ANDROID_HOME (set ANDROID_HOME). See README → Quest app."
  command -v cargo-ndk >/dev/null || { echo "Installing cargo-ndk"; cargo install cargo-ndk; }
  rustup target add aarch64-linux-android >/dev/null 2>&1 || true
  "$REPO/quest/scripts/build-apk.sh"
  quest_connected || die "No Quest found over adb. Enable developer mode, plug in USB, accept the prompt in the headset, then run: ./setup.sh quest"
  local host; host="$(lan_ip)"
  "$REPO/quest/scripts/install-quest.sh" --no-log ${host:+--host "$host"}
}

case "$MODE" in
  laptop) setup_laptop "$@" ;;
  quest)  setup_quest ;;
  auto)
    setup_laptop "$@"
    if quest_connected; then
      setup_quest
    else
      say "2/2  Quest: skipped (no headset on adb). Plug it in and run: ./setup.sh quest"
    fi
    ;;
esac

say "Done"
cat <<'EOF'
Put on the headset and open hyprvr (Library → Unknown Sources).
It finds the laptop on your network; type the PIN it asks for:
    vrd pin
Free 360° backgrounds: press Download in the headset's environment picker,
or run: vrd env fetch
EOF
