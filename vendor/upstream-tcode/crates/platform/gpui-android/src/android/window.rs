use super::{display::AndroidDisplay, host};
use android_activity::input::{KeyAction, Keycode, MotionAction};
use gpui::{
    Bounds, Capslock, DevicePixels, DispatchEventResult, Edges, FrameRequestSource, GpuSpecs,
    KeyUpEvent, Keystroke, Modifiers, Pixels, PlatformAtlas, PlatformDisplay, PlatformInput,
    PlatformInputHandler, PlatformWindow, Point, PromptButton, PromptLevel, RequestFrameOptions,
    Scene, Size, TextInputConfiguration, TextInputStateChange, TouchEvent, TouchId, TouchPhase,
    WindowAppearance, WindowBackgroundAppearance, WindowBounds, WindowInsets, WindowVisibility,
    point, px, size,
};
use gpui_mobile_pacing::FramePacer;
use gpui_mobile_touch::TwoFingerGestures;
use gpui_wgpu::{GpuContext, WgpuRenderer, WgpuSurfaceConfig};
use ndk::native_window::NativeWindow;
use raw_window_handle::{
    AndroidDisplayHandle, DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle,
    RawDisplayHandle, WindowHandle,
};
use std::{
    cell::{Cell, RefCell},
    fmt,
    rc::Rc,
};

#[derive(Clone)]
pub(crate) struct NativeSurface {
    window: NativeWindow,
}

impl NativeSurface {
    fn new(window: NativeWindow) -> Self {
        Self { window }
    }

    fn physical_size(&self) -> Size<DevicePixels> {
        size(
            DevicePixels(self.window.width().max(1)),
            DevicePixels(self.window.height().max(1)),
        )
    }
}

impl fmt::Debug for NativeSurface {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeSurface")
            .field("width", &self.window.width())
            .field("height", &self.window.height())
            .finish()
    }
}

impl HasWindowHandle for NativeSurface {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        self.window.window_handle()
    }
}

impl HasDisplayHandle for NativeSurface {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        let raw = RawDisplayHandle::Android(AndroidDisplayHandle::new());
        // SAFETY: Android's display handle contains no borrowed pointers.
        Ok(unsafe { DisplayHandle::borrow_raw(raw) })
    }
}

#[derive(Default)]
struct Callbacks {
    request_frame: Option<Box<dyn FnMut(RequestFrameOptions)>>,
    input: Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>,
    active: Option<Box<dyn FnMut(bool)>>,
    hover: Option<Box<dyn FnMut(bool)>>,
    resize: Option<ResizeCallback>,
    moved: Option<Box<dyn FnMut()>>,
    display: Option<Box<dyn FnMut()>>,
    should_close: Option<Box<dyn FnMut() -> bool>>,
    hit_test: Option<Box<dyn FnMut() -> Option<gpui::WindowControlArea>>>,
    close: Option<Box<dyn FnOnce()>>,
    appearance: Option<Box<dyn FnMut()>>,
    insets: Option<Box<dyn FnMut(WindowInsets)>>,
    visual_viewport: Option<Box<dyn FnMut()>>,
    visibility: Option<Box<dyn FnMut(WindowVisibility)>>,
    back: Option<Box<dyn FnMut()>>,
}

type ResizeCallback = Box<dyn FnMut(Size<Pixels>, f32)>;

struct WindowState {
    native_surface: Option<NativeSurface>,
    renderer: WgpuRenderer,
    bounds: Bounds<Pixels>,
    scale_factor: f32,
    appearance: WindowAppearance,
    background: WindowBackgroundAppearance,
    active: bool,
    mouse_position: Point<Pixels>,
    input_handler: Option<PlatformInputHandler>,
    input_configuration: TextInputConfiguration,
    title: String,
    insets: WindowInsets,
    back_enabled: bool,
}

pub(crate) struct AndroidWindowInner {
    display: Rc<AndroidDisplay>,
    gpu_context: GpuContext,
    state: RefCell<WindowState>,
    callbacks: RefCell<Callbacks>,
    pacing: RefCell<FramePacer>,
    forced_frame_requested: Cell<bool>,
    frames_suspended: Cell<bool>,
    /// Run once the next frame has been presented.
    after_frame: RefCell<Vec<Box<dyn FnOnce()>>>,
    reported_visibility: Cell<Option<WindowVisibility>>,
    keyboard_tap: Cell<Option<Point<Pixels>>>,
    touch_gestures: RefCell<TwoFingerGestures>,
    input_sync: RefCell<crate::text_input::InputSync>,
}

#[derive(Clone)]
pub(crate) struct AndroidWindow(pub(crate) Rc<AndroidWindowInner>);

impl AndroidWindow {
    pub(crate) fn new(
        display: Rc<AndroidDisplay>,
        gpu_context: GpuContext,
        scale_factor: f32,
        appearance: WindowAppearance,
        background: WindowBackgroundAppearance,
        native_window: NativeWindow,
    ) -> anyhow::Result<Self> {
        let surface = NativeSurface::new(native_window);
        let physical_size = surface.physical_size();
        let logical_size = logical_size(physical_size, scale_factor);
        let bounds = Bounds::new(point(px(0.0), px(0.0)), logical_size);
        display.set_bounds(bounds);
        crate::FIRST_FRAME_RENDERED.store(false, std::sync::atomic::Ordering::Release);
        let renderer = WgpuRenderer::new(
            gpu_context.clone(),
            &surface,
            WgpuSurfaceConfig {
                size: physical_size,
                transparent: background != WindowBackgroundAppearance::Opaque,
                preferred_present_mode: Some(gpui_wgpu::wgpu::PresentMode::Fifo),
            },
            None,
        )?;

        Ok(Self(Rc::new(AndroidWindowInner {
            display,
            gpu_context,
            state: RefCell::new(WindowState {
                native_surface: Some(surface),
                renderer,
                bounds,
                scale_factor,
                appearance,
                background,
                active: true,
                mouse_position: point(px(0.0), px(0.0)),
                input_handler: None,
                input_configuration: TextInputConfiguration::default(),
                title: String::new(),
                insets: WindowInsets::default(),
                back_enabled: false,
            }),
            callbacks: RefCell::new(Callbacks::default()),
            pacing: RefCell::new(FramePacer::default()),
            forced_frame_requested: Cell::new(true),
            frames_suspended: Cell::new(false),
            after_frame: RefCell::new(Vec::new()),
            reported_visibility: Cell::new(None),
            keyboard_tap: Cell::new(None),
            touch_gestures: RefCell::default(),
            input_sync: RefCell::new(Default::default()),
        })))
    }

    pub(crate) fn set_surface(&self, native_window: Option<NativeWindow>) {
        let mut state = self.0.state.borrow_mut();
        match native_window {
            Some(native_window) => {
                let surface = NativeSurface::new(native_window);
                let physical_size = surface.physical_size();
                let config = WgpuSurfaceConfig {
                    size: physical_size,
                    transparent: state.background != WindowBackgroundAppearance::Opaque,
                    preferred_present_mode: Some(gpui_wgpu::wgpu::PresentMode::Fifo),
                };
                let instance = self
                    .0
                    .gpu_context
                    .borrow()
                    .as_ref()
                    .expect("GPU context initialized with first Android surface")
                    .instance
                    .clone();
                if let Err(error) = state.renderer.replace_surface(&surface, config, &instance) {
                    log::error!("failed to recreate Android Vulkan surface: {error:#}");
                    return;
                }
                state.native_surface = Some(surface);
                drop(state);
                self.resize_from_native_window();
                self.schedule_frame();
                self.0.forced_frame_requested.set(true);
            }
            None => {
                self.0.pacing.borrow_mut().suspend();
                host::pause_frames();
                state.renderer.unconfigure_surface();
                state.native_surface = None;
                drop(state);
            }
        }
        self.report_visibility();
    }

    /// The activity's surface exists exactly while it is on screen, so it is
    /// the window's visibility.
    fn current_visibility(&self) -> WindowVisibility {
        if self.0.state.borrow().native_surface.is_some() {
            WindowVisibility::Visible
        } else {
            WindowVisibility::Hidden
        }
    }

    /// Reports a visibility transition to GPUI once `on_visibility_change`
    /// has registered its callback; only changes since the last report fire.
    fn report_visibility(&self) {
        let Some(reported) = self.0.reported_visibility.get() else {
            return;
        };
        let visibility = self.current_visibility();
        if reported == visibility {
            return;
        }
        self.0.reported_visibility.set(Some(visibility));
        let callback = self.0.callbacks.borrow_mut().visibility.take();
        if let Some(mut callback) = callback {
            callback(visibility);
            self.0.callbacks.borrow_mut().visibility = Some(callback);
        }
    }

    pub(crate) fn resize_from_native_window(&self) {
        let (logical, scale) = {
            let mut state = self.0.state.borrow_mut();
            let Some(surface) = state.native_surface.as_ref() else {
                return;
            };
            let physical = surface.physical_size();
            state.renderer.update_drawable_size(physical);
            let logical = logical_size(physical, state.scale_factor);
            state.bounds = Bounds::new(point(px(0.0), px(0.0)), logical);
            self.0.display.set_bounds(state.bounds);
            (logical, state.scale_factor)
        };
        self.invoke_resize(logical, scale);
        self.schedule_frame();
    }

    pub(crate) fn set_scale_factor(&self, scale_factor: f32) {
        let changed = {
            let mut state = self.0.state.borrow_mut();
            if (state.scale_factor - scale_factor).abs() < f32::EPSILON {
                false
            } else {
                state.scale_factor = scale_factor;
                true
            }
        };
        if changed {
            self.resize_from_native_window();
        }
    }

    pub(crate) fn set_appearance(&self, appearance: WindowAppearance) {
        let changed = {
            let mut state = self.0.state.borrow_mut();
            if state.appearance == appearance {
                false
            } else {
                state.appearance = appearance;
                true
            }
        };
        if changed {
            let callback = self.0.callbacks.borrow_mut().appearance.take();
            if let Some(mut callback) = callback {
                callback();
                self.0.callbacks.borrow_mut().appearance = Some(callback);
            }
            self.schedule_frame();
        }
    }

    pub(crate) fn set_active(&self, active: bool) {
        let changed = {
            let mut state = self.0.state.borrow_mut();
            let changed = state.active != active;
            state.active = active;
            changed
        };
        if changed {
            let callback = self.0.callbacks.borrow_mut().active.take();
            if let Some(mut callback) = callback {
                callback(active);
                self.0.callbacks.borrow_mut().active = Some(callback);
            }
        }
    }

    pub(crate) fn update_insets(
        &self,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        ime_bottom: i32,
    ) {
        let scale = self.scale_factor();
        let insets = WindowInsets {
            safe_area: Edges {
                top: px(top as f32 / scale),
                right: px(right as f32 / scale),
                bottom: px(bottom as f32 / scale),
                left: px(left as f32 / scale),
            },
            ime: Edges {
                top: px(0.0),
                right: px(0.0),
                bottom: px(ime_bottom as f32 / scale),
                left: px(0.0),
            },
        };
        let previous = std::mem::replace(&mut self.0.state.borrow_mut().insets, insets.clone());
        if previous == insets {
            return;
        }
        // GPUI refreshes the window from these callbacks and wakes the frame
        // source itself, so no frame is scheduled here.
        let callback = self.0.callbacks.borrow_mut().insets.take();
        if let Some(mut callback) = callback {
            callback(insets.clone());
            self.0.callbacks.borrow_mut().insets = Some(callback);
        }
        if previous.ime == insets.ime {
            return;
        }
        let callback = self.0.callbacks.borrow_mut().visual_viewport.take();
        if let Some(mut callback) = callback {
            callback();
            self.0.callbacks.borrow_mut().visual_viewport = Some(callback);
        }
    }

    pub(crate) fn handle_back(&self) -> bool {
        if !self.0.state.borrow().back_enabled {
            return false;
        }
        let callback = self.0.callbacks.borrow_mut().back.take();
        if let Some(mut callback) = callback {
            callback();
            self.0.callbacks.borrow_mut().back = Some(callback);
            true
        } else {
            false
        }
    }

    pub(crate) fn handle_host_event(&self, event: host::HostEvent) {
        if matches!(
            &event,
            host::HostEvent::InputState { .. }
                | host::HostEvent::CommitText(_)
                | host::HostEvent::SetComposingText(_)
                | host::HostEvent::FinishComposing
                | host::HostEvent::DeleteBackward
                | host::HostEvent::Key { .. }
        ) {
            self.0.pacing.borrow_mut().input(std::time::Instant::now());
            host::request_frame();
        }
        match event {
            host::HostEvent::InputState {
                revision,
                serial,
                state,
            } => {
                self.sync_input_state(false);
                let old = {
                    let mut sync = self.0.input_sync.borrow_mut();
                    if sync.accept(revision, serial) {
                        sync.state.clone()
                    } else {
                        None
                    }
                };
                if let Some(old) = old {
                    self.with_input_handler(|handler| state.apply(&old, handler));
                    self.0.input_sync.borrow_mut().state = Some(state);
                }
                self.sync_input_state(true);
            }
            host::HostEvent::CommitText(text) => {
                self.with_input_handler(|handler| {
                    handler.replace_text_in_range(None, &text);
                    handler.unmark_text();
                });
            }
            host::HostEvent::SetComposingText(text) => {
                self.with_input_handler(|handler| {
                    handler.replace_and_mark_text_in_range(None, &text, None);
                });
            }
            host::HostEvent::FinishComposing => {
                self.with_input_handler(PlatformInputHandler::unmark_text);
            }
            host::HostEvent::DeleteBackward => {
                self.delete_backward();
            }
            host::HostEvent::Key {
                key_code,
                down,
                unicode_code_point,
                meta_state,
            } => self.handle_host_key(key_code, down, unicode_code_point, meta_state),
            host::HostEvent::Insets {
                left,
                top,
                right,
                bottom,
                ime_bottom,
            } => self.update_insets(left, top, right, bottom, ime_bottom),
            // The platform answers these itself; a window handler is never asked.
            host::HostEvent::Back
            | host::HostEvent::ScrollCapture(_)
            | host::HostEvent::Frame
            | host::HostEvent::RefreshRate(_) => {}
        }
    }

    fn delete_backward(&self) {
        let mut handled = false;
        self.with_input_handler(|handler| {
            handled = crate::text_input::delete_backward(handler);
        });
        // Terminal handlers accept committed text but expose no editable buffer.
        // Release the handler borrow before dispatching to the focused view.
        if !handled {
            let keystroke = Keystroke {
                key: "backspace".into(),
                key_char: None,
                modifiers: Modifiers::default(),
            };
            self.dispatch_input(PlatformInput::KeyDown(crate::text_input::ime_key_down(
                keystroke.clone(),
                false,
            )));
            self.dispatch_input(PlatformInput::KeyUp(KeyUpEvent { keystroke }));
        }
    }

    fn with_input_handler(&self, callback: impl FnOnce(&mut PlatformInputHandler)) {
        let handler = self.0.state.borrow_mut().input_handler.take();
        if let Some(mut handler) = handler {
            callback(&mut handler);
            self.0.state.borrow_mut().input_handler = Some(handler);
            self.schedule_frame();
        }
    }

    // Query outside GPUI's draw callback, when its window can be borrowed again.
    fn sync_input_state(&self, force: bool) {
        let mut handler = self.0.state.borrow_mut().input_handler.take();
        let state = handler
            .as_mut()
            .and_then(crate::text_input::TextInputState::read);
        self.0.state.borrow_mut().input_handler = handler;
        let mut sync = self.0.input_sync.borrow_mut();
        if sync.observe(state) || force {
            host::sync_input(sync.revision, sync.serial, sync.state.clone());
        }
    }

    pub(crate) fn handle_motion(&self, event: &android_activity::input::MotionEvent<'_>) -> bool {
        let action = event.action();
        if event.pointer_count() == 0 {
            return false;
        }
        if !matches!(
            action,
            MotionAction::Down
                | MotionAction::PointerDown
                | MotionAction::Move
                | MotionAction::PointerUp
                | MotionAction::Up
                | MotionAction::Cancel
        ) {
            return false;
        }

        self.0.pacing.borrow_mut().set_touching(
            !matches!(action, MotionAction::Up | MotionAction::Cancel),
            std::time::Instant::now(),
        );
        host::request_frame();

        let action_pointer = event
            .pointer_index()
            .min(event.pointer_count().saturating_sub(1));
        let scale = self.scale_factor();
        let down_time = u64::try_from(event.down_time()).unwrap_or_default();
        // ACTION_DOWN begins every gesture, so no finger of an earlier one is
        // still down even if its release never arrived.
        if action == MotionAction::Down {
            self.0.touch_gestures.take();
        }

        let mut touches = Vec::with_capacity(event.pointer_count());
        for pointer in event.pointers() {
            let phase = match action {
                MotionAction::Down | MotionAction::PointerDown
                    if pointer.pointer_index() == action_pointer =>
                {
                    TouchPhase::Started
                }
                MotionAction::Up | MotionAction::PointerUp
                    if pointer.pointer_index() == action_pointer =>
                {
                    TouchPhase::Ended
                }
                MotionAction::Cancel => TouchPhase::Cancelled,
                _ => TouchPhase::Moved,
            };
            let position = point(px(pointer.x() / scale), px(pointer.y() / scale));
            self.0.state.borrow_mut().mouse_position = position;

            if phase == TouchPhase::Started {
                // Resolve against the handler installed by this tap's frame,
                // so switching between two inputs works as well as retapping one.
                self.0.keyboard_tap.set(Some(position));
                self.schedule_frame();
            }

            // Android pointer ids are stable only within one gesture. Pairing
            // each id with ACTION_DOWN's monotonic timestamp gives GPUI the
            // same never-reused touch identity that the UIKit bridge assigns.
            let pointer_id = u64::try_from(pointer.pointer_id()).unwrap_or_default() & 0x3f;
            let id = TouchId(down_time.wrapping_mul(64).wrapping_add(pointer_id));
            let pressure = pointer.pressure();
            touches.push(TouchEvent {
                id,
                phase,
                position,
                predicted_position: None,
                force: pressure.is_finite().then(|| pressure.clamp(0.0, 1.0)),
            });
        }
        let inputs = self.0.touch_gestures.borrow_mut().handle(touches);
        for input in inputs {
            self.dispatch_input(input);
        }
        true
    }

    pub(crate) fn handle_key(&self, event: &android_activity::input::KeyEvent<'_>) -> KeyResult {
        if event.key_code() == Keycode::Back && event.action() == KeyAction::Up {
            return KeyResult::Back;
        }
        let Some(key) = key_name(event.key_code()) else {
            return KeyResult::Unhandled;
        };
        let raw_meta = event.meta_state().0;
        let modifiers = Modifiers {
            shift: raw_meta & 0x1 != 0,
            alt: raw_meta & 0x2 != 0,
            control: raw_meta & 0x1000 != 0,
            platform: raw_meta & 0x10000 != 0,
            function: false,
        };
        let key_char = printable_key(event.key_code(), modifiers.shift);
        let keystroke = Keystroke {
            modifiers,
            key,
            key_char,
        };
        match event.action() {
            KeyAction::Down => {
                let multi_line = self.0.state.borrow().input_configuration.input_action
                    == gpui::TextInputAction::Enter;
                let mut input = crate::text_input::ime_key_down(keystroke, multi_line);
                // Hardware printable keys still visit bindings; only plain
                // multiline Enter uses the text replacement path.
                input.prefer_character_input &= input.keystroke.key == "enter";
                input.is_held = event.repeat_count() > 0;
                let text_to_insert = input
                    .keystroke
                    .key_char
                    .clone()
                    .filter(|_| !modifiers.control && !modifiers.alt && !modifiers.platform);
                let result = self.dispatch_input(PlatformInput::KeyDown(input));
                if result.propagate
                    && let Some(text) = text_to_insert
                {
                    self.with_input_handler(|handler| {
                        handler.replace_text_in_range(None, &text);
                    });
                }
            }
            KeyAction::Up => {
                self.dispatch_input(PlatformInput::KeyUp(KeyUpEvent { keystroke }));
            }
            _ => return KeyResult::Unhandled,
        }
        KeyResult::Handled
    }

    fn handle_host_key(&self, key_code: i32, down: bool, unicode_code_point: i32, meta_state: i32) {
        let key_code = Keycode::from(key_code as u32);
        if key_code == Keycode::Del && meta_state & (0x2 | 0x1000 | 0x10000) == 0 {
            if down {
                self.delete_backward();
            }
            return;
        }
        if key_code == Keycode::Back && !down {
            self.handle_back();
            return;
        }
        let key_char = u32::try_from(unicode_code_point)
            .ok()
            .and_then(char::from_u32)
            .filter(|character| !character.is_control())
            .map(|character| character.to_string());
        let Some(key) = key_name(key_code).or_else(|| key_char.clone()) else {
            return;
        };
        let raw_meta = meta_state as u32;
        let keystroke = Keystroke {
            modifiers: Modifiers {
                shift: raw_meta & 0x1 != 0,
                alt: raw_meta & 0x2 != 0,
                control: raw_meta & 0x1000 != 0,
                platform: raw_meta & 0x10000 != 0,
                function: false,
            },
            key,
            key_char: key_char.clone(),
        };
        if down {
            let multi_line = self.0.state.borrow().input_configuration.input_action
                == gpui::TextInputAction::Enter;
            let event = crate::text_input::ime_key_down(keystroke, multi_line);
            let key_char = event.keystroke.key_char.clone();
            let result = self.dispatch_input(PlatformInput::KeyDown(event));
            if result.propagate
                && let Some(text) = key_char.filter(|_| raw_meta & (0x2 | 0x1000 | 0x10000) == 0)
            {
                self.with_input_handler(|handler| {
                    handler.replace_text_in_range(None, &text);
                });
            }
        } else {
            self.dispatch_input(PlatformInput::KeyUp(KeyUpEvent { keystroke }));
        }
    }

    pub(crate) fn pump_frame(&self, force: bool) {
        if self.0.frames_suspended.get() || self.current_visibility() != WindowVisibility::Visible {
            host::pause_frames();
            return;
        }
        let now = std::time::Instant::now();
        let needed = self.0.pacing.borrow_mut().begin_frame(now);
        let forced = self.0.forced_frame_requested.replace(false);
        let force = force || forced;
        if !force && !needed {
            host::pause_frames();
            return;
        }
        let require_presentation = force || self.0.pacing.borrow().active(now);
        let callback = self.0.callbacks.borrow_mut().request_frame.take();
        if let Some(mut callback) = callback {
            callback(RequestFrameOptions {
                require_presentation,
                force_render: force,
                signal_source: FrameRequestSource::NativeCallback,
                ..Default::default()
            });
            self.0.callbacks.borrow_mut().request_frame = Some(callback);
        }
        // `request_frame` presents synchronously, so the frame is on screen.
        for callback in std::mem::take(&mut *self.0.after_frame.borrow_mut()) {
            callback();
        }
        if self
            .0
            .pacing
            .borrow_mut()
            .end_frame(std::time::Instant::now())
        {
            host::request_frame();
        } else {
            host::pause_frames();
        }
        self.sync_input_state(false);
        if let Some(position) = self.0.keyboard_tap.take() {
            self.with_input_handler(|handler| {
                if handler
                    .element_bounds()
                    .is_some_and(|bounds| bounds.contains(&position))
                {
                    // IME dismissal preserves GPUI focus, so FocusGained alone
                    // cannot reopen it when the user resumes editing.
                    host::show_keyboard();
                }
            });
        }
    }

    pub(crate) fn set_frames_suspended(&self, suspended: bool) {
        self.0.frames_suspended.set(suspended);
        if suspended {
            self.0.pacing.borrow_mut().suspend();
            host::pause_frames();
        } else {
            self.schedule_forced_frame();
        }
    }

    pub(crate) fn display_changed(&self) {
        let callback = self.0.callbacks.borrow_mut().display.take();
        if let Some(mut callback) = callback {
            callback();
            self.0.callbacks.borrow_mut().display = Some(callback);
        }
    }

    pub(crate) fn schedule_forced_frame(&self) {
        self.0.forced_frame_requested.set(true);
        self.schedule_frame();
    }

    pub(crate) fn after_next_frame(&self, callback: Box<dyn FnOnce()>) {
        self.0.after_frame.borrow_mut().push(callback);
        self.schedule_forced_frame();
    }

    fn dispatch_input(&self, input: PlatformInput) -> DispatchEventResult {
        self.0.pacing.borrow_mut().input(std::time::Instant::now());
        host::request_frame();
        let callback = self.0.callbacks.borrow_mut().input.take();
        if let Some(mut callback) = callback {
            let result = callback(input);
            self.0.callbacks.borrow_mut().input = Some(callback);
            result
        } else {
            DispatchEventResult {
                propagate: true,
                default_prevented: false,
            }
        }
    }

    fn invoke_resize(&self, logical: Size<Pixels>, scale: f32) {
        let callback = self.0.callbacks.borrow_mut().resize.take();
        if let Some(mut callback) = callback {
            callback(logical, scale);
            self.0.callbacks.borrow_mut().resize = Some(callback);
        }
    }

    pub(crate) fn invoke_close(&self) {
        let should_close = self.0.callbacks.borrow_mut().should_close.take();
        let allow = if let Some(mut callback) = should_close {
            let allow = callback();
            self.0.callbacks.borrow_mut().should_close = Some(callback);
            allow
        } else {
            true
        };
        if allow && let Some(callback) = self.0.callbacks.borrow_mut().close.take() {
            callback();
        }
    }
}

pub(crate) enum KeyResult {
    Handled,
    Unhandled,
    Back,
}

fn logical_size(physical: Size<DevicePixels>, scale: f32) -> Size<Pixels> {
    size(
        px(physical.width.0 as f32 / scale),
        px(physical.height.0 as f32 / scale),
    )
}

fn key_name(key: Keycode) -> Option<String> {
    let name = match key {
        Keycode::A => "a",
        Keycode::B => "b",
        Keycode::C => "c",
        Keycode::D => "d",
        Keycode::E => "e",
        Keycode::F => "f",
        Keycode::G => "g",
        Keycode::H => "h",
        Keycode::I => "i",
        Keycode::J => "j",
        Keycode::K => "k",
        Keycode::L => "l",
        Keycode::M => "m",
        Keycode::N => "n",
        Keycode::O => "o",
        Keycode::P => "p",
        Keycode::Q => "q",
        Keycode::R => "r",
        Keycode::S => "s",
        Keycode::T => "t",
        Keycode::U => "u",
        Keycode::V => "v",
        Keycode::W => "w",
        Keycode::X => "x",
        Keycode::Y => "y",
        Keycode::Z => "z",
        Keycode::Keycode0 => "0",
        Keycode::Keycode1 => "1",
        Keycode::Keycode2 => "2",
        Keycode::Keycode3 => "3",
        Keycode::Keycode4 => "4",
        Keycode::Keycode5 => "5",
        Keycode::Keycode6 => "6",
        Keycode::Keycode7 => "7",
        Keycode::Keycode8 => "8",
        Keycode::Keycode9 => "9",
        Keycode::Enter => "enter",
        Keycode::Space => "space",
        Keycode::Tab => "tab",
        Keycode::Del => "backspace",
        Keycode::ForwardDel => "delete",
        Keycode::DpadLeft => "left",
        Keycode::DpadRight => "right",
        Keycode::DpadUp => "up",
        Keycode::DpadDown => "down",
        Keycode::Escape => "escape",
        _ => return None,
    };
    Some(name.to_owned())
}

fn printable_key(key: Keycode, shift: bool) -> Option<String> {
    let name = key_name(key)?;
    if name.len() != 1 {
        return (key == Keycode::Space).then(|| " ".to_owned());
    }
    Some(if shift { name.to_uppercase() } else { name })
}

impl HasWindowHandle for AndroidWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let state = self.0.state.borrow();
        let surface = state
            .native_surface
            .as_ref()
            .ok_or(HandleError::NotSupported)?;
        let raw = surface.window_handle()?.as_raw();
        // SAFETY: `native_surface` owns the ANativeWindow for this borrow.
        Ok(unsafe { WindowHandle::borrow_raw(raw) })
    }
}

impl HasDisplayHandle for AndroidWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        let raw = RawDisplayHandle::Android(AndroidDisplayHandle::new());
        // SAFETY: Android's display handle contains no borrowed pointers.
        Ok(unsafe { DisplayHandle::borrow_raw(raw) })
    }
}

impl PlatformWindow for AndroidWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        self.0.state.borrow().bounds
    }

    fn is_maximized(&self) -> bool {
        true
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Fullscreen(self.bounds())
    }

    fn content_size(&self) -> Size<Pixels> {
        self.bounds().size
    }

    /// The IME overlays an edge-to-edge window rather than resizing it, so
    /// the visible part of the layout viewport is everything above its inset.
    /// System bars stay in `insets`; GPUI intersects both for
    /// `fully_visible_bounds`.
    fn visual_viewport_bounds(&self) -> Bounds<Pixels> {
        let state = self.0.state.borrow();
        let mut bounds = Bounds::new(Point::default(), state.bounds.size);
        bounds.size.height = (bounds.size.height - state.insets.ime.bottom).max(px(0.));
        bounds
    }

    fn on_visual_viewport_changed(&self, callback: Box<dyn FnMut()>) {
        self.0.callbacks.borrow_mut().visual_viewport = Some(callback);
    }

    fn resize(&mut self, _size: Size<Pixels>) {}

    fn scale_factor(&self) -> f32 {
        self.0.state.borrow().scale_factor
    }

    fn appearance(&self) -> WindowAppearance {
        self.0.state.borrow().appearance
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.0.display.clone())
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.0.state.borrow().mouse_position
    }

    fn modifiers(&self) -> Modifiers {
        Modifiers::default()
    }

    fn capslock(&self) -> Capslock {
        Capslock::default()
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        self.0.state.borrow_mut().input_handler = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.0.state.borrow_mut().input_handler.take()
    }

    fn set_text_input_configuration(&mut self, configuration: TextInputConfiguration) {
        self.0.state.borrow_mut().input_configuration = configuration.clone();
        host::configure_input(configuration);
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[PromptButton],
    ) -> Option<futures::channel::oneshot::Receiver<usize>> {
        None
    }

    fn activate(&self) {
        self.set_active(true);
    }

    fn is_active(&self) -> bool {
        self.0.state.borrow().active
    }

    fn visibility(&self) -> WindowVisibility {
        self.current_visibility()
    }

    fn is_hovered(&self) -> bool {
        false
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        self.0.state.borrow().background
    }

    fn set_title(&mut self, title: &str) {
        self.0.state.borrow_mut().title = title.to_owned();
    }

    fn set_background_appearance(&self, background: WindowBackgroundAppearance) {
        self.0.state.borrow_mut().background = background;
    }

    fn minimize(&self) {}

    fn zoom(&self) {}

    fn toggle_fullscreen(&self) {}

    fn is_fullscreen(&self) -> bool {
        true
    }

    fn frame_waker(&self) -> Option<Rc<dyn Fn()>> {
        let window = self.clone();
        Some(Rc::new(move || window.schedule_frame()))
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.0.callbacks.borrow_mut().request_frame = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        self.0.callbacks.borrow_mut().input = Some(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.callbacks.borrow_mut().active = Some(callback);
    }

    fn on_visibility_change(&self, callback: Box<dyn FnMut(WindowVisibility)>) {
        self.0
            .reported_visibility
            .set(Some(self.current_visibility()));
        self.0.callbacks.borrow_mut().visibility = Some(callback);
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.callbacks.borrow_mut().hover = Some(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.0.callbacks.borrow_mut().resize = Some(callback);
    }

    fn on_display_changed(&self, callback: Box<dyn FnMut()>) {
        self.0.callbacks.borrow_mut().display = Some(callback);
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        self.0.callbacks.borrow_mut().moved = Some(callback);
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.0.callbacks.borrow_mut().should_close = Some(callback);
    }

    fn on_hit_test_window_control(
        &self,
        callback: Box<dyn FnMut() -> Option<gpui::WindowControlArea>>,
    ) {
        self.0.callbacks.borrow_mut().hit_test = Some(callback);
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.0.callbacks.borrow_mut().close = Some(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        self.0.callbacks.borrow_mut().appearance = Some(callback);
    }

    fn draw(&self, scene: &Scene) {
        let mut state = self.0.state.borrow_mut();
        if state.native_surface.is_some() {
            if state.renderer.draw(scene) {
                crate::FIRST_FRAME_RENDERED.store(true, std::sync::atomic::Ordering::Release);
            }
        }
    }

    fn schedule_frame(&self) {
        self.0.pacing.borrow_mut().request();
        if !self.0.frames_suspended.get() {
            host::request_frame();
        }
    }

    fn sprite_atlas(&self) -> RcOrArcAtlas {
        self.0.state.borrow().renderer.sprite_atlas().clone()
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        false
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        self.0.state.borrow().renderer.gpu_specs()
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {}

    fn insets(&self) -> WindowInsets {
        self.0.state.borrow().insets.clone()
    }

    fn on_insets_changed(&self, callback: Box<dyn FnMut(WindowInsets)>) {
        self.0.callbacks.borrow_mut().insets = Some(callback);
    }

    fn set_back_handler(&self, callback: Box<dyn FnMut()>) {
        self.0.callbacks.borrow_mut().back = Some(callback);
    }

    fn set_back_enabled(&self, enabled: bool) {
        self.0.state.borrow_mut().back_enabled = enabled;
    }

    fn show_soft_keyboard(&self) {
        host::show_keyboard();
    }

    fn hide_soft_keyboard(&self) {
        host::hide_keyboard();
    }

    fn text_input_state_changed(&self, change: TextInputStateChange) {
        match change {
            TextInputStateChange::FocusGained => host::show_keyboard(),
            TextInputStateChange::FocusLost => {
                self.0.state.borrow_mut().input_handler = None;
                host::hide_keyboard();
            }
            TextInputStateChange::SelectionChanged | TextInputStateChange::ContentChanged => {}
        }
    }
}

type RcOrArcAtlas = std::sync::Arc<dyn PlatformAtlas>;
