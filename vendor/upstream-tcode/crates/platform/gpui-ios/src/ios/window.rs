//! GPUI window backed by the Swift host's `CAMetalLayer` UIView.

use super::{IosDisplay, raw_handles::IosRawHandles};
use gpui::{
    Bounds, Capslock, DevicePixels, DispatchEventResult, FrameRequestSource, GpuSpecs, Modifiers,
    Pixels, PlatformAtlas, PlatformDisplay, PlatformInput, PlatformInputHandler, PlatformWindow,
    Point, PromptButton, PromptLevel, RequestFrameOptions, Scene, Size, TextInputConfiguration,
    TextInputStateChange, TouchEvent, WindowAppearance, WindowBackgroundAppearance, WindowBounds,
    WindowControlArea, WindowInsets, WindowParams, WindowVisibility, px, size,
};
use gpui_mobile_pacing::FramePacer;
use gpui_mobile_touch::TwoFingerGestures;
use gpui_wgpu::{GpuContext, WgpuContext, WgpuRenderer, WgpuSurfaceConfig, wgpu};
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, UiKitDisplayHandle, UiKitWindowHandle, WindowHandle,
};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    ffi::c_void,
    ptr::NonNull,
    rc::Rc,
    sync::Arc,
    time::Instant,
};

type RequestFrameCallback = Box<dyn FnMut(RequestFrameOptions)>;
type InputCallback = Box<dyn FnMut(PlatformInput) -> DispatchEventResult>;
type BoolCallback = Box<dyn FnMut(bool)>;
type ResizeCallback = Box<dyn FnMut(Size<Pixels>, f32)>;
type UnitCallback = Box<dyn FnMut()>;
type ShouldCloseCallback = Box<dyn FnMut() -> bool>;
type HitTestCallback = Box<dyn FnMut() -> Option<WindowControlArea>>;
type InsetsCallback = Box<dyn FnMut(WindowInsets)>;
type VisibilityCallback = Box<dyn FnMut(WindowVisibility)>;

pub(crate) struct IosWindow {
    view: Cell<NonNull<c_void>>,
    raw_handles: RefCell<IosRawHandles>,
    gpu_context: GpuContext,
    surface_attached: Cell<bool>,
    /// Set between `sceneDidEnterBackground` and `sceneWillEnterForeground`,
    /// while the host has paused its display link.
    in_background: Cell<bool>,
    reported_visibility: Cell<Option<WindowVisibility>>,
    bounds: Cell<Bounds<Pixels>>,
    scale_factor: Cell<f32>,
    appearance: Cell<WindowAppearance>,
    active: Cell<bool>,
    mouse_position: Cell<Point<Pixels>>,
    keyboard: crate::key_input::Keyboard,
    background: Cell<WindowBackgroundAppearance>,
    first_frame_rendered: Cell<bool>,
    insets: RefCell<WindowInsets>,
    input_handler: RefCell<Option<PlatformInputHandler>>,
    renderer: RefCell<WgpuRenderer>,
    request_frame_callback: RefCell<Option<RequestFrameCallback>>,
    input_callback: RefCell<Option<InputCallback>>,
    active_callback: RefCell<Option<BoolCallback>>,
    hover_callback: RefCell<Option<BoolCallback>>,
    resize_callback: RefCell<Option<ResizeCallback>>,
    moved_callback: RefCell<Option<UnitCallback>>,
    should_close_callback: RefCell<Option<ShouldCloseCallback>>,
    hit_test_callback: RefCell<Option<HitTestCallback>>,
    close_callback: RefCell<Option<Box<dyn FnOnce()>>>,
    appearance_callback: RefCell<Option<UnitCallback>>,
    insets_callback: RefCell<Option<InsetsCallback>>,
    visual_viewport_callback: RefCell<Option<UnitCallback>>,
    visibility_callback: RefCell<Option<VisibilityCallback>>,
    dispatching_input: Cell<bool>,
    pending_input: RefCell<VecDeque<PlatformInput>>,
    requesting_frame: Cell<bool>,
    pacing: Rc<RefCell<FramePacer>>,
    forced_frame: Cell<bool>,
    active_touches: Cell<usize>,
    touch_gestures: RefCell<TwoFingerGestures>,
    display_callback: RefCell<Option<UnitCallback>>,
    using_input_handler: Cell<bool>,
    text_input_active: Cell<bool>,
}

impl std::fmt::Debug for IosWindow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IosWindow")
            .field("view", &self.view.get())
            .field("bounds", &self.bounds.get())
            .field("scale_factor", &self.scale_factor.get())
            .finish_non_exhaustive()
    }
}

impl IosWindow {
    pub(crate) fn new(
        _handle: gpui::AnyWindowHandle,
        params: WindowParams,
    ) -> anyhow::Result<Self> {
        super::assert_main_thread();
        let metrics = super::ffi::host_metrics();
        let view = super::ffi::host_view()?;
        let logical_size = if metrics.width > 0.0 && metrics.height > 0.0 {
            size(px(metrics.width), px(metrics.height))
        } else {
            params.bounds.size
        };
        let scale_factor = metrics.scale.max(1.0);
        let raw_handles = Self::raw_handles(view);

        let descriptor = wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            flags: wgpu::InstanceFlags::default(),
            backend_options: wgpu::BackendOptions::default(),
            memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
            display: Some(Box::new(raw_handles.clone())),
        };
        let instance = wgpu::Instance::new(descriptor);
        let target = wgpu::SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: None,
            raw_window_handle: raw_handles.window,
        };
        let bootstrap_surface = unsafe { instance.create_surface_unsafe(target) }?;
        let context = WgpuContext::new(instance, &bootstrap_surface, None)?;
        let surface_capabilities = bootstrap_surface.get_capabilities(&context.adapter);
        let surface_format = [
            wgpu::TextureFormat::Bgra8Unorm,
            wgpu::TextureFormat::Rgba8Unorm,
        ]
        .into_iter()
        .find(|format| surface_capabilities.formats.contains(format))
        .or_else(|| {
            surface_capabilities
                .formats
                .iter()
                .find(|format| !format.is_srgb())
                .copied()
        })
        .or_else(|| surface_capabilities.formats.first().copied())
        .ok_or_else(|| anyhow::anyhow!("iOS Metal surface reports no supported formats"))?;
        let format_features = context.adapter.get_texture_format_features(surface_format);
        let glass_usages = wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST;
        let glass_enabled = format_features.allowed_usages.contains(glass_usages)
            && format_features
                .flags
                .contains(wgpu::TextureFormatFeatureFlags::FILTERABLE);
        drop(bootstrap_surface);
        let gpu_context = Rc::new(RefCell::new(Some(context)));
        let drawable_size = size(
            DevicePixels((f32::from(logical_size.width) * scale_factor).round() as i32),
            DevicePixels((f32::from(logical_size.height) * scale_factor).round() as i32),
        );
        let renderer = WgpuRenderer::new(
            gpu_context.clone(),
            &raw_handles,
            WgpuSurfaceConfig {
                size: drawable_size,
                transparent: false,
                preferred_present_mode: Some(wgpu::PresentMode::Fifo),
            },
            None,
        )?;
        log::info!(
            "GPUI iOS Metal surface format selected: {surface_format:?}; scale factor: \
             {scale_factor:.2}; glass enabled: {glass_enabled}"
        );

        Ok(Self {
            view: Cell::new(view),
            raw_handles: RefCell::new(raw_handles),
            gpu_context,
            surface_attached: Cell::new(true),
            in_background: Cell::new(false),
            reported_visibility: Cell::new(None),
            bounds: Cell::new(Bounds::new(Default::default(), logical_size)),
            scale_factor: Cell::new(scale_factor),
            appearance: Cell::new(metrics.appearance),
            active: Cell::new(metrics.active),
            mouse_position: Cell::new(Point::default()),
            keyboard: crate::key_input::Keyboard::default(),
            background: Cell::new(WindowBackgroundAppearance::Opaque),
            first_frame_rendered: Cell::new(false),
            insets: RefCell::new(metrics.insets()),
            input_handler: RefCell::new(None),
            renderer: RefCell::new(renderer),
            request_frame_callback: RefCell::new(None),
            input_callback: RefCell::new(None),
            active_callback: RefCell::new(None),
            hover_callback: RefCell::new(None),
            resize_callback: RefCell::new(None),
            moved_callback: RefCell::new(None),
            should_close_callback: RefCell::new(None),
            hit_test_callback: RefCell::new(None),
            close_callback: RefCell::new(None),
            appearance_callback: RefCell::new(None),
            insets_callback: RefCell::new(None),
            visual_viewport_callback: RefCell::new(None),
            visibility_callback: RefCell::new(None),
            dispatching_input: Cell::new(false),
            pending_input: RefCell::new(VecDeque::new()),
            requesting_frame: Cell::new(false),
            pacing: Rc::new(RefCell::new(FramePacer::default())),
            forced_frame: Cell::new(true),
            active_touches: Cell::new(0),
            touch_gestures: RefCell::default(),
            display_callback: RefCell::new(None),
            using_input_handler: Cell::new(false),
            text_input_active: Cell::new(false),
        })
    }

    pub(crate) fn register(&self) {
        super::ffi::register_window(self);
    }

    fn raw_handles(view: NonNull<c_void>) -> IosRawHandles {
        IosRawHandles {
            window: RawWindowHandle::UiKit(UiKitWindowHandle::new(view)),
            display: RawDisplayHandle::UiKit(UiKitDisplayHandle::new()),
        }
    }

    pub(crate) fn needs_surface_replacement(&self, view: NonNull<c_void>) -> bool {
        !self.surface_attached.get() || self.view.get() != view
    }

    pub(crate) fn replace_surface_from_host(
        &self,
        view: NonNull<c_void>,
        width: f32,
        height: f32,
        scale_factor: f32,
    ) -> anyhow::Result<()> {
        let scale_factor = scale_factor.max(1.0);
        let raw_handles = Self::raw_handles(view);
        let instance = self
            .gpu_context
            .borrow()
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("iOS GPU context is unavailable"))?
            .instance
            .clone();
        let drawable_size = size(
            DevicePixels((width.max(1.0) * scale_factor).round() as i32),
            DevicePixels((height.max(1.0) * scale_factor).round() as i32),
        );
        let mut renderer = self.renderer.borrow_mut();
        renderer.replace_surface(
            &raw_handles,
            WgpuSurfaceConfig {
                size: drawable_size,
                transparent: self.background.get() != WindowBackgroundAppearance::Opaque,
                preferred_present_mode: Some(wgpu::PresentMode::Fifo),
            },
            &instance,
        )?;
        drop(renderer);

        self.view.set(view);
        *self.raw_handles.borrow_mut() = raw_handles;
        self.surface_attached.set(true);
        self.resize_from_host(width, height, scale_factor);
        self.schedule_forced_frame();
        self.report_visibility();
        log::info!("GPUI iOS Metal surface replaced at scale factor {scale_factor:.2}");
        Ok(())
    }

    pub(crate) fn detach_surface_from_host(&self, view: NonNull<c_void>) {
        if self.view.get() != view || !self.surface_attached.replace(false) {
            return;
        }
        self.active_touches.set(0);
        self.touch_gestures.take();
        self.pacing.borrow_mut().suspend();
        self.renderer.borrow_mut().unconfigure_surface();
        self.report_visibility();
    }

    pub(crate) fn update_background(&self, in_background: bool) {
        if self.in_background.replace(in_background) == in_background {
            return;
        }
        if in_background {
            self.active_touches.set(0);
            self.touch_gestures.take();
            self.pacing.borrow_mut().suspend();
        } else {
            self.schedule_frame();
        }
        self.report_visibility();
    }

    fn current_visibility(&self) -> WindowVisibility {
        if self.surface_attached.get() && !self.in_background.get() {
            WindowVisibility::Visible
        } else {
            WindowVisibility::Hidden
        }
    }

    /// Reports a visibility transition to GPUI once `on_visibility_change`
    /// has registered its callback; only changes since the last report fire.
    fn report_visibility(&self) {
        let Some(reported) = self.reported_visibility.get() else {
            return;
        };
        let visibility = self.current_visibility();
        if reported == visibility {
            return;
        }
        self.reported_visibility.set(Some(visibility));
        let mut callback = self.visibility_callback.borrow_mut().take();
        if let Some(callback) = callback.as_mut() {
            callback(visibility);
        }
        if self.visibility_callback.borrow().is_none() {
            *self.visibility_callback.borrow_mut() = callback;
        }
    }

    pub(crate) fn schedule_forced_frame(&self) {
        self.forced_frame.set(true);
        self.schedule_frame();
    }

    pub(crate) fn display_link_tick(&self) -> bool {
        if self.current_visibility() != WindowVisibility::Visible {
            return false;
        }
        let now = Instant::now();
        if self.requesting_frame.replace(true) {
            return true;
        }
        if self.pacing.borrow_mut().begin_frame(now) {
            let force_render = self.forced_frame.replace(false);
            let require_presentation = self.pacing.borrow().active(now);
            let mut callback = self.request_frame_callback.borrow_mut().take();
            if let Some(callback) = callback.as_mut() {
                callback(RequestFrameOptions {
                    require_presentation,
                    force_render,
                    signal_source: FrameRequestSource::NativeCallback,
                    ..Default::default()
                });
            }
            if self.request_frame_callback.borrow().is_none() {
                *self.request_frame_callback.borrow_mut() = callback;
            }
        }
        self.requesting_frame.set(false);
        self.pacing.borrow_mut().end_frame(Instant::now())
    }

    pub(crate) fn display_changed(&self) {
        let mut callback = self.display_callback.borrow_mut().take();
        if let Some(callback) = callback.as_mut() {
            callback();
        }
        *self.display_callback.borrow_mut() = callback;
        self.schedule_frame();
    }

    pub(crate) fn touch_activity(&self, count: usize, phase: gpui::TouchPhase) {
        let active = match phase {
            gpui::TouchPhase::Started => self.active_touches.get() + count,
            gpui::TouchPhase::Ended | gpui::TouchPhase::Cancelled => {
                self.active_touches.get().saturating_sub(count)
            }
            gpui::TouchPhase::Moved => self.active_touches.get(),
        };
        self.active_touches.set(active);
        self.pacing
            .borrow_mut()
            .set_touching(active > 0, Instant::now());
        super::ffi::host_schedule_frame();
    }

    pub(crate) fn dispatch_touches(&self, touches: impl IntoIterator<Item = TouchEvent>) {
        let inputs = self.touch_gestures.borrow_mut().handle(touches);
        for input in inputs {
            self.dispatch_input(input);
        }
    }

    pub(crate) fn input_activity(&self) {
        self.pacing.borrow_mut().input(Instant::now());
        super::ffi::host_schedule_frame();
    }

    pub(crate) fn dispatch_input(&self, input: PlatformInput) -> DispatchEventResult {
        self.input_activity();
        if self.dispatching_input.replace(true) {
            self.pending_input.borrow_mut().push_back(input);
            return DispatchEventResult::default();
        }

        let mut first_result = None;
        let mut next = Some(input);
        while let Some(input) = next {
            let mut callback = self.input_callback.borrow_mut().take();
            let result = callback
                .as_mut()
                .map_or_else(DispatchEventResult::default, |callback| callback(input));
            if self.input_callback.borrow().is_none() {
                *self.input_callback.borrow_mut() = callback;
            }
            first_result.get_or_insert(result);
            next = self.pending_input.borrow_mut().pop_front();
        }
        self.dispatching_input.set(false);
        first_result.unwrap_or_default()
    }

    pub(crate) fn set_mouse_position(&self, position: Point<Pixels>) {
        self.mouse_position.set(position);
    }

    pub(crate) fn resize_from_host(&self, width: f32, height: f32, scale_factor: f32) {
        let size = size(px(width.max(1.0)), px(height.max(1.0)));
        let scale_factor = scale_factor.max(1.0);
        if self.bounds.get().size == size
            && (self.scale_factor.get() - scale_factor).abs() < f32::EPSILON
        {
            return;
        }

        self.bounds.set(Bounds::new(Default::default(), size));
        self.scale_factor.set(scale_factor);
        self.renderer.borrow_mut().update_drawable_size(gpui::size(
            DevicePixels((width * scale_factor).round().max(1.0) as i32),
            DevicePixels((height * scale_factor).round().max(1.0) as i32),
        ));
        let mut callback = self.resize_callback.borrow_mut().take();
        if let Some(callback) = callback.as_mut() {
            callback(size, scale_factor);
        }
        if self.resize_callback.borrow().is_none() {
            *self.resize_callback.borrow_mut() = callback;
        }
        self.schedule_forced_frame();
    }

    pub(crate) fn update_insets(&self, insets: WindowInsets) {
        let previous = self.insets.replace(insets.clone());
        if previous == insets {
            return;
        }
        let keyboard_moved = previous.ime != insets.ime;
        let mut callback = self.insets_callback.borrow_mut().take();
        if let Some(callback) = callback.as_mut() {
            callback(insets);
        }
        if self.insets_callback.borrow().is_none() {
            *self.insets_callback.borrow_mut() = callback;
        }
        if !keyboard_moved {
            return;
        }
        let mut callback = self.visual_viewport_callback.borrow_mut().take();
        if let Some(callback) = callback.as_mut() {
            callback();
        }
        if self.visual_viewport_callback.borrow().is_none() {
            *self.visual_viewport_callback.borrow_mut() = callback;
        }
    }

    pub(crate) fn update_active(&self, active: bool) {
        if self.active.replace(active) == active {
            return;
        }
        let mut callback = self.active_callback.borrow_mut().take();
        if let Some(callback) = callback.as_mut() {
            callback(active);
        }
        if self.active_callback.borrow().is_none() {
            *self.active_callback.borrow_mut() = callback;
        }
    }

    pub(crate) fn update_appearance(&self, appearance: WindowAppearance) {
        if self.appearance.replace(appearance) == appearance {
            return;
        }
        let mut callback = self.appearance_callback.borrow_mut().take();
        if let Some(callback) = callback.as_mut() {
            callback();
        }
        if self.appearance_callback.borrow().is_none() {
            *self.appearance_callback.borrow_mut() = callback;
        }
        self.schedule_forced_frame();
    }

    pub(crate) fn with_input_handler<R>(
        &self,
        f: impl FnOnce(&mut PlatformInputHandler) -> R,
    ) -> Option<R> {
        if !self.text_input_active.get() {
            return None;
        }
        self.with_stored_input_handler(f)
    }

    pub(crate) fn with_stored_input_handler<R>(
        &self,
        f: impl FnOnce(&mut PlatformInputHandler) -> R,
    ) -> Option<R> {
        // UIKit may commit after GPUI deactivates text input.
        if self.using_input_handler.replace(true) {
            return None;
        }
        // Handler calls update GPUI and may re-enter the platform during layout.
        let mut handler = self.input_handler.borrow_mut().take();
        let result = handler.as_mut().map(f);
        if self.input_handler.borrow().is_none() {
            *self.input_handler.borrow_mut() = handler;
        }
        self.using_input_handler.set(false);
        result
    }

    pub(crate) fn text_input_active(&self) -> bool {
        self.text_input_active.get()
    }

    pub(crate) fn delete_backward(&self) {
        for input in self.keyboard.delete_backward() {
            self.dispatch_input(input);
        }
        self.schedule_forced_frame();
    }

    pub(crate) fn key_event(
        &self,
        key: String,
        key_char: Option<String>,
        modifiers: Modifiers,
        down: bool,
        repeat: bool,
    ) -> bool {
        let input = self
            .keyboard
            .key_event(key, key_char, modifiers, down, repeat);
        let result = self.dispatch_input(input);
        self.schedule_forced_frame();
        !result.propagate || result.default_prevented
    }
}

impl Drop for IosWindow {
    fn drop(&mut self) {
        super::ffi::unregister_window(self);
        if let Some(callback) = self.close_callback.get_mut().take() {
            callback();
        }
    }
}

impl HasWindowHandle for IosWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        Ok(unsafe { WindowHandle::borrow_raw(self.raw_handles.borrow().window) })
    }
}

impl HasDisplayHandle for IosWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(unsafe { DisplayHandle::borrow_raw(self.raw_handles.borrow().display) })
    }
}

impl PlatformWindow for IosWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        self.bounds.get()
    }

    fn is_maximized(&self) -> bool {
        true
    }

    fn window_bounds(&self) -> WindowBounds {
        WindowBounds::Fullscreen(self.bounds.get())
    }

    fn content_size(&self) -> Size<Pixels> {
        self.bounds.get().size
    }

    /// The keyboard is a UIKit window over this view, so the visible part of
    /// the layout viewport is everything above its cover. Safe areas stay in
    /// `insets`; GPUI intersects both for `fully_visible_bounds`.
    fn visual_viewport_bounds(&self) -> Bounds<Pixels> {
        let mut bounds = Bounds::new(Point::default(), self.content_size());
        bounds.size.height = (bounds.size.height - self.insets.borrow().ime.bottom).max(px(0.));
        bounds
    }

    fn on_visual_viewport_changed(&self, callback: Box<dyn FnMut()>) {
        *self.visual_viewport_callback.borrow_mut() = Some(callback);
    }

    fn resize(&mut self, _size: Size<Pixels>) {}

    fn scale_factor(&self) -> f32 {
        self.scale_factor.get()
    }

    fn appearance(&self) -> WindowAppearance {
        self.appearance.get()
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(Rc::new(IosDisplay))
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.mouse_position.get()
    }

    fn modifiers(&self) -> Modifiers {
        self.keyboard.modifiers()
    }

    fn capslock(&self) -> Capslock {
        Capslock::default()
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        *self.input_handler.borrow_mut() = Some(input_handler);
        // GPUI emits only focus state changes. Reinstalling the focused handler
        // after rendering also exposes programmatic edits and caret moves.
        if self.text_input_active.get() {
            super::ffi::host_text_input_changed();
        }
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.input_handler.borrow_mut().take()
    }

    fn set_text_input_configuration(&mut self, configuration: TextInputConfiguration) {
        super::ffi::host_configure_text_input(&configuration);
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

    fn activate(&self) {}

    fn is_active(&self) -> bool {
        self.active.get()
    }

    fn visibility(&self) -> WindowVisibility {
        self.current_visibility()
    }

    fn is_hovered(&self) -> bool {
        false
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        self.background.get()
    }

    fn set_title(&mut self, _title: &str) {}

    fn set_background_appearance(&self, appearance: WindowBackgroundAppearance) {
        self.background.set(appearance);
        self.renderer
            .borrow_mut()
            .update_transparency(appearance != WindowBackgroundAppearance::Opaque);
    }

    fn minimize(&self) {}

    fn zoom(&self) {}

    fn toggle_fullscreen(&self) {}

    fn is_fullscreen(&self) -> bool {
        true
    }

    fn frame_waker(&self) -> Option<Rc<dyn Fn()>> {
        let pacing = self.pacing.clone();
        Some(Rc::new(move || {
            pacing.borrow_mut().request();
            super::ffi::host_schedule_frame();
        }))
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        *self.request_frame_callback.borrow_mut() = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        *self.input_callback.borrow_mut() = Some(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        *self.active_callback.borrow_mut() = Some(callback);
    }

    fn on_visibility_change(&self, callback: Box<dyn FnMut(WindowVisibility)>) {
        self.reported_visibility
            .set(Some(self.current_visibility()));
        *self.visibility_callback.borrow_mut() = Some(callback);
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        *self.hover_callback.borrow_mut() = Some(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        *self.resize_callback.borrow_mut() = Some(callback);
    }

    fn on_display_changed(&self, callback: Box<dyn FnMut()>) {
        *self.display_callback.borrow_mut() = Some(callback);
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        *self.moved_callback.borrow_mut() = Some(callback);
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        *self.should_close_callback.borrow_mut() = Some(callback);
    }

    fn on_hit_test_window_control(&self, callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
        *self.hit_test_callback.borrow_mut() = Some(callback);
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        *self.close_callback.borrow_mut() = Some(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        *self.appearance_callback.borrow_mut() = Some(callback);
    }

    fn draw(&self, scene: &Scene) {
        if self.renderer.borrow_mut().draw(scene) && !self.first_frame_rendered.replace(true) {
            super::ffi::host_first_frame_rendered();
        }
    }

    fn schedule_frame(&self) {
        self.pacing.borrow_mut().request();
        super::ffi::host_schedule_frame();
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.renderer.borrow().sprite_atlas().clone()
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        false
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        self.renderer.borrow().gpu_specs()
    }

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {}

    fn insets(&self) -> WindowInsets {
        self.insets.borrow().clone()
    }

    fn on_insets_changed(&self, callback: Box<dyn FnMut(WindowInsets)>) {
        *self.insets_callback.borrow_mut() = Some(callback);
    }

    fn show_soft_keyboard(&self) {
        super::ffi::host_show_keyboard();
    }

    fn hide_soft_keyboard(&self) {
        super::ffi::host_hide_keyboard();
    }

    fn text_input_state_changed(&self, change: TextInputStateChange) {
        match change {
            TextInputStateChange::FocusGained => {
                self.text_input_active.set(true);
                super::ffi::host_show_keyboard();
                super::ffi::host_text_input_changed();
            }
            TextInputStateChange::FocusLost => {
                self.text_input_active.set(false);
                super::ffi::host_hide_keyboard();
                super::ffi::host_text_input_changed();
            }
            TextInputStateChange::SelectionChanged | TextInputStateChange::ContentChanged => {
                super::ffi::host_text_input_changed();
            }
        }
    }
}
