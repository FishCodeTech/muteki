use super::{
    dispatcher::AndroidDispatcher,
    display::AndroidDisplay,
    host::{self, HostEvent, ScrollCaptureRequest},
    text::AndroidTextSystem,
    window::{AndroidWindow, AndroidWindowInner, KeyResult},
};
use android_activity::{AndroidApp, InputStatus, MainEvent, PollEvent, input::InputEvent};
use anyhow::{Result, anyhow};
use futures::channel::oneshot;
use gpui::{
    Action, ActivityGuard, AnyWindowHandle, AppLifecyclePhase, BackgroundExecutor, ClipboardItem,
    CursorStyle, DummyKeyboardMapper, ForegroundExecutor, GestureTuning, Keymap, Menu, MenuItem,
    PathPromptOptions, Platform, PlatformDisplay, PlatformGestures, PlatformKeyboardLayout,
    PlatformKeyboardMapper, PlatformTextSystem, PlatformWindow, ScrollPhysics, Task, ThermalState,
    WindowAppearance, WindowParams, point, px, size,
};
use gpui_wgpu::{CosmicTextSystem, GpuContext};
use ndk::configuration::UiModeNight;
use smallvec::SmallVec;
use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    rc::{Rc, Weak},
    sync::Arc,
    time::Duration,
};

#[derive(Default)]
struct PlatformCallbacks {
    open_urls: Option<Box<dyn FnMut(Vec<String>)>>,
    quit: Option<Box<dyn FnMut() -> bool>>,
    reopen: Option<Box<dyn FnMut()>>,
    displays_changed: Option<Box<dyn FnMut()>>,
    system_sleep: Option<Box<dyn FnMut()>>,
    system_wake: Option<Box<dyn FnMut()>>,
    lifecycle: Option<Box<dyn FnMut(AppLifecyclePhase)>>,
    memory_warning: Option<Box<dyn FnMut()>>,
    app_menu_action: Option<ActionCallback>,
    will_open_app_menu: Option<Box<dyn FnMut()>>,
    validate_app_menu: Option<ValidateActionCallback>,
    thermal: Option<Box<dyn FnMut()>>,
    keyboard_layout: Option<Box<dyn FnMut()>>,
    process_back: Option<Box<dyn FnMut()>>,
    scroll_capture: Option<Box<dyn FnMut(ScrollCaptureRequest)>>,
}

type ActionCallback = Box<dyn FnMut(&dyn Action)>;
type ValidateActionCallback = Box<dyn FnMut(&dyn Action) -> bool>;

pub(crate) struct AndroidPlatform {
    app: AndroidApp,
    dispatcher: Arc<AndroidDispatcher>,
    background_executor: BackgroundExecutor,
    foreground_executor: ForegroundExecutor,
    text_system: Arc<dyn PlatformTextSystem>,
    gpu_context: GpuContext,
    display: Rc<AndroidDisplay>,
    window: RefCell<Option<Weak<AndroidWindowInner>>>,
    active_handle: Cell<Option<AnyWindowHandle>>,
    callbacks: RefCell<PlatformCallbacks>,
    clipboard: RefCell<Option<ClipboardItem>>,
    quit_pending: Cell<bool>,
    quitting: Cell<bool>,
}

impl AndroidPlatform {
    pub(crate) fn new(app: AndroidApp) -> Self {
        let dispatcher = AndroidDispatcher::new(app.create_waker());
        let background_executor = BackgroundExecutor::new(dispatcher.clone());
        let foreground_executor = ForegroundExecutor::new(dispatcher.clone());
        let text_system = load_android_text_system();
        let display = Rc::new(AndroidDisplay::new(gpui::Bounds::new(
            point(px(0.0), px(0.0)),
            size(px(1.0), px(1.0)),
        )));

        Self {
            app,
            dispatcher,
            background_executor,
            foreground_executor,
            text_system,
            gpu_context: Rc::new(RefCell::new(None)),
            display,
            window: RefCell::new(None),
            active_handle: Cell::new(None),
            callbacks: RefCell::new(PlatformCallbacks::default()),
            clipboard: RefCell::new(None),
            quit_pending: Cell::new(false),
            quitting: Cell::new(false),
        }
    }

    fn try_quit(&self) {
        if self.quitting.get() {
            return;
        }

        let callback = self.callbacks.borrow_mut().quit.take();
        let allow = if let Some(mut callback) = callback {
            let allow = callback();
            self.callbacks.borrow_mut().quit = Some(callback);
            allow
        } else {
            true
        };
        if allow {
            self.quit_pending.set(false);
            self.quitting.set(true);
            host::finish_activity();
        } else {
            // GPUI returns false when quit is requested from inside an App
            // update because shutdown cannot borrow the App until that update
            // ends. Retry at the event-loop boundary instead of swallowing the
            // request as though it were a cancellation.
            self.quit_pending.set(true);
        }
        self.app.create_waker().wake();
    }

    pub(crate) fn set_process_back_callback(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().process_back = Some(callback);
    }

    pub(crate) fn set_scroll_capture_callback(
        &self,
        callback: Box<dyn FnMut(ScrollCaptureRequest)>,
    ) {
        self.callbacks.borrow_mut().scroll_capture = Some(callback);
    }

    /// Run `callback` once the next frame has been presented; a frame is
    /// forced if nothing else requests one.
    pub(crate) fn after_next_frame(&self, callback: Box<dyn FnOnce()>) {
        match self.window() {
            Some(window) => window.after_next_frame(callback),
            None => callback(),
        }
    }

    fn emit_scroll_capture(&self, request: ScrollCaptureRequest) {
        let callback = self.callbacks.borrow_mut().scroll_capture.take();
        if let Some(mut callback) = callback {
            callback(request);
            self.callbacks.borrow_mut().scroll_capture = Some(callback);
        } else if let ScrollCaptureRequest::Search { request } = request {
            host::scroll_capture_bounds(request, None);
        }
    }

    fn window(&self) -> Option<AndroidWindow> {
        self.window
            .borrow()
            .as_ref()
            .and_then(Weak::upgrade)
            .map(AndroidWindow)
    }

    fn scale_factor(&self) -> f32 {
        self.app.config().density().unwrap_or(160) as f32 / 160.0
    }

    fn appearance(&self) -> WindowAppearance {
        match self.app.config().ui_mode_night() {
            UiModeNight::Yes => WindowAppearance::Dark,
            _ => WindowAppearance::Light,
        }
    }

    fn emit_displays_changed(&self) {
        let callback = self.callbacks.borrow_mut().displays_changed.take();
        if let Some(mut callback) = callback {
            callback();
            if self.callbacks.borrow().displays_changed.is_none() {
                self.callbacks.borrow_mut().displays_changed = Some(callback);
            }
        }
    }

    fn emit_lifecycle(&self, phase: AppLifecyclePhase) {
        let callback = self.callbacks.borrow_mut().lifecycle.take();
        if let Some(mut callback) = callback {
            callback(phase);
            self.callbacks.borrow_mut().lifecycle = Some(callback);
        }
    }

    fn emit_memory_warning(&self) {
        let callback = self.callbacks.borrow_mut().memory_warning.take();
        if let Some(mut callback) = callback {
            callback();
            self.callbacks.borrow_mut().memory_warning = Some(callback);
        }
    }

    fn emit_process_back(&self) {
        let callback = self.callbacks.borrow_mut().process_back.take();
        if let Some(mut callback) = callback {
            callback();
            self.callbacks.borrow_mut().process_back = Some(callback);
        }
    }

    fn process_inputs(&self) {
        let Some(window) = self.window() else {
            return;
        };
        let Ok(mut events) = self.app.input_events_iter() else {
            log::error!("failed to obtain Android input event iterator");
            return;
        };
        loop {
            let mut back = false;
            let read = events.next(|event| match event {
                InputEvent::MotionEvent(event) => {
                    if window.handle_motion(event) {
                        InputStatus::Handled
                    } else {
                        InputStatus::Unhandled
                    }
                }
                InputEvent::KeyEvent(event) => match window.handle_key(event) {
                    KeyResult::Handled => InputStatus::Handled,
                    KeyResult::Unhandled => InputStatus::Unhandled,
                    KeyResult::Back => {
                        back = true;
                        InputStatus::Handled
                    }
                },
                _ => InputStatus::Unhandled,
            });
            if back && !window.handle_back() {
                self.emit_process_back();
            }
            if !read {
                break;
            }
        }
    }

    fn process_host_events(&self) {
        // Choreographer queues a frame every vsync. When drawing outlasts one,
        // drawing each queued frame in turn would keep input unread for the
        // whole backlog, so a drain draws at most once.
        let mut frame = false;
        for event in host::drain() {
            if let HostEvent::RefreshRate(rate) = event {
                if self.display.set_refresh_rate(rate) {
                    if let Some(window) = self.window() {
                        window.display_changed();
                    }
                    self.emit_displays_changed();
                }
                continue;
            }
            let Some(window) = self.window() else {
                continue;
            };
            match event {
                HostEvent::Frame => frame = true,
                HostEvent::Back => {
                    if !window.handle_back() {
                        self.emit_process_back();
                    }
                }
                HostEvent::ScrollCapture(request) => self.emit_scroll_capture(request),
                event => window.handle_host_event(event),
            }
        }
        if frame && let Some(window) = self.window() {
            window.pump_frame(false);
        }
    }

    fn handle_main_event(&self, event: MainEvent<'_>) {
        match event {
            MainEvent::InputAvailable => self.process_inputs(),
            MainEvent::InitWindow { .. } => {
                if let (Some(window), Some(native_window)) =
                    (self.window(), self.app.native_window())
                {
                    window.set_surface(Some(native_window));
                    self.emit_displays_changed();
                }
            }
            MainEvent::TerminateWindow { .. } => {
                if let Some(window) = self.window() {
                    // This must happen inside the poll callback while ANativeWindow is valid.
                    window.set_surface(None);
                }
            }
            MainEvent::WindowResized { .. } | MainEvent::ContentRectChanged { .. } => {
                if let Some(window) = self.window() {
                    window.resize_from_native_window();
                    self.emit_displays_changed();
                }
            }
            MainEvent::RedrawNeeded { .. } => {
                if let Some(window) = self.window() {
                    window.pump_frame(true);
                }
            }
            MainEvent::GainedFocus => {
                if let Some(window) = self.window() {
                    window.set_active(true);
                }
            }
            MainEvent::LostFocus => {
                if let Some(window) = self.window() {
                    window.set_active(false);
                }
            }
            MainEvent::ConfigChanged { .. } => {
                if let Some(window) = self.window() {
                    window.set_scale_factor(self.scale_factor());
                    window.set_appearance(self.appearance());
                }
            }
            MainEvent::LowMemory => self.emit_memory_warning(),
            MainEvent::Start => self.emit_lifecycle(AppLifecyclePhase::Foreground),
            MainEvent::Resume { .. } => {
                self.emit_lifecycle(AppLifecyclePhase::Active);
                if let Some(window) = self.window() {
                    window.set_active(true);
                    window.set_frames_suspended(false);
                }
            }
            MainEvent::Pause => {
                self.emit_lifecycle(AppLifecyclePhase::Inactive);
                if let Some(window) = self.window() {
                    window.set_frames_suspended(true);
                    window.set_active(false);
                }
            }
            MainEvent::Stop => self.emit_lifecycle(AppLifecyclePhase::Background),
            MainEvent::Destroy => {
                if let Some(window) = self.window() {
                    window.invoke_close();
                    window.set_surface(None);
                }
                self.quitting.set(true);
            }
            MainEvent::InsetsChanged { .. } | MainEvent::SaveState { .. } => {}
            _ => {}
        }
    }
}

fn load_android_text_system() -> Arc<dyn PlatformTextSystem> {
    let text_system = CosmicTextSystem::new_without_system_fonts("Roboto");
    let mut font_paths = fs::read_dir("/system/fonts")
        .into_iter()
        .flatten()
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            matches!(
                path.extension().and_then(|extension| extension.to_str()),
                Some("ttf" | "otf" | "ttc")
            )
        })
        .collect::<Vec<_>>();
    font_paths.sort();
    let fonts = font_paths
        .into_iter()
        .filter_map(|path| match fs::read(&path) {
            Ok(bytes) => Some(Cow::Owned(bytes)),
            Err(error) => {
                log::warn!("failed to read Android font {}: {error}", path.display());
                None
            }
        })
        .collect::<Vec<_>>();
    if let Err(error) = text_system.add_fonts(fonts) {
        log::error!("failed to register Android system fonts: {error:#}");
    }
    Arc::new(AndroidTextSystem::new(text_system))
}

struct AndroidKeyboardLayout;

struct AndroidGestures;

impl PlatformGestures for AndroidGestures {
    fn tuning(&self) -> GestureTuning {
        GestureTuning {
            long_press_duration: Duration::from_millis(450),
            scroll_physics: ScrollPhysics::android(),
            ..GestureTuning::default()
        }
    }
}

impl PlatformKeyboardLayout for AndroidKeyboardLayout {
    fn id(&self) -> &str {
        "android-system"
    }

    fn name(&self) -> &str {
        "Android system keyboard"
    }
}

impl Platform for AndroidPlatform {
    fn background_executor(&self) -> BackgroundExecutor {
        self.background_executor.clone()
    }

    fn foreground_executor(&self) -> ForegroundExecutor {
        self.foreground_executor.clone()
    }

    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.text_system.clone()
    }

    fn run(&self, on_finish_launching: Box<dyn FnOnce()>) {
        let mut launch = Some(on_finish_launching);
        while !self.quitting.get() {
            self.app.poll_events(None, |event| match event {
                PollEvent::Main(event) => self.handle_main_event(event),
                PollEvent::Wake | PollEvent::Timeout => {}
                _ => {}
            });

            if launch.is_some() && self.app.native_window().is_some() {
                launch.take().expect("launch callback present")();
            }
            self.dispatcher.drain_main_queue();
            self.process_host_events();
            if self.quit_pending.replace(false) {
                self.try_quit();
            }
        }
    }

    fn quit(&self) {
        self.try_quit();
    }

    fn restart(&self, _binary_path: Option<PathBuf>, _arguments: Vec<OsString>) {
        log::warn!("application restart is not supported on Android");
    }

    fn activate(&self, _ignoring_other_apps: bool) {}

    fn hide(&self) {}

    fn hide_other_apps(&self) {}

    fn unhide_other_apps(&self) {}

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        vec![self.display.clone()]
    }

    fn on_displays_changed(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().displays_changed = Some(callback);
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.display.clone())
    }

    fn active_window(&self) -> Option<AnyWindowHandle> {
        self.active_handle.get()
    }

    fn open_window(
        &self,
        handle: AnyWindowHandle,
        _options: WindowParams,
    ) -> Result<Box<dyn PlatformWindow>> {
        if self.window().is_some() {
            return Err(anyhow!("Android currently supports one GPUI window"));
        }
        let native_window = self
            .app
            .native_window()
            .ok_or_else(|| anyhow!("Android native window is not available"))?;
        let window = AndroidWindow::new(
            self.display.clone(),
            self.gpu_context.clone(),
            self.scale_factor(),
            self.appearance(),
            gpui::WindowBackgroundAppearance::Opaque,
            native_window,
        )?;
        *self.window.borrow_mut() = Some(Rc::downgrade(&window.0));
        self.active_handle.set(Some(handle));
        Ok(Box::new(window))
    }

    fn window_appearance(&self) -> WindowAppearance {
        self.appearance()
    }

    fn open_url(&self, url: &str) {
        host::open_url(url);
    }

    fn on_open_urls(&self, callback: Box<dyn FnMut(Vec<String>)>) {
        self.callbacks.borrow_mut().open_urls = Some(callback);
    }

    fn register_url_scheme(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn prompt_for_paths(
        &self,
        _options: PathPromptOptions,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        let (sender, receiver) = oneshot::channel();
        let _ = sender.send(Ok(None));
        receiver
    }

    fn prompt_for_new_path(
        &self,
        _directory: &Path,
        _suggested_name: Option<&str>,
    ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        let (sender, receiver) = oneshot::channel();
        let _ = sender.send(Ok(None));
        receiver
    }

    fn can_select_mixed_files_and_dirs(&self) -> bool {
        false
    }

    fn reveal_path(&self, _path: &Path) {}

    fn open_with_system(&self, _path: &Path) {}

    fn on_quit(&self, callback: Box<dyn FnMut() -> bool>) {
        self.callbacks.borrow_mut().quit = Some(callback);
    }

    fn on_reopen(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().reopen = Some(callback);
    }

    // Android reports device sleep only through the activity lifecycle
    // (`on_app_lifecycle`), so these callbacks are kept but never fired.
    fn on_system_sleep(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().system_sleep = Some(callback);
    }

    fn on_system_wake(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().system_wake = Some(callback);
    }

    fn on_app_lifecycle(&self, callback: Box<dyn FnMut(AppLifecyclePhase)>) {
        self.callbacks.borrow_mut().lifecycle = Some(callback);
    }

    fn on_memory_warning(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().memory_warning = Some(callback);
    }

    fn gestures(&self) -> Option<Rc<dyn PlatformGestures>> {
        Some(Rc::new(AndroidGestures))
    }

    fn set_menus(&self, _menus: Vec<Menu>, _keymap: &Keymap) {}

    fn set_dock_menu(&self, _menu: Vec<MenuItem>, _keymap: &Keymap) {}

    fn update_jump_list(
        &self,
        _menus: Vec<MenuItem>,
        _entries: Vec<SmallVec<[PathBuf; 2]>>,
    ) -> Task<Vec<SmallVec<[PathBuf; 2]>>> {
        Task::ready(Vec::new())
    }

    fn on_app_menu_action(&self, callback: Box<dyn FnMut(&dyn Action)>) {
        self.callbacks.borrow_mut().app_menu_action = Some(callback);
    }

    fn on_will_open_app_menu(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().will_open_app_menu = Some(callback);
    }

    fn on_validate_app_menu_command(&self, callback: Box<dyn FnMut(&dyn Action) -> bool>) {
        self.callbacks.borrow_mut().validate_app_menu = Some(callback);
    }

    fn thermal_state(&self) -> ThermalState {
        ThermalState::Nominal
    }

    fn on_thermal_state_change(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().thermal = Some(callback);
    }

    fn prevent_idle_sleep(&self, _reason: &str) -> Task<Result<ActivityGuard>> {
        Task::ready(Ok(ActivityGuard::noop()))
    }

    fn app_path(&self) -> Result<PathBuf> {
        Err(anyhow!(
            "Android applications do not have an executable path"
        ))
    }

    fn path_for_auxiliary_executable(&self, name: &str) -> Result<PathBuf> {
        Err(anyhow!(
            "Android applications cannot launch auxiliary executable {name}"
        ))
    }

    fn set_cursor_style(&self, _style: CursorStyle) {}

    fn hide_cursor_until_mouse_moves(&self) {}

    fn is_cursor_visible(&self) -> bool {
        false
    }

    fn should_auto_hide_scrollbars(&self) -> bool {
        true
    }

    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        host::read_clipboard()
            .map(ClipboardItem::new_string)
            .or_else(|| self.clipboard.borrow().clone())
    }

    fn write_to_clipboard(&self, item: ClipboardItem) {
        if let Some(text) = item.text() {
            host::write_clipboard(text.to_string());
        }
        *self.clipboard.borrow_mut() = Some(item);
    }

    fn write_credentials(&self, _url: &str, _username: &str, _password: &[u8]) -> Task<Result<()>> {
        Task::ready(Err(anyhow!(
            "credential storage is not implemented on Android"
        )))
    }

    fn read_credentials(&self, _url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        Task::ready(Ok(None))
    }

    fn delete_credentials(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Ok(()))
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(AndroidKeyboardLayout)
    }

    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        Rc::new(DummyKeyboardMapper)
    }

    fn on_keyboard_layout_change(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().keyboard_layout = Some(callback);
    }
}
