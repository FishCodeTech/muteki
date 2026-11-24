# gpui-ios

`gpui-ios` is the UIKit platform backend for the gpui-fast GPUI used by the
tcode iOS host. It is deliberately an embedded backend: `UIApplication` owns
the process and run loop, Swift supplies a `UIView`, and
`Application::run_embedded` keeps GPUI alive while UIKit drives frames and
input.

The public Rust entry point is:

```rust
pub fn platform() -> Rc<dyn gpui::Platform>
```

It is process-wide, lazily initialized, and must be called on the UIKit main
thread. On non-iOS targets the crate still compiles; calling `platform()`
panics with a platform-specific diagnostic.

## Architecture

The Swift host creates a `GPUIHostView` whose backing layer is
`CAMetalLayer`, then passes the unretained `UIView` pointer and its logical
geometry through `gpui_ios_attach_view`. `IosWindow` wraps that pointer in
`raw-window-handle`'s UIKit handle, creates a Metal-only `wgpu::Instance`, and
hands its context to gpui-fast's `gpui_wgpu::WgpuRenderer`. Logical
resizes are converted to device pixels before `update_drawable_size`. The
UIKit content scale is also applied to the renderer at creation, on every
scale change, and immediately after a detached surface is replaced so glass
blur, thickness, refraction, and edge widths remain point-correct. The window
reports `WindowVisibility::Hidden` while the view is detached or the scene is
in the background (`sceneDidEnterBackground` until `sceneWillEnterForeground`,
when the host also pauses its display link).

UIKit and GPUI both enter window/application state only on the main thread.
The process-wide platform, attached view, and active window are therefore
thread-local rather than marked `Send`. `IosDispatcher` sends foreground work
to the GCD main queue, background work to GCD global queues selected by GPUI
priority, delayed work through `dispatch_after_f`, and realtime work to a
named Rust thread. GPUI's `frame_waker` and `schedule_frame` wake the attached
screen's `CADisplayLink`; `gpui_ios_request_frame` returns whether it should
continue ticking. [FramePacer](../frame-pacing/src/lib.rs) owns demand and the
activity grace period. Isolated invalidations, including caret timers and
incoming content, receive one frame and return to pause; touch input and
next-frame animations sustain the source. The host requests the screen's
maximum refresh rate with a permissive minimum, so system power and thermal
policy can clamp it. `gpui_ios_display_changed` reports that screen's maximum
rate to GPUI and screen/scene changes recreate the native display link.

The host is a universal iPhone/iPad app. iPhone supports portrait and both
landscapes; iPad additionally supports upside-down portrait. During rotation,
UIKit forwards the new safe-area and keyboard cover before the new logical
bounds, then `IosWindow` resizes the renderer's drawable and notifies GPUI's
window-bounds observer. Rotation therefore crosses the shared responsive
breakpoint without rebuilding the shell or its state.

`IosTextSystem` uses CoreText for shaping and `zed-font-kit`'s CoreText loader
for metrics and rasterization. The default family is
`.AppleSystemUIFont`; CoreText's cascade supplies PingFang for Chinese and
Apple Color Emoji where needed. CoreText UTF-16 run indices are converted back
to the UTF-8 byte indices expected by GPUI.

Unsafe code and all C/Objective-C-facing state are confined to `src/ios/`.
Swift retains every UIKit object; Rust keeps only a non-owning pointer for the
lifetime of the attached platform window.

## Host boundary

The forward ABI declarations live in
[BridgingHeader.h](../../ios/host/Sources/BridgingHeader.h); the Rust exports
are in [ffi.rs](src/ios/ffi.rs). The Swift host implements the reverse callbacks
in [HostCallbacks.swift](../../ios/host/Sources/HostCallbacks.swift), including
frame scheduling, keyboard presentation, input configuration, clipboard and URL
opening. Keep declarations synchronized when changing that boundary. Callbacks
copy transient byte buffers synchronously; UIKit objects remain Swift-owned.

## Touch and scrolling behavior

The host assigns a monotonically increasing `TouchId` when each `UITouch`
begins and sends all changed contacts in a single C array. Rust emits one raw
`PlatformInput::Touch` per contact. For moves, UIKit's last predicted touch is
included for latency compensation, while actual coordinates remain the source
of hit testing and velocity.

GPUI contains the gesture arena, so this backend intentionally does
not also synthesize mouse events. GPUI selects the primary touch, defers the
mouse-down/up click pair until the tap wins, emits drag scrolling as
`ScrollWheelEvent { delta: ScrollDelta::Pixels(..) }`, advances iOS-style fling
momentum on later frames, and preserves its own long-press deadline. A second
touch can therefore participate in GPUI's multi-touch recognition without
creating a second mouse pointer, and small movements do not break click or
long-press timing. The backend supplies touch events and platform tuning; application components
choose which gestures to handle. The same portable arena serves Android.

## Keyboard bridge

The Swift host keeps a transparent `UIView` conforming to `UITextInput` in the
view hierarchy. When GPUI reports text focus, it becomes first responder. Marked
text is sent through `replace_and_mark_text_in_range`; a confirmed Chinese
candidate is sent through `replace_text_in_range`, which replaces the marked
range and commits the composition. UIKit can make several edits for one
keystroke (Backspace selects the previous character, then deletes it), so the
edits of one run-loop turn are presented together in a single frame. Text assistance and return-key presentation are
updated from `TextInputConfiguration`. Keyboard frame notifications update
`WindowInsets::ime` and shrink the window's visual viewport by the same cover,
while `safeAreaInsets` update `WindowInsets::safe_area`; GPUI refreshes the
window from those callbacks and the display link presents the next frame.
`Window::request_virtual_keyboard` and `dismiss_virtual_keyboard` reach the
same proxy as text focus does, so an application can reopen a dismissed
keyboard from an explicit tap without changing GPUI focus.

## Build and run

From the repository root, with Xcode, XcodeGen and the corresponding Rust target
installed:

```sh
crates/ios/host/build.sh --simulator
crates/ios/host/build.sh --device
```

The script builds `tcode-ios`, copies `libtcode_ios.a` into the ignored host
`lib/` directory, generates `Tcode.xcodeproj`, and builds the Debug `Tcode`
scheme. The simulator destination defaults to iPhone 18 Pro / iOS 27.0; set
`TCODE_IOS_SIMULATOR_OS` for another installed runtime. Set
`TCODE_IOS_RUST_PROFILE=release` for an optimized Rust library; the Xcode host
configuration remains Debug. Device builds are unsigned and need signing and
provisioning before installation.

The simulator app is written to
`crates/ios/host/build/Build/Products/Debug-iphonesimulator/Tcode.app`:

```sh
xcrun simctl install booted crates/ios/host/build/Build/Products/Debug-iphonesimulator/Tcode.app
xcrun simctl launch booted com.tryanks.tcode
```

The layout rule the shell applies on this build is owned by
[`tcode-ui::window_seam`](../../ui/src/window_seam.rs). See
[remote work mode](../../../docs/remote.md) for
pairing with a host.

## Current limitations

- One attached UIKit view and one GPUI window are supported. Native secondary
  windows, dialogs, menus, drag-and-drop, screen capture, and cursor APIs are
  not implemented.
- GPUI credential APIs return an unsupported error. tcode pairing persistence
  belongs to the application host, not this platform backend.
- IME candidate-window caret positioning and continuous interpolation of the
  keyboard animation are not yet implemented; endpoint insets are exact.
- Custom CoreText feature dictionaries and explicit fallback lists are retained
  in font cache keys but are not yet applied to attributed runs.
- Accessibility is limited to GPUI semantics that UIKit can observe indirectly;
  there is no native UIKit accessibility-tree adapter yet.
- The display abstraction currently describes the attached main screen only.

## Attribution

The module boundaries and host/backend handshake were informed by the
ISC-licensed `gpui-toolkit/crates/gpui-ios` reference supplied with this
worktree. This implementation was written for the different `gpui-pre` 0.3.5
traits and renderer APIs; no reference source was vendored or copied into this
crate.

## Scroll target ownership

Both mobile backends use GPUI's private gesture recognizer and scroll dispatch.
See the [shared scroll ownership constraints](../gpui-android/README.md#scroll-target-ownership)
for why nesting is resolved per event by gpui-base's scrollable mask rather
than by element capture. UIKit's actual and predicted coordinates and GPUI's
gesture recognition are preserved unchanged.
