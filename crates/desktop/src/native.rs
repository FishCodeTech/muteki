//! AppKit pieces nsh does not expose: the main web view's UI delegate
//! (JavaScript dialogs, new windows), the close barrier, and the native
//! preview's snapshots and input. Everything here runs on the main thread.
use crate::policy::{self, BridgeError, NewWindow};
use block2::{Block, RcBlock};
use dispatch2::DispatchQueue;
use nsh_platform_macos::objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use objc2::{
    AnyThread, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::{Allocated, Retained},
    runtime::{AnyClass, AnyObject, AnyProtocol, Bool, Imp, NSObject, NSObjectProtocol, Sel},
    sel,
};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ffi::CStr,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc::UnboundedSender;

#[path="native_preview.rs"]
mod window_preview;
pub(crate) use window_preview::{open as open_window_preview,invoke as invoke_window_preview,dispose as dispose_window_preview,hide_inactive as hide_window_preview,cleared as window_preview_cleared};
#[path = "native_report_preview.rs"]
mod report_preview;

pub(crate) fn report_preview_command(window: &str, document: &str, method: &str, args: &Value) -> crate::policy::Outcome {
    report_preview::command(window, document, method, args)
}

pub(crate) fn dispose_report_preview(window: String) {
    on_main(move || report_preview::dispose(&window));
}

pub(crate) fn warm_report_preview(window: String, document: String) {
    on_main(move || report_preview::warm(&window, &document));
}

pub const MAIN_ID: &str = "muteki-main";

/// What other threads hand to the engine's bridge loop.
pub enum Notice {
    /// The answer to the page's call `id`, worked out off the engine thread.
    Reply(Value, Result<Value, BridgeError>),
    /// The service's identity: service id and identity id.
    /// An event for the page's listeners.
    Event(String, Value),
    NotificationConsentChoice(Value, Value, usize),
    NotificationPrepared(Value, Value, Result<Value, BridgeError>),
    NotificationScheduled(String, Result<(), BridgeError>),
    NotificationClicked(String),
    NotificationNavigate {
        window_id: String,
        route: String,
        thread_id: String,
        test: bool,
        receipt_id: Value,
    },
    DeepLink(String),
    WindowMessage(String, String),
    WindowClosed(String),
    WindowLoading(String),
    Speech(String,String,Value),
    WindowPreview(String,String,Value),
    ReportPreview(String, String, Value),
    WindowEvent(String,String,Value),
    Connection(String),
    FocusChanged,
    PreviewDataCleared {
        id: Value,
        preview_id: String,
        generation: u64,
        profile_id: Option<String>,
        cleared_website_data: bool,
        cleared_http_auth: bool,
        result: Result<(), BridgeError>,
    },
}

static NOTICES: OnceLock<UnboundedSender<Notice>> = OnceLock::new();
static UI_ORIGIN: OnceLock<String> = OnceLock::new();
static WINDOW_URLS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
/// The main page's bridge answered since its last navigation. Other workbench
/// documents can still acknowledge a close while the main page is loading.
pub static BRIDGE_READY: AtomicBool = AtomicBool::new(false);

fn window_urls() -> &'static Mutex<HashMap<String, String>> {
    WINDOW_URLS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn record_window_url(window_id: &str, url: &str) {
    if let Ok(mut map) = window_urls().lock() {
        if let Some(current) = map.get_mut(window_id) {
            *current = url.to_owned();
        }
    }
}

pub fn register_main_window(origin: &str) {
    register_window("main", origin);
}

fn register_window(window_id: &str, url: &str) {
    window_urls().lock().unwrap_or_else(|error| error.into_inner()).insert(window_id.to_owned(), url.to_owned());
}

pub fn verify_sender_origin(window_id: &str, expected_origin: &str) -> bool {
    if expected_origin.is_empty() {
        return false;
    }
    let Ok(map) = window_urls().lock() else {
        return false;
    };
    match map.get(window_id) {
        Some(url) if !url.is_empty() => policy::main_navigation(url, expected_origin) == policy::MainNavigation::Allow,
        _ => false,
    }
}

pub fn set_notices(sender: UnboundedSender<Notice>) {
    let _ = NOTICES.set(sender);
}

pub fn notify(notice: Notice) {
    if let Some(sender) = NOTICES.get() {
        let _ = sender.send(notice);
    }
}

pub fn on_main(f: impl FnOnce() + Send + 'static) {
    DispatchQueue::main().exec_async(f);
}

fn class(name: &CStr) -> &'static AnyClass {
    AnyClass::get(name).unwrap_or_else(|| panic!("AppKit class {name:?} is missing"))
}

fn ns(text: &str) -> Retained<NSString> {
    NSString::from_str(text)
}

fn notification_center() -> Result<Retained<AnyObject>, BridgeError> {
    static LOADED: OnceLock<bool> = OnceLock::new();
    if !*LOADED.get_or_init(|| unsafe {
        !libc::dlopen(c"/System/Library/Frameworks/UserNotifications.framework/UserNotifications".as_ptr(), libc::RTLD_NOW).is_null()
    }) {
        return Err(BridgeError::new("desktop.notifications_unsupported", "系统通知框架不可用。"));
    }
    // currentNotificationCenter raises an uncatchable NSException (aborting the process) when the
    // executable is not inside a .app bundle, as with the unpackaged dev binary.
    // SAFETY: NSBundle.mainBundle and bundlePath are always available.
    let in_app_bundle = unsafe {
        let bundle: Retained<AnyObject> = msg_send![class(c"NSBundle"), mainBundle];
        let path: Retained<NSString> = msg_send![&bundle, bundlePath];
        path.to_string().ends_with(".app")
    };
    if !in_app_bundle {
        return Err(BridgeError::new("desktop.notifications_unsupported", "未打包的开发版不支持系统通知，请使用安装后的 Muteki.app。"));
    }
    let center = AnyClass::get(c"UNUserNotificationCenter")
        .ok_or_else(|| BridgeError::new("desktop.notifications_unsupported", "系统通知中心不可用。"))?;
    // SAFETY: Apple UserNotifications exposes this process-wide singleton.
    let center: Retained<AnyObject> = unsafe { msg_send![center, currentNotificationCenter] };
    static DELEGATE: OnceLock<usize> = OnceLock::new();
    let delegate = *DELEGATE.get_or_init(|| {
        if let Some(protocol) = AnyProtocol::get(c"UNUserNotificationCenterDelegate") {
            unsafe extern "C" {
                fn class_addProtocol(class: *mut AnyClass, protocol: *const AnyProtocol) -> Bool;
            }
            let class = NotificationDelegate::class();
            if !class.conforms_to(protocol) {
                unsafe { class_addProtocol(class as *const AnyClass as *mut AnyClass, protocol); }
            }
        }
        Retained::into_raw(NotificationDelegate::new()) as usize
    });
    unsafe { let _: () = msg_send![&center, setDelegate: delegate as *mut AnyObject]; }
    Ok(center)
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "MutekiNotificationDelegate"]
    struct NotificationDelegate;

    unsafe impl NSObjectProtocol for NotificationDelegate {}

    impl NotificationDelegate {
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(&self, _center: &AnyObject, _notification: &AnyObject, done: &Block<dyn Fn(usize)>) {
            done.call((24usize,));
        }

        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(&self, _center: &AnyObject, response: &AnyObject, done: &Block<dyn Fn()>) {
            // SAFETY: Response -> notification -> request -> identifier are live in this callback.
            let id = unsafe {
                let notification: Retained<AnyObject> = msg_send![response, notification];
                let request: Retained<AnyObject> = msg_send![&notification, request];
                let identifier: Retained<NSString> = msg_send![&request, identifier];
                identifier.to_string()
            };
            notify(Notice::NotificationClicked(id));
            done.call(());
        }
    }
);

impl NotificationDelegate {
    fn new() -> Retained<Self> {
        let this = Self::alloc();
        unsafe { msg_send![this, init] }
    }
}

/// Read-only macOS permission status. The system may run this callback on a
/// background thread, so callers wait outside AppKit's main loop.
pub fn notification_settings() -> Result<Value, BridgeError> {
    let center = notification_center()?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let callback = RcBlock::new(move |settings: *mut AnyObject| {
        let value = if settings.is_null() {
            Err(BridgeError::new("desktop.notification_status_failed", "系统未返回通知设置。"))
        } else {
            // SAFETY: UNNotificationSettings is retained during its completion handler.
            unsafe {
                let status: isize = msg_send![&*settings, authorizationStatus];
                let alert: isize = msg_send![&*settings, alertSetting];
                let sound: isize = msg_send![&*settings, soundSetting];
                let center: isize = msg_send![&*settings, notificationCenterSetting];
                let lock: isize = msg_send![&*settings, lockScreenSetting];
                let style: isize = msg_send![&*settings, alertStyle];
                let bundle: Retained<AnyObject> = msg_send![class(c"NSBundle"), mainBundle];
                let bundle_id: Option<Retained<NSString>> = msg_send![&bundle, bundleIdentifier];
                Ok(json!({
                    "bundleIdentifier": bundle_id.map(|id| id.to_string()).unwrap_or_default(),
                    "authorizationStatus": match status { 0 => "notDetermined", 1 => "denied", 2 => "authorized", 3 => "provisional", 4 => "ephemeral", _ => "unknown" },
                    "authorizationStatusRaw": status,
                    "alertSettingRaw": alert,
                    "soundSettingRaw": sound,
                    "notificationCenterSettingRaw": center,
                    "lockScreenSettingRaw": lock,
                    "alertStyleRaw": style,
                }))
            }
        };
        let _ = sender.send(value);
    });
    // SAFETY: The framework copies the block for its asynchronous callback.
    unsafe { let _: () = msg_send![&center, getNotificationSettingsWithCompletionHandler: &*callback]; }
    receiver.recv_timeout(Duration::from_secs(5)).map_err(|_| BridgeError::new("desktop.notification_status_timeout", "macOS 通知设置查询未及时返回。"))?
}

pub fn request_notification_authorization() -> Result<bool, BridgeError> {
    let center = notification_center()?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let callback = RcBlock::new(move |granted: Bool, error: *mut AnyObject| {
        let result = if error.is_null() {
            Ok(granted.as_bool())
        } else {
            // SAFETY: NSError is live during the completion handler.
            let message: Retained<NSString> = unsafe { msg_send![&*error, localizedDescription] };
            Err(BridgeError::new("desktop.notification_authorization_failed", message.to_string()))
        };
        let _ = sender.send(result);
    });
    unsafe { let _: () = msg_send![&center, requestAuthorizationWithOptions: 6usize, completionHandler: &*callback]; }
    receiver.recv_timeout(Duration::from_secs(120)).map_err(|_| BridgeError::new("desktop.notification_status_timeout", "macOS 通知授权未及时返回。"))?
}

pub fn schedule_notification(identifier: &str, title: &str, body: &str) -> Result<(), BridgeError> {
    let center = notification_center()?;
    // SAFETY: A nil trigger schedules an immediate local notification.
    unsafe {
        let content: Retained<AnyObject> = msg_send![class(c"UNMutableNotificationContent"), new];
        let _: () = msg_send![&content, setTitle: &*ns(title)];
        let _: () = msg_send![&content, setBody: &*ns(body)];
        let request: Retained<AnyObject> = msg_send![class(c"UNNotificationRequest"),
            requestWithIdentifier: &*ns(identifier), content: &*content, trigger: std::ptr::null::<AnyObject>()];
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let waiter = Arc::new(Mutex::new(Some(sender)));
        let callback_waiter = waiter.clone();
        let note_id = identifier.to_owned();
        let callback = RcBlock::new(move |error: *mut AnyObject| {
            let result = if error.is_null() {
                Ok(())
            } else {
                let message: Retained<NSString> = msg_send![&*error, localizedDescription];
                Err(BridgeError::new("desktop.notification_send_failed", message.to_string()))
            };
            let mut waiter = callback_waiter.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(sender) = waiter.take() {
                if let Err(result) = sender.send(result) {
                    notify(Notice::NotificationScheduled(note_id.clone(), result.0));
                }
            } else {
                notify(Notice::NotificationScheduled(note_id.clone(), result));
            }
        });
        let _: () = msg_send![&center, addNotificationRequest: &*request, withCompletionHandler: &*callback];
        match receiver.recv_timeout(Duration::from_secs(10)) {
            Ok(result) => result,
            Err(_) => {
                let mut waiter = waiter.lock().unwrap_or_else(|error| error.into_inner());
                waiter.take();
                receiver.try_recv().unwrap_or_else(|_| Err(BridgeError::new("desktop.notification_show_unknown", "系统未返回通知投递回执。")))
            }
        }
    }
}

pub fn remove_notification(identifier: &str) {
    let Ok(center) = notification_center() else { return };
    // SAFETY: Both removal APIs accept an array of identifiers.
    unsafe {
        let ids: Retained<AnyObject> = msg_send![class(c"NSArray"), arrayWithObject: &*ns(identifier)];
        let _: () = msg_send![&center, removePendingNotificationRequestsWithIdentifiers: &*ids];
        let _: () = msg_send![&center, removeDeliveredNotificationsWithIdentifiers: &*ids];
    }
}

fn retain(object: &AnyObject) -> Retained<AnyObject> {
    // SAFETY: a live object reference.
    unsafe { Retained::retain(object as *const AnyObject as *mut AnyObject) }.expect("non-null object")
}

fn same(a: &AnyObject, b: &AnyObject) -> bool {
    std::ptr::eq(a, b)
}

fn subviews(view: &AnyObject) -> Vec<Retained<AnyObject>> {
    // SAFETY: NSView's documented accessors.
    unsafe {
        let views: Retained<AnyObject> = msg_send![view, subviews];
        let count: usize = msg_send![&views, count];
        (0..count).map(|i| msg_send![&views, objectAtIndex: i]).collect()
    }
}

fn is_web_view(view: &AnyObject) -> bool {
    // SAFETY: NSObject's isKindOfClass:.
    unsafe { msg_send![view, isKindOfClass: class(c"WKWebView")] }
}

fn first_web_view(view: &AnyObject) -> Option<Retained<AnyObject>> {
    if is_web_view(view) {
        return Some(retain(view));
    }
    subviews(view).iter().find_map(|sub| first_web_view(sub))
}

fn identified(view: &AnyObject, identifier: &NSString) -> Option<Retained<AnyObject>> {
    // SAFETY: NSAccessibility's identifier of an NSView.
    let own: Option<Retained<NSString>> = unsafe { msg_send![view, accessibilityIdentifier] };
    if own.is_some_and(|own| own.isEqualToString(identifier)) {
        return first_web_view(view);
    }
    subviews(view).iter().find_map(|sub| identified(sub, identifier))
}

/// The WKWebView of the nsh element with `test_id(identifier)`.
pub fn web_view(identifier: &str) -> Option<Retained<AnyObject>> {
    let identifier = ns(identifier);
    // SAFETY: NSApplication's window list, on the main thread.
    unsafe {
        let app: Retained<AnyObject> = msg_send![class(c"NSApplication"), sharedApplication];
        let windows: Retained<AnyObject> = msg_send![&app, windows];
        let count: usize = msg_send![&windows, count];
        (0..count).find_map(|i| {
            let window: Retained<AnyObject> = msg_send![&windows, objectAtIndex: i];
            let content: Option<Retained<AnyObject>> = msg_send![&window, contentView];
            content.and_then(|content| identified(&content, &identifier))
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FocusSnapshot {
    pub initialized: bool,
    pub app_active: bool,
    pub key_window_id: Option<String>,
    pub preview_focused_window_id: Option<String>,
}

static FOCUS_STATE: OnceLock<Mutex<FocusSnapshot>> = OnceLock::new();
static DESIRED_DOCK_BADGE: AtomicUsize = AtomicUsize::new(0);
static APPLIED_DOCK_BADGE: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn focus_state() -> &'static Mutex<FocusSnapshot> {
    FOCUS_STATE.get_or_init(|| Mutex::new(FocusSnapshot::default()))
}

fn applied_dock_badge() -> &'static Mutex<Option<String>> {
    APPLIED_DOCK_BADGE.get_or_init(|| Mutex::new(None))
}

pub fn focus_snapshot() -> FocusSnapshot {
    focus_state()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

fn compute_focus_snapshot_on_main() -> FocusSnapshot {
    // SAFETY: NSApplication activation, keyWindow, and WKWebView firstResponder on AppKit's main thread.
    unsafe {
        let app: Retained<AnyObject> = msg_send![class(c"NSApplication"), sharedApplication];
        let active: Bool = msg_send![&app, isActive];
        let key: Option<Retained<AnyObject>> = msg_send![&app, keyWindow];
        let key_window_id = if active.as_bool() {
            key.and_then(|key| {
                let visible: Bool = msg_send![&key, isVisible];
                let mini: Bool = msg_send![&key, isMiniaturized];
                if !visible.as_bool() || mini.as_bool() {
                    return None;
                }
                if let Some(secondary) = WORKBENCH_WINDOWS.with(|windows| {
                    windows
                        .borrow()
                        .iter()
                        .find(|(_, entry)| same(&entry.window, &key))
                        .map(|(id, _)| id.clone())
                }) {
                    return Some(secondary);
                }
                let main_win: Option<Retained<AnyObject>> =
                    web_view(MAIN_ID).and_then(|web| msg_send![&web, window]);
                main_win.filter(|win| same(win, &key)).map(|_| "main".to_owned())
            })
        } else {
            None
        };
        let preview_focused_window_id = key_window_id
            .as_ref()
            .filter(|wid| window_preview::focused_in(wid))
            .cloned();
        FocusSnapshot {
            initialized: true,
            app_active: active.as_bool(),
            key_window_id,
            preview_focused_window_id,
        }
    }
}

fn report_focus_state_on_main() {
    if MainThreadMarker::new().is_none() {
        return;
    }
    let next = compute_focus_snapshot_on_main();
    let changed = {
        let mut guard = focus_state().lock().unwrap_or_else(|error| error.into_inner());
        if *guard != next {
            *guard = next;
            true
        } else {
            false
        }
    };
    if changed {
        notify(Notice::FocusChanged);
    }
}

pub fn refresh_focus_state() {
    if MainThreadMarker::new().is_some() {
        report_focus_state_on_main();
    } else {
        on_main(report_focus_state_on_main);
    }
}

fn sync_dock_tile_on_main() -> Option<String> {
    let count = DESIRED_DOCK_BADGE.load(Ordering::SeqCst);
    // SAFETY: NSApplication.sharedApplication.dockTile on AppKit's main thread.
    let badge = unsafe {
        let app: Retained<AnyObject> = msg_send![class(c"NSApplication"), sharedApplication];
        let tile: Option<Retained<AnyObject>> = msg_send![&app, dockTile];
        tile.and_then(|tile| {
            if count > 0 {
                let label = ns(&count.to_string());
                let _: () = msg_send![&tile, setBadgeLabel: &*label];
            } else {
                let _: () = msg_send![&tile, setBadgeLabel: std::ptr::null::<NSString>()];
            }
            let _: () = msg_send![&tile, display];
            let current: Option<Retained<NSString>> = msg_send![&tile, badgeLabel];
            current.map(|s| s.to_string()).filter(|s| !s.is_empty())
        })
    };
    *applied_dock_badge().lock().unwrap_or_else(|error| error.into_inner()) = badge.clone();
    badge
}

pub fn set_desired_dock_badge(count: usize) {
    DESIRED_DOCK_BADGE.store(count, Ordering::SeqCst);
}

pub fn flush_dock_badge_on_main() -> (usize, Option<String>) {
    let count = DESIRED_DOCK_BADGE.load(Ordering::SeqCst);
    (count, sync_dock_tile_on_main())
}

pub fn set_dock_badge(count: usize) {
    set_desired_dock_badge(count);
    if MainThreadMarker::new().is_some() {
        let _ = sync_dock_tile_on_main();
    } else {
        on_main(|| {
            let _ = sync_dock_tile_on_main();
        });
    }
}

pub fn dock_badge() -> Option<String> {
    if MainThreadMarker::new().is_some() {
        // SAFETY: NSApplication.sharedApplication.dockTile on AppKit's main thread.
        let badge = unsafe {
            let app: Retained<AnyObject> = msg_send![class(c"NSApplication"), sharedApplication];
            let tile: Option<Retained<AnyObject>> = msg_send![&app, dockTile];
            tile.and_then(|tile| {
                let current: Option<Retained<NSString>> = msg_send![&tile, badgeLabel];
                current.map(|s| s.to_string()).filter(|s| !s.is_empty())
            })
        };
        *applied_dock_badge().lock().unwrap_or_else(|error| error.into_inner()) = badge.clone();
        badge
    } else {
        applied_dock_badge().lock().unwrap_or_else(|error| error.into_inner()).clone()
    }
}

pub fn restore_and_focus_window(window_id: &str) -> Result<Retained<AnyObject>, BridgeError> {
    let web = workbench_web_view(window_id)
        .or_else(|| web_view(MAIN_ID))
        .ok_or_else(|| BridgeError::new("desktop.notification_navigation_failed", "聊天窗口不可用。"))?;
    unsafe {
        let window: Option<Retained<AnyObject>> = msg_send![&web, window];
        if let Some(window) = window {
            let mini: Bool = msg_send![&window, isMiniaturized];
            if mini.as_bool() {
                let _: () = msg_send![&window, deminiaturize: std::ptr::null::<AnyObject>()];
            }
            let _: () = msg_send![&window, makeKeyAndOrderFront: std::ptr::null::<AnyObject>()];
        }
        let app: Retained<AnyObject> = msg_send![class(c"NSApplication"), sharedApplication];
        let _: () = msg_send![&app, activateIgnoringOtherApps: Bool::YES];
    }
    report_focus_state_on_main();
    Ok(web)
}

fn build_window_route(window_id: &str, route: &str) -> Result<(Retained<AnyObject>, Retained<AnyObject>), BridgeError> {
    let web = restore_and_focus_window(window_id)?;
    let url = format!("{}{route}", crate::bridge::context().origin.trim_end_matches('/'));
    record_window_url(window_id, &url);
    let ns_url: Option<Retained<AnyObject>> = unsafe { msg_send![class(c"NSURL"), URLWithString: &*ns(&url)] };
    let ns_url = ns_url.ok_or_else(|| BridgeError::new("desktop.notification_navigation_failed", "目标路由地址无效。"))?;
    Ok((web, ns_url))
}

pub fn prepare_window_route(window_id: &str, route: &str) -> Result<(), BridgeError> {
    build_window_route(window_id, route).map(|_| ())
}

pub fn navigate_window_route(window_id: &str, route: &str) -> Result<(), BridgeError> {
    let (web, ns_url) = build_window_route(window_id, route)?;
    unsafe {
        let request: Retained<AnyObject> = msg_send![class(c"NSURLRequest"), requestWithURL: &*ns_url];
        let _: Option<Retained<AnyObject>> = msg_send![&web, loadRequest: &*request];
    }
    Ok(())
}

pub fn notification_route(thread_id: &str, test: bool) -> String {
    if test {
        "/settings/notifications".to_owned()
    } else {
        let encoded = thread_id
            .as_bytes()
            .iter()
            .map(|byte| {
                if byte.is_ascii_alphanumeric() || b"-._~".contains(byte) {
                    (*byte as char).to_string()
                } else {
                    format!("%{byte:02X}")
                }
            })
            .collect::<String>();
        format!("/chat/{encoded}")
    }
}

/// A modal alert; the index of the button chosen.
pub fn alert(warning: bool, message: &str, detail: &str, buttons: &[&str]) -> usize {
    // SAFETY: NSAlert on the main thread; runModal returns
    // NSAlertFirstButtonReturn (1000) plus the button index.
    unsafe {
        let alert: Retained<AnyObject> = msg_send![class(c"NSAlert"), new];
        let _: () = msg_send![&alert, setAlertStyle: if warning { 0usize } else { 1usize }];
        let _: () = msg_send![&alert, setMessageText: &*ns(message)];
        let _: () = msg_send![&alert, setInformativeText: &*ns(detail)];
        for button in buttons {
            let _: Retained<AnyObject> = msg_send![&alert, addButtonWithTitle: &*ns(button)];
        }
        let response: isize = msg_send![&alert, runModal];
        usize::try_from(response - 1000).unwrap_or(0)
    }
}

fn open_in_browser(url: &str) -> bool {
    // SAFETY: NSWorkspace/NSURL documented API.
    unsafe {
        let url: Option<Retained<AnyObject>> = msg_send![class(c"NSURL"), URLWithString: &*ns(url)];
        let Some(url) = url else { return false };
        let workspace: Retained<AnyObject> = msg_send![class(c"NSWorkspace"), sharedWorkspace];
        msg_send![&workspace, openURL: &*url]
    }
}

/// Validates `url`, asks the user for confirmation when it points to an
/// external origin, and opens it in the system browser.
pub fn confirm_and_open_in_browser(url: &str) -> Result<bool, BridgeError> {
    let parsed = policy::web_url(url)
        .ok_or_else(|| BridgeError::new("desktop.external_url_invalid", "只能在浏览器打开 http:// 或 https:// 地址。"))?;
    let origin = UI_ORIGIN.get().map_or("", String::as_str);
    if policy::needs_external_url_confirmation(&parsed, origin) {
        let chosen = alert(
            false,
            "在系统浏览器中打开外部链接？",
            &format!("{parsed}\n请确认该链接来源可信。"),
            &["取消", "打开"],
        );
        if chosen != 1 {
            return Ok(false);
        }
    }
    if open_in_browser(parsed.as_str()) {
        Ok(true)
    } else {
        Err(BridgeError::new("desktop.external_open_failed", "系统浏览器没有打开该地址。"))
    }
}

pub struct UiIvars {
    original: Option<Retained<AnyObject>>,
    /// The main UI's web view (else the preview's).
    main: bool,
    window_id: String,
}

/// NSURLErrorCancelled and WebKit's "frame load interrupted": a navigation
/// replaced by another or turned into a download, not a failure to show.
fn superseded(error: &AnyObject) -> bool {
    // SAFETY: NSError's domain and code.
    unsafe {
        let domain: Retained<NSString> = msg_send![error, domain];
        let code: isize = msg_send![error, code];
        let domain = domain.to_string();
        (domain == "NSURLErrorDomain" && code == -999) || (domain == "WebKitErrorDomain" && code == 102)
    }
}

fn url_text(url: Option<Retained<AnyObject>>) -> String {
    url.and_then(|url| {
        // SAFETY: NSURL's absoluteString.
        let text: Option<Retained<NSString>> = unsafe { msg_send![&url, absoluteString] };
        text.map(|text| text.to_string())
    })
    .unwrap_or_default()
}

/// Filters WebKit's proposed native menu without replacing editing, link, or
/// image actions. Identifiers come from WebKit's WKMenuItemIdentifiers.mm.
fn filter_context_menu(menu: &AnyObject, workbench: bool) -> bool {
    // SAFETY: WebKit supplies an NSMenu containing NSMenuItems on the main
    // thread. Removing an item does not invalidate the retained current item.
    unsafe {
        let mut index: isize = 0;
        let mut previous_separator = true;
        loop {
            let count: isize = msg_send![menu, numberOfItems];
            if index >= count {
                break;
            }
            let item: Retained<AnyObject> = msg_send![menu, itemAtIndex: index];
            let identifier: Option<Retained<NSString>> = msg_send![&item, identifier];
            let identifier = identifier.map(|identifier| identifier.to_string());
            let inspect = identifier.as_deref() == Some("WKMenuItemIdentifierInspectElement");
            let navigation = workbench && matches!(identifier.as_deref(),
                Some("WKMenuItemIdentifierGoBack" | "WKMenuItemIdentifierGoForward" | "WKMenuItemIdentifierReload"));
            let tag: isize = msg_send![&item, tag];
            let action: Option<Sel> = msg_send![&item, action];
            let stop = workbench && tag == 11 && action == Some(sel!(forwardContextMenuAction:));
            let separator: bool = msg_send![&item, isSeparatorItem];
            let submenu: Option<Retained<AnyObject>> = msg_send![&item, submenu];
            let emptied_submenu = submenu.is_some_and(|submenu| {
                let before: isize = msg_send![&submenu, numberOfItems];
                let has_items = filter_context_menu(&submenu, workbench);
                before > 0 && !has_items
            });
            if inspect || navigation || stop || emptied_submenu || (separator && previous_separator) {
                let _: () = msg_send![menu, removeItemAtIndex: index];
                continue;
            }
            previous_separator = separator;
            index += 1;
        }
        if index > 0 && previous_separator {
            let _: () = msg_send![menu, removeItemAtIndex: index - 1];
            index -= 1;
        }
        index > 0
    }
}

define_class!(
    /// Adds JavaScript dialogs and new windows to nsh's UI delegate, which
    /// it forwards everything else to.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = UiIvars]
    #[name = "MutekiUIDelegate"]
    struct UiDelegate;

    unsafe impl NSObjectProtocol for UiDelegate {}

    impl UiDelegate {
        #[unsafe(method(respondsToSelector:))]
        fn responds_to(&self, selector: Sel) -> bool {
            // SAFETY: NSObject's respondsToSelector: on super and the forwarded delegate.
            let own: bool = unsafe { msg_send![super(self), respondsToSelector: selector] };
            own || self.ivars().original.as_ref().is_some_and(|original| unsafe { msg_send![&**original, respondsToSelector: selector] })
        }

        #[unsafe(method(forwardingTargetForSelector:))]
        fn forwarding_target(&self, _selector: Sel) -> *mut AnyObject {
            self.ivars().original.as_ref().map_or(std::ptr::null_mut(), |original| Retained::as_ptr(original).cast_mut())
        }

        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, window: &AnyObject) -> Bool {
            if CLOSE_ALLOWED.get() {
                return Bool::YES;
            }
            let _delegate = retain(self);
            request_window_close(window);
            Bool::NO
        }

        #[unsafe(method(_webView:getContextMenuFromProposedMenu:forElement:userInfo:completionHandler:))]
        fn context_menu(&self, _web: &AnyObject, menu: &AnyObject, _element: &AnyObject, _user_info: *mut AnyObject, done: &Block<dyn Fn(*mut AnyObject)>) {
            let menu = if filter_context_menu(menu, self.ivars().main) {
                menu as *const AnyObject as *mut AnyObject
            } else {
                std::ptr::null_mut()
            };
            done.call((menu,));
        }

        #[unsafe(method(webView:runJavaScriptAlertPanelWithMessage:initiatedByFrame:completionHandler:))]
        fn run_alert(&self, _web: &AnyObject, message: &NSString, _frame: &AnyObject, done: &Block<dyn Fn()>) {
            alert(false, &message.to_string(), "", &["好"]);
            done.call(());
        }

        #[unsafe(method(webView:runJavaScriptConfirmPanelWithMessage:initiatedByFrame:completionHandler:))]
        fn run_confirm(&self, _web: &AnyObject, message: &NSString, _frame: &AnyObject, done: &Block<dyn Fn(Bool)>) {
            let chosen = alert(false, &message.to_string(), "", &["确定", "取消"]);
            done.call((Bool::new(chosen == 0),));
        }

        #[unsafe(method(webView:createWebViewWithConfiguration:forNavigationAction:windowFeatures:))]
        fn create_web_view(&self, web: &AnyObject, configuration: &AnyObject, action: &AnyObject, _features: &AnyObject) -> *mut AnyObject {
            // SAFETY: WKNavigationAction's request URL.
            let (request, url) = unsafe {
                let request: Retained<AnyObject> = msg_send![action, request];
                let url = url_text(msg_send![&request, URL]);
                (request, url)
            };
            if !self.ivars().main && reqwest::Url::parse(&url).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
                && policy::preview_url(&url, UI_ORIGIN.get().map_or("", String::as_str), crate::backend::backend_port()).is_err() {
                window_preview::changed(&self.ivars().window_id, web, "error", None);
                return std::ptr::null_mut();
            }
            if url.starts_with("muteki://") {
                notify(Notice::DeepLink(url));
                return std::ptr::null_mut();
            }
            if !self.ivars().main && policy::web_url(&url).is_some() {
                // SAFETY: loading the navigation's own request in the view that asked for it.
                let _: Option<Retained<AnyObject>> = unsafe { msg_send![web, loadRequest: &*request] };
                return std::ptr::null_mut();
            }
            let opened = match policy::new_window(&url) {
                NewWindow::Child if !self.ivars().main => NewWindow::Deny,
                other => other,
            };
            match opened {
                NewWindow::External(url) => {
                    if let Err(error) = confirm_and_open_in_browser(&url) {
                        eprintln!("Muteki: the system browser did not open {url}: {}", error.message);
                    }
                    std::ptr::null_mut()
                }
                NewWindow::Deny => {
                    eprintln!("Muteki: refused a new window for {url}");
                    std::ptr::null_mut()
                }
                NewWindow::Child => Retained::autorelease_return(self.child_window(configuration)),
            }
        }

        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        fn decide_policy(&self, web: &AnyObject, action: &AnyObject, decide: &Block<dyn Fn(isize)>) {
            // SAFETY: WKNavigationAction's target frame and request.
            let (main_frame, url) = unsafe {
                let frame: Option<Retained<AnyObject>> = msg_send![action, targetFrame];
                let main_frame = frame.is_some_and(|frame| msg_send![&*frame, isMainFrame]);
                let request: Retained<AnyObject> = msg_send![action, request];
                (main_frame, url_text(msg_send![&request, URL]))
            };
            if !self.ivars().main && reqwest::Url::parse(&url).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
                && policy::preview_url(&url, UI_ORIGIN.get().map_or("", String::as_str), crate::backend::backend_port()).is_err() {
                window_preview::changed(&self.ivars().window_id, web, "error", None);
                return decide.call((0,));
            }
            if self.ivars().main && main_frame {
                if crate::updates::barrier_active(){return decide.call((0,));}
                if url.starts_with("muteki://") {
                    notify(Notice::DeepLink(url));
                    return decide.call((0,));
                }
                match policy::main_navigation(&url, UI_ORIGIN.get().map_or("", String::as_str)) {
                    policy::MainNavigation::Allow => {
                        record_window_url(&self.ivars().window_id, &url);
                    }
                    policy::MainNavigation::External(url) => {
                        if let Err(error) = confirm_and_open_in_browser(&url) {
                            eprintln!("Muteki: the system browser did not open {url}: {}", error.message);
                        }
                        return decide.call((0,));
                    }
                    policy::MainNavigation::Deny => {
                        eprintln!("Muteki: refused to navigate the main window to {url}");
                        return decide.call((0,));
                    }
                }
            }
            match &self.ivars().original {
                // SAFETY: the same delegate method of the delegate this one wraps.
                Some(original) => unsafe {
                    let _: () = msg_send![&**original, webView: web, decidePolicyForNavigationAction: action, decisionHandler: decide];
                },
                None => decide.call((1,)),
            }
        }

        #[unsafe(method(webView:didStartProvisionalNavigation:))]
        fn did_start_navigation(&self,web:&AnyObject,navigation:*mut AnyObject){
            if self.ivars().main&&self.ivars().original.is_none(){notify(Notice::WindowLoading(self.ivars().window_id.clone()));}
            if !self.ivars().main{window_preview::changed(&self.ivars().window_id,web,"loading",None);}
            if let Some(original)=&self.ivars().original{let responds:bool=unsafe{msg_send![&**original,respondsToSelector:sel!(webView:didStartProvisionalNavigation:)]};if responds{unsafe{let _:()=msg_send![&**original,webView:web,didStartProvisionalNavigation:navigation];}}}
        }
        #[unsafe(method(webView:didFinishNavigation:))]
        fn did_finish_navigation(&self,web:&AnyObject,navigation:*mut AnyObject){
            if !self.ivars().main{window_preview::changed(&self.ivars().window_id,web,"loaded",None);}
            if let Some(original)=&self.ivars().original{let responds:bool=unsafe{msg_send![&**original,respondsToSelector:sel!(webView:didFinishNavigation:)]};if responds{unsafe{let _:()=msg_send![&**original,webView:web,didFinishNavigation:navigation];}}}
        }
        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail(&self, web: &AnyObject, navigation: *mut AnyObject, error: &AnyObject) {
            if superseded(error) {
                return;
            }
            if !self.ivars().main{window_preview::changed(&self.ivars().window_id,web,"error",Some(error));}
            if let Some(original) = &self.ivars().original {
                // SAFETY: the same delegate method of the delegate this one wraps.
                let _: () = unsafe { msg_send![&**original, webView: web, didFailNavigation: navigation, withError: error] };
            }
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional(&self, web: &AnyObject, navigation: *mut AnyObject, error: &AnyObject) {
            if superseded(error) {
                return;
            }
            if !self.ivars().main{window_preview::changed(&self.ivars().window_id,web,"error",Some(error));}
            if let Some(original) = &self.ivars().original {
                // SAFETY: the same delegate method of the delegate this one wraps.
                let _: () = unsafe { msg_send![&**original, webView: web, didFailProvisionalNavigation: navigation, withError: error] };
            }
        }

        #[unsafe(method(webViewDidClose:))]
        fn web_view_did_close(&self, web: &AnyObject) {
            // SAFETY: NSView's window.
            let window: Option<Retained<AnyObject>> = unsafe { msg_send![web, window] };
            let Some(window) = window else { return };
            CHILDREN.with(|children| {
                let mut children = children.borrow_mut();
                if let Some(index) = children.iter().position(|child| same(child, &window)) {
                    let child = children.remove(index);
                    // SAFETY: closing a window this delegate opened.
                    let _: () = unsafe { msg_send![&child, close] };
                }
            });
        }
    }
);

pub struct ScriptMessageIvars {
    window_id: String,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = ScriptMessageIvars]
    #[name = "MutekiScriptMessageHandler"]
    struct ScriptMessageHandler;

    unsafe impl NSObjectProtocol for ScriptMessageHandler {}

    impl ScriptMessageHandler {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive_message(&self, _controller: &AnyObject, message: &AnyObject) {
            unsafe {
                let frame: Option<Retained<AnyObject>> = msg_send![message, frameInfo];
                let is_main = frame.as_ref().is_some_and(|f| msg_send![&**f, isMainFrame]);
                if !is_main {
                    return;
                }
                let body: Option<Retained<AnyObject>> = msg_send![message, body];
                let Some(body) = body else { return };
                let is_str: bool = msg_send![&body, isKindOfClass: class(c"NSString")];
                if !is_str {
                    return;
                }
                let text: Retained<NSString> = Retained::cast_unchecked(body);
                notify(Notice::WindowMessage(self.ivars().window_id.clone(), text.to_string()));
            }
        }
    }
);

impl ScriptMessageHandler {
    fn new(mtm: MainThreadMarker, window_id: String) -> Retained<Self> {
        if let Some(protocol) = AnyProtocol::get(c"WKScriptMessageHandler") {
            unsafe extern "C" {
                fn class_addProtocol(class: *mut AnyClass, protocol: *const AnyProtocol) -> Bool;
            }
            let cls = Self::class();
            if !cls.conforms_to(protocol) {
                unsafe {
                    class_addProtocol(cls as *const AnyClass as *mut AnyClass, protocol);
                }
            }
        }
        let this = mtm.alloc::<Self>().set_ivars(ScriptMessageIvars { window_id });
        unsafe { msg_send![super(this), init] }
    }
}

struct WorkbenchWindowEntry {
    window: Retained<AnyObject>,
    web: Retained<AnyObject>,
    _delegate: Retained<UiDelegate>,
    _handler: Retained<ScriptMessageHandler>,
}

thread_local! {
    static DELEGATE: RefCell<Option<Retained<UiDelegate>>> = const { RefCell::new(None) };
    static CHILDREN: RefCell<Vec<Retained<AnyObject>>> = const { RefCell::new(Vec::new()) };
    static WORKBENCH_WINDOWS: RefCell<HashMap<String, WorkbenchWindowEntry>> = RefCell::new(HashMap::new());
    static CLOSE_ALLOWED: Cell<bool> = const { Cell::new(false) };
    static CLOSE_PENDING: RefCell<Option<String>> = const { RefCell::new(None) };
    static FLUSH_CALLBACKS: RefCell<HashMap<String, Box<dyn FnOnce(bool, String)>>> = RefCell::new(HashMap::new());
    static QUIT_OR_CLOSE_IN_FLIGHT: Cell<bool> = const { Cell::new(false) };
    static QUIT_CONFIRMATION: RefCell<Option<Value>> = const { RefCell::new(None) };
    static FOCUS_OBSERVERS: RefCell<Vec<Retained<AnyObject>>> = const { RefCell::new(Vec::new()) };
}

fn workbench_web_view(window_id: &str) -> Option<Retained<AnyObject>> {
    if window_id.is_empty() || window_id == "main" {
        return web_view(MAIN_ID);
    }
    WORKBENCH_WINDOWS.with(|windows| windows.borrow().get(window_id).map(|entry| retain(&entry.web)))
}

/// Match the native traffic lights, titlebar and resize background to the page.
pub fn set_window_chrome(window_id: &str, theme: &str, mode: Option<&str>, title: &str) -> Result<Value, BridgeError> {
    let web = workbench_web_view(window_id).ok_or_else(|| BridgeError::new("desktop.window_missing", "工作台窗口不可用。"))?;
    unsafe {
        let window: Option<Retained<AnyObject>> = msg_send![&web, window];
        if let Some(window) = window {
            let appearance: Option<Retained<AnyObject>> = if mode == Some("system") {
                None
            } else {
                let name = if theme == "light" { "NSAppearanceNameAqua" } else { "NSAppearanceNameDarkAqua" };
                msg_send![class(c"NSAppearance"), appearanceNamed: &*ns(name)]
            };
            let _: () = msg_send![&window, setAppearance: appearance.as_deref()];
            let window_title = if title.starts_with(crate::APP_NAME) { title.to_owned() } else { format!("{} · {title}", crate::APP_NAME) };
            let _: () = msg_send![&window, setTitle: &*ns(&window_title)];
            let color: Retained<AnyObject> = msg_send![class(c"NSColor"), windowBackgroundColor];
            let _: () = msg_send![&window, setBackgroundColor: &*color];
        }
        let back: Bool = msg_send![&web, canGoBack];
        let forward: Bool = msg_send![&web, canGoForward];
        Ok(json!({ "canGoBack": back.as_bool(), "canGoForward": forward.as_bool() }))
    }
}

pub fn window_action(window_id: &str, action: &str) -> Result<Value, BridgeError> {
    let web = workbench_web_view(window_id).ok_or_else(|| BridgeError::new("desktop.window_missing", "工作台窗口不可用。"))?;
    unsafe {
        match action {
            "back" => { let _: Option<Retained<AnyObject>> = msg_send![&web, goBack]; }
            "forward" => { let _: Option<Retained<AnyObject>> = msg_send![&web, goForward]; }
            "drag" | "zoom" | "close" => {
                let window: Option<Retained<AnyObject>> = msg_send![&web, window];
                if let Some(window) = window {
                    if action == "close" {
                        let _: () = msg_send![&window, performClose: std::ptr::null::<AnyObject>()];
                    } else if action == "zoom" {
                        let _: () = msg_send![&window, zoom: std::ptr::null::<AnyObject>()];
                    } else {
                        let app: Retained<AnyObject> = msg_send![class(c"NSApplication"), sharedApplication];
                        let event: Option<Retained<AnyObject>> = msg_send![&app, currentEvent];
                        if let Some(event) = event {
                            let kind: usize = msg_send![&event, type];
                            if kind == 1 { let _: () = msg_send![&window, performWindowDragWithEvent: &*event]; }
                        }
                    }
                }
            }
            _ => return Err(BridgeError::new("desktop.window_action_invalid", "窗口操作不可用。")),
        }
    }
    Ok(Value::Null)
}

pub fn send_to_window(window_id: &str, script: &str) {
    let Some(web) = workbench_web_view(window_id) else {
        return;
    };
    let script = ns(script);
    unsafe {
        let _: () = msg_send![&web, evaluateJavaScript: &*script, completionHandler: std::ptr::null::<AnyObject>()];
    }
}

pub fn spawn_workbench_window(window_id: &str, url: &str, bridge_script: &str) -> Result<(), BridgeError> {
    let mtm = MainThreadMarker::new().ok_or_else(|| BridgeError::new("desktop.window_create_failed", "必须在主线程创建窗口。"))?;
    let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1280.0, 860.0));
    unsafe {
        let config: Retained<AnyObject> = msg_send![class(c"WKWebViewConfiguration"), new];
        let controller: Retained<AnyObject> = msg_send![&config, userContentController];
        let handler = ScriptMessageHandler::new(mtm, window_id.to_owned());
        let _: () = msg_send![&controller, addScriptMessageHandler: &*handler, name: &*ns("nsh")];
        let user_script_alloc: Allocated<AnyObject> = msg_send![class(c"WKUserScript"), alloc];
        let user_script: Retained<AnyObject> = msg_send![
            user_script_alloc,
            initWithSource: &*ns(bridge_script),
            injectionTime: 0isize,
            forMainFrameOnly: Bool::YES
        ];
        let _: () = msg_send![&controller, addUserScript: &*user_script];

        let web_alloc: Allocated<AnyObject> = msg_send![class(c"WKWebView"), alloc];
        let web: Retained<AnyObject> = msg_send![web_alloc, initWithFrame: frame, configuration: &*config];
        configure_web_view(&web);
        let delegate = UiDelegate::new(mtm, None, true, window_id.to_owned());
        let _: () = msg_send![&web, setUIDelegate: &*delegate];
        let _: () = msg_send![&web, setNavigationDelegate: &*delegate];

        let ns_url: Option<Retained<AnyObject>> = msg_send![class(c"NSURL"), URLWithString: &*ns(url)];
        let ns_url = ns_url.ok_or_else(|| BridgeError::new("desktop.window_create_failed", "新窗口地址无效。"))?;
        let request: Retained<AnyObject> = msg_send![class(c"NSURLRequest"), requestWithURL: &*ns_url];
        let _: Option<Retained<AnyObject>> = msg_send![&web, loadRequest: &*request];

        let window_alloc: Allocated<AnyObject> = msg_send![class(c"NSWindow"), alloc];
        let window: Retained<AnyObject> = msg_send![
            window_alloc,
            initWithContentRect: frame,
            styleMask: (15usize | (1 << 15)),
            backing: 2usize,
            defer: false
        ];
        let _: () = msg_send![&window, setReleasedWhenClosed: false];
        let _: () = msg_send![&window, setDelegate: &*delegate];
        let _: () = msg_send![&window, setTitle: &*ns(crate::APP_NAME)];
        let toolbar_alloc: Allocated<AnyObject> = msg_send![class(c"NSToolbar"), alloc];
        let toolbar: Retained<AnyObject> = msg_send![toolbar_alloc, initWithIdentifier: &*ns("muteki.titlebar")];
        let _: () = msg_send![&window, setToolbar: &*toolbar];
        let _: () = msg_send![&window, setToolbarStyle: 4isize];
        let _: () = msg_send![&window, setTitlebarAppearsTransparent: Bool::YES];
        let _: () = msg_send![&window, setTitleVisibility: 1isize];
        let _: () = msg_send![&window, setContentView: &*web];
        let _: () = msg_send![&window, center];
        let _: () = msg_send![&window, makeKeyAndOrderFront: std::ptr::null::<AnyObject>()];

        WORKBENCH_WINDOWS.with(|windows| {
            windows.borrow_mut().insert(
                window_id.to_owned(),
                WorkbenchWindowEntry {
                    window,
                    web,
                    _delegate: delegate,
                    _handler: handler,
                },
            );
        });
        register_window(window_id, url);
        report_focus_state_on_main();
    }
    Ok(())
}

impl UiDelegate {
    fn new(mtm: MainThreadMarker, original: Option<Retained<AnyObject>>, main: bool, window_id: String) -> Retained<Self> {
        let this = mtm.alloc::<Self>().set_ivars(UiIvars { original, main, window_id });
        // SAFETY: NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }

    /// A window for a page-made document; it shares the opener's
    /// configuration, as WebKit requires.
    fn child_window(&self, configuration: &AnyObject) -> Retained<AnyObject> {
        CHILDREN.with(|children| {
            // SAFETY: NSWindow's isVisible.
            children.borrow_mut().retain(|child| unsafe { msg_send![&**child, isVisible] });
        });
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1000.0, 760.0));
        // SAFETY: WKWebView and NSWindow designated initializers on the main thread.
        unsafe {
            let web: Allocated<AnyObject> = msg_send![class(c"WKWebView"), alloc];
            let web: Retained<AnyObject> = msg_send![web, initWithFrame: frame, configuration: configuration];
            configure_web_view(&web);
            let _: () = msg_send![&web, setUIDelegate: self];
            let window: Allocated<AnyObject> = msg_send![class(c"NSWindow"), alloc];
            let window: Retained<AnyObject> = msg_send![window, initWithContentRect: frame, styleMask: 15usize, backing: 2usize, defer: false];
            let _: () = msg_send![&window, setReleasedWhenClosed: false];
            let _: () = msg_send![&window, setTitle: &*ns(crate::APP_NAME)];
            let _: () = msg_send![&window, setContentView: &*web];
            let _: () = msg_send![&window, center];
            let _: () = msg_send![&window, makeKeyAndOrderFront: std::ptr::null::<AnyObject>()];
            CHILDREN.with(|children| children.borrow_mut().push(window));
            web
        }
    }
}

static ORIGINAL_TERMINATE: OnceLock<usize> = OnceLock::new();
static ORIGINAL_SHOULD_CLOSE: OnceLock<usize> = OnceLock::new();
static ORIGINAL_REOPEN: OnceLock<usize> = OnceLock::new();
static ORIGINAL_OPEN_URLS: OnceLock<usize> = OnceLock::new();

unsafe extern "C-unwind" fn should_terminate(this: *mut AnyObject, cmd: Sel, app: *mut AnyObject) -> usize {
    if CLOSE_ALLOWED.get() {
        if let Some(original) = ORIGINAL_TERMINATE.get() {
            // SAFETY: the IMP this method replaced, with its own signature.
            let original: unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> usize = unsafe { std::mem::transmute(*original) };
            return unsafe { original(this, cmd, app) };
        }
        return 1;
    }
    request_quit();
    0
}

unsafe extern "C-unwind" fn window_should_close(this: *mut AnyObject, cmd: Sel, window: *mut AnyObject) -> Bool {
    if CLOSE_ALLOWED.get() {
        if let Some(original) = ORIGINAL_SHOULD_CLOSE.get() {
            // SAFETY: the IMP this method replaced, with its own signature.
            let original: unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> Bool = unsafe { std::mem::transmute(*original) };
            return unsafe { original(this, cmd, window) };
        }
        return Bool::YES;
    }
    if !window.is_null() {
        request_window_close(unsafe { &*window });
    }
    Bool::NO
}

unsafe extern "C-unwind" fn application_reopen(_this: *mut AnyObject, _cmd: Sel, _app: *mut AnyObject, _visible: Bool) -> Bool {
    let _ = restore_and_focus_window("main");
    Bool::YES
}

unsafe extern "C-unwind" fn application_open_urls(_this: *mut AnyObject, _cmd: Sel, _app: *mut AnyObject, urls: *mut AnyObject) {
    if urls.is_null() {
        return;
    }
    unsafe {
        let count: usize = msg_send![&*urls, count];
        for i in 0..count {
            let url: Option<Retained<AnyObject>> = msg_send![&*urls, objectAtIndex: i];
            let text = url_text(url);
            if !text.is_empty() {
                notify(Notice::DeepLink(text));
            }
        }
    }
}

unsafe extern "C-unwind" fn handle_get_url_event(_this: *mut AnyObject, _cmd: Sel, event: *mut AnyObject, _reply: *mut AnyObject) {
    if event.is_null() {
        return;
    }
    const KEY_DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");
    // SAFETY: an NSAppleEventDescriptor delivered by NSAppleEventManager.
    let text = unsafe {
        let direct: Option<Retained<AnyObject>> = msg_send![&*event, paramDescriptorForKeyword: KEY_DIRECT_OBJECT];
        let Some(direct) = direct else { return };
        let value: Option<Retained<NSString>> = msg_send![&*direct, stringValue];
        value.map(|value| value.to_string()).unwrap_or_default()
    };
    if !text.is_empty() {
        notify(Notice::DeepLink(text));
    }
}

/// Routes Quit, window close, Dock reopen, and custom URL scheme events.
/// nsh's app delegate exists once `run_app` is running.
fn install_close_barrier() {
    if ORIGINAL_TERMINATE.get().is_some() {
        return;
    }
    let Some(delegate) = AnyClass::get(c"NshAppDelegate") else {
        eprintln!("Muteki: nsh's app delegate is missing; closing skips the draft flush.");
        return;
    };
    let delegate = delegate as *const AnyClass as *mut AnyClass;
    // SAFETY: replacing/adding delegate methods with matching Objective-C signatures.
    unsafe {
        let terminate: Imp = std::mem::transmute(should_terminate as unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> usize);
        let close: Imp = std::mem::transmute(window_should_close as unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> Bool);
        let reopen: Imp = std::mem::transmute(application_reopen as unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject, Bool) -> Bool);
        let open_urls: Imp = std::mem::transmute(application_open_urls as unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject, *mut AnyObject));
        if let Some(original) = objc2::ffi::class_replaceMethod(delegate, sel!(applicationShouldTerminate:), terminate, c"Q@:@".as_ptr()) {
            let _ = ORIGINAL_TERMINATE.set(original as usize);
        }
        if let Some(original) = objc2::ffi::class_replaceMethod(delegate, sel!(windowShouldClose:), close, c"B@:@".as_ptr()) {
            let _ = ORIGINAL_SHOULD_CLOSE.set(original as usize);
        }
        if let Some(original) = objc2::ffi::class_replaceMethod(delegate, sel!(applicationShouldHandleReopen:hasVisibleWindows:), reopen, c"B@:@B".as_ptr()) {
            let _ = ORIGINAL_REOPEN.set(original as usize);
        }
        if let Some(original) = objc2::ffi::class_replaceMethod(delegate, sel!(application:openURLs:), open_urls, c"v@:@@".as_ptr()) {
            let _ = ORIGINAL_OPEN_URLS.set(original as usize);
        }
        let get_url: Imp = std::mem::transmute(handle_get_url_event as unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject, *mut AnyObject));
        objc2::ffi::class_addMethod(delegate, sel!(handleGetURLEvent:withReplyEvent:), get_url, c"v@:@@".as_ptr());
        let app: Retained<AnyObject> = msg_send![class(c"NSApplication"), sharedApplication];
        let target: Option<Retained<AnyObject>> = msg_send![&*app, delegate];
        if let Some(target) = target {
            const GET_URL: u32 = u32::from_be_bytes(*b"GURL");
            let manager: Retained<AnyObject> = msg_send![class(c"NSAppleEventManager"), sharedAppleEventManager];
            let _: () = msg_send![&*manager, setEventHandler: &*target, andSelector: sel!(handleGetURLEvent:withReplyEvent:), forEventClass: GET_URL, andEventID: GET_URL];
        }
    }
}

/// Quits without asking the page again, once its drafts are known saved.
pub fn quit_now() {
    allow_close_and_quit();
}

fn allow_close_and_quit() {
    CLOSE_ALLOWED.set(true);
    nsh_platform_macos::quit_app();
}

/// Runs the persistence flush barrier (`before-close`) before an action such as
/// reload, notification navigation, deep-link navigation, window close, or quit.
pub fn request_flush_barrier(reason: &str, done: impl FnOnce(bool, String) + 'static) {
    let token=uuid::Uuid::new_v4().to_string();
    FLUSH_CALLBACKS.with(|callbacks|{callbacks.borrow_mut().insert(token.clone(),Box::new(done));});
    let _=reason;
    std::thread::spawn(move||{
        let result=crate::updates::flush_drafts();
        let owned=result.is_ok();
        on_main(move||{
            let result=result.and_then(|_|crate::updates::validate_barrier());
            if owned{crate::updates::release_barrier();}
            if let Some(callback)=FLUSH_CALLBACKS.with(|callbacks|callbacks.borrow_mut().remove(&token)) {
                match result {Ok(())=>callback(true,String::new()),Err(error)=>callback(false,error.message)}
            }
        });
    });
}

/// Triggers the full app-quit barrier (draft flush + active-task confirmation).
pub fn request_quit() {
    if QUIT_OR_CLOSE_IN_FLIGHT.get() {
        if let Some(request) = QUIT_CONFIRMATION.with(|pending| pending.borrow().clone()) {
            present_quit_confirmation(request);
        }
        return;
    }
    QUIT_OR_CLOSE_IN_FLIGHT.set(true);
    if !BRIDGE_READY.load(Ordering::SeqCst) && !crate::updates::has_documents() {
        check_quit_activity(false);
        return;
    }
    request_flush_barrier("quit", move |persisted, error| {
        if !persisted {
            let (title, detail) = if error.is_empty() {
                (
                    "无法确认草稿已保存",
                    "页面没有在 5 秒内确认输入内容已保存。保持打开可以继续编辑；仍然退出可能丢失尚未保存的输入。",
                )
            } else {
                ("草稿保存失败。仍然退出 Muteki？", error.as_str())
            };
            if alert(true, title, detail, &["保持打开", "仍然退出"]) != 1 {
                QUIT_OR_CLOSE_IN_FLIGHT.set(false);
                return;
            }
        }
        check_quit_activity(!persisted);
    });
}

fn check_quit_activity(allow_unsaved_drafts: bool) {
    std::thread::spawn(move || {
        let activity = crate::backend::quit_check();
        on_main(move || {
            let Some(activity) = activity else {
                finish_quit(allow_unsaved_drafts, None);
                return;
            };
            let window_id = unsafe {
                let app: Retained<AnyObject> = msg_send![class(c"NSApplication"), sharedApplication];
                let key: Option<Retained<AnyObject>> = msg_send![&app, keyWindow];
                WORKBENCH_WINDOWS.with(|windows| {
                    windows.borrow().iter().find(|(_, entry)| key.as_ref().is_some_and(|key| same(key, &entry.window)))
                        .map(|(id, _)| id.clone()).unwrap_or_else(|| "main".to_owned())
                })
            };
            if !BRIDGE_READY.load(Ordering::SeqCst) {
                let detail = crate::backend::describe_tasks(&activity);
                if alert(true, "仍有后台任务在运行", &format!("{detail}\n\n退出会停止本机 Runner，并中断正在运行的任务。"), &["继续运行", "仍然退出"]) == 1 {
                    finish_quit(allow_unsaved_drafts, None);
                } else {
                    QUIT_OR_CLOSE_IN_FLIGHT.set(false);
                    std::thread::spawn(|| {
                        if let Err(error) = crate::backend::set_writes(true) { eprintln!("Muteki: could not cancel quit: {error}"); }
                    });
                }
                return;
            }
            let mut request = activity;
            request["token"] = json!(uuid::Uuid::new_v4().to_string());
            request["windowId"] = json!(window_id);
            request["allowUnsavedDrafts"] = json!(allow_unsaved_drafts);
            QUIT_CONFIRMATION.with(|pending| *pending.borrow_mut() = Some(request.clone()));
            present_quit_confirmation(request);
        });
    });
}

fn complete_quit(reply: Option<Value>) {
    QUIT_CONFIRMATION.with(|pending| pending.borrow_mut().take());
    if let Some(id) = reply {
        notify(Notice::Reply(id, Ok(Value::Null)));
    }
    allow_close_and_quit();
}

fn finish_quit(allow_unsaved_drafts: bool, reply: Option<Value>) {
    std::thread::spawn(move || {
        let result = crate::updates::flush_drafts();
        let owned = result.is_ok();
        on_main(move || {
            let result = result.map_err(|error| error.message).and_then(|_| {
                crate::updates::with_valid_barrier(|| {
                    complete_quit(reply.clone());
                    Ok(())
                })
            });
            if let Err(error) = result {
                if owned {
                    crate::updates::release_barrier();
                }
                if allow_unsaved_drafts {
                    complete_quit(reply);
                } else {
                    resume_after_quit_failure(error, reply);
                }
            }
        });
    });
}

fn resume_after_quit_failure(error: String, reply: Option<Value>) {
    std::thread::spawn(move || {
        let restored = if crate::backend::control_token().is_some() {
            crate::backend::set_writes(true)
        } else {
            Ok(())
        };
        on_main(move || {
            if let Some(request) = QUIT_CONFIRMATION.with(|pending| pending.borrow_mut().take()) {
                notify(Notice::WindowEvent(request["windowId"].as_str().unwrap_or("main").to_owned(), "quit-requested".into(), Value::Null));
            }
            QUIT_OR_CLOSE_IN_FLIGHT.set(false);
            let mut message = format!("草稿未能保存，尚未退出。{error}");
            if let Err(error) = restored {
                message.push_str(&format!("\n恢复服务写入失败：{error}"));
            }
            if let Some(id) = reply {
                notify(Notice::Reply(id, Err(BridgeError::new("desktop.quit_drafts_unsaved", message.clone()))));
            }
            alert(true, "尚未退出 Muteki", &message, &["继续编辑"]);
        });
    });
}

fn present_quit_confirmation(request: Value) {
    let window = request["windowId"].as_str().unwrap_or("main");
    if let Err(error) = restore_and_focus_window(window) {
        eprintln!("Muteki: could not focus quit confirmation: {}", error.message);
    }
    notify(Notice::WindowEvent(window.to_owned(), "quit-requested".into(), request));
}

pub fn pending_quit(window: &str) -> Value {
    QUIT_CONFIRMATION.with(|pending| pending.borrow().as_ref().filter(|request| request["windowId"] == window).cloned().unwrap_or(Value::Null))
}

/// Only the window holding this request may answer it. A stale/double answer
/// cannot quit the app or dismiss a later confirmation.
pub fn answer_quit(id: Value, window: &str, token: &str, confirmed: bool) {
    let valid = QUIT_CONFIRMATION.with(|pending| {
        let mut pending = pending.borrow_mut();
        let Some(request) = pending.as_mut().filter(|request| request["windowId"] == window && request["token"] == token && request["answering"] != true) else { return false };
        request["answering"] = json!(true);
        true
    });
    if !valid {
        notify(Notice::Reply(id, Err(BridgeError::new("desktop.quit_stale", "退出请求已改变，请重新操作。"))));
        return;
    }
    if confirmed {
        let allow_unsaved_drafts = QUIT_CONFIRMATION.with(|pending| pending.borrow().as_ref().is_some_and(|request| request["allowUnsavedDrafts"] == true));
        finish_quit(allow_unsaved_drafts, Some(id));
        return;
    }
    std::thread::spawn(move || {
        let result = crate::backend::set_writes(true).map_err(|error| BridgeError::new("desktop.quit_cancel_failed", error));
        on_main(move || {
            if result.is_ok() {
                if let Some(request) = QUIT_CONFIRMATION.with(|pending| pending.borrow_mut().take()) {
                    notify(Notice::WindowEvent(request["windowId"].as_str().unwrap_or("main").to_owned(), "quit-requested".into(), Value::Null));
                }
                QUIT_OR_CLOSE_IN_FLIGHT.set(false);
            } else {
                QUIT_CONFIRMATION.with(|pending| {
                    if let Some(request) = pending.borrow_mut().as_mut() { request["answering"] = json!(false); }
                });
            }
            notify(Notice::Reply(id, result.map(|_| Value::Null)));
        });
    });
}

/// Triggers the window-close barrier on macOS: flushes drafts, then hides the
/// primary window (keeping the app alive for notifications and Dock reopen) or
/// closes a secondary workbench window.
fn request_window_close(window: &AnyObject) {
    if QUIT_OR_CLOSE_IN_FLIGHT.get() {
        return;
    }
    let window_ref = retain(window);
    let secondary_id = WORKBENCH_WINDOWS.with(|windows| {
        windows
            .borrow()
            .iter()
            .find(|(_, entry)| same(&entry.window, &window_ref))
            .map(|(id, _)| id.clone())
    });
    if !BRIDGE_READY.load(Ordering::SeqCst) && !crate::updates::has_documents() {
        finish_window_close(&window_ref, secondary_id);
        return;
    }
    QUIT_OR_CLOSE_IN_FLIGHT.set(true);
    request_flush_barrier("window-close", move |persisted, error| {
        QUIT_OR_CLOSE_IN_FLIGHT.set(false);
        if !persisted {
            let (title, detail) = if error.is_empty() {
                (
                    "无法确认草稿已保存",
                    "页面没有在 5 秒内确认输入内容已保存。保持打开可以继续编辑；仍然关闭可能丢失尚未保存的输入。",
                )
            } else {
                ("草稿保存失败。仍然关闭窗口？", error.as_str())
            };
            if alert(true, title, detail, &["保持打开", "关闭窗口"]) != 1 {
                return;
            }
        }
        finish_window_close(&window_ref, secondary_id);
    });
}

fn finish_window_close(window: &AnyObject, secondary_id: Option<String>) {
    if let Some(id) = &secondary_id {
        window_urls().lock().unwrap_or_else(|error| error.into_inner()).remove(id);
    }
    unsafe {
        let _: () = msg_send![window, orderOut: std::ptr::null::<AnyObject>()];
    }
    if let Some(id) = secondary_id {
        WORKBENCH_WINDOWS.with(|windows| {
            windows.borrow_mut().remove(&id);
        });
        notify(Notice::WindowClosed(id));
    }
    report_focus_state_on_main();
}

/// The page's answer to a close or persistence-barrier request.
pub fn acknowledge_close(token: &str, persisted: bool, error: &str) -> Result<(), BridgeError> {
    let callback = FLUSH_CALLBACKS.with(|callbacks| callbacks.borrow_mut().remove(token));
    let current = CLOSE_PENDING.with(|pending| {
        let mut pending = pending.borrow_mut();
        let current = pending.as_deref() == Some(token);
        if current {
            *pending = None;
        }
        current
    });
    if let Some(cb) = callback {
        eprintln!("Muteki: the page acknowledged the flush barrier (drafts saved: {persisted})");
        cb(persisted, error.to_owned());
        return Ok(());
    }
    if !current {
        return Err(BridgeError::new("desktop.close_ack_stale", "关闭请求已改变。"));
    }
    eprintln!("Muteki: the page acknowledged the close request (drafts saved: {persisted})");
    if !persisted && alert(true, "草稿保存失败。仍然关闭窗口？", error, &["保持打开", "关闭"]) != 1 {
        return Ok(());
    }
    allow_close_and_quit();
    Ok(())
}

/// Disables opt-in inspection and allows the display's refresh rate. Native
/// context menus are filtered separately by UiDelegate at presentation time.
pub fn configure_web_view(web: &AnyObject) {
    set_web_inspector_enabled(web, false);
    unsafe {
        let config: Option<Retained<AnyObject>> = msg_send![web, configuration];
        if let Some(config) = config {
            let prefs: Option<Retained<AnyObject>> = msg_send![&config, preferences];
            if let Some(prefs) = prefs {
                allow_display_refresh_rate(&prefs);
            }
        }
    }
}

/// Inspection is opt-in through the browser preview's explicit tools action;
/// it is never enabled for the application's own interface.
fn set_web_inspector_enabled(web: &AnyObject, enabled: bool) {
    unsafe {
        let can_set_inspectable: bool = msg_send![web, respondsToSelector: sel!(setInspectable:)];
        if can_set_inspectable {
            let _: () = msg_send![web, setInspectable: Bool::new(enabled)];
        }
        let config: Option<Retained<AnyObject>> = msg_send![web, configuration];
        if let Some(config) = config {
            let prefs: Option<Retained<AnyObject>> = msg_send![&config, preferences];
            if let Some(prefs) = prefs {
                let value: Retained<AnyObject> = msg_send![class(c"NSNumber"), numberWithBool: Bool::new(enabled)];
                let _: () = msg_send![&prefs, setValue: &*value, forKey: &*ns("developerExtrasEnabled")];
            }
        }
    }
}

/// WebKit paces rendering updates near 60fps until its internal
/// `PreferPageRenderingUpdatesNear60FPSEnabled` feature is turned off, which
/// halves the frame rate on 120Hz ProMotion displays. The feature is only
/// reachable through WKPreferences SPI, so each step checks it exists.
fn allow_display_refresh_rate(prefs: &AnyObject) {
    const FEATURE: &str = "PreferPageRenderingUpdatesNear60FPSEnabled";
    unsafe {
        let preferences = class(c"WKPreferences");
        let listable: bool = msg_send![preferences, respondsToSelector: sel!(_features)];
        let settable: bool = msg_send![prefs, respondsToSelector: sel!(_setEnabled:forFeature:)];
        if !listable || !settable {
            return;
        }
        let features: Option<Retained<AnyObject>> = msg_send![preferences, _features];
        let Some(features) = features else { return };
        let count: usize = msg_send![&features, count];
        for index in 0..count {
            let feature: Retained<AnyObject> = msg_send![&features, objectAtIndex: index];
            let has_key: bool = msg_send![&feature, respondsToSelector: sel!(key)];
            if !has_key {
                continue;
            }
            let key: Option<Retained<NSString>> = msg_send![&feature, key];
            if key.is_some_and(|key| key.to_string() == FEATURE) {
                let _: () = msg_send![prefs, _setEnabled: Bool::NO, forFeature: &*feature];
                return;
            }
        }
    }
}

/// Puts a [`UiDelegate`] in front of nsh's delegate of `web`, as its UI and
/// navigation delegate (nsh uses one object for both).
fn wrap(mtm: MainThreadMarker, web: &AnyObject, main: bool) -> Retained<UiDelegate> {
    configure_web_view(web);
    // SAFETY: WKWebView's UIDelegate and navigationDelegate properties (weak;
    // the caller retains the wrapper).
    unsafe {
        let original: Option<Retained<AnyObject>> = msg_send![web, UIDelegate];
        let navigation: Option<Retained<AnyObject>> = msg_send![web, navigationDelegate];
        let shared = match (&original, &navigation) {
            (Some(a), Some(b)) => same(a, b),
            (None, None) => true,
            _ => false,
        };
        let delegate = UiDelegate::new(mtm, original, main, "main".to_owned());
        let _: () = msg_send![web, setUIDelegate: &*delegate];
        if shared {
            let _: () = msg_send![web, setNavigationDelegate: &*delegate];
        } else {
            eprintln!("Muteki: the web view's navigation delegate is separate; navigation stays nsh's.");
        }
        delegate
    }
}

fn install_focus_observers() {
    FOCUS_OBSERVERS.with(|slot| {
        if !slot.borrow().is_empty() {
            return;
        }
        // SAFETY: NSNotificationCenter observer registration on the main thread; retained tokens stay alive for process lifetime.
        unsafe {
            let center: Retained<AnyObject> = msg_send![class(c"NSNotificationCenter"), defaultCenter];
            let names = [
                "NSApplicationDidBecomeActiveNotification",
                "NSApplicationDidResignActiveNotification",
                "NSApplicationDidHideNotification",
                "NSApplicationDidUnhideNotification",
                "NSWindowDidBecomeKeyNotification",
                "NSWindowDidResignKeyNotification",
                "NSWindowDidMiniaturizeNotification",
                "NSWindowDidDeminiaturizeNotification",
            ];
            let mut tokens = Vec::with_capacity(names.len());
            for name in names {
                let block = RcBlock::new(|_note: *mut AnyObject| {
                    refresh_focus_state();
                });
                let token: Retained<AnyObject> = msg_send![
                    &center,
                    addObserverForName: &*ns(name),
                    object: std::ptr::null::<AnyObject>(),
                    queue: std::ptr::null::<AnyObject>(),
                    usingBlock: &*block
                ];
                tokens.push(token);
            }
            *slot.borrow_mut() = tokens;
        }
    });
}

/// Takes over the main web view's delegates and the close path, once the
/// web view exists. Main-window navigation stays on `origin`.
pub fn install(origin: String) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    record_window_url("main", &origin);
    let _ = UI_ORIGIN.set(origin);
    install_close_barrier();
    install_focus_observers();
    report_focus_state_on_main();
    if DELEGATE.with(|delegate| delegate.borrow().is_some()) {
        return;
    }
    let Some(web) = web_view(MAIN_ID) else {
        eprintln!("Muteki: the main web view is not mounted yet.");
        return;
    };
    let delegate = wrap(mtm, &web, true);
    DELEGATE.with(|slot| *slot.borrow_mut() = Some(delegate));
}

/// Loads `url` in the preview, keeping its history.
pub fn load(url: &str) -> Result<(), BridgeError> {
    policy::preview_url(url, UI_ORIGIN.get().map_or("", String::as_str), crate::backend::backend_port())?;
    let web = preview_view()?;
    // SAFETY: NSURL/NSURLRequest and WKWebView's loadRequest:.
    unsafe {
        let target: Option<Retained<AnyObject>> = msg_send![class(c"NSURL"), URLWithString: &*ns(url)];
        let target = target.ok_or_else(|| BridgeError::new("desktop.browser_url_invalid", "只能打开 http:// 或 https:// 地址。"))?;
        let request: Retained<AnyObject> = msg_send![class(c"NSURLRequest"), requestWithURL: &*target];
        let _: Option<Retained<AnyObject>> = msg_send![&web, loadRequest: &*request];
    }
    Ok(())
}

fn script_error(error: &AnyObject) -> String {
    // SAFETY: NSError's userInfo and localizedDescription.
    unsafe {
        let info: Option<Retained<AnyObject>> = msg_send![error, userInfo];
        let message: Option<Retained<AnyObject>> = info.and_then(|info| msg_send![&info, objectForKey: &*ns("WKJavaScriptExceptionMessage")]);
        if let Some(message) = message {
            let is_string: bool = msg_send![&message, isKindOfClass: class(c"NSString")];
            if is_string {
                let message: Retained<NSString> = Retained::cast_unchecked(message);
                return message.to_string();
            }
        }
        let description: Retained<NSString> = msg_send![error, localizedDescription];
        description.to_string()
    }
}

/// Runs `script` (an expression, awaited) in the preview page's own world;
/// its value comes back as JSON.
pub fn exec(script: &str, done: impl FnOnce(Result<Value, BridgeError>) + 'static) {
    let web = match preview_view() {
        Ok(web) => web,
        Err(error) => return done(Err(error)),
    };
    let body = format!("const value = await (\n{script}\n);\nreturn JSON.stringify(value === undefined ? null : value);");
    // SAFETY: WKWebView's callAsyncJavaScript:arguments:inFrame:inContentWorld:completionHandler:
    // (macOS 11+), on the main thread.
    unsafe {
        let arguments: Retained<AnyObject> = msg_send![class(c"NSDictionary"), dictionary];
        let world: Retained<AnyObject> = msg_send![class(c"WKContentWorld"), pageWorld];
        let done = RefCell::new(Some(done));
        let block = RcBlock::new(move |result: *mut AnyObject, error: *mut AnyObject| {
            let Some(done) = done.borrow_mut().take() else { return };
            if !error.is_null() {
                return done(Err(BridgeError::new("desktop.preview_script_failed", script_error(&*error))));
            }
            if result.is_null() {
                return done(Ok(Value::Null));
            }
            let is_string: bool = msg_send![&*result, isKindOfClass: class(c"NSString")];
            if !is_string {
                return done(Err(BridgeError::new("desktop.preview_script_failed", "页面脚本没有返回可读取的结果。")));
            }
            let text = (*(result as *const NSString)).to_string();
            done(serde_json::from_str(&text).map_err(|error| BridgeError::new("desktop.preview_script_failed", format!("页面脚本返回了无效结果：{error}"))))
        });
        let _: () = msg_send![&web,
            callAsyncJavaScript: &*ns(&body),
            arguments: &*arguments,
            inFrame: std::ptr::null::<AnyObject>(),
            inContentWorld: &*world,
            completionHandler: &*block];
    }
}

fn preview_view() -> Result<Retained<AnyObject>, BridgeError> {
    window_preview::view().ok_or_else(|| BridgeError::new("desktop.browser_preview_closed", "浏览器页面已关闭或替换，请重新打开页面后再操作。").retryable())
}

/// Draws `image` into a `width`×`height` point bitmap, so screenshot
/// coordinates match the page's CSS pixels.
unsafe fn png(image: &AnyObject, width: usize, height: usize) -> Option<Vec<u8>> {
    if width == 0 || height == 0 {
        return None;
    }
    // SAFETY: NSBitmapImageRep/NSGraphicsContext documented drawing API on the main thread.
    unsafe {
        let rep: Allocated<AnyObject> = msg_send![class(c"NSBitmapImageRep"), alloc];
        let rep: Option<Retained<AnyObject>> = msg_send![rep,
            initWithBitmapDataPlanes: std::ptr::null_mut::<*mut u8>(),
            pixelsWide: width as isize,
            pixelsHigh: height as isize,
            bitsPerSample: 8isize,
            samplesPerPixel: 4isize,
            hasAlpha: true,
            isPlanar: false,
            colorSpaceName: &*ns("NSDeviceRGBColorSpace"),
            bytesPerRow: 0isize,
            bitsPerPixel: 0isize];
        let rep = rep?;
        let context: Option<Retained<AnyObject>> = msg_send![class(c"NSGraphicsContext"), graphicsContextWithBitmapImageRep: &*rep];
        let context = context?;
        let _: () = msg_send![class(c"NSGraphicsContext"), saveGraphicsState];
        let _: () = msg_send![class(c"NSGraphicsContext"), setCurrentContext: &*context];
        let target = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(width as f64, height as f64));
        let _: () = msg_send![image, drawInRect: target];
        let _: () = msg_send![&context, flushGraphics];
        let _: () = msg_send![class(c"NSGraphicsContext"), restoreGraphicsState];
        let properties: Retained<AnyObject> = msg_send![class(c"NSDictionary"), dictionary];
        let data: Option<Retained<AnyObject>> = msg_send![&rep, representationUsingType: 4usize, properties: &*properties];
        let data = data?;
        let length: usize = msg_send![&data, length];
        let bytes: *const u8 = msg_send![&data, bytes];
        Some(std::slice::from_raw_parts(bytes, length).to_vec())
    }
}

/// A PNG of the preview (or `rect` of it) in CSS pixels.
pub fn snapshot(rect: Option<(f64, f64, f64, f64)>, done: impl FnOnce(Result<Value, BridgeError>) + 'static) {
    let web = match preview_view() {
        Ok(web) => web,
        Err(error) => return done(Err(error)),
    };
    // SAFETY: WKWebView's bounds and takeSnapshotWithConfiguration:completionHandler:.
    unsafe {
        let bounds: NSRect = msg_send![&web, bounds];
        let zoom: f64 = msg_send![&web, pageZoom];
        let zoom = if zoom.is_finite() && zoom > 0.0 { zoom } else { 1.0 };
        let config: Retained<AnyObject> = msg_send![class(c"WKSnapshotConfiguration"), new];
        let (width, height) = match rect {
            Some((x, y, w, h)) => {
                let rx = (x * zoom).clamp(0.0, bounds.size.width);
                let ry = (y * zoom).clamp(0.0, bounds.size.height);
                let rw = (w * zoom).clamp(0.0, (bounds.size.width - rx).max(0.0));
                let rh = (h * zoom).clamp(0.0, (bounds.size.height - ry).max(0.0));
                if rw <= 0.0 || rh <= 0.0 {
                    return done(Err(BridgeError::new("desktop.preview_capture_invalid", "截图区域无效。")));
                }
                let _: () = msg_send![&config, setRect: NSRect::new(NSPoint::new(rx, ry), NSSize::new(rw, rh))];
                (rw / zoom, rh / zoom)
            }
            None => (bounds.size.width / zoom, bounds.size.height / zoom),
        };
        let (width, height) = (width.round().max(0.0) as usize, height.round().max(0.0) as usize);
        let done = RefCell::new(Some(done));
        let block = RcBlock::new(move |image: *mut AnyObject, error: *mut AnyObject| {
            let Some(done) = done.borrow_mut().take() else { return };
            if image.is_null() {
                let detail = if error.is_null() {
                    String::new()
                } else {
                    let description: Retained<NSString> = msg_send![&*error, localizedDescription];
                    description.to_string()
                };
                return done(Err(BridgeError::new("desktop.preview_capture_empty", format!("截图为空：预览可能尚未绘制。{detail}")).retryable()));
            }
            match png(&*image, width, height) {
                Some(bytes) => {
                    use base64::Engine;
                    done(Ok(json!({"mime": "image/png", "data": base64::engine::general_purpose::STANDARD.encode(bytes), "width": width, "height": height})))
                }
                None => done(Err(BridgeError::new("desktop.preview_capture_failed", "无法编码预览截图。"))),
            }
        });
        let _: () = msg_send![&web, takeSnapshotWithConfiguration: &*config, completionHandler: &*block];
    }
}

fn uptime() -> f64 {
    // SAFETY: NSProcessInfo's systemUptime.
    unsafe {
        let info: Retained<AnyObject> = msg_send![class(c"NSProcessInfo"), processInfo];
        msg_send![&info, systemUptime]
    }
}

fn preview_window(web: &AnyObject) -> Result<Retained<AnyObject>, BridgeError> {
    // SAFETY: NSView's window.
    let window: Option<Retained<AnyObject>> = unsafe { msg_send![web, window] };
    window.ok_or_else(|| BridgeError::new("desktop.browser_preview_missing", "右侧浏览器当前不可见。").retryable())
}

/// A left click at the preview's CSS point. The events go straight to the
/// web view: through NSWindow, the first click on a window that is not key
/// only activates it and never reaches the page.
pub fn click(x: f64, y: f64) -> Result<(), BridgeError> {
    let web = preview_view()?;
    let window = preview_window(&web)?;
    // SAFETY: NSView coordinate conversion and NSEvent mouse events.
    unsafe {
        focus_preview(&window, &web);
        let flipped: bool = msg_send![&web, isFlipped];
        let bounds: NSRect = msg_send![&web, bounds];
        let zoom: f64 = msg_send![&web, pageZoom];
        let zoom = if zoom.is_finite() && zoom > 0.0 { zoom } else { 1.0 };
        let vx = x * zoom;
        let vy = y * zoom;
        let local = NSPoint::new(vx, if flipped { vy } else { bounds.size.height - vy });
        let point: NSPoint = msg_send![&web, convertPoint: local, toView: std::ptr::null::<AnyObject>()];
        let number: isize = msg_send![&window, windowNumber];
        for kind in [1usize, 2usize] {
            let event: Option<Retained<AnyObject>> = msg_send![class(c"NSEvent"),
                mouseEventWithType: kind,
                location: point,
                modifierFlags: 0usize,
                timestamp: uptime(),
                windowNumber: number,
                context: std::ptr::null::<AnyObject>(),
                eventNumber: 0isize,
                clickCount: 1isize,
                pressure: 1.0f32];
            let event = event.ok_or_else(|| BridgeError::new("desktop.browser_input_failed", "无法生成点击事件。"))?;
            if kind == 1 {
                let _: () = msg_send![&web, mouseDown: &*event];
            } else {
                let _: () = msg_send![&web, mouseUp: &*event];
            }
        }
    }
    Ok(())
}

/// Keyboard focus inside the preview, unless it already holds it.
unsafe fn focus_preview(window: &AnyObject, web: &AnyObject) {
    // SAFETY: NSWindow first responder.
    unsafe {
        let responder: Option<Retained<AnyObject>> = msg_send![window, firstResponder];
        let inside = responder.is_some_and(|responder| {
            let is_view: bool = msg_send![&responder, isKindOfClass: class(c"NSView")];
            is_view && {
                let descendant: bool = msg_send![&responder, isDescendantOf: web];
                descendant
            }
        });
        if !inside {
            let _: bool = msg_send![window, makeFirstResponder: web];
        }
    }
}

pub fn press(key: &str, modifiers: &[String], repeat: usize) -> Result<(), BridgeError> {
    let (code, characters) = policy::key_event(key)?;
    let flags = policy::modifier_flags(modifiers)?;
    let web = preview_view()?;
    let window = preview_window(&web)?;
    // SAFETY: NSWindow first responder and NSEvent key events.
    unsafe {
        focus_preview(&window, &web);
        let number: isize = msg_send![&window, windowNumber];
        let text = ns(&characters);
        for _ in 0..repeat.max(1) {
            for kind in [10usize, 11usize] {
                let event: Option<Retained<AnyObject>> = msg_send![class(c"NSEvent"),
                    keyEventWithType: kind,
                    location: NSPoint::new(0.0, 0.0),
                    modifierFlags: flags,
                    timestamp: uptime(),
                    windowNumber: number,
                    context: std::ptr::null::<AnyObject>(),
                    characters: &*text,
                    charactersIgnoringModifiers: &*text,
                    isARepeat: false,
                    keyCode: code];
                let event = event.ok_or_else(|| BridgeError::new("desktop.browser_input_failed", "无法生成按键事件。"))?;
                let _: () = msg_send![&window, sendEvent: &*event];
            }
        }
    }
    Ok(())
}

pub fn set_zoom(factor: f64) -> Result<(), BridgeError> {
    let web = preview_view()?;
    // SAFETY: WKWebView's pageZoom (macOS 11+).
    let _: () = unsafe { msg_send![&web, setPageZoom: factor] };
    Ok(())
}

/// The preview's `prefers-color-scheme`, by its view appearance.
pub fn set_color_scheme(scheme: &str) -> Result<(), BridgeError> {
    let name = match scheme {
        "system" => None,
        "light" => Some("NSAppearanceNameAqua"),
        "dark" => Some("NSAppearanceNameDarkAqua"),
        _ => return Err(BridgeError::new("desktop.preview_color_scheme_invalid", "颜色方案无效。")),
    };
    let web = preview_view()?;
    // SAFETY: NSAppearance by name and NSView's appearance.
    unsafe {
        let appearance: Option<Retained<AnyObject>> = match name {
            Some(name) => msg_send![class(c"NSAppearance"), appearanceNamed: &*ns(name)],
            None => None,
        };
        let _: () = msg_send![&web, setAppearance: appearance.as_deref()];
    }
    Ok(())
}

pub fn reload_from_origin() -> Result<(), BridgeError> {
    let web = preview_view()?;
    // SAFETY: WKWebView's reloadFromOrigin.
    let _: Option<Retained<AnyObject>> = unsafe { msg_send![&web, reloadFromOrigin] };
    Ok(())
}

pub fn open_preview_devtools() -> Result<Value, BridgeError> {
    let web = preview_view()?;
    set_web_inspector_enabled(&web, true);
    unsafe {
        let has_inspector: bool = msg_send![&web, respondsToSelector: sel!(_inspector)];
        if has_inspector {
            let inspector: Option<Retained<AnyObject>> = msg_send![&web, _inspector];
            if let Some(inspector) = inspector {
                let can_show0: bool = msg_send![&inspector, respondsToSelector: sel!(show)];
                let can_show1: bool = msg_send![&inspector, respondsToSelector: sel!(show:)];
                if can_show0 {
                    let _: () = msg_send![&inspector, show];
                    return Ok(json!({ "opened": true, "inspectable": true, "driver": "webkit-inspector" }));
                }
                if can_show1 {
                    let _: () = msg_send![&inspector, show: std::ptr::null::<AnyObject>()];
                    return Ok(json!({ "opened": true, "inspectable": true, "driver": "webkit-inspector" }));
                }
            }
        }
    }
    Ok(json!({ "opened": false, "inspectable": true, "driver": "webkit-inspectable" }))
}

fn verify_store_profile(store: &AnyObject, expected_profile: Option<&str>) -> bool {
    let Some(expected_profile) = expected_profile else {
        return true;
    };
    unsafe {
        let actual: Option<Retained<AnyObject>> = msg_send![store, identifier];
        let expected_alloc: Allocated<AnyObject> = msg_send![class(c"NSUUID"), alloc];
        let expected: Option<Retained<AnyObject>> = msg_send![expected_alloc, initWithUUIDString: &*ns(expected_profile)];
        match (actual, expected) {
            (Some(actual), Some(expected)) => msg_send![&actual, isEqual: &*expected],
            _ => false,
        }
    }
}

/// Removes site data and/or HTTP authentication credentials from the preview's
/// data store, verifying both the profile UUID and preview generation on
/// completion before notifying the bridge to rotate/reload the preview.
pub fn clear_preview_data(
    reply_id: Value,
    preview_id: String,
    expected_generation: u64,
    expected_profile: Option<String>,
    clear_website_data: bool,
    clear_http_auth: bool,
) {
    let web = match preview_view() {
        Ok(web) => web,
        Err(error) => {
            notify(Notice::PreviewDataCleared {
                id: reply_id,
                preview_id,
                generation: expected_generation,
                profile_id: expected_profile,
                cleared_website_data: false,
                cleared_http_auth: false,
                result: Err(error),
            });
            return;
        }
    };
    unsafe {
        let config: Retained<AnyObject> = msg_send![&web, configuration];
        let store: Retained<AnyObject> = msg_send![&config, websiteDataStore];
        if !verify_store_profile(&store, expected_profile.as_deref()) {
            notify(Notice::PreviewDataCleared {
                id: reply_id,
                preview_id,
                generation: expected_generation,
                profile_id: expected_profile,
                cleared_website_data: false,
                cleared_http_auth: false,
                result: Err(BridgeError::new("desktop.preview_changed", "预览已改变。")),
            });
            return;
        }
        let store_class=class(c"WKWebsiteDataStore");
        let types:Retained<AnyObject>=if clear_website_data {
            let public:Retained<AnyObject>=msg_send![store_class,allWebsiteDataTypes];
            msg_send![&public,mutableCopy]
        } else {msg_send![class(c"NSMutableSet"),new]};
        if clear_http_auth {
            let available:bool=msg_send![store_class,respondsToSelector:sel!(_allWebsiteDataTypesIncludingPrivate)];
            let credential_type=ns("_WKWebsiteDataTypeCredentials");
            let supported=if available {let all:Retained<AnyObject>=msg_send![store_class,_allWebsiteDataTypesIncludingPrivate];msg_send![&all,containsObject:&*credential_type]}else{false};
            if !supported {
                notify(Notice::PreviewDataCleared{id:reply_id,preview_id,generation:expected_generation,profile_id:expected_profile,cleared_website_data:false,cleared_http_auth:false,result:Err(BridgeError::new("desktop.preview_auth_clear_unavailable","当前 WebKit 未提供预览会话的认证清理能力。"))});
                return;
            }
            let _:()=msg_send![&types,addObject:&*credential_type];
        }
        let date: Retained<AnyObject> = msg_send![class(c"NSDate"), distantPast];
        let reply_id = RefCell::new(Some(reply_id));
        let preview_id = preview_id.clone();
        let expected_profile_clone = expected_profile.clone();
        let store_ref = retain(&store);
        let block = RcBlock::new(move || {
            let Some(id) = reply_id.borrow_mut().take() else { return };
            let still_same_profile = verify_store_profile(&store_ref, expected_profile_clone.as_deref());
            let result = if still_same_profile {
                Ok(())
            } else {
                Err(BridgeError::new("desktop.preview_changed", "预览已改变。"))
            };
            notify(Notice::PreviewDataCleared {
                id,
                preview_id: preview_id.clone(),
                generation: expected_generation,
                profile_id: expected_profile_clone.clone(),
                cleared_website_data: clear_website_data,
                cleared_http_auth: clear_http_auth,
                result,
            });
        });
        let _: () = msg_send![&store,
            removeDataOfTypes: &*types,
            modifiedSince: &*date,
            completionHandler: &*block];
    }
}

/// Checks or requests macOS Microphone (`AVCaptureDevice`) and Speech
/// Recognition (`SFSpeechRecognizer`) permissions.
pub fn microphone_and_speech_permissions(request_if_needed: bool) -> Value {
    static AV_LOADED: OnceLock<bool> = OnceLock::new();
    static SPEECH_LOADED: OnceLock<bool> = OnceLock::new();
    let av_ok = *AV_LOADED.get_or_init(|| unsafe {
        !libc::dlopen(
            c"/System/Library/Frameworks/AVFoundation.framework/AVFoundation".as_ptr(),
            libc::RTLD_NOW,
        )
        .is_null()
    });
    let speech_ok = *SPEECH_LOADED.get_or_init(|| unsafe {
        !libc::dlopen(
            c"/System/Library/Frameworks/Speech.framework/Speech".as_ptr(),
            libc::RTLD_NOW,
        )
        .is_null()
    });

    let (has_mic_plist, has_speech_plist) = unsafe {
        let bundle: Retained<AnyObject> = msg_send![class(c"NSBundle"), mainBundle];
        let info: Option<Retained<AnyObject>> = msg_send![&bundle, infoDictionary];
        let has_key = |key: &str| -> bool {
            info.as_ref()
                .and_then(|dict| {
                    let val: Option<Retained<AnyObject>> = msg_send![&**dict, objectForKey: &*ns(key)];
                    val
                })
                .is_some()
        };
        (has_key("NSMicrophoneUsageDescription"), has_key("NSSpeechRecognitionUsageDescription"))
    };

    let mut mic_permission = "default";
    if av_ok {
        if let Some(device_cls) = AnyClass::get(c"AVCaptureDevice") {
            unsafe {
                let media_type = ns("soun");
                let status: isize = msg_send![device_cls, authorizationStatusForMediaType: &*media_type];
                mic_permission = match status {
                    0 => "default",
                    1 => "restricted",
                    2 => "denied",
                    3 => "granted",
                    _ => "default",
                };
                if request_if_needed && mic_permission == "default" && has_mic_plist {
                    let (tx, rx) = std::sync::mpsc::sync_channel(1);
                    let block = RcBlock::new(move |granted: Bool| {
                        let _ = tx.send(granted.as_bool());
                    });
                    let _: () = msg_send![device_cls, requestAccessForMediaType: &*media_type, completionHandler: &*block];
                    if let Ok(granted) = rx.recv_timeout(Duration::from_secs(30)) {
                        mic_permission = if granted { "granted" } else { "denied" };
                    }
                }
            }
        }
    }

    let mut speech_permission = "default";
    if speech_ok {
        if let Some(recognizer_cls) = AnyClass::get(c"SFSpeechRecognizer") {
            unsafe {
                let status: isize = msg_send![recognizer_cls, authorizationStatus];
                speech_permission = match status {
                    0 => "default",
                    1 => "denied",
                    2 => "restricted",
                    3 => "granted",
                    _ => "default",
                };
                if request_if_needed && speech_permission == "default" && has_speech_plist {
                    let (tx, rx) = std::sync::mpsc::sync_channel(1);
                    let block = RcBlock::new(move |new_status: isize| {
                        let _ = tx.send(new_status);
                    });
                    let _: () = msg_send![recognizer_cls, requestAuthorization: &*block];
                    if let Ok(new_status) = rx.recv_timeout(Duration::from_secs(30)) {
                        speech_permission = match new_status {
                            1 => "denied",
                            2 => "restricted",
                            3 => "granted",
                            _ => "default",
                        };
                    }
                }
            }
        }
    }

    let granted = mic_permission != "denied" && mic_permission != "restricted" && speech_permission != "denied" && speech_permission != "restricted";
    json!({
        "granted": granted,
        "permission": if mic_permission == "granted" && speech_permission == "granted" { "granted" } else if !granted { "denied" } else { "default" },
        "microphonePermission": mic_permission,
        "speechPermission": speech_permission,
        "status": if granted { "ready" } else { "denied" },
        "host": "desktop-client",
    })
}
