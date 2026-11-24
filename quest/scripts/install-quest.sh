#!/usr/bin/env bash
# Install hyprvr on a Quest connected over adb (USB or `adb connect <ip>:5555`), launch it
# and follow its log. Pass --no-log to skip logcat, --host <ip> to push files/config.toml.
set -euo pipefail

QUEST="$(cd "$(dirname "$0")/.." && pwd)"
APK="${APK:-$QUEST/build/hyprvr-quest.apk}"
PKG=dev.hyprvr.quest
ADB="${ADB:-adb}"
command -v "$ADB" >/dev/null || ADB="${ANDROID_HOME:-/opt/android/sdk}/platform-tools/adb"

LOG=1
HOST=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-log) LOG=0 ;;
        --host) HOST="$2"; shift ;;
        *) echo "usage: $0 [--no-log] [--host <pc-ip>]" >&2; exit 2 ;;
    esac
    shift
done

[[ -f "$APK" ]] || { echo "no APK at $APK; run scripts/build-apk.sh first" >&2; exit 1; }
"$ADB" get-state >/dev/null

echo "== installing $APK"
"$ADB" install -r -g "$APK"

if [[ -n "$HOST" ]]; then
    echo "== writing files/config.toml (host = $HOST)"
    printf 'host = "%s"\n' "$HOST" | "$ADB" shell "run-as $PKG sh -c 'mkdir -p files && cat > files/config.toml'"
fi

"$ADB" shell am force-stop "$PKG" || true
"$ADB" logcat -c || true
echo "== launching"
"$ADB" shell am start -n "$PKG/android.app.NativeActivity"

if [[ "$LOG" == 1 ]]; then
    echo "== logcat (Ctrl-C to stop)"
    exec "$ADB" logcat -v time -s hyprvr:V OpenXR:V OpenXR-Loader:V VrApi:I AndroidRuntime:E DEBUG:V libc:F
fi
