#!/usr/bin/env bash
# Build the signed hyprvr Quest APK: quest/build/hyprvr-quest.apk
#
# Needs: Android SDK (build-tools, platform android-34), NDK, cargo-ndk, rustup target
# aarch64-linux-android, a JDK (keytool), curl, unzip.
# Env overrides: ANDROID_HOME, ANDROID_NDK_HOME, BUILD_TOOLS, PLATFORM, PROFILE (release|debug),
# OPENXR_LOADER_VERSION, KEYSTORE.
set -euo pipefail

QUEST="$(cd "$(dirname "$0")/.." && pwd)"
ANDROID_HOME="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-/opt/android/sdk}}"
if [[ -z "${ANDROID_NDK_HOME:-}" ]]; then
    ANDROID_NDK_HOME="$(ls -d "$ANDROID_HOME"/ndk/* 2>/dev/null | sort -V | tail -1)"
fi
export ANDROID_NDK_HOME
BUILD_TOOLS="${BUILD_TOOLS:-$(ls -d "$ANDROID_HOME"/build-tools/* | sort -V | tail -1)}"
PLATFORM="${PLATFORM:-$ANDROID_HOME/platforms/android-34}"
PROFILE="${PROFILE:-release}"
OPENXR_LOADER_VERSION="${OPENXR_LOADER_VERSION:-1.1.65}"
KEYSTORE="${KEYSTORE:-$HOME/.android/hyprvr-debug.keystore}"

OUT="$QUEST/build"
STAGE="$OUT/stage"
APK_UNSIGNED="$OUT/hyprvr-quest-unsigned.apk"
APK_ALIGNED="$OUT/hyprvr-quest-aligned.apk"
APK="$OUT/hyprvr-quest.apk"

echo "== NDK: $ANDROID_NDK_HOME"
echo "== build-tools: $BUILD_TOOLS"
echo "== platform: $PLATFORM"

# 1. Native library.
cd "$QUEST"
cargo_flags=(-p hyprvr-quest)
[[ "$PROFILE" == release ]] && cargo_flags+=(--release)
cargo ndk -t arm64-v8a -P 29 build "${cargo_flags[@]}"
LIB="$QUEST/target/aarch64-linux-android/$PROFILE/libhyprvr_quest.so"
[[ -f "$LIB" ]] || { echo "missing $LIB" >&2; exit 1; }

# 2. Khronos OpenXR loader from Maven Central (cached).
LOADER_DIR="$OUT/openxr-loader-$OPENXR_LOADER_VERSION"
LOADER="$LOADER_DIR/jni/arm64-v8a/libopenxr_loader.so"
if [[ ! -f "$LOADER" ]]; then
    mkdir -p "$LOADER_DIR"
    AAR="$LOADER_DIR/loader.aar"
    URL="https://repo1.maven.org/maven2/org/khronos/openxr/openxr_loader_for_android/$OPENXR_LOADER_VERSION/openxr_loader_for_android-$OPENXR_LOADER_VERSION.aar"
    echo "== downloading $URL"
    curl -fsSL -o "$AAR" "$URL"
    if [[ "$OPENXR_LOADER_VERSION" == 1.1.65 ]]; then
        echo "a298c326085b09181c85235fb04d37510939bd30938ed4322c093c09780e8643  $AAR" | sha256sum -c --quiet
    else
        echo "warning: no pinned checksum for loader $OPENXR_LOADER_VERSION" >&2
    fi
    (cd "$LOADER_DIR" && unzip -q -o loader.aar 'jni/arm64-v8a/libopenxr_loader.so')
fi

# 3. Stage libs.
rm -rf "$STAGE"
mkdir -p "$STAGE/lib/arm64-v8a"
cp "$LIB" "$STAGE/lib/arm64-v8a/libhyprvr_quest.so"
cp "$LOADER" "$STAGE/lib/arm64-v8a/libopenxr_loader.so"
# libc++_shared is needed only if something links the shared C++ runtime.
if "$ANDROID_NDK_HOME"/toolchains/llvm/prebuilt/*/bin/llvm-readelf -d "$LIB" "$LOADER" 2>/dev/null | grep -q 'libc++_shared.so'; then
    cp "$ANDROID_NDK_HOME"/toolchains/llvm/prebuilt/*/sysroot/usr/lib/aarch64-linux-android/libc++_shared.so "$STAGE/lib/arm64-v8a/"
fi
if [[ "$PROFILE" == release ]]; then
    # Keep symbols out of the APK (the unstripped .so stays in target/ for symbolication).
    "$ANDROID_NDK_HOME"/toolchains/llvm/prebuilt/*/bin/llvm-strip --strip-debug "$STAGE/lib/arm64-v8a/libhyprvr_quest.so"
fi

# 4. Package: manifest via aapt2, then add libs.
rm -f "$APK_UNSIGNED" "$APK_ALIGNED" "$APK"
"$BUILD_TOOLS/aapt2" link -o "$APK_UNSIGNED" -I "$PLATFORM/android.jar" --manifest "$QUEST/AndroidManifest.xml"
(cd "$STAGE" && zip -q -r "$APK_UNSIGNED" lib)

# 5. Align + sign.
"$BUILD_TOOLS/zipalign" -f -P 16 4 "$APK_UNSIGNED" "$APK_ALIGNED"
if [[ ! -f "$KEYSTORE" ]]; then
    echo "== creating debug keystore $KEYSTORE"
    mkdir -p "$(dirname "$KEYSTORE")"
    keytool -genkeypair -keystore "$KEYSTORE" -storepass android -keypass android -alias hyprvr \
        -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=hyprvr debug" >/dev/null
fi
"$BUILD_TOOLS/apksigner" sign --v2-signing-enabled true --v3-signing-enabled true --ks "$KEYSTORE" --ks-pass pass:android --key-pass pass:android \
    --ks-key-alias hyprvr --out "$APK" "$APK_ALIGNED"
rm -f "$APK_UNSIGNED" "$APK_ALIGNED" "$APK.idsig"

echo "== built $APK ($(du -h "$APK" | cut -f1))"
