# gpui-android

Android platform backend for the gpui-fast GPUI used by tcode. The
crate is an ordinary Rust dependency on every target, but its implementation is
compiled only for Android. Calling `platform()` elsewhere fails with a clear
panic instead of pulling Android libraries into host builds.

## Architecture

`tcode-android` enters through `android_main`, initializes this crate with the
`android_activity::AndroidApp`, and constructs `gpui::Application` with the
process-local platform. Android's native activity loop remains the GPUI
foreground executor. Work submitted to the background executor is distributed
over a small Rust worker pool; delayed work uses timer threads and foreground
continuations wake the Android looper. GPUI's `frame_waker` and
`schedule_frame` request a coalesced Java `Choreographer` callback; unrelated
looper wakes do not render. [FramePacer](../frame-pacing/src/lib.rs) owns demand
and activity grace, shared with iOS. Isolated invalidations return to pause;
touch input and next-frame animations keep callbacks scheduled. Pausing the
activity or losing its surface suspends pacing without losing invalidation.
The Java host reports the display's maximum refresh capability and uses
`Surface.setFrameRate` on API 30+, with seamless-only switching on API 31+.
Idle clears the frame-rate vote, leaving system power and thermal policy in
control.

The platform exposes one full-screen `PlatformWindow`. It owns the current
`ANativeWindow`, a `gpui_wgpu::WgpuRenderer`, and the shared `WgpuContext`.
`InitWindow` creates or replaces the Vulkan surface. `TerminateWindow`
unconfigures it before Android invalidates the native window, while preserving
the device, pipelines, and sprite atlas for resume. The surface's presence is
also the window's `WindowVisibility`: `InitWindow` and `TerminateWindow`
report the transition through `on_visibility_change`. Density converts Android
device pixels into GPUI logical pixels. `uiMode` supplies light/dark appearance.

`CosmicTextSystem` is populated from `/system/fonts` because fontdb does not
load Android system fonts automatically. This includes Android's Noto CJK fonts
for mixed Latin/Chinese text and `/system/fonts/NotoColorEmoji.ttf` for emoji.
`AndroidTextSystem` keeps cosmic-text shaping and delegates color emoji rasterization
to Android's software Canvas on API 31+, supporting modern COLRv1 system fonts
without packaging a second font. Older devices use Swash's CBDT/CBLC path.
tcode also registers its shared UI and monospace fonts, exactly once.

A missing system emoji font is not expected on supported stock Android devices
(minSdk 26); Android has supplied color emoji since 4.4, and compatibility rules
require the system font. For custom ROM distributions that omit it, the application
can read an optional `app/src/main/assets/fonts/NotoColorEmoji.ttf` bitmap fallback
only when the system file is absent. Standard builds contain no fallback font or
font license asset. Supply the font's license as well if packaging that optional
asset. Android 13 introduced COLRv1 system emoji; file presence alone did not make
the previous Swash-only renderer compatible.

## Java/JNI surface

The Gradle host supplies `com.tryanks.tcode.GpuiActivity`, a `NativeActivity`
subclass with a one-pixel focusable editor view. Its `BaseInputConnection`
provides the IME protocol Android requires without covering or intercepting the
native rendering surface.

The Java declarations live in
[GpuiActivity.java](../../android/host/app/src/main/java/com/tryanks/tcode/GpuiActivity.java).
The matching JNI exports in [tcode-android](../../android/src/lib.rs) forward
callbacks to this backend. Rust calls activity methods on Android's Java UI
thread; incoming callbacks are queued for the native activity/GPUI thread.
Keep the method signatures at these two ends synchronized.

URL opening uses an Android `ACTION_VIEW` intent on the Java UI thread, handing
HTTP(S) links to the browser or associated app. If no app can handle the URL,
the host logs the failure without crashing.

Editable fields publish Android's text, selection and composing region after
each completed IME batch. GPUI applies the changed UTF-16 range and sends its
state back after app edits, cursor moves and draft clears. Revision and edit
serial checks prevent delayed updates from restoring stale text. Terminal
handlers keep the committed-text and control-key path because they expose no
editable buffer. Hardware/IME key events become GPUI `KeyDown`/`KeyUp` events.
System-bar, display-cutout, and IME geometry becomes `WindowInsets`, and the
IME inset also shrinks the window's visual viewport; GPUI refreshes the window
from those callbacks and wakes the frame source itself. Applications reach the
keyboard through `Window::request_virtual_keyboard` and
`dismiss_virtual_keyboard`, which call the same activity methods as text focus.
A GPUI window back handler takes precedence; `set_back_callback` exposes
otherwise unhandled system back actions to the host application.

## Pointer mapping

Android `MotionEvent`s are forwarded as GPUI `TouchEvent`s, using every pointer,
Android's per-gesture pointer ids, logical coordinates, pressure, and the
corresponding started/moved/ended/cancelled phase. Pointer ids are paired with
the motion stream's monotonic down time so a reused Android id cannot collide
with an earlier GPUI touch.

Gesture interpretation lives in GPUI's portable gesture arena, the same
path used by `gpui-ios`. Android supplies only platform tuning: a 450 ms long
press and `ScrollPhysics::android()`. Tap synthesis, touch slop, scroll capture,
drag-cancels-click, velocity sampling, and momentum are therefore shared with
iOS rather than reimplemented in this backend.

## Build and run

From the repository root, with the Android SDK/NDK, JDK and `cargo-ndk`
installed:

```sh
crates/android/host/build.sh
adb install -r crates/android/host/app/build/outputs/apk/debug/app-debug.apk
adb shell am start -W -n com.tryanks.tcode/.GpuiActivity
```

The script builds the arm64 Rust library and Gradle Debug APK. Set
`ANDROID_HOME`, `ANDROID_NDK_HOME` and `JAVA_HOME` to your local installations;
the script's defaults are Homebrew paths. `CARGO_NDK_PLATFORM` defaults to 26.
The debug build embeds the shared font/SVG assets so it does not depend on
source paths from the development machine.

The keyboard bridge has device tests using Android's real `BaseInputConnection`.
With an emulator or device attached, run from `crates/android/host`:

```sh
./gradlew connectedDebugAndroidTest \
  -Pandroid.testInstrumentationRunnerArguments.class=com.tryanks.tcode.GpuiInputConnectionTest
```

These cover batched word replacement, composing regions, selection, Unicode
deletion and retired connections. Native range/synchronization tests run with
`cargo test -p gpui-android --lib --locked` from the repository root.

For testing performance on a phone, run `crates/android/host/build.sh --release`.
This uses the `android-release` Cargo profile: optimization level 3, fat LTO,
one codegen unit, unwind panics, and unstripped symbols with line information.
The build script copies the exact unstripped library to
`crates/android/host/app/build/unstripped/arm64-v8a/libtcode_android.so`, then runs
NDK `llvm-strip --strip-unneeded` on the separate `src/main/jniLibs` packaging copy.
This retains the exported JNI entry points and unwind tables. Preserve the
unstripped directory together with its APK before another release build overwrites
it. For a native crash log:

```sh
"$ANDROID_NDK_HOME/ndk-stack" \
  -sym crates/android/host/app/build/unstripped/arm64-v8a -dump crash.txt
```

Gradle runs `assembleRelease` with R8 and resource shrinking, using the debug
signing key for local installation. Rust-to-Java `gpui*` callbacks, the `previewHost` field and `PreviewHost.command`
are explicitly kept in `app/proguard-rules.pro`. Preserve `app/build/outputs/mapping/release/mapping.txt`
for Java stack traces too. The optimized APK is
`app/build/outputs/apk/release/app-release.apk`; the no-flag development build
remains `app/build/outputs/apk/debug/app-debug.apk`. Both modes print their path
and byte size. Only arm64-v8a is packaged.
The staged native library is removed before `cargo ndk` copies its output:
cargo-ndk's timestamp freshness check can otherwise reuse a newer dev library
when switching to a cached release, or preserve a previously stripped copy.
Before assembling either variant the script removes `app/build/outputs` and
`app/build/intermediates`, preserving `app/build/unstripped`. AGP's incremental
ZIP writer can otherwise leave dead bytes after a large native library shrinks;
compare the file's byte size with ZIP entry totals when diagnosing this.

Native libraries are stored uncompressed and page-aligned (`useLegacyPackaging=false`),
so Android loads them directly from the APK instead of extracting a second copy.
This increases the APK's byte size relative to ZIP-compressed libraries, but
reduces installed storage. The manifest leaves `extractNativeLibs` to AGP.
For reproducible size experiments, override the profile without editing Cargo.toml:

```sh
CARGO_PROFILE_ANDROID_RELEASE_OPT_LEVEL=s crates/android/host/build.sh --release
CARGO_PROFILE_ANDROID_RELEASE_OPT_LEVEL=z crates/android/host/build.sh --release
```

The layout rule the shell applies on this build is owned by
[`tcode-ui::window_seam`](../../ui/src/window_seam.rs). See
[remote work mode](../../../docs/remote.md) for
pairing with a host. Android emulator loopback is the emulator itself; use a
host address reachable from the device.

## Current limitations

- Android supports a single GPUI window; desktop window management operations
  are intentionally no-ops.
- Generic GPUI file dialogs, system credential storage, notifications,
  and accessibility bridging are not implemented in this backend;
  applications must provide any required services in their host.
- The text clipboard is bridged to Android `ClipboardManager`. Non-text GPUI
  clipboard data is retained only in-process.
- Raw multi-touch reaches the portable gesture arena, but this backend does not
  yet translate stylus buttons, hover, or hardware mouse-wheel axes.

## Attribution

The architecture and Android integration patterns were studied from
`gpui-toolkit/crates/gpui-android` and its showcase host, copyright 2025 Pierre
F. Aubert, licensed under the ISC license. This backend was written for the
different `gpui-pre` 0.3.5 interfaces rather than vendoring that source. The
reference's ISC permission and warranty notice remain applicable to ideas and
adapted integration patterns: use, copying, modification, and distribution are
permitted with the copyright and permission notice retained; the software is
provided “AS IS” without warranty.

## Scroll target ownership

The platform forwards actual touch coordinates unchanged. GPUI 0.3.5 owns the
private `TouchGestureRecognizer`, including slop, pan deltas, long presses,
selection drags, velocity, and frame-driven momentum. Its `gestures.rs` emits
pan start/move/end/cancel scroll positions at `ActiveTouch::start_position` and
copies that position into momentum. Anchoring raw platform touches instead would
remove the motion needed by that recognizer and break selection drags.

A scroll coordinate is not element capture: `Window::dispatch_mouse_event`
hit-tests the current rendered frame on every event. Content moving under even
a fixed anchor can therefore change the hitboxes. `HitboxId::should_handle_scroll`
uses that hit-test list, not pointer capture; the `div` overflow listener also
allows propagation after changing its offset. This does not implement exclusive
child scrolling with ancestor takeover only at a limit.

tcode does not add capture. Nesting is resolved per event by gpui-base's
scrollable mask, composed in [`tcode-ui::scroll`](../../ui/src/scroll.rs): a
bounded vertical area consumes the scroll events it can use in the capture
phase and lets the rest bubble to its ancestor, so it chains to the ancestor
at an edge; a horizontal strip owns horizontal movement even at its edges and
leaves vertical movement to the view behind it. Page-level viewports scroll
through GPUI's own handlers and stretch at their edges through gpui-base's
`ScrollBounce`. Native touch coordinates and GPUI's tap, long-press, and
selection recognition remain unchanged; no second platform recognizer is
needed.
