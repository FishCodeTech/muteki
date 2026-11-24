//! Narrow C ABI shared with the UIKit host.
//!
//! UIKit objects remain owned by Swift. Rust stores only a non-owning pointer
//! to the attached `UIView` and consumes value-type snapshots from callbacks.

use super::IosWindow;
use crate::text_input::{self, NOT_FOUND, TextRange};
use anyhow::{Result, anyhow};
use gpui::{
    AppLifecyclePhase, Autocapitalize, Edges, Modifiers, TextInputAction, TextInputConfiguration,
    TouchEvent, TouchId, TouchPhase, WindowAppearance, WindowInsets, point, px,
};
use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    ptr::NonNull,
    slice,
};

/// One UIKit touch serialized for the Rust input pipeline.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct GpuiIosTouch {
    pub identifier: u64,
    pub x: f64,
    pub y: f64,
    pub predicted_x: f64,
    pub predicted_y: f64,
    pub force: f32,
    pub has_prediction: u8,
    pub has_force: u8,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct HostMetrics {
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) scale: f32,
    pub(crate) refresh_rate: f64,
    pub(crate) safe_top: f32,
    pub(crate) safe_right: f32,
    pub(crate) safe_bottom: f32,
    pub(crate) safe_left: f32,
    pub(crate) keyboard_height: f32,
    pub(crate) appearance: WindowAppearance,
    pub(crate) active: bool,
}

impl Default for HostMetrics {
    fn default() -> Self {
        Self {
            width: 0.0,
            height: 0.0,
            scale: 1.0,
            refresh_rate: 0.0,
            safe_top: 0.0,
            safe_right: 0.0,
            safe_bottom: 0.0,
            safe_left: 0.0,
            keyboard_height: 0.0,
            appearance: WindowAppearance::Light,
            active: true,
        }
    }
}

impl HostMetrics {
    pub(crate) fn insets(self) -> WindowInsets {
        WindowInsets {
            safe_area: Edges {
                top: px(self.safe_top),
                right: px(self.safe_right),
                bottom: px(self.safe_bottom),
                left: px(self.safe_left),
            },
            ime: Edges {
                top: px(0.0),
                right: px(0.0),
                bottom: px(self.keyboard_height),
                left: px(0.0),
            },
        }
    }
}

#[derive(Default)]
struct HostState {
    view: Option<NonNull<c_void>>,
    window: Option<NonNull<IosWindow>>,
    metrics: HostMetrics,
}

thread_local! {
    static HOST: RefCell<HostState> = RefCell::new(HostState::default());
    static INITIALIZED: Cell<bool> = const { Cell::new(false) };
}

unsafe extern "C" {
    fn gpui_ios_host_log(level: u32, bytes: *const u8, length: usize);
    fn gpui_ios_host_schedule_frame();
    fn gpui_ios_host_first_frame_rendered();
    fn gpui_ios_host_show_keyboard();
    fn gpui_ios_host_hide_keyboard();
    fn gpui_ios_host_haptic_long_press();
    fn gpui_ios_host_haptic_threshold();
    fn gpui_ios_host_haptic_selection();
    fn gpui_ios_host_text_input_changed();
    fn gpui_ios_host_configure_text_input(
        autocorrect: u8,
        autocapitalize: u32,
        suggestions: u8,
        input_action: u32,
    );
    fn gpui_ios_host_open_url(bytes: *const u8, length: usize);
    fn gpui_ios_host_set_paste_input(owner: u64);
    /// Each colour is sRGB red, green, blue and alpha.
    fn gpui_ios_host_set_paste_control(
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        foreground_red: f64,
        foreground_green: f64,
        foreground_blue: f64,
        foreground_alpha: f64,
        background_red: f64,
        background_green: f64,
        background_blue: f64,
        background_alpha: f64,
        corner_radius: f64,
    );
    fn gpui_ios_host_clipboard_text_length() -> usize;
    fn gpui_ios_host_read_clipboard(bytes: *mut u8, capacity: usize) -> usize;
    fn gpui_ios_host_write_clipboard(bytes: *const u8, length: usize);
}

struct IosLogger;

static IOS_LOGGER: IosLogger = IosLogger;

impl log::Log for IosLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Info
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let message = format!(
            "[{}][{}] {}",
            record.level(),
            record.target(),
            record.args()
        );
        // SAFETY: the Swift executable copies this UTF-8 buffer synchronously.
        unsafe { gpui_ios_host_log(record.level() as u32, message.as_ptr(), message.len()) }
    }

    fn flush(&self) {}
}

fn install_logger() {
    if log::set_logger(&IOS_LOGGER).is_ok() {
        log::set_max_level(log::LevelFilter::Info);
    }
}

pub(crate) fn host_metrics() -> HostMetrics {
    HOST.with(|host| host.borrow().metrics)
}

pub(crate) fn host_view() -> Result<NonNull<c_void>> {
    HOST.with(|host| {
        host.borrow()
            .view
            .ok_or_else(|| anyhow!("gpui_ios_attach_view must run before opening a GPUI window"))
    })
}

pub(crate) fn register_window(window: &IosWindow) {
    HOST.with(|host| {
        host.borrow_mut().window = Some(NonNull::from(window));
    });
}

pub(crate) fn unregister_window(window: &IosWindow) {
    HOST.with(|host| {
        let mut host = host.borrow_mut();
        if host.window == Some(NonNull::from(window)) {
            host.window = None;
        }
    });
}

fn with_window(f: impl FnOnce(&IosWindow)) {
    let window = HOST.with(|host| host.borrow().window);
    if let Some(window) = window {
        // SAFETY: `register_window` stores the address of the boxed platform
        // window, and `Drop` unregisters it before that allocation is freed.
        f(unsafe { window.as_ref() });
    }
}

pub(crate) fn host_first_frame_rendered() {
    // SAFETY: The UIKit host exports this callback and drawing runs on its main thread.
    unsafe { gpui_ios_host_first_frame_rendered() }
}

pub(crate) fn host_schedule_frame() {
    // SAFETY: the Swift executable provides this symbol and accepts no state.
    unsafe { gpui_ios_host_schedule_frame() }
}

pub(crate) fn host_show_keyboard() {
    // SAFETY: UIKit work is performed by Swift on this same main-thread call.
    unsafe { gpui_ios_host_show_keyboard() }
}

pub(crate) fn host_hide_keyboard() {
    // SAFETY: UIKit work is performed by Swift on this same main-thread call.
    unsafe { gpui_ios_host_hide_keyboard() }
}

pub(crate) fn host_haptic_long_press() {
    // SAFETY: UIKit work is performed by Swift on this same main-thread call.
    unsafe { gpui_ios_host_haptic_long_press() }
}

pub(crate) fn host_haptic_threshold() {
    // SAFETY: UIKit work is performed by Swift on this same main-thread call.
    unsafe { gpui_ios_host_haptic_threshold() }
}

pub(crate) fn host_haptic_selection() {
    // SAFETY: UIKit work is performed by Swift on this same main-thread call.
    unsafe { gpui_ios_host_haptic_selection() }
}

pub(crate) fn host_text_input_changed() {
    // SAFETY: Swift schedules notification outside the current GPUI update.
    unsafe { gpui_ios_host_text_input_changed() }
}

pub(crate) fn host_configure_text_input(configuration: &TextInputConfiguration) {
    let autocapitalize = match configuration.autocapitalize {
        Autocapitalize::None => 0,
        Autocapitalize::Words => 1,
        Autocapitalize::Sentences => 2,
        Autocapitalize::Characters => 3,
    };
    let action = match configuration.input_action {
        TextInputAction::Unspecified => 0,
        TextInputAction::Enter => 1,
        TextInputAction::Done => 2,
        TextInputAction::Go => 3,
        TextInputAction::Next => 4,
        TextInputAction::Previous => 5,
        TextInputAction::Search => 6,
        TextInputAction::Send => 7,
    };
    // SAFETY: all arguments are plain C values documented by the bridging header.
    unsafe {
        gpui_ios_host_configure_text_input(
            u8::from(configuration.autocorrect),
            autocapitalize,
            u8::from(configuration.suggestions),
            action,
        );
    }
}

pub(crate) fn host_open_url(url: &str) {
    // SAFETY: the buffer remains alive for the duration of the Swift call.
    unsafe { gpui_ios_host_open_url(url.as_ptr(), url.len()) }
}

pub(crate) fn host_set_paste_input(owner: u64) {
    // SAFETY: UIKit only records this scalar identity.
    unsafe { gpui_ios_host_set_paste_input(owner) };
}

pub(crate) fn host_set_paste_control(control: Option<crate::PasteControl>) {
    let control = control.unwrap_or(crate::PasteControl {
        bounds: gpui::Bounds::default(),
        foreground: gpui::Hsla::default(),
        background: gpui::Hsla::default(),
        corner_radius: gpui::px(0.),
    });
    let bounds = control.bounds;
    let foreground = control.foreground.to_rgb();
    let background = control.background.to_rgb();
    // UIKit only updates a subview's geometry and style; it must not call back into GPUI during paint.
    unsafe {
        gpui_ios_host_set_paste_control(
            bounds.origin.x.into(),
            bounds.origin.y.into(),
            bounds.size.width.into(),
            bounds.size.height.into(),
            foreground.r.into(),
            foreground.g.into(),
            foreground.b.into(),
            foreground.a.into(),
            background.r.into(),
            background.g.into(),
            background.b.into(),
            background.a.into(),
            control.corner_radius.into(),
        )
    };
}

pub(crate) fn host_read_clipboard() -> Option<String> {
    // SAFETY: the first host call returns the byte capacity needed by the second.
    let length = unsafe { gpui_ios_host_clipboard_text_length() };
    if length == 0 {
        return None;
    }
    let mut bytes = vec![0_u8; length];
    // SAFETY: `bytes` is writable for exactly `length` bytes.
    let written = unsafe { gpui_ios_host_read_clipboard(bytes.as_mut_ptr(), bytes.len()) };
    bytes.truncate(written.min(bytes.len()));
    String::from_utf8(bytes).ok()
}

pub(crate) fn host_write_clipboard(text: &str) {
    // SAFETY: the buffer remains alive for the duration of the Swift call.
    unsafe { gpui_ios_host_write_clipboard(text.as_ptr(), text.len()) }
}

fn utf8(bytes: *const u8, length: usize) -> Option<String> {
    if length == 0 {
        return Some(String::new());
    }
    if bytes.is_null() {
        return None;
    }
    // SAFETY: the ABI requires `bytes` to address `length` readable bytes for
    // the duration of this call. The returned value never escapes the call.
    let bytes = unsafe { slice::from_raw_parts(bytes, length) };
    std::str::from_utf8(bytes).ok().map(str::to_owned)
}

fn dispatch_touches(touches: *const GpuiIosTouch, count: usize, phase: TouchPhase) {
    if count == 0 || touches.is_null() {
        return;
    }
    // SAFETY: Swift passes a contiguous temporary array valid until this
    // synchronous function returns.
    let touches = unsafe { slice::from_raw_parts(touches, count) };
    with_window(|window| {
        window.touch_activity(count, phase);
        window.dispatch_touches(touches.iter().map(|touch| {
            let position = point(px(touch.x as f32), px(touch.y as f32));
            window.set_mouse_position(position);
            let predicted_position = (touch.has_prediction != 0)
                .then(|| point(px(touch.predicted_x as f32), px(touch.predicted_y as f32)));
            let force = (touch.has_force != 0).then(|| touch.force.clamp(0.0, 1.0));
            TouchEvent {
                id: TouchId(touch.identifier),
                phase,
                position,
                predicted_position,
                force,
            }
        }));
    });
}

fn modifiers(bits: u32) -> Modifiers {
    Modifiers {
        shift: bits & 1 != 0,
        control: bits & 2 != 0,
        alt: bits & 4 != 0,
        platform: bits & 8 != 0,
        function: bits & 16 != 0,
    }
}

/// Initializes the process-wide platform on the UIKit main thread.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_init() {
    super::assert_main_thread();
    install_logger();
    INITIALIZED.with(|initialized| {
        if !initialized.replace(true) {
            let _ = super::platform();
        }
    });
}

/// Attaches the process-wide platform to a `UIView` backed by `CAMetalLayer`.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_attach_view(
    view: *mut c_void,
    width: f32,
    height: f32,
    scale: f32,
    dark: u8,
) -> u8 {
    super::assert_main_thread();
    let Some(view) = NonNull::new(view) else {
        return 0;
    };
    let (previous_view, previous_metrics) = HOST.with(|host| {
        let mut host = host.borrow_mut();
        let previous = (host.view, host.metrics);
        host.view = Some(view);
        host.metrics.width = width.max(0.0);
        host.metrics.height = height.max(0.0);
        host.metrics.scale = scale.max(1.0);
        host.metrics.appearance = if dark != 0 {
            WindowAppearance::Dark
        } else {
            WindowAppearance::Light
        };
        previous
    });

    let mut replacement_error = None;
    with_window(|window| {
        if window.needs_surface_replacement(view) {
            replacement_error = window
                .replace_surface_from_host(view, width, height, scale)
                .err();
        } else {
            window.resize_from_host(width, height, scale);
        }
    });
    if let Some(error) = replacement_error {
        HOST.with(|host| {
            let mut host = host.borrow_mut();
            host.view = previous_view;
            host.metrics = previous_metrics;
        });
        log::error!("failed to replace the GPUI iOS Metal surface: {error:#}");
        return 0;
    }
    super::with_platform(super::IosPlatform::notify_displays_changed);
    1
}

/// Removes the current non-owning UIView reference.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_detach_view(view: *mut c_void) {
    let view = NonNull::new(view);
    let detached = HOST.with(|host| {
        let mut host = host.borrow_mut();
        if host.view == view {
            host.view = None;
            true
        } else {
            false
        }
    });
    if detached && let Some(view) = view {
        with_window(|window| window.detach_surface_from_host(view));
    }
}

/// Drives one GPUI frame from `CADisplayLink`.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_request_frame() -> u8 {
    let mut keep_running = false;
    with_window(|window| keep_running = window.display_link_tick());
    u8::from(keep_running)
}

/// Updates the attached screen's capability, independently of its current system clamp.
#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_display_changed(refresh_rate: f64) {
    if !refresh_rate.is_finite() || refresh_rate <= 0.0 {
        return;
    }
    let changed = HOST.with(|host| {
        let mut host = host.borrow_mut();
        let changed = host.metrics.refresh_rate != refresh_rate;
        host.metrics.refresh_rate = refresh_rate;
        changed
    });
    if changed {
        with_window(IosWindow::display_changed);
        super::with_platform(super::IosPlatform::notify_displays_changed);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_touches_began(touches: *const GpuiIosTouch, count: usize) {
    dispatch_touches(touches, count, TouchPhase::Started);
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_touches_moved(touches: *const GpuiIosTouch, count: usize) {
    dispatch_touches(touches, count, TouchPhase::Moved);
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_touches_ended(touches: *const GpuiIosTouch, count: usize) {
    dispatch_touches(touches, count, TouchPhase::Ended);
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_touches_cancelled(touches: *const GpuiIosTouch, count: usize) {
    dispatch_touches(touches, count, TouchPhase::Cancelled);
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_resize(width: f32, height: f32, scale: f32) {
    HOST.with(|host| {
        let mut host = host.borrow_mut();
        host.metrics.width = width.max(0.0);
        host.metrics.height = height.max(0.0);
        host.metrics.scale = scale.max(1.0);
    });
    with_window(|window| window.resize_from_host(width, height, scale));
    super::with_platform(super::IosPlatform::notify_displays_changed);
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_scale_factor_changed(scale: f32) {
    let metrics = HOST.with(|host| {
        let mut host = host.borrow_mut();
        host.metrics.scale = scale.max(1.0);
        host.metrics
    });
    with_window(|window| window.resize_from_host(metrics.width, metrics.height, metrics.scale));
    super::with_platform(super::IosPlatform::notify_displays_changed);
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_safe_area_changed(top: f32, right: f32, bottom: f32, left: f32) {
    let insets = HOST.with(|host| {
        let mut host = host.borrow_mut();
        host.metrics.safe_top = top.max(0.0);
        host.metrics.safe_right = right.max(0.0);
        host.metrics.safe_bottom = bottom.max(0.0);
        host.metrics.safe_left = left.max(0.0);
        host.metrics.insets()
    });
    with_window(|window| window.update_insets(insets));
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_keyboard_frame_changed(height: f32) {
    let insets = HOST.with(|host| {
        let mut host = host.borrow_mut();
        host.metrics.keyboard_height = height.max(0.0);
        host.metrics.insets()
    });
    with_window(|window| window.update_insets(insets));
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_appearance_changed(dark: u8) {
    let appearance = if dark != 0 {
        WindowAppearance::Dark
    } else {
        WindowAppearance::Light
    };
    HOST.with(|host| host.borrow_mut().metrics.appearance = appearance);
    with_window(|window| window.update_appearance(appearance));
}

fn lifecycle(phase: AppLifecyclePhase, active: Option<bool>) {
    if let Some(active) = active {
        HOST.with(|host| host.borrow_mut().metrics.active = active);
        with_window(|window| window.update_active(active));
    }
    match phase {
        AppLifecyclePhase::Background => with_window(|window| window.update_background(true)),
        AppLifecyclePhase::Foreground => with_window(|window| window.update_background(false)),
        _ => {}
    }
    super::with_platform(|platform| platform.notify_lifecycle(phase));
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_lifecycle_active() {
    lifecycle(AppLifecyclePhase::Active, Some(true));
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_lifecycle_inactive() {
    lifecycle(AppLifecyclePhase::Inactive, Some(false));
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_lifecycle_background() {
    lifecycle(AppLifecyclePhase::Background, Some(false));
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_lifecycle_foreground() {
    lifecycle(AppLifecyclePhase::Foreground, None);
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_memory_warning() {
    super::with_platform(super::IosPlatform::notify_memory_warning);
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_text_range(kind: u32) -> TextRange {
    let mut result = None;
    with_window(|window| {
        if !window.text_input_active() {
            return;
        }
        result = window
            .with_input_handler(|handler| match kind {
                0 => handler
                    .selected_text_range(true)
                    .map(|selection| selection.range),
                1 => handler.marked_text_range(),
                _ => text_input::text_length(handler).map(|length| 0..length),
            })
            .flatten();
    });
    result.into()
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_read_text(range: TextRange, bytes: *mut u8, capacity: usize) -> usize {
    let Some(range) = range.range() else {
        return NOT_FOUND;
    };
    let mut text = None;
    with_window(|window| {
        text = window
            .with_input_handler(|handler| text_input::read_text(handler, range))
            .flatten();
    });
    let Some(text) = text else {
        return NOT_FOUND;
    };
    if !bytes.is_null() && capacity >= text.len() {
        // SAFETY: Swift supplies a writable buffer of `capacity` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(text.as_ptr(), bytes, text.len());
        }
    }
    text.len()
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_replace_text(
    range: TextRange,
    bytes: *const u8,
    length: usize,
    marked: u8,
    selection: TextRange,
) {
    if let Some(text) = utf8(bytes, length) {
        with_window(|window| {
            window.input_activity();
            let replace = |handler: &mut gpui::PlatformInputHandler| {
                if marked != 0 {
                    handler.replace_and_mark_text_in_range(range.range(), &text, selection.range());
                } else {
                    handler.replace_text_in_range(range.range(), &text);
                }
            };
            if marked != 0 {
                window.with_input_handler(replace);
            } else {
                window.with_stored_input_handler(replace);
            }
        });
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_select_text(range: TextRange) {
    let Some(range) = range.range() else {
        return;
    };
    with_window(|window| {
        window.input_activity();
        window.with_input_handler(|handler| text_input::select_text(handler, range));
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_unmark_text() {
    with_window(|window| {
        window.input_activity();
        window.with_stored_input_handler(text_input::commit_marked_text);
    });
}

#[derive(Default)]
#[repr(C)]
pub struct TextRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_text_rect(range: TextRange) -> TextRect {
    let mut rect = TextRect::default();
    if let Some(range) = range.range() {
        with_window(|window| {
            if let Some(bounds) = window
                .with_input_handler(|handler| handler.bounds_for_range(range))
                .flatten()
            {
                rect = TextRect {
                    x: f32::from(bounds.origin.x) as f64,
                    y: f32::from(bounds.origin.y) as f64,
                    width: f32::from(bounds.size.width) as f64,
                    height: f32::from(bounds.size.height) as f64,
                };
            }
        });
    }
    rect
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_character_at_point(x: f64, y: f64) -> usize {
    let mut index = None;
    with_window(|window| {
        index = window
            .with_input_handler(|handler| {
                handler.character_index_for_point(point(px(x as f32), px(y as f32)))
            })
            .flatten();
    });
    index.unwrap_or(NOT_FOUND)
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_move_caret_toward(x: f64, y: f64) {
    with_window(|window| {
        window.input_activity();
        window.with_input_handler(|handler| {
            let Some(selection) = handler.selected_text_range(true) else {
                return;
            };
            let head = if selection.reversed {
                selection.range.start
            } else {
                selection.range.end
            };
            let target = point(px(x as f32), px(y as f32));
            if let Some(offset) = text_input::caret_toward(handler, head, target) {
                text_input::select_text(handler, offset..offset);
            }
        });
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_delete_backward() {
    with_window(IosWindow::delete_backward);
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_key_event(
    key_bytes: *const u8,
    key_length: usize,
    character_bytes: *const u8,
    character_length: usize,
    modifier_bits: u32,
    down: u8,
    repeat: u8,
) -> u8 {
    let Some(key) = utf8(key_bytes, key_length) else {
        return 0;
    };
    let character = (character_length > 0)
        .then(|| utf8(character_bytes, character_length))
        .flatten();
    let mut handled = false;
    with_window(|window| {
        handled = window.key_event(
            key,
            character,
            modifiers(modifier_bits),
            down != 0,
            repeat != 0,
        );
    });
    u8::from(handled)
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_open_url_received(bytes: *const u8, length: usize) {
    let Some(url) = utf8(bytes, length) else {
        return;
    };
    super::with_platform(|platform| platform.notify_open_urls(vec![url]));
}

#[unsafe(no_mangle)]
pub extern "C" fn gpui_ios_paste_text(bytes: *const u8, length: usize) {
    if let Some(text) = utf8(bytes, length) {
        with_window(|window| {
            window
                .with_input_handler(|handler| handler.paste(gpui::ClipboardItem::new_string(text)));
            window.schedule_forced_frame();
        });
    }
}
