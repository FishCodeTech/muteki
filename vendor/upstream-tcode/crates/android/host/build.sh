#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# rust-embed normally reads assets from the current directory in dev builds,
# which does not exist inside an APK. Embed only the existing asset crate (and
# its rust-embed runtime) while keeping debug assertions for application code.
GRADLE_TASK=assembleDebug
RELEASE=false
CARGO_BUILD_ARGS=(
    --config 'profile.dev.package.gpui-kit-assets.debug-assertions=false'
    --config 'profile.dev.package.rust-embed.debug-assertions=false'
)
if [[ $# -eq 1 && $1 == --release ]]; then
    CARGO_BUILD_ARGS=(--profile android-release)
    GRADLE_TASK=assembleRelease
    RELEASE=true
elif [[ $# -ne 0 ]]; then
    echo "Usage: $0 [--release]" >&2
    exit 2
fi

export ANDROID_HOME="${ANDROID_HOME:-/opt/homebrew/share/android-commandlinetools}"
export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$ANDROID_HOME/ndk/27.1.12297006}"
export JAVA_HOME="${JAVA_HOME:-/opt/homebrew/opt/openjdk@21}"
export CARGO_NDK_PLATFORM="${CARGO_NDK_PLATFORM:-26}"

cd "$CRATE_DIR"
# cargo-ndk checks destination mtime, not profile or contents. A newer dev or
# stripped staging copy must never hide the exact Cargo artifact for this build.
rm -f host/app/src/main/jniLibs/arm64-v8a/libtcode_android.so
cargo ndk -t arm64-v8a -o host/app/src/main/jniLibs build -p tcode-android \
    "${CARGO_BUILD_ARGS[@]}"
# Dependencies that also declare a cdylib target (iroh and iroh-relay do, for
# wasm) get copied by cargo-ndk as well; only the app library belongs in the APK.
find host/app/src/main/jniLibs/arm64-v8a -name '*.so' ! -name libtcode_android.so -delete

if $RELEASE; then
    # Preserve the exact build before stripping only the copy delivered to Gradle.
    SYMBOL_DIR="$SCRIPT_DIR/app/build/unstripped/arm64-v8a"
    mkdir -p "$SYMBOL_DIR"
    cp host/app/src/main/jniLibs/arm64-v8a/libtcode_android.so "$SYMBOL_DIR/"
    LLVM_STRIP=("$ANDROID_NDK_HOME"/toolchains/llvm/prebuilt/*/bin/llvm-strip)
    "${LLVM_STRIP[0]}" --strip-unneeded host/app/src/main/jniLibs/arm64-v8a/libtcode_android.so
fi

# AGP's incremental ZIP writer can retain dead space after a native library shrinks.
# Keep crash symbols, but force fresh APK entries and native packaging intermediates.
rm -rf "$SCRIPT_DIR/app/build/outputs" "$SCRIPT_DIR/app/build/intermediates"
cd "$SCRIPT_DIR"
./gradlew "$GRADLE_TASK"

VARIANT=debug
if $RELEASE; then VARIANT=release; fi
APK_PATH="$SCRIPT_DIR/app/build/outputs/apk/$VARIANT/app-$VARIANT.apk"
APK_SIZE="$(wc -c < "$APK_PATH" | tr -d '[:space:]')"
printf '\nAPK: %s\nSize: %s bytes\n' "$APK_PATH" "$APK_SIZE"
