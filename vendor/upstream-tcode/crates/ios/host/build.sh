#!/bin/bash

set -euo pipefail

HOST_DIR="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE_DIR="$(cd "$HOST_DIR/../../.." && pwd)"
MODE="${1:---simulator}"
RUST_PROFILE="${TCODE_IOS_RUST_PROFILE:-debug}"

case "$RUST_PROFILE" in
    debug)
        RUST_PRODUCT_DIR="debug"
        XCODE_CONFIGURATION="Debug"
        ;;
    release)
        RUST_PRODUCT_DIR="release"
        XCODE_CONFIGURATION="Release"
        ;;
    *)
        echo "TCODE_IOS_RUST_PROFILE must be debug or release" >&2
        exit 2
        ;;
esac

case "$MODE" in
    --simulator)
        RUST_TARGET="aarch64-apple-ios-sim"
        SIMULATOR_OS="${TCODE_IOS_SIMULATOR_OS:-27.0}"
        DESTINATION="platform=iOS Simulator,name=iPhone 18 Pro,OS=$SIMULATOR_OS"
        SDK="iphonesimulator"
        PRODUCT_DIR="$XCODE_CONFIGURATION-iphonesimulator"
        CODE_SIGNING_ALLOWED="YES"
        ;;
    --device)
        RUST_TARGET="aarch64-apple-ios"
        DESTINATION="generic/platform=iOS"
        SDK="iphoneos"
        PRODUCT_DIR="$XCODE_CONFIGURATION-iphoneos"
        CODE_SIGNING_ALLOWED="NO"
        ;;
    *)
        echo "usage: $0 [--simulator|--device]" >&2
        exit 2
        ;;
esac

echo "Building tcode-ios for $RUST_TARGET ($RUST_PROFILE)"
cd "$WORKSPACE_DIR"
if [[ "$RUST_PROFILE" == "release" ]]; then
    cargo build -p tcode-ios --target "$RUST_TARGET" --release --locked
else
    # rust-embed otherwise looks for dependency assets in the app's runtime
    # directory. Embed only the existing asset crate in the dev static library.
    cargo build -p tcode-ios --target "$RUST_TARGET" \
        --config 'profile.dev.package.gpui-kit-assets.debug-assertions=false' \
        --config 'profile.dev.package.rust-embed.debug-assertions=false'
fi

mkdir -p "$HOST_DIR/lib"
cp "$WORKSPACE_DIR/target/$RUST_TARGET/$RUST_PRODUCT_DIR/libtcode_ios.a" "$HOST_DIR/lib/libtcode_ios.a"

cd "$HOST_DIR"
xcodegen generate --spec project.yml

XCODE_SETTINGS=()
if [[ -n "${TCODE_BUILD_VERSION:-}" ]]; then
    XCODE_SETTINGS+=(MARKETING_VERSION="$TCODE_BUILD_VERSION")
fi

echo "Building UIKit host for $DESTINATION"
xcodebuild \
    -project Tcode.xcodeproj \
    -scheme Tcode \
    -configuration "$XCODE_CONFIGURATION" \
    -sdk "$SDK" \
    -destination "$DESTINATION" \
    -derivedDataPath build \
    ONLY_ACTIVE_ARCH=YES \
    ARCHS=arm64 \
    CODE_SIGNING_ALLOWED="$CODE_SIGNING_ALLOWED" \
    ${XCODE_SETTINGS[@]+"${XCODE_SETTINGS[@]}"} \
    build

echo "Built Tcode host at $HOST_DIR/build/Build/Products/$PRODUCT_DIR/Tcode.app"
