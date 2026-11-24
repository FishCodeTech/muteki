use crate::text_input::TextInputState;
use android_activity::AndroidApp;
use gpui::TextInputConfiguration;
use jni::{
    JavaVM, jni_sig, jni_str,
    objects::{JObject, JString, JValue},
    signature::MethodSignature,
    strings::JNIStr,
};
use parking_lot::Mutex;
use std::collections::VecDeque;

#[derive(Debug)]
pub(crate) enum HostEvent {
    Frame,
    RefreshRate(f32),
    CommitText(String),
    SetComposingText(String),
    FinishComposing,
    DeleteBackward,
    InputState {
        revision: u64,
        serial: u64,
        state: TextInputState,
    },
    Key {
        key_code: i32,
        down: bool,
        unicode_code_point: i32,
        meta_state: i32,
    },
    Insets {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        ime_bottom: i32,
    },
    Back,
    ScrollCapture(ScrollCaptureRequest),
}

/// One step of the system screenshot tool's scrolling capture, in physical
/// window pixels. The activity drives the sequence: one `Search`, then for a
/// chosen target `Start`, any number of `Image` requests and `End`. `Search`
/// and `Image` each wait for their reply from the UI thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollCaptureRequest {
    /// Which rectangle of the window scrolls; answered by
    /// [`scroll_capture_bounds`].
    Search {
        request: u64,
    },
    Start,
    /// The tile starting `top` pixels below the rectangle's top at `Start`;
    /// answered by [`scroll_capture_rendered`] once a frame showing it has
    /// been presented.
    Image {
        request: u64,
        top: i32,
    },
    End,
}

pub(super) static APP: Mutex<Option<AndroidApp>> = Mutex::new(None);
static EVENTS: Mutex<VecDeque<HostEvent>> = Mutex::new(VecDeque::new());

pub(crate) fn initialize(app: &AndroidApp) {
    *APP.lock() = Some(app.clone());
}

fn enqueue(event: HostEvent) {
    EVENTS.lock().push_back(event);
    if let Some(app) = APP.lock().as_ref() {
        app.create_waker().wake();
    }
}

pub(crate) fn drain() -> Vec<HostEvent> {
    EVENTS.lock().drain(..).collect()
}

pub fn frame() {
    enqueue(HostEvent::Frame);
}

pub fn refresh_rate(rate: f32) {
    if rate.is_finite() && rate > 0.0 {
        enqueue(HostEvent::RefreshRate(rate));
    }
}

pub(crate) fn request_frame() {
    with_activity(jni_str!("gpuiRequestFrame"), jni_sig!(() -> void), vec![]);
}

pub(crate) fn pause_frames() {
    with_activity(jni_str!("gpuiPauseFrames"), jni_sig!(() -> void), vec![]);
}

enum OwnedArgument {
    Bool(bool),
    Int(i32),
    Long(u64),
    Text(Option<String>),
}

impl OwnedArgument {
    fn as_jvalue<'a>(&self, text: &'a JObject<'a>) -> JValue<'a> {
        match self {
            Self::Bool(value) => JValue::Bool(*value),
            Self::Int(value) => JValue::Int(*value),
            Self::Long(value) => JValue::Long(*value as i64),
            Self::Text(_) => JValue::Object(text),
        }
    }
}

fn with_activity(
    method: &'static JNIStr,
    signature: MethodSignature<'static, 'static>,
    args: Vec<OwnedArgument>,
) {
    let Some(app) = APP.lock().clone() else {
        return;
    };
    let callback_app = app.clone();
    app.run_on_java_main_thread(Box::new(move || {
        // SAFETY: Android owns this VM for the duration of the process.
        let vm = unsafe { JavaVM::from_raw(callback_app.vm_as_ptr().cast()) };
        // The UI thread is already attached, so this only pushes a local frame.
        let result = vm.attach_current_thread(|env| -> jni::errors::Result<()> {
            // SAFETY: `activity_as_ptr` is the live NativeActivity instance.
            let activity = unsafe { JObject::from_raw(env, callback_app.activity_as_ptr().cast()) };
            let strings = args
                .iter()
                .map(|arg| match arg {
                    OwnedArgument::Text(Some(text)) => env.new_string(text).map(JObject::from),
                    _ => Ok(JObject::null()),
                })
                .collect::<jni::errors::Result<Vec<_>>>()?;
            let args = args
                .iter()
                .zip(&strings)
                .map(|(arg, text)| arg.as_jvalue(text))
                .collect::<Vec<_>>();
            env.call_method(&activity, method, &signature, &args)
                .inspect_err(|_| env.exception_clear())?;
            Ok(())
        });
        if let Err(error) = result {
            log::error!("JNI {method} failed: {error}");
        }
    }));
}

pub(crate) fn show_keyboard() {
    with_activity(jni_str!("gpuiShowKeyboard"), jni_sig!("()V"), Vec::new());
}

pub(crate) fn hide_keyboard() {
    with_activity(jni_str!("gpuiHideKeyboard"), jni_sig!("()V"), Vec::new());
}

pub fn haptic_long_press() {
    with_activity(jni_str!("gpuiHapticLongPress"), jni_sig!("()V"), Vec::new());
}

pub fn haptic_threshold() {
    with_activity(jni_str!("gpuiHapticThreshold"), jni_sig!("()V"), Vec::new());
}

pub fn haptic_selection() {
    with_activity(jni_str!("gpuiHapticSelection"), jni_sig!("()V"), Vec::new());
}

pub(crate) fn configure_input(configuration: TextInputConfiguration) {
    // GPUI has no separate multiline flag; Enter explicitly requests a line break.
    with_activity(
        jni_str!("gpuiConfigureInput"),
        jni_sig!("(ZIZIZ)V"),
        vec![
            OwnedArgument::Bool(configuration.autocorrect),
            OwnedArgument::Int(configuration.autocapitalize as i32),
            OwnedArgument::Bool(configuration.suggestions),
            OwnedArgument::Int(configuration.input_action as i32),
            OwnedArgument::Bool(configuration.input_action == gpui::TextInputAction::Enter),
        ],
    );
}

pub(crate) fn finish_activity() {
    with_activity(jni_str!("gpuiFinish"), jni_sig!("()V"), Vec::new());
}

pub(crate) fn open_url(url: &str) {
    with_activity(
        jni_str!("gpuiOpenUrl"),
        jni_sig!("(Ljava/lang/String;)V"),
        vec![OwnedArgument::Text(Some(url.to_owned()))],
    );
}

pub(crate) fn read_clipboard() -> Option<String> {
    let app = APP.lock().clone()?;
    // SAFETY: Android owns this VM for the duration of the process.
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
    vm.attach_current_thread(|env| {
        // SAFETY: `activity_as_ptr` is the live NativeActivity instance.
        let activity = unsafe { JObject::from_raw(env, app.activity_as_ptr().cast()) };
        let object = env
            .call_method(
                &activity,
                jni_str!("gpuiReadClipboard"),
                jni_sig!("()Ljava/lang/String;"),
                &[],
            )
            .inspect_err(|error| {
                log::error!("JNI gpuiReadClipboard failed: {error}");
                env.exception_clear();
            })?
            .l()?;
        if object.is_null() {
            return Ok(None);
        }
        JString::cast_local(env, object)?
            .try_to_string(env)
            .map(Some)
    })
    .ok()
    .flatten()
}

pub(crate) fn write_clipboard(text: String) {
    let Some(app) = APP.lock().clone() else {
        return;
    };
    let callback_app = app.clone();
    app.run_on_java_main_thread(Box::new(move || {
        // SAFETY: Android owns this VM for the duration of the process.
        let vm = unsafe { JavaVM::from_raw(callback_app.vm_as_ptr().cast()) };
        let result = vm.attach_current_thread(|env| -> jni::errors::Result<()> {
            // SAFETY: `activity_as_ptr` is the live NativeActivity instance.
            let activity = unsafe { JObject::from_raw(env, callback_app.activity_as_ptr().cast()) };
            let text = env.new_string(text)?;
            env.call_method(
                &activity,
                jni_str!("gpuiWriteClipboard"),
                jni_sig!("(Ljava/lang/String;)V"),
                &[JValue::Object(text.as_ref())],
            )
            .inspect_err(|_| env.exception_clear())?;
            Ok(())
        });
        if let Err(error) = result {
            log::error!("JNI gpuiWriteClipboard failed: {error}");
        }
    }));
}

pub fn commit_text(text: String) {
    enqueue(HostEvent::CommitText(text));
}

pub fn set_composing_text(text: String) {
    enqueue(HostEvent::SetComposingText(text));
}

pub fn finish_composing_text() {
    enqueue(HostEvent::FinishComposing);
}

pub fn delete_backward() {
    enqueue(HostEvent::DeleteBackward);
}

pub fn input_state(revision: u64, serial: u64, state: TextInputState) {
    enqueue(HostEvent::InputState {
        revision,
        serial,
        state,
    });
}

pub(crate) fn sync_input(revision: u64, serial: u64, state: Option<TextInputState>) {
    let selection = state.as_ref().map_or(0..0, |state| state.selection.clone());
    let marked = state.as_ref().and_then(|state| state.marked.clone());
    with_activity(
        jni_str!("gpuiSyncInput"),
        jni_sig!("(JJLjava/lang/String;IIII)V"),
        vec![
            OwnedArgument::Long(revision),
            OwnedArgument::Long(serial),
            OwnedArgument::Text(state.map(|state| state.text)),
            OwnedArgument::Int(selection.start as i32),
            OwnedArgument::Int(selection.end as i32),
            OwnedArgument::Int(marked.as_ref().map_or(-1, |range| range.start as i32)),
            OwnedArgument::Int(marked.as_ref().map_or(-1, |range| range.end as i32)),
        ],
    );
}

pub fn key_event(key_code: i32, down: bool, unicode_code_point: i32, meta_state: i32) {
    enqueue(HostEvent::Key {
        key_code,
        down,
        unicode_code_point,
        meta_state,
    });
}

pub fn on_insets(left: i32, top: i32, right: i32, bottom: i32, ime_bottom: i32) {
    enqueue(HostEvent::Insets {
        left,
        top,
        right,
        bottom,
        ime_bottom,
    });
}

pub fn on_back() {
    enqueue(HostEvent::Back);
}

pub fn scroll_capture(request: ScrollCaptureRequest) {
    enqueue(HostEvent::ScrollCapture(request));
}

/// Answer a [`ScrollCaptureRequest::Search`]: the scrolling rectangle in
/// physical window pixels, or `None` to decline the capture.
pub fn scroll_capture_bounds(request: u64, bounds: Option<[i32; 4]>) {
    let [left, top, right, bottom] = bounds.unwrap_or_default();
    with_activity(
        jni_str!("gpuiScrollCaptureBounds"),
        jni_sig!("(JIIII)V"),
        vec![
            OwnedArgument::Long(request),
            OwnedArgument::Int(left),
            OwnedArgument::Int(top),
            OwnedArgument::Int(right),
            OwnedArgument::Int(bottom),
        ],
    );
}

/// Answer a [`ScrollCaptureRequest::Image`]: the presented frame shows the
/// content starting `scrolled` physical pixels below the rectangle's top at
/// `Start`, or `None` when the timeline is no longer being captured.
pub fn scroll_capture_rendered(request: u64, scrolled: Option<i32>) {
    with_activity(
        jni_str!("gpuiScrollCaptureRendered"),
        jni_sig!("(JZI)V"),
        vec![
            OwnedArgument::Long(request),
            OwnedArgument::Bool(scrolled.is_some()),
            OwnedArgument::Int(scrolled.unwrap_or_default()),
        ],
    );
}

/// Rasterize on the calling render thread; a software Canvas needs no UI-thread hop.
pub(crate) fn rasterize_emoji(glyph: u32, size: f32) -> anyhow::Result<Option<Vec<i32>>> {
    let app = APP
        .lock()
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Android host not initialized"))?;
    // SAFETY: Android owns the VM and live activity for the lifetime of this app handle.
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
    vm.attach_current_thread(|env| -> anyhow::Result<Option<Vec<i32>>> {
        // SAFETY: the app handle keeps this NativeActivity alive.
        let activity = unsafe { JObject::from_raw(env, app.activity_as_ptr().cast()) };
        let result = (|| -> anyhow::Result<Option<Vec<i32>>> {
            let object = env
                .call_method(
                    &activity,
                    jni_str!("gpuiRasterizeEmoji"),
                    jni_sig!("(IF)[I"),
                    &[JValue::Int(glyph.try_into()?), JValue::Float(size)],
                )?
                .l()?;
            if object.is_null() {
                return Ok(None);
            }
            let array = env.cast_local::<jni::objects::JIntArray>(object)?;
            let mut pixels = vec![0; array.len(env)?];
            array.get_region(env, 0, &mut pixels)?;
            Ok(Some(pixels))
        })();
        if result.is_err() {
            env.exception_clear();
        }
        result
    })
}
