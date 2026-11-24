//! `gpui::Platform` implementation for an externally-driven UIKit app.

use super::{IosDispatcher, IosDisplay, IosTextSystem, IosWindow};
use anyhow::{Result, anyhow};
use futures::channel::oneshot;
use gpui::{
    Action, ActivityGuard, AnyWindowHandle, AppLifecyclePhase, BackgroundExecutor, ClipboardItem,
    CursorStyle, DummyKeyboardMapper, ForegroundExecutor, GestureTuning, Keymap, Menu, MenuItem,
    PathPromptOptions, Platform, PlatformDisplay, PlatformGestures, PlatformKeyboardLayout,
    PlatformKeyboardMapper, PlatformTextSystem, PlatformWindow, Task, ThermalState,
    WindowAppearance, WindowParams,
};
use std::{
    cell::RefCell,
    ffi::OsString,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::Duration,
};

pub(crate) struct IosPlatform {
    background_executor: BackgroundExecutor,
    foreground_executor: ForegroundExecutor,
    text_system: Arc<dyn PlatformTextSystem>,
    state: RefCell<IosPlatformState>,
}

#[derive(Default)]
struct IosPlatformState {
    active_window: Option<AnyWindowHandle>,
    native_paste: Option<ClipboardItem>,
    open_urls: Option<Box<dyn FnMut(Vec<String>)>>,
    on_quit: Option<Box<dyn FnMut() -> bool>>,
    on_reopen: Option<Box<dyn FnMut()>>,
    on_displays_changed: Option<Box<dyn FnMut()>>,
    on_system_sleep: Option<Box<dyn FnMut()>>,
    on_system_wake: Option<Box<dyn FnMut()>>,
    on_lifecycle: Option<Box<dyn FnMut(AppLifecyclePhase)>>,
    on_memory_warning: Option<Box<dyn FnMut()>>,
    on_thermal_state_change: Option<Box<dyn FnMut()>>,
}

impl IosPlatform {
    pub(crate) fn new() -> Self {
        let dispatcher = Arc::new(IosDispatcher);
        Self {
            background_executor: BackgroundExecutor::new(dispatcher.clone()),
            foreground_executor: ForegroundExecutor::new(dispatcher),
            text_system: Arc::new(IosTextSystem::new()),
            state: RefCell::new(IosPlatformState::default()),
        }
    }

    pub(crate) fn with_native_paste(&self, item: ClipboardItem, paste: impl FnOnce()) {
        let previous = self.state.borrow_mut().native_paste.replace(item);
        paste();
        self.state.borrow_mut().native_paste = previous;
    }

    pub(crate) fn notify_lifecycle(&self, phase: AppLifecyclePhase) {
        let mut callback = self.state.borrow_mut().on_lifecycle.take();
        if let Some(callback) = callback.as_mut() {
            callback(phase);
        }
        if self.state.borrow().on_lifecycle.is_none() {
            self.state.borrow_mut().on_lifecycle = callback;
        }
    }

    pub(crate) fn notify_displays_changed(&self) {
        let mut callback = self.state.borrow_mut().on_displays_changed.take();
        if let Some(callback) = callback.as_mut() {
            callback();
        }
        if self.state.borrow().on_displays_changed.is_none() {
            self.state.borrow_mut().on_displays_changed = callback;
        }
    }

    pub(crate) fn notify_memory_warning(&self) {
        let mut callback = self.state.borrow_mut().on_memory_warning.take();
        if let Some(callback) = callback.as_mut() {
            callback();
        }
        if self.state.borrow().on_memory_warning.is_none() {
            self.state.borrow_mut().on_memory_warning = callback;
        }
    }

    pub(crate) fn notify_open_urls(&self, urls: Vec<String>) {
        let mut callback = self.state.borrow_mut().open_urls.take();
        if let Some(callback) = callback.as_mut() {
            callback(urls);
        }
        if self.state.borrow().open_urls.is_none() {
            self.state.borrow_mut().open_urls = callback;
        }
    }
}

struct IosKeyboardLayout;

struct IosGestures;

impl PlatformGestures for IosGestures {
    fn tuning(&self) -> GestureTuning {
        GestureTuning {
            long_press_duration: Duration::from_millis(450),
            ..GestureTuning::default()
        }
    }
}

impl PlatformKeyboardLayout for IosKeyboardLayout {
    fn id(&self) -> &str {
        "ios"
    }

    fn name(&self) -> &str {
        "iOS"
    }
}

impl Platform for IosPlatform {
    fn background_executor(&self) -> BackgroundExecutor {
        self.background_executor.clone()
    }

    fn foreground_executor(&self) -> ForegroundExecutor {
        self.foreground_executor.clone()
    }

    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        Arc::clone(&self.text_system)
    }

    fn run(&self, on_finish_launching: Box<dyn FnOnce()>) {
        super::assert_main_thread();
        on_finish_launching();
    }

    fn quit(&self) {
        let mut callback = self.state.borrow_mut().on_quit.take();
        if let Some(callback) = callback.as_mut() {
            let _ = callback();
        }
        if self.state.borrow().on_quit.is_none() {
            self.state.borrow_mut().on_quit = callback;
        }
    }

    fn restart(&self, _binary_path: Option<PathBuf>, _arguments: Vec<OsString>) {
        log::warn!("iOS does not permit applications to restart themselves");
    }

    fn activate(&self, _ignoring_other_apps: bool) {}

    fn hide(&self) {}

    fn hide_other_apps(&self) {}

    fn unhide_other_apps(&self) {}

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        vec![Rc::new(IosDisplay)]
    }

    fn on_displays_changed(&self, callback: Box<dyn FnMut()>) {
        self.state.borrow_mut().on_displays_changed = Some(callback);
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(Rc::new(IosDisplay))
    }

    fn active_window(&self) -> Option<AnyWindowHandle> {
        self.state.borrow().active_window
    }

    fn window_stack(&self) -> Option<Vec<AnyWindowHandle>> {
        Some(self.active_window().into_iter().collect())
    }

    fn open_window(
        &self,
        handle: AnyWindowHandle,
        params: WindowParams,
    ) -> Result<Box<dyn PlatformWindow>> {
        super::assert_main_thread();
        let window = Box::new(IosWindow::new(handle, params)?);
        window.register();
        self.state.borrow_mut().active_window = Some(handle);
        Ok(window)
    }

    fn window_appearance(&self) -> WindowAppearance {
        super::ffi::host_metrics().appearance
    }

    fn open_url(&self, url: &str) {
        super::ffi::host_open_url(url);
    }

    fn on_open_urls(&self, callback: Box<dyn FnMut(Vec<String>)>) {
        self.state.borrow_mut().open_urls = Some(callback);
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
        self.state.borrow_mut().on_quit = Some(callback);
    }

    fn on_reopen(&self, callback: Box<dyn FnMut()>) {
        self.state.borrow_mut().on_reopen = Some(callback);
    }

    // UIKit exposes no system sleep notifications; `on_app_lifecycle` is the
    // closest signal, so these callbacks are kept but never fired.
    fn on_system_sleep(&self, callback: Box<dyn FnMut()>) {
        self.state.borrow_mut().on_system_sleep = Some(callback);
    }

    fn on_system_wake(&self, callback: Box<dyn FnMut()>) {
        self.state.borrow_mut().on_system_wake = Some(callback);
    }

    fn on_app_lifecycle(&self, callback: Box<dyn FnMut(AppLifecyclePhase)>) {
        self.state.borrow_mut().on_lifecycle = Some(callback);
    }

    fn on_memory_warning(&self, callback: Box<dyn FnMut()>) {
        self.state.borrow_mut().on_memory_warning = Some(callback);
    }

    fn gestures(&self) -> Option<Rc<dyn PlatformGestures>> {
        Some(Rc::new(IosGestures))
    }

    fn set_menus(&self, _menus: Vec<Menu>, _keymap: &Keymap) {}

    fn set_dock_menu(&self, _menu: Vec<MenuItem>, _keymap: &Keymap) {}

    fn on_app_menu_action(&self, _callback: Box<dyn FnMut(&dyn Action)>) {}

    fn on_will_open_app_menu(&self, _callback: Box<dyn FnMut()>) {}

    fn on_validate_app_menu_command(&self, _callback: Box<dyn FnMut(&dyn Action) -> bool>) {}

    fn thermal_state(&self) -> ThermalState {
        ThermalState::Nominal
    }

    fn on_thermal_state_change(&self, callback: Box<dyn FnMut()>) {
        self.state.borrow_mut().on_thermal_state_change = Some(callback);
    }

    fn prevent_idle_sleep(&self, _reason: &str) -> Task<Result<ActivityGuard>> {
        Task::ready(Ok(ActivityGuard::noop()))
    }

    fn app_path(&self) -> Result<PathBuf> {
        std::env::current_exe()?
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow!("application executable has no bundle directory"))
    }

    fn path_for_auxiliary_executable(&self, name: &str) -> Result<PathBuf> {
        Ok(self.app_path()?.join(name))
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
        self.state
            .borrow()
            .native_paste
            .clone()
            .or_else(|| super::ffi::host_read_clipboard().map(ClipboardItem::new_string))
    }

    fn write_to_clipboard(&self, item: ClipboardItem) {
        if let Some(text) = item.text() {
            super::ffi::host_write_clipboard(&text);
        }
    }

    fn write_credentials(&self, _url: &str, _username: &str, _password: &[u8]) -> Task<Result<()>> {
        Task::ready(Err(anyhow!(
            "credential storage is not implemented by the GPUI iOS shell"
        )))
    }

    fn read_credentials(&self, _url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        Task::ready(Err(anyhow!(
            "credential storage is not implemented by the GPUI iOS shell"
        )))
    }

    fn delete_credentials(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(Err(anyhow!(
            "credential storage is not implemented by the GPUI iOS shell"
        )))
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(IosKeyboardLayout)
    }

    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        Rc::new(DummyKeyboardMapper)
    }

    fn on_keyboard_layout_change(&self, _callback: Box<dyn FnMut()>) {}
}
