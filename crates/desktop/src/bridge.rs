//! The engine side of the desktop bridge: the page's calls, their answers
//! and the events the page listens to. Work that blocks runs on its own
//! thread, AppKit work on the main thread; both answer through
//! [`native::notify`], which the bridge loop turns into replies.
use crate::{
    attachments, notifications,
    native::{self, Notice},
    policy::{self, BridgeError, Outcome},
    updates,
};
use base64::Engine as _;
use nsh_core::{
    desktop::{MenuRole, Shortcut},
    element::desktop::{MenuItem, set_application_menu},
    file_dialog::OpenDialog,
    prelude::{ElementRef, Owner, Signal, effect, open_path, pick_paths, reveal_path, signal, spawn},
};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    future::Future,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Mutex, OnceLock, atomic::Ordering},
};

/// What the shell knows before the UI mounts.
pub struct Context {
    pub origin: String,
    pub state: PathBuf,
    /// The secret the injected bridge script sends with every call: frames
    /// other than the main page can reach the message handler but never
    /// see the script's closure.
    pub key: String,
}

static CONTEXT: OnceLock<Context> = OnceLock::new();
static INITIAL_DEEP_LINKS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

pub fn set_context(context:Context)->Result<(),BridgeError>{
    crate::connections::initialize(&context.state,&context.origin,&crate::backend::backend_origin())?;
    let _=CONTEXT.set(context);Ok(())
}

pub fn context() -> &'static Context {
    CONTEXT.get().expect("desktop context is set before the UI starts")
}

pub fn queue_startup_deep_link(url: String) {
    let queue = INITIAL_DEEP_LINKS.get_or_init(|| Mutex::new(Vec::new()));
    if let Ok(mut guard) = queue.lock() {
        guard.push(url);
    }
}

fn take_startup_deep_links() -> Vec<String> {
    let Some(queue) = INITIAL_DEEP_LINKS.get() else {
        return Vec::new();
    };
    queue.lock().map(|mut g| std::mem::take(&mut *g)).unwrap_or_default()
}

struct Grant {
    window_id:String,
    document_id:String,
    scope_version:u64,
    thread_id: String,
    workspace_id: String,
    service_id: String,
    identity_id: String,
    client_root: PathBuf,
}

struct NotificationEntry {
    window_id:String,
    document_id:String,
    id: String,
    scope: notifications::Scope,
    thread_id: String,
    event_id: String,
    dedupe_key: String,
    test: bool,
    shown: bool,
    sound_requested: bool,
    system: Option<Value>,
    history: Vec<Value>,
    last: Value,
}

impl NotificationEntry {
    fn receipt(&mut self, sequence: u64, status: &str, code: &str, message: &str) -> Value {
        let mut step = json!({"status":status,"at":chrono::Utc::now().to_rfc3339()});
        if !code.is_empty() { step["code"] = json!(code); }
        if !message.is_empty() { step["message"] = json!(message); }
        self.history.push(step);
        let mut value = json!({
            "connectionVersion":self.scope.connection_version,"serviceId":self.scope.service_id,"identityId":self.scope.identity_id,
            "windowId":self.window_id,"documentId":self.document_id,"id":self.id,"threadId":self.thread_id,"eventId":self.event_id,"dedupeKey":self.dedupe_key,
            "seq":sequence,"status":status,"shown":self.shown,"soundRequested":self.sound_requested,
            "shownReceiptMeaning":"system-accepted-scheduling","history":self.history,
        });
        if self.test { value["test"] = json!(true); }
        if let Some(system) = &self.system {
            for field in ["systemPermission", "systemNotificationSettings", "systemPermissionError"] {
                if !system[field].is_null() { value[field] = system[field].clone(); }
            }
        }
        if !code.is_empty() { value["code"] = json!(code); }
        if !message.is_empty() { value["message"] = json!(message); }
        self.last = value.clone();
        value
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DockUnreadThread {
    thread_id: String,
    visible_thread_ids: Vec<String>,
}

#[derive(Clone, Debug, Default)]
struct WindowState {
    window_id: String,
    document_id: String,
    route: String,
    visible: bool,
    focused: bool,
    visible_threads: Vec<String>,
}

#[derive(Default)]
struct Inner {
    scope: policy::ScopeState,
    route: String,
    visible: bool,
    focused: bool,
    visible_threads: Vec<String>,
    document_id: String,
    windows: HashMap<String, WindowState>,
    window_scopes:HashMap<String,u64>,
    retired_documents: HashMap<String, std::collections::VecDeque<String>>,
    next_window_seq: u64,
    active_window_id: String,
    ui_ready: bool,
    pending_deep_links: Vec<String>,
    pending_calls: HashMap<String, policy::CallEnvelope>,
    paths: HashMap<String, PathBuf>,
    grants: HashMap<String, Grant>,
    notification_sequence: u64,
    notification_deliveries: HashMap<String,Value>,
    dedupe_store: notifications::ScopedDedupeStore,
    notifications: HashMap<String, NotificationEntry>,
    dock_unread_by_scope: HashMap<String, Vec<DockUnreadThread>>,
    last_dock_badge_count: Option<usize>,
}

struct Core {
    main: ElementRef,
    accelerators: Signal<BTreeMap<String, String>>,
    locale: Signal<String>,
    owner: Owner,
    inner: RefCell<Inner>,
}

#[derive(Clone)]
pub struct Shell(Rc<Core>);

fn bad(code: &'static str, message: &str) -> BridgeError {
    BridgeError::new(code, message)
}

fn play_cue_sound() -> Result<(), BridgeError> {
    let mut child = crate::shell_env::command("/usr/bin/afplay")
        .arg("/System/Library/Sounds/Glass.aiff")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| BridgeError::new("desktop.notification_sound_failed", error.to_string()))?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

pub(crate) fn preview_profile(scope:&str)->Result<String,BridgeError>{
    let failed=|error:std::io::Error|BridgeError::new("desktop.preview_profile_failed",error.to_string());
    let file=context().state.join("desktop/preview-profiles.json");
    let mut profiles:BTreeMap<String,String>=match std::fs::read(&file){
        Ok(bytes)=>serde_json::from_slice(&bytes).map_err(|error|BridgeError::new("desktop.preview_profile_failed",error.to_string()))?,
        Err(error)if error.kind()==std::io::ErrorKind::NotFound=>BTreeMap::new(),Err(error)=>return Err(failed(error))
    };
    if let Some(id)=profiles.get(scope){uuid::Uuid::parse_str(id).map_err(|error|BridgeError::new("desktop.preview_profile_failed",error.to_string()))?;return Ok(id.clone());}
    let id=uuid::Uuid::new_v4().to_string();profiles.insert(scope.into(),id.clone());
    let parent=file.parent().expect("private preview profile directory");std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent).map_err(failed)?;
    let temporary=file.with_extension(format!("{}.tmp",uuid::Uuid::new_v4().simple()));
    let result=(||->std::io::Result<()>{
        let mut output=std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
        output.write_all(&serde_json::to_vec(&profiles)?)?;output.sync_all()?;std::fs::rename(&temporary,&file)?;std::fs::File::open(parent)?.sync_all()
    })();
    if let Err(error)=result{let _=std::fs::remove_file(temporary);return Err(failed(error));}Ok(id)
}

fn equal_secret(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn is_report_editor_route(route: &str) -> bool {
    matches!(route.split(['?', '#']).next(), Some("/ctf/reports" | "/pentest/reports"))
}

fn off_thread(id: Value, work: impl FnOnce() -> Outcome + Send + 'static) {
    std::thread::spawn(move || native::notify(Notice::Reply(id, work())));
}

/// Answers `id` from AppKit work on the main thread.
fn on_main(id: Value, work: impl FnOnce() -> Outcome + Send + 'static) {
    native::on_main(move || native::notify(Notice::Reply(id, work())));
}

fn done(result: Result<(), BridgeError>) -> Outcome {
    result.map(|()| Value::Null)
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata().is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

fn name_of(path: &Path) -> String {
    path.file_name().map_or_else(|| path.to_string_lossy().into_owned(), |name| name.to_string_lossy().into_owned())
}

fn finite(params: &Value, key: &str) -> Option<f64> {
    params.get(key).and_then(Value::as_f64).filter(|value| value.is_finite())
}

impl Shell {
    /// The shell of one window; call in the root component.
    pub fn new(main: ElementRef) -> Shell {
        let mut windows = HashMap::new();
        windows.insert(
            "main".to_owned(),
            WindowState {
                window_id: "main".to_owned(),
                ..WindowState::default()
            },
        );
        let inner = Inner {
            scope: crate::connections::scope("main"),
            windows,
            next_window_seq: 1,
            active_window_id: "main".to_owned(),
            pending_deep_links: take_startup_deep_links(),
            ..Inner::default()
        };
        let shell = Shell(Rc::new(Core {
            main,
            accelerators: signal(policy::default_accelerators()),
            locale: signal("zh-CN".to_owned()),
            owner: Owner::current(),
            inner: RefCell::new(inner),
        }));
        let (sender, mut notices) = tokio::sync::mpsc::unbounded_channel();
        native::set_notices(sender);
        let looping = shell.clone();
        spawn(async move {
            while let Some(notice) = notices.recv().await {
                looping.notice(notice);
            }
        });
        let menus = shell.clone();
        effect(move || {
            let accelerators = menus.0.accelerators.get();
            let locale = menus.0.locale.get();
            set_application_menu(menus.menu(&accelerators, &locale));
        });
        shell
    }

    fn menu(&self, accelerators: &BTreeMap<String, String>, locale: &str) -> Vec<MenuItem> {
        let labels = policy::menu_labels(locale);
        let command = |label: &str, name: &'static str, accelerator: Option<&str>| {
            let shell = self.clone();
            let item = MenuItem::new(label).on_select(move || shell.command(name));
            match accelerator.and_then(policy::accelerator_to_shortcut) {
                Some(shortcut) if Shortcut::parse(&shortcut, true).is_ok() => item.shortcut(&shortcut),
                _ => item,
            }
        };
        let custom = |name: &str| accelerators.get(name).map(String::as_str);
        let role = |role: MenuRole, label: &str| MenuItem::role(role).label(label);
        vec![
            MenuItem::submenu(crate::APP_NAME, [
                role(MenuRole::About, labels.about),
                MenuItem::separator(),
                command(labels.settings, "settings", Some("CmdOrCtrl+,")),
                command(labels.shortcuts, "shortcuts", None),
                MenuItem::separator(),
                role(MenuRole::Hide, labels.hide),
                role(MenuRole::HideOthers, labels.hide_others),
                role(MenuRole::ShowAll, labels.show_all),
                MenuItem::separator(),
                role(MenuRole::Quit, labels.quit),
            ]),
            MenuItem::submenu(labels.edit, [
                role(MenuRole::Undo, labels.undo),
                role(MenuRole::Redo, labels.redo),
                MenuItem::separator(),
                role(MenuRole::Cut, labels.cut),
                role(MenuRole::Copy, labels.copy),
                role(MenuRole::Paste, labels.paste),
                role(MenuRole::SelectAll, labels.select_all),
            ]),
            MenuItem::submenu(labels.chat, [
                command(labels.new_chat, "new-chat", custom("new-chat")),
                command(labels.new_window, "new-window", Some("CmdOrCtrl+N")),
                command(labels.search, "search", custom("search")),
            ]),
            MenuItem::submenu(labels.view, [
                command(labels.sidebar, "sidebar", custom("sidebar")),
                MenuItem::separator(),
                command(labels.back, "back", Some("CmdOrCtrl+[")),
                command(labels.forward, "forward", Some("CmdOrCtrl+]")),
                command(labels.reload, "reload", Some("CmdOrCtrl+R")),
                MenuItem::separator(),
                role(MenuRole::ToggleFullScreen, labels.full_screen),
            ]),
            MenuItem::submenu(labels.window, [
                role(MenuRole::Minimize, labels.minimize),
                role(MenuRole::Zoom, labels.zoom),
                MenuItem::separator(),
                role(MenuRole::Close, labels.close_window),
            ]),
        ]
    }

    fn command(&self, name: &str) {
        match name {
            "back" => self.0.main.go_back(),
            "forward" => self.0.main.go_forward(),
            "new-window" => {
                let _ = self.open_new_window(&json!({}));
            }
            "reload" => {
                let shell = self.clone();
                native::on_main(move || {
                    native::request_flush_barrier("reload", move |persisted, error| {
                        if !persisted {
                            let detail = if error.is_empty() {
                                "页面没有在 5 秒内确认草稿已保存。继续刷新可能丢失尚未保存的输入。"
                            } else {
                                error.as_str()
                            };
                            if native::alert(true, "草稿保存失败。仍然刷新当前页面？", detail, &["取消", "刷新"]) != 1 {
                                return;
                            }
                        }
                        native::notify(Notice::Event("command".into(), json!({ "name": "reload", "flushed": true })));
                    });
                });
                let _ = shell;
            }
            _ => self.emit("command", json!({ "name": name })),
        }
    }

    fn send_to(&self, window_id: &str, mut message: Value) {
        let (doc_id, scope_ver, conn_ver) = {
            let inner = self.0.inner.borrow();
            let doc = inner
                .windows
                .get(window_id)
                .map(|w| w.document_id.clone())
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| inner.document_id.clone());
            let scope=crate::connections::scope(window_id);(doc,scope.scope_version,scope.connection_version)
        };
        if message["windowId"].is_null() {
            message["windowId"] = json!(window_id);
        }
        if message["documentId"].is_null() && !doc_id.is_empty() {
            message["documentId"] = json!(doc_id);
        }
        if message["scopeVersion"].is_null() {
            message["scopeVersion"] = json!(scope_ver);
        }
        if message["connectionVersion"].is_null() {
            message["connectionVersion"] = json!(conn_ver);
        }
        let script = format!("window.__mutekiDesktopReceive && window.__mutekiDesktopReceive({message});");
        if window_id.is_empty() || window_id == "main" {
            self.0.main.inject_java_script(script);
        } else {
            let wid = window_id.to_owned();
            native::on_main(move || native::send_to_window(&wid, &script));
        }
    }

    fn broadcast(&self, name: &str, value: Value) {
        let window_ids: Vec<String> = {
            let inner = self.0.inner.borrow();
            if inner.windows.is_empty() {
                vec!["main".to_owned()]
            } else {
                inner.windows.keys().cloned().collect()
            }
        };
        for wid in window_ids {
            self.send_to(&wid, json!({ "t": "event", "n": name, "v": value }));
        }
    }

    fn reply(&self, id: &Value, mut outcome: Outcome) {
        let envelope = self.0.inner.borrow_mut().pending_calls.remove(&id.to_string());
        let Some(envelope)=envelope else{return};
        let response_id=envelope.id.clone();
        let window_id=envelope.window_id.clone();
        let doc_id=envelope.document_id.clone();
        { let env=envelope;
            let (current_doc, current_scope) = {
                let inner = self.0.inner.borrow();
                let cur_doc = inner
                    .windows
                    .get(&env.window_id)
                    .map(|w| w.document_id.clone())
                    .filter(|d| !d.is_empty())
                    .unwrap_or_else(|| inner.document_id.clone());
                (cur_doc,crate::connections::scope(&env.window_id).scope_version)
            };
            if let Err(stale_error) = env.validate_reply(&current_doc, current_scope) {
                if stale_error.code == "desktop.document_stale" {
                    return;
                }
                outcome = Err(stale_error);
            }
        }
        let mut msg = match outcome {
            Ok(value) => json!({ "t": "reply", "id": response_id, "ok": true, "v": value }),
            Err(error) => json!({ "t": "reply", "id": response_id, "ok": false, "e": error.to_json() }),
        };
        msg["windowId"] = json!(window_id);
        if !doc_id.is_empty() {
            msg["documentId"] = json!(doc_id);
        }
        self.send_to(&window_id, msg);
    }

    fn emit(&self,name:&str,value:Value){if name=="state"{let windows=self.0.inner.borrow().windows.keys().cloned().collect::<Vec<_>>();for window in windows{self.send_to(&window,json!({"t":"event","n":"state","v":self.state_for(&window)}));}}else{self.broadcast(name,value);}}

    fn notice(&self, notice: Notice) {
        match notice {
            Notice::Reply(id, outcome) => self.reply(&id, outcome),
            Notice::Event(name, value) => self.emit(&name, value),
            Notice::NotificationConsentChoice(id, params, choice) => self.notification_consent_chosen(id, &params, choice),
            Notice::NotificationPrepared(id, params, settings) => self.notification_prepared(id, &params, settings),
            Notice::NotificationScheduled(entry_id, result) => self.notification_scheduled(&entry_id, result),
            Notice::NotificationClicked(entry_id) => self.notification_clicked(&entry_id),
            Notice::NotificationNavigate { window_id, route, thread_id, test, receipt_id } => {
                let has_document = {
                    let inner = self.0.inner.borrow();
                    inner
                        .windows
                        .get(&window_id)
                        .map(|w| !w.document_id.is_empty())
                        .unwrap_or_else(|| (window_id.is_empty() || window_id == "main") && !inner.document_id.is_empty())
                };
                if has_document {
                    self.send_to(
                        &window_id,
                        json!({
                            "t": "event",
                            "n": "navigate",
                            "v": { "route": route, "source": "notification", "threadId": thread_id, "test": test }
                        }),
                    );
                } else {
                    native::on_main(move || {
                        if let Err(error) = native::navigate_window_route(&window_id, &route) {
                            native::notify(Notice::WindowEvent(
                                window_id,
                                "notification-navigation-error".into(),
                                json!({ "id": receipt_id, "threadId": thread_id, "error": error.to_json() }),
                            ));
                        }
                    });
                }
            }
            Notice::WindowEvent(window,name,event)=>self.send_to(&window,json!({"t":"event","n":name,"v":event})),
            Notice::WindowPreview(window,document,event)=>{let current=self.0.inner.borrow().windows.get(&window).map(|window|window.document_id.clone());if current.as_deref()==Some(document.as_str()){self.send_to(&window,json!({"t":"event","n":"preview","v":event,"documentId":document}));}},
            Notice::ReportPreview(window, document, event) => {
                let current = self.0.inner.borrow().windows.get(&window).is_some_and(|owner| owner.document_id == document);
                if current {
                    self.send_to(&window, json!({"t":"event","n":"report-preview","v":event,"documentId":document}));
                }
            }
            Notice::Speech(window,document,event)=>{let current=self.0.inner.borrow().windows.get(&window).map(|window|window.document_id.clone());if current.as_deref()==Some(document.as_str()){let valid={let scope=crate::connections::scope(&window);event["serviceId"]==scope.service_id&&event["identityId"]==scope.identity_id&&event["connectionVersion"]==scope.connection_version};if valid{self.send_to(&window,json!({"t":"event","n":"speech","v":event,"documentId":document}));}}},
            Notice::Connection(window)=>{
                let scope=crate::connections::scope(&window);
                let changed={let mut inner=self.0.inner.borrow_mut();let previous=inner.window_scopes.insert(window.clone(),scope.scope_version);if window=="main"{inner.scope=scope.clone();}previous.is_some_and(|version|version!=scope.scope_version)};
                if changed {
                    native::dispose_report_preview(window.clone());
                    self.cancel_window_notifications(&window, false, "desktop.notification_scope_changed", "通知所属的工作台已改变。");
                    crate::speech::cancel_window(&window);native::dispose_window_preview(&window);
                    let pending={let mut inner=self.0.inner.borrow_mut();inner.grants.retain(|_,grant|grant.window_id!=window);inner.pending_calls.iter().filter(|(_,envelope)|envelope.window_id==window&&envelope.scope_bound).map(|(id,_)|id.clone()).collect::<Vec<_>>()};
                    for id in pending{if let Ok(id)=serde_json::from_str::<Value>(&id){self.reply(&id,Err(bad("desktop.connection_changed","所属工作台已切换，请在当前连接重试。")));}}
                }
                self.sync_dock_badge();
                self.send_to(&window,json!({"t":"event","n":"state","v":self.state_for(&window)}));
            }
            Notice::FocusChanged => {
                self.sync_dock_badge();
                self.emit("state", Value::Null);
            }
            Notice::DeepLink(url) => self.enqueue_or_handle_deep_link(url),
            Notice::WindowMessage(window_id, data) => self.receive_from_window(&window_id, &data),
            Notice::WindowLoading(window_id)=>{
                self.cancel_window_notifications(&window_id, true, "desktop.notification_document_changed", "通知所属的页面已刷新。");
                crate::speech::cancel_window(&window_id);native::dispose_window_preview(&window_id);self.retire_window_document(&window_id);
            }
            Notice::WindowClosed(window_id) => {
                native::dispose_report_preview(window_id.clone());
                self.cancel_window_notifications(&window_id, false, "desktop.notification_window_closed", "通知所属的窗口已关闭。");
                updates::document_closed(&window_id);
                crate::speech::cancel_window(&window_id);
                native::dispose_window_preview(&window_id);
                crate::connections::close(&window_id);
                {
                    let mut inner = self.0.inner.borrow_mut();
                    inner.windows.remove(&window_id);
                    inner.window_scopes.remove(&window_id);
                    inner.retired_documents.remove(&window_id);
                    if inner.active_window_id == window_id {
                        inner.active_window_id = "main".to_owned();
                    }
                }
                self.sync_dock_badge();
            }
            Notice::PreviewDataCleared {
                id,
                preview_id,
                generation,
                profile_id,
                cleared_website_data,
                cleared_http_auth,
                result,
            } => {
                let owner=self.0.inner.borrow().pending_calls.get(&id.to_string()).map(|envelope|(envelope.window_id.clone(),envelope.document_id.clone()));
                if let Some((window,document))=owner{let outcome=result.and_then(|_|native::window_preview_cleared(&window,&document,&preview_id,generation,profile_id.as_deref(),cleared_website_data,cleared_http_auth));self.reply(&id,outcome);}
            },
        }
    }

    fn current_visible_threads(&self) -> Vec<String> {
        let inner = self.0.inner.borrow();
        let mut all = policy::collect_visible_threads(&inner.route, &inner.visible_threads);
        for win in inner.windows.values() {
            if win.visible {
                for tid in policy::collect_visible_threads(&win.route, &win.visible_threads) {
                    if !all.contains(&tid) {
                        all.push(tid);
                    }
                }
            }
        }
        all
    }

    fn state_for(&self,window:&str)->Value{let mut value=self.state();if let Some(fields)=crate::connections::snapshot(window).as_object(){for(key,item)in fields{value[key]=item.clone();}}let focus=native::focus_snapshot();value["previewFocused"]=json!(focus.preview_focused_window_id.as_deref()==Some(window));let inner=self.0.inner.borrow();if let Some(owner)=inner.windows.get(window){value["windowId"]=json!(window);value["documentId"]=json!(owner.document_id);value["route"]=json!(owner.route);value["notificationVisibleThreadIds"]=json!(owner.visible_threads);}let scope=crate::connections::scope(window);value["notificationOwner"]=json!(!scope.service_id.is_empty()&&!scope.identity_id.is_empty());value["notificationWorkspaceKey"]=json!(notifications::Scope{connection_version:scope.connection_version,service_id:scope.service_id,identity_id:scope.identity_id,origin:scope.origin}.key());value}

    fn state(&self) -> Value {
        let inner = self.0.inner.borrow();
        let owner = !inner.scope.service_id.is_empty() && !inner.scope.identity_id.is_empty();
        let conn_ver = inner.scope.connection_version;
        let scope_ver = inner.scope.scope_version;
        let scope = owner.then(|| notifications::Scope {
            connection_version: conn_ver,
            service_id: inner.scope.service_id.clone(),
            identity_id: inner.scope.identity_id.clone(),
            origin: crate::backend::backend_origin(),
        });
        let focus = native::focus_snapshot();
        let win_focused = if focus.initialized {
            focus.key_window_id.is_some()
        } else {
            inner.focused
        };
        let prev_focused = focus.preview_focused_window_id.is_some();
        drop(inner);
        let visible = if self.0.inner.borrow().visible && win_focused {
            self.current_visible_threads()
        } else {
            Vec::new()
        };
        let inner = self.0.inner.borrow();
        let windows_list: Vec<Value> = inner
            .windows
            .values()
            .map(|win| {
                json!({
                    "windowId": win.window_id,
                    "documentId": win.document_id,
                    "route": win.route,
                    "visible": win.visible,
                    "focused": win.focused,
                })
            })
            .collect();
        json!({
            "capabilities": policy::manifest(),
            "connectionVersion": conn_ver,
            "scopeVersion": scope_ver,
            "windowId": inner.active_window_id,
            "documentId": inner.document_id,
            "windows": windows_list,
            "serviceId": inner.scope.service_id,
            "identityId": inner.scope.identity_id,
            "origin": context().origin,
            "transportOrigin": "",
            "route": inner.route,
            "previewFocused": prev_focused,
            "dockBadge": native::dock_badge(),
            "notificationOwner": owner,
            "notificationWorkspaceKey": scope.map(|scope| scope.key()).unwrap_or_default(),
            "notificationVisibleThreadIds": visible,
            "desktopVersion": updates::app_version(),
        })
    }

    fn thread_visible_in(&self,window:&str,document:&str,thread:&str)->bool{self.0.inner.borrow().windows.get(window).is_some_and(|owner|owner.document_id==document&&(policy::route_thread(&owner.route).as_deref()==Some(thread)||owner.visible_threads.iter().any(|id|id==thread)))}

    /// A call from the main page (`window.nsh.postMessage`).
    pub fn receive(&self, data: &str) {
        self.receive_from_window("main", data);
    }

    fn receive_from_window(&self, default_window_id: &str, data: &str) {
        let Ok(message) = serde_json::from_str::<Value>(data) else { return };
        if message["t"] != "call" {
            return;
        }
        if !message["k"].as_str().is_some_and(|key| equal_secret(key, &context().key)) {
            eprintln!("Muteki: ignored a bridge call without the page key");
            return;
        }
        let window_id=default_window_id.to_owned();
        if message["windowId"].as_str().is_some_and(|claimed|claimed!=window_id){return;}
        if !native::verify_sender_origin(&window_id, &context().origin) {
            eprintln!("Muteki: rejected bridge call from untrusted sender frame on window {window_id}");
            return;
        }
        let document_id = message["documentId"].as_str().unwrap_or_default().to_owned();
        let original_id=message["id"].clone();
        let id=json!(serde_json::to_string(&json!([window_id,document_id,original_id])).expect("bridge request tuple"));
        let method = message["m"].as_str().unwrap_or_default().to_owned();
        let accepted = {
            let inner = self.0.inner.borrow();
            let retired = inner.retired_documents.get(&window_id).is_some_and(|documents| documents.contains(&document_id));
            let active = inner.windows.get(&window_id).map(|window| window.document_id.as_str()).unwrap_or("");
            !document_id.is_empty() && !retired
                && (active == document_id || (active.is_empty() && method == "ui.ready"))
        };
        if !accepted {
            self.send_to(&window_id, json!({"t":"reply","id":original_id,"ok":false,
                "windowId":window_id,"documentId":document_id,
                "e":bad("desktop.document_stale", "页面文档尚未登记或已失效，请等待当前页面就绪后重试。").to_json()}));
            return;
        }
        if method == "ui.ready" {
            updates::document_ready(&window_id,&document_id);
        }
        let mut params=message["p"].clone();if !params.is_object(){params=json!({});}params["__windowId"]=json!(window_id);params["__documentId"]=json!(document_id);if matches!(method.as_str(),"ui.view"|"ui.ready"){params["windowId"]=json!(window_id);params["documentId"]=json!(document_id);}

        {
            let mut inner = self.0.inner.borrow_mut();
            inner.window_scopes.entry(window_id.clone()).or_insert_with(||crate::connections::scope(&window_id).scope_version);
            if method == "ui.ready" {
                if window_id == "main" && inner.document_id != document_id {
                    inner.document_id = document_id.clone();
                }
                let entry = inner.windows.entry(window_id.clone()).or_insert_with(|| WindowState {
                    window_id: window_id.clone(),
                    ..WindowState::default()
                });
                entry.document_id = document_id.clone();
            }
            let scope=crate::connections::scope(&window_id);let scope_version=scope.scope_version;let connection_version=scope.connection_version;
            inner.pending_calls.insert(
                id.to_string(),
                policy::CallEnvelope {
                    id: original_id,
                    window_id: window_id.clone(),
                    document_id,
                    scope_version,
                    connection_version,
                    scope_bound: policy::is_scope_bound_method(&method),
                    method: method.clone(),
                },
            );
        }

        if let Some(outcome) = self.call(&id, &method, &params) {
            self.reply(&id, outcome);
        }
    }

    fn retire_window_document(&self, window: &str) {
        native::dispose_report_preview(window.to_owned());
        let mut inner = self.0.inner.borrow_mut();
        let document = inner.windows.get_mut(window).map(|entry| std::mem::take(&mut entry.document_id)).unwrap_or_default();
        if !document.is_empty() {
            let retired = inner.retired_documents.entry(window.to_owned()).or_default();
            retired.push_back(document);
            if retired.len() > 16 { retired.pop_front(); }
        }
        if window == "main" { inner.document_id.clear(); }
        inner.pending_calls.retain(|_, envelope| envelope.window_id != window);
        drop(inner);
        updates::document_closed(window);
    }

    /// Engine-side futures (dialogs, opening things) answer `id` when done.
    fn later(&self, id: &Value, future: impl Future<Output = Outcome> + 'static) {
        let shell = self.clone();
        let id = id.clone();
        self.0.owner.run(|| {
            spawn(async move {
                let outcome = future.await;
                shell.reply(&id, outcome);
            })
        });
    }

    /// The answer now, or `None` when it comes later.
    fn call(&self, id: &Value, method: &str, p: &Value) -> Option<Outcome> {
        let result = match method {
            "window.chrome" => {
                let window = p["__windowId"].as_str().unwrap_or("main").to_owned();
                let theme = p["theme"].as_str().unwrap_or("dark").to_owned();
                let mode = p["mode"].as_str().map(str::to_owned);
                let title = p["title"].as_str().unwrap_or(crate::APP_NAME).to_owned();
                on_main(id.clone(), move || native::set_window_chrome(&window, &theme, mode.as_deref(), &title));
                return None;
            }
            "window.action" => {
                let window = p["__windowId"].as_str().unwrap_or("main").to_owned();
                let action = p["action"].as_str().unwrap_or_default().to_owned();
                on_main(id.clone(), move || native::window_action(&window, &action));
                return None;
            }
            "state" => Ok(self.state_for(p["__windowId"].as_str().unwrap_or("main"))),
            "connection.transport"=>crate::connections::transport(p["__windowId"].as_str().unwrap_or("main")),
            "connection.configure"|"connection.resume"=>{let window=p["__windowId"].as_str().unwrap_or("main");crate::connections::configure(window,method=="connection.configure")},
            "connection.connect"|"connection.local"=>{let window=p["__windowId"].as_str().unwrap_or("main").to_owned();let selected=if method=="connection.local"{None}else{p["origin"].as_str().map(str::to_owned)};off_thread(id.clone(),move||crate::connections::connect(&window,selected.as_deref()));return None;},
            "auth.token" => Ok(crate::connections::token(p["__windowId"].as_str().unwrap_or("main")).map_or(Value::Null,Value::String)),
            "auth.set"=>{let window=p["__windowId"].as_str().unwrap_or("main");let scope=crate::connections::scope(window);if p["serviceId"].as_str().is_some_and(|id|id!=scope.service_id)||p["identityId"].as_str().is_some_and(|id|id!=scope.identity_id){Err(bad("desktop.auth_scope_changed","登录结果属于已切换的服务。"))}else{let token=p["token"].as_str();let result=crate::connections::set_token(window,token);if result.is_ok()&&token.filter(|v|!v.is_empty()).is_none(){let key=notifications::Scope{connection_version:scope.connection_version,service_id:scope.service_id,identity_id:scope.identity_id,origin:scope.origin}.key();self.0.inner.borrow_mut().dock_unread_by_scope.remove(&key);self.sync_dock_badge();}result}},
            "openExternal" => match policy::string(p, "url").and_then(policy::web_url) {
                Some(url) => {
                    on_main(id.clone(), move || {
                        native::confirm_and_open_in_browser(url.as_str()).map(|_| Value::Null)
                    });
                    return None;
                }
                None => Err(bad("desktop.external_url_invalid", "只能在浏览器打开 http:// 或 https:// 地址。")),
            },
            "window.open" | "openWindow" | "desktop:new-window" => self.open_new_window(p),
            "reportPreview.present" | "reportPreview.hide" | "reportPreview.scroll" | "reportPreview.scrollBy" | "reportPreview.focus" | "reportPreview.pointer" | "reportPreview.close" => {
                let window = p["__windowId"].as_str().unwrap_or("main").to_owned();
                let document = p["__documentId"].as_str().unwrap_or_default().to_owned();
                let report_route = self.0.inner.borrow().windows.get(&window).is_some_and(|owner| {
                    owner.document_id == document && is_report_editor_route(&owner.route)
                });
                if method == "reportPreview.present" && !report_route {
                    Err(bad("desktop.report_preview_scope", "当前页面不是报告编辑器。"))
                } else {
                    let method = method.to_owned();
                    let params = p.clone();
                    on_main(id.clone(), move || native::report_preview_command(&window, &document, &method, &params));
                    return None;
                }
            }
            "deepLink.open" => {
                let Some(url) = policy::string(p, "url").filter(|u| !u.is_empty()) else {
                    return Some(Err(bad("desktop.deep_link_invalid", "无效的 Muteki 链接。")));
                };
                let parsed = match policy::parse_deep_link(url) {
                    Ok(parsed) => parsed,
                    Err(error) => return Some(Err(error)),
                };
                self.enqueue_or_handle_deep_link(parsed.url.clone());
                Ok(json!({ "route": parsed.route, "threadId": parsed.thread_id, "serviceId": parsed.service_id }))
            }
            "ui.view" => self.view(p),
            "ui.ready" => {
                if let Some(doc_id) = policy::string(p, "documentId").filter(|d| !d.is_empty()) {
                    let wid = policy::string(p, "windowId").filter(|w| !w.is_empty()).unwrap_or("main");
                    let mut inner = self.0.inner.borrow_mut();
                    if wid == "main" {
                        inner.document_id = doc_id.to_owned();
                    }
                    inner
                        .windows
                        .entry(wid.to_owned())
                        .or_insert_with(|| WindowState {
                            window_id: wid.to_owned(),
                            ..WindowState::default()
                        })
                        .document_id = doc_id.to_owned();
                }
                self.0.inner.borrow_mut().ui_ready = true;
                native::BRIDGE_READY.store(true, Ordering::SeqCst);
                let origin = context().origin.clone();
                native::on_main(move || native::install(origin));
                updates::ui_ready();
                self.drain_deep_links();
                Ok(Value::Null)
            }
            "selectPath" => return self.select_path(id, p),
            "openPath" => return self.open_path(id, p),
            "selectWorkspaceRoot" => return self.select_workspace_root(id, p),
            "openWorkspaceFile" => return self.open_workspace_file(id, p),
            "openWorkspaceInEditor" => return self.open_in_editor(id, p),
            "setMenuAccelerators" => {
                policy::sync_accelerators(p).map(|(next, locale)| {
                    self.0.accelerators.set(next.clone());
                    if let Some(locale) = locale {
                        self.0.locale.set(locale);
                    }
                    json!(next)
                })
            }
            method if method.starts_with("preview.")=>return self.preview_call(id,method,p),
            "attachment.cache" | "attachment.restore" | "attachment.remove" | "attachment.usage" => return self.attachment(id, method, p),
            "terminal.authorize" => self.authorize_terminal(p),
            "quit.pending" => {
                let window = p["__windowId"].as_str().unwrap_or("main").to_owned();
                on_main(id.clone(), move || Ok(native::pending_quit(&window)));
                return None;
            }
            "quit.reply" => {
                let window = p["__windowId"].as_str().unwrap_or("main").to_owned();
                let token = p["token"].as_str().unwrap_or_default().to_owned();
                let confirmed = p["confirmed"] == true;
                let id = id.clone();
                native::on_main(move || native::answer_quit(id, &window, &token, confirmed));
                return None;
            }
            "close.ack" => {
                let token = policy::string(p, "token").unwrap_or_default().to_owned();
                let persisted = p["persisted"] == true;
                let error = policy::string(p, "error").unwrap_or_default().to_owned();
                if updates::acknowledge(&token,p["__windowId"].as_str().unwrap_or("main"),p["__documentId"].as_str().unwrap_or(""),persisted,&error) {
                    return Some(Ok(Value::Null));
                }
                on_main(id.clone(), move || done(native::acknowledge_close(&token, persisted, &error)));
                return None;
            }
            "updates.status" => Ok(updates::status()),
            "updates.check" => {
                off_thread(id.clone(), || Ok(updates::check()));
                return None;
            }
            "updates.install" => {
                off_thread(id.clone(), || updates::install().map_err(|error| BridgeError::new(error.code, error.message)));
                return None;
            }
            "openPermissionSettings" => {
                let pane = match policy::string(p, "kind") {
                    Some("microphone") => "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone",
                    Some("speechRecognition") => "x-apple.systempreferences:com.apple.preference.security?Privacy_SpeechRecognition",
                    Some("notifications") => "x-apple.systempreferences:com.apple.preference.notifications",
                    _ => return Some(Err(bad("desktop.permission_kind_invalid", "不支持的系统权限设置。"))),
                };
                off_thread(id.clone(), move || match crate::shell_env::command("/usr/bin/open").arg(pane).status() {
                    Ok(status) if status.success() => Ok(Value::Null),
                    Ok(status) => Err(BridgeError::new("desktop.settings_open_failed", format!("系统设置没有打开（{status}）。"))),
                    Err(error) => Err(BridgeError::new("desktop.settings_open_failed", format!("系统设置没有打开：{error}"))),
                });
                return None;
            }
            "speech.requestMicrophone" => {
                off_thread(id.clone(), || Ok(native::microphone_and_speech_permissions(true)));
                return None;
            }
            "speech.start" => self.speech_start(p),
            "speech.finish" => self.speech_finish(p),
            "speech.cancel" => self.speech_cancel(p),

            "notification.status" => return self.notification_status(id, p),
            "notification.request" => return self.notification_request(id, p),
            "notification.send" => return self.notification_send(id, p, false),
            "notification.test" => return self.notification_send(id, p, true),
            "notification.sound" => self.notification_sound(p),
            "notification.badge" => return self.notification_badge(id, p),
            "notification.dockBadge" => {
                on_main(id.clone(), || Ok(json!({ "badge": native::dock_badge() })));
                return None;
            }
            _ => Err(BridgeError::new("desktop.method_unknown", format!("桌面桥不支持 {method}。"))),
        };
        Some(result)
    }

    fn open_new_window(&self, p: &Value) -> Outcome {
        if updates::barrier_active(){return Err(bad("desktop.persistence_busy","正在保存全部窗口草稿，请稍后打开新窗口。"));}
        let route = policy::string(p, "route")
            .or_else(|| policy::string(p, "path"))
            .filter(|r| !r.is_empty())
            .unwrap_or("/");
        if !route.starts_with('/') || route.starts_with("//") || route.contains('\\') {
            return Err(bad("desktop.route_invalid", "新窗口路由无效。"));
        }
        let window_id = {
            let mut inner = self.0.inner.borrow_mut();
            inner.next_window_seq += 1;
            let wid = format!("window-{}", inner.next_window_seq);
            inner.windows.insert(
                wid.clone(),
                WindowState {
                    window_id: wid.clone(),
                    route: route.to_owned(),
                    visible: true,
                    focused: true,
                    ..WindowState::default()
                },
            );
            inner.active_window_id = wid.clone();
            wid
        };
        let url = format!("{}{route}", context().origin.trim_end_matches('/'));
        let script = include_str!("bridge.js")
            .replace("/*__MUTEKI_BROWSER_PAGE__*/", include_str!("browser_page.js"))
            .replace("__MUTEKI_BRIDGE_KEY__", &context().key)
            .replace("__MUTEKI_WINDOW_ID__", &window_id);
        let wid = window_id.clone();
        let parent=p["__windowId"].as_str().unwrap_or("main").to_owned();
        std::thread::spawn(move||{match crate::connections::new_window(&wid,&parent){Ok(_)=>native::on_main(move||{if let Err(error)=native::spawn_workbench_window(&wid,&url,&script){crate::connections::close(&wid);native::notify(Notice::WindowClosed(wid));eprintln!("Muteki window failed: {}",error.message);}}),Err(error)=>{native::notify(Notice::WindowClosed(wid));eprintln!("Muteki window connection failed: {}",error.message);}}});
        Ok(json!({ "windowId": window_id, "route": route }))
    }

    fn enqueue_or_handle_deep_link(&self, raw_url: String) {
        let ready = {
            let mut inner = self.0.inner.borrow_mut();
            if !inner.ui_ready {
                inner.pending_deep_links.push(raw_url);
                return;
            }
            true
        };
        if ready {
            self.handle_deep_link(&raw_url);
        }
    }

    fn drain_deep_links(&self) {
        let queued: Vec<String> = {
            let mut inner = self.0.inner.borrow_mut();
            if !inner.ui_ready {
                return;
            }
            std::mem::take(&mut inner.pending_deep_links)
        };
        for raw in queued {
            self.handle_deep_link(&raw);
        }
    }

    fn handle_deep_link(&self, raw_url: &str) {
        let parsed = match policy::parse_deep_link(raw_url) {
            Ok(parsed) => parsed,
            Err(error) => {
                eprintln!("Muteki: rejected invalid deep link {raw_url}: {}", error.message);
                return;
            }
        };
        let target_window = self.0.inner.borrow().active_window_id.clone();
        let target_window = if target_window.is_empty() { "main".to_owned() } else { target_window };
        let target_scope=crate::connections::scope(&target_window);
        let current_service=target_scope.service_id.clone();
        let route = parsed.route.clone();
        let link_event = json!({
            "url": parsed.url,
            "route": parsed.route,
            "threadId": parsed.thread_id,
            "serviceId": parsed.service_id,
            "identityId": parsed.identity_id,
        });
        let cross_service = parsed
            .service_id
            .as_ref()
            .filter(|s| !s.is_empty())
            .is_some_and(|want| !current_service.is_empty() && want != &current_service);
        let want_service = parsed.service_id.clone().unwrap_or_default();

        native::on_main(move || {
            if cross_service {
                let choice = native::alert(
                    false,
                    "跨服务打开 Muteki 链接？",
                    &format!(
                        "该链接指向服务 {want_service}，而当前连接的服务是 {current_service}。是否在当前工作台打开 {route}？"
                    ),
                    &["取消", "继续打开"],
                );
                if choice != 1 {
                    return;
                }
            }
            native::request_flush_barrier("deep-link", move |persisted, error| {
                if !persisted {
                    let detail = if error.is_empty() {
                        "页面没有在 5 秒内确认草稿已保存。继续跳转可能丢失尚未保存的输入。"
                    } else {
                        error.as_str()
                    };
                    if native::alert(true, "草稿保存失败。仍然打开该链接？", detail, &["取消", "继续打开"]) != 1 {
                        return;
                    }
                }
                if crate::connections::scope(&target_window)!=target_scope{return;}
                let _ = native::restore_and_focus_window(&target_window);
                native::notify(Notice::WindowEvent(target_window.clone(),"deep-link".into(), link_event.clone()));
                native::notify(Notice::WindowEvent(target_window,"navigate".into(), json!({ "route": route, "source": "deep-link" })));
            });
        });
    }

    fn speech_start(&self,p:&Value)->Outcome{
        let window=p["__windowId"].as_str().unwrap_or("main");let document=p["__documentId"].as_str().unwrap_or("");let scope=crate::connections::scope(window);let inner=self.0.inner.borrow();let route=inner.windows.get(window).map(|window|window.route.as_str()).unwrap_or("");
        if !(route=="/chat"||route.starts_with("/chat/")){return Err(bad("desktop.speech.chat_required","请在聊天页面开始语音输入。"));}
        if scope.service_id.is_empty()||p["serviceId"]!=scope.service_id||p["identityId"]!=scope.identity_id||p["connectionVersion"].as_u64()!=Some(scope.connection_version){return Err(bad("desktop.speech.scope_changed","语音输入所属的工作台已经改变。"));}
        crate::speech::start(window,document,route,scope.scope_version,p)
    }
    fn speech_finish(&self,p:&Value)->Outcome{crate::speech::finish(p["__windowId"].as_str().unwrap_or("main"),p["__documentId"].as_str().unwrap_or(""),p)}
    fn speech_cancel(&self,p:&Value)->Outcome{crate::speech::cancel(p["__windowId"].as_str().unwrap_or("main"),p["__documentId"].as_str().unwrap_or(""),p)}

    fn notification_scope(&self, p: &Value) -> Result<notifications::Scope, BridgeError> {
        let scope=crate::connections::scope(p["__windowId"].as_str().unwrap_or("main"));
        let expected_version = scope.connection_version;
        if scope.service_id.is_empty()
            || scope.identity_id.is_empty()
            || p["connectionVersion"].as_u64() != Some(expected_version)
            || p["serviceId"].as_str() != Some(scope.service_id.as_str())
            || p["identityId"].as_str() != Some(scope.identity_id.as_str())
        {
            return Err(bad("desktop.notification_scope_changed", "通知授权所属的工作台已改变，请在当前工作台重试。"));
        }
        Ok(notifications::Scope {
            connection_version: expected_version,
            service_id: scope.service_id.clone(),
            identity_id: scope.identity_id.clone(),
            origin: scope.origin.clone(),
        })
    }

    fn notification_visible(&self,scope:&notifications::Scope,thread:&str,test:bool)->bool{
        if test{return false}
        let focus = native::focus_snapshot();
        self.0.inner.borrow().windows.values().any(|window|{
            let window_focused = if focus.initialized {
                focus.key_window_id.as_deref() == Some(window.window_id.as_str())
            } else {
                window.focused
            };
            if !window.visible||!window_focused{return false}
            let active=crate::connections::scope(&window.window_id);
            scope.matches_active(&notifications::Scope{connection_version:active.connection_version,service_id:active.service_id,identity_id:active.identity_id,origin:active.origin})&&policy::collect_visible_threads(&window.route,&window.visible_threads).iter().any(|visible|visible==thread)
        })
    }

    fn compute_dock_badge_count(&self) -> usize {
        let focus = native::focus_snapshot();
        let mut live_scopes = std::collections::HashSet::new();
        let mut active_visible_by_scope: HashMap<String, std::collections::HashSet<String>> =
            HashMap::new();
        {
            let inner = self.0.inner.borrow();
            for win in inner.windows.values() {
                if crate::connections::snapshot(&win.window_id)["configuring"] == true {
                    continue;
                }
                let scope_state = crate::connections::scope(&win.window_id);
                if scope_state.service_id.is_empty() || scope_state.identity_id.is_empty() {
                    continue;
                }
                let scope_key = notifications::Scope {
                    connection_version: scope_state.connection_version,
                    service_id: scope_state.service_id,
                    identity_id: scope_state.identity_id,
                    origin: scope_state.origin,
                }
                .key();
                live_scopes.insert(scope_key.clone());
                let actively_focused = win.visible
                    && if focus.initialized {
                        focus.key_window_id.as_deref() == Some(win.window_id.as_str())
                    } else {
                        win.focused
                    };
                if actively_focused {
                    let visible = active_visible_by_scope.entry(scope_key).or_default();
                    for tid in policy::collect_visible_threads(&win.route, &win.visible_threads) {
                        visible.insert(tid);
                    }
                }
            }
        }
        let mut inner = self.0.inner.borrow_mut();
        inner.dock_unread_by_scope.retain(|key, _| live_scopes.contains(key));
        let mut count = 0usize;
        for (scope_key, entries) in &inner.dock_unread_by_scope {
            let visible = active_visible_by_scope.get(scope_key);
            let mut seen = std::collections::HashSet::new();
            for entry in entries {
                if entry.thread_id.is_empty() || !seen.insert(entry.thread_id.as_str()) {
                    continue;
                }
                let shown_in_foreground = visible.is_some_and(|set| {
                    set.contains(&entry.thread_id)
                        || entry.visible_thread_ids.iter().any(|tid| set.contains(tid))
                });
                if !shown_in_foreground {
                    count += 1;
                }
            }
        }
        count
    }

    fn sync_dock_badge(&self) {
        let count = self.compute_dock_badge_count();
        let changed = {
            let mut inner = self.0.inner.borrow_mut();
            if inner.last_dock_badge_count != Some(count) {
                inner.last_dock_badge_count = Some(count);
                true
            } else {
                false
            }
        };
        if changed {
            native::set_dock_badge(count);
        }
    }

    fn notification_badge(&self, id: &Value, p: &Value) -> Option<Outcome> {
        let scope = match self.notification_scope(p) {
            Ok(scope) => scope,
            Err(error) => return Some(Err(error)),
        };
        let Some(rows) = p["threads"].as_array() else {
            return Some(Err(bad("desktop.notification_badge_invalid", "Dock 角标会话列表无效。")));
        };
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            let thread_id = if let Some(s) = row.as_str() {
                s.trim().to_owned()
            } else if let Some(s) = row["threadId"].as_str() {
                s.trim().to_owned()
            } else {
                continue;
            };
            if thread_id.is_empty() {
                continue;
            }
            let mut visible_thread_ids = Vec::new();
            visible_thread_ids.push(thread_id.clone());
            if let Some(arr) = row["visibleThreadIds"].as_array() {
                for item in arr.iter().filter_map(Value::as_str) {
                    let trimmed = item.trim();
                    if !trimmed.is_empty()
                        && !visible_thread_ids.iter().any(|existing| existing == trimmed)
                    {
                        visible_thread_ids.push(trimmed.to_owned());
                    }
                }
            }
            entries.push(DockUnreadThread {
                thread_id,
                visible_thread_ids,
            });
        }
        self.0
            .inner
            .borrow_mut()
            .dock_unread_by_scope
            .insert(scope.key(), entries);
        let count = self.compute_dock_badge_count();
        self.0.inner.borrow_mut().last_dock_badge_count = Some(count);
        native::set_desired_dock_badge(count);
        on_main(id.clone(), move || {
            let (applied_count, badge) = native::flush_dock_badge_on_main();
            Ok(json!({
                "connectionVersion": scope.connection_version,
                "serviceId": scope.service_id,
                "identityId": scope.identity_id,
                "count": applied_count,
                "badge": badge,
            }))
        });
        None
    }

    fn notification_status(&self, id: &Value, p: &Value) -> Option<Outcome> {
        let scope = match self.notification_scope(p) { Ok(scope) => scope, Err(error) => return Some(Err(error)) };
        let choice = match notifications::choice(context(), &scope) { Ok(choice) => choice, Err(error) => return Some(Err(error)) };
        let delivery = {
            let inner = self.0.inner.borrow();
            inner.notification_deliveries.get(&scope.key()).cloned()
        };
        off_thread(id.clone(), move || Ok(notifications::status(&scope, choice, native::notification_settings(), delivery.as_ref())));
        None
    }

    fn notification_request(&self, id: &Value, p: &Value) -> Option<Outcome> {
        let scope = match self.notification_scope(p) { Ok(scope) => scope, Err(error) => return Some(Err(error)) };
        let id = id.clone();
        let params = p.clone();
        native::on_main(move || {
            let choice = native::alert(false, "允许此工作台发送桌面通知？", &format!("{}\n系统通知权限由操作系统单独管理。", scope.origin), &["取消", "不允许", "允许"]);
            native::notify(Notice::NotificationConsentChoice(id, params, choice));
        });
        None
    }

    fn notification_consent_chosen(&self, id: Value, params: &Value, choice: usize) {
        if !self.0.inner.borrow().pending_calls.contains_key(&id.to_string()){return;}
        let scope = match self.notification_scope(params) {
            Ok(scope) => scope,
            Err(error) => { self.reply(&id, Err(error)); return; }
        };
        if choice != 0 {
            if let Err(error) = notifications::save_choice(context(), &scope, choice == 2) {
                self.reply(&id, Err(error));
                return;
            }
        }
        let consent = match notifications::choice(context(), &scope) {
            Ok(consent) => consent,
            Err(error) => { self.reply(&id, Err(error)); return; }
        };
        off_thread(id, move || {
            if choice == 2 { native::request_notification_authorization()?; }
            Ok(notifications::status(&scope, consent, native::notification_settings(), None))
        });
    }

    fn notification_event(&self, entry: &mut NotificationEntry, status: &str, code: &str, message: &str) -> Value {
        let value = {
            let mut inner = self.0.inner.borrow_mut();
            inner.notification_sequence += 1;
            let value = entry.receipt(inner.notification_sequence, status, code, message);
            inner.notification_deliveries.insert(entry.scope.key(),value.clone());
            value
        };
        let current=crate::connections::scope(&entry.window_id);
        if entry.scope.matches_active(&notifications::Scope{connection_version:current.connection_version,service_id:current.service_id,identity_id:current.identity_id,origin:current.origin}){self.send_to(&entry.window_id,json!({"t":"event","n":"notification","v":value.clone()}));}
        value
    }

    fn notification_send(&self, id: &Value, p: &Value, test: bool) -> Option<Outcome> {
        let scope = match self.notification_scope(p) { Ok(scope) => scope, Err(error) => return Some(Err(error)) };
        let choice = match notifications::choice(context(), &scope) { Ok(choice) => choice, Err(error) => return Some(Err(error)) };
        if choice != Some(true) {
            return Some(Err(bad("desktop.notifications_workspace_denied", "此工作台尚未获得通知授权。")));
        }
        let mut params = p.clone();
        if test {
            let event = uuid::Uuid::new_v4().to_string();
            params["threadId"] = json!("__notification_test__");
            params["eventId"] = json!(event);
            params["dedupeKey"] = json!(format!("notification-test:{event}"));
            params["title"] = json!("Muteki 测试通知");
            params["body"] = json!("这是一条测试通知。点击返回通知设置。");
            params["_test"] = json!(true);
        }
        let required = ["threadId", "eventId", "dedupeKey", "title", "body"];
        if required.iter().any(|key| params[*key].as_str().is_none())
            || ["threadId", "eventId", "dedupeKey"].iter().any(|key| params[*key].as_str() == Some(""))
        {
            return Some(Err(bad("desktop.notification_input_invalid", "通知缺少会话或事件标识。")));
        }
        if !params["wantSound"].is_null() && !params["wantSound"].is_boolean() {
            return Some(Err(bad("desktop.notification_sound_invalid", "wantSound 必须是布尔值。")));
        }
        let id = id.clone();
        std::thread::spawn(move || native::notify(Notice::NotificationPrepared(id, params, native::notification_settings())));
        None
    }

    fn notification_prepared(&self, id: Value, p: &Value, settings: Result<Value, BridgeError>) {
        if !self.0.inner.borrow().pending_calls.contains_key(&id.to_string()){return;}
        let scope = match self.notification_scope(p) {
            Ok(scope) => scope,
            Err(error) => { self.reply(&id, Err(error)); return; }
        };
        let choice = match notifications::choice(context(), &scope) { Ok(choice) => choice, Err(error) => { self.reply(&id, Err(error)); return; } };
        if choice != Some(true) {
            self.reply(&id, Err(bad("desktop.notifications_workspace_denied", "此工作台尚未获得通知授权。")));
            return;
        }
        let thread = p["threadId"].as_str().unwrap_or_default().to_owned();
        let event = p["eventId"].as_str().unwrap_or_default().to_owned();
        let dedupe = p["dedupeKey"].as_str().unwrap_or_default().to_owned();
        let test = p["_test"] == true;
        let system = notifications::status(&scope, Some(true), settings, None);
        let entry_key=json!([scope.key(),thread]).to_string();
        let mut entry = NotificationEntry {
            window_id:p["__windowId"].as_str().unwrap_or("main").into(),document_id:p["__documentId"].as_str().unwrap_or("").into(),
            id: uuid::Uuid::new_v4().to_string(), scope: scope.clone(), thread_id: thread.clone(), event_id: event.clone(),
            dedupe_key: dedupe.clone(), test, shown: false, sound_requested: p["wantSound"] == true,
            system: Some(system.clone()), history: Vec::new(), last: Value::Null,
        };
        let visible=self.notification_visible(&scope,&thread,test);
        if visible {
            let receipt = self.notification_event(&mut entry, "closed", "desktop.notification_thread_visible", "当前会话已在前台显示。");
            self.reply(&id, Ok(receipt));
            return;
        }
        if system["systemPermission"] == "denied" {
            let receipt = self.notification_event(&mut entry, "closed", "desktop.notifications_system_denied", "系统已拒绝通知授权。" );
            self.reply(&id, Ok(receipt));
            return;
        }
        let prior = self.0.inner.borrow().dedupe_store.get_delivered(&scope, &dedupe);
        if let Some(mut prior) = prior {
            let mut inner = self.0.inner.borrow_mut();
            inner.notification_sequence += 1;
            prior["seq"] = json!(inner.notification_sequence);
            prior["requestedEventId"] = json!(event);
            prior["code"] = json!("desktop.notification_already_delivered");
            drop(inner);
            self.reply(&id, Ok(prior));
            return;
        }
        if let Some(previous) = self.0.inner.borrow().notifications.get(&entry_key)
            && previous.scope.key() == scope.key() && previous.dedupe_key == entry.dedupe_key
        {
            let mut receipt = previous.last.clone();
            receipt["requestedEventId"] = json!(event);
            self.reply(&id, Ok(receipt));
            return;
        }
        let previous = self.0.inner.borrow_mut().notifications.remove(&entry_key);
        if let Some(mut previous) = previous {
            self.notification_event(&mut previous, "closed", "desktop.notification_replaced", "此会话的新通知取代了上一条通知。");
            let previous_id = previous.id;
            std::thread::spawn(move || native::remove_notification(&previous_id));
        }
        self.notification_event(&mut entry, "submitted", "", "");
        if entry.sound_requested {
            if let Err(error) = play_cue_sound() {
                let receipt = self.notification_event(&mut entry, "failed", error.code, &error.message);
                self.reply(&id, Ok(receipt));
                return;
            }
        }
        let receipt = self.notification_event(&mut entry, "awaiting_show", "", "");
        let note_id = entry.id.clone();
        let title = p["title"].as_str().unwrap_or_default().to_owned();
        let body = p["body"].as_str().unwrap_or_default().to_owned();
        self.0.inner.borrow_mut().notifications.insert(entry_key, entry);
        std::thread::spawn(move || {
            let result = native::schedule_notification(&note_id, &title, &body);
            native::notify(Notice::NotificationScheduled(note_id, result));
        });
        self.reply(&id, Ok(receipt));
    }

    fn cancel_window_notifications(&self, window: &str, pending_only: bool, code: &str, message: &str) {
        let entries = {
            let mut inner = self.0.inner.borrow_mut();
            let keys = inner.notifications.iter()
                .filter(|(_, entry)| entry.window_id == window && (!pending_only || !entry.shown))
                .map(|(key, _)| key.clone()).collect::<Vec<_>>();
            keys.into_iter().filter_map(|key| inner.notifications.remove(&key)).collect::<Vec<_>>()
        };
        for mut entry in entries {
            self.notification_event(&mut entry, "closed", code, message);
            std::thread::spawn(move || native::remove_notification(&entry.id));
        }
    }

    fn notification_scheduled(&self, entry_id: &str, result: Result<(), BridgeError>) {
        let thread = self.0.inner.borrow().notifications.iter()
            .find(|(_, entry)| entry.id == entry_id).map(|(thread, _)| thread.clone());
        let Some(thread) = thread else {
            let entry_id = entry_id.to_owned();
            std::thread::spawn(move || native::remove_notification(&entry_id));
            return;
        };
        let Some(mut entry) = self.0.inner.borrow_mut().notifications.remove(&thread) else { return };
        let active = crate::connections::scope(&entry.window_id);
        let owner_valid = self.0.inner.borrow().windows.get(&entry.window_id)
            .is_some_and(|window| window.document_id == entry.document_id)
            && entry.scope.matches_active(&notifications::Scope {
                connection_version: active.connection_version, service_id: active.service_id,
                identity_id: active.identity_id, origin: active.origin,
            });
        if !owner_valid && !entry.shown {
            self.notification_event(&mut entry, "closed", "desktop.notification_owner_changed", "通知所属的页面或工作台已改变。");
            std::thread::spawn(move || native::remove_notification(&entry.id));
            return;
        }
        match result {
            Ok(()) => {
                entry.shown = true;
                let value = self.notification_event(&mut entry, "shown", "", "");
                self.0.inner.borrow_mut().dedupe_store.record_delivered(&entry.scope, &entry.dedupe_key, value);
                self.0.inner.borrow_mut().notifications.insert(thread, entry);
            }
            Err(error) if error.code == "desktop.notification_show_unknown" => {
                if !entry.shown {
                    self.notification_event(&mut entry, "outcome_unknown", error.code, &error.message);
                }
                self.0.inner.borrow_mut().notifications.insert(thread, entry);
            }
            Err(error) => { self.notification_event(&mut entry, "failed", error.code, &error.message); }
        }
    }

    fn notification_clicked(&self, entry_id: &str) {
        let thread = self.0.inner.borrow().notifications.iter()
            .find(|(_, entry)| entry.id == entry_id).map(|(thread, _)| thread.clone());
        let Some(thread) = thread else { return };
        let Some(mut entry) = self.0.inner.borrow_mut().notifications.remove(&thread) else { return };
        let target_window=entry.window_id.clone();
        let target_scope=crate::connections::scope(&target_window);
        let scope_valid=entry.scope.matches_active(&notifications::Scope{connection_version:target_scope.connection_version,service_id:target_scope.service_id.clone(),identity_id:target_scope.identity_id.clone(),origin:target_scope.origin.clone()});
        let remove_id = entry_id.to_owned();
        std::thread::spawn(move || native::remove_notification(&remove_id));
        if !scope_valid {
            self.notification_event(&mut entry, "closed", "desktop.notification_scope_changed", "通知所属的工作台已改变。");
            return;
        }
        entry.shown = true;
        let clicked_receipt = self.notification_event(&mut entry, "clicked", "", "");
        self.0
            .inner
            .borrow_mut()
            .dedupe_store
            .record_delivered(&entry.scope, &entry.dedupe_key, clicked_receipt.clone());

        let is_test = entry.test;
        let target_thread = entry.thread_id.clone();
        native::on_main(move || {
            if let Err(error) = native::restore_and_focus_window(&target_window) {
                native::notify(Notice::WindowEvent(target_window.clone(),
                    "notification-navigation-error".into(),
                    json!({ "id": clicked_receipt["id"], "threadId": target_thread, "error": error.to_json() }),
                ));
                return;
            }
            native::request_flush_barrier("notification-navigation", move |persisted, flush_err| {
                if !persisted {
                    let detail = if flush_err.is_empty() {
                        "页面没有在 5 秒内确认草稿已保存。继续跳转可能丢失尚未保存的输入。"
                    } else {
                        flush_err.as_str()
                    };
                    if native::alert(true, "草稿保存失败。仍然跳转到通知会话？", detail, &["取消", "跳转"]) != 1 {
                        return;
                    }
                }
                if crate::connections::scope(&target_window)!=target_scope{return;}
                let route = native::notification_route(&target_thread, is_test);
                if let Err(error) = native::prepare_window_route(&target_window, &route) {
                    native::notify(Notice::WindowEvent(target_window.clone(),
                        "notification-navigation-error".into(),
                        json!({ "id": clicked_receipt["id"], "threadId": target_thread, "error": error.to_json() }),
                    ));
                    return;
                }
                native::notify(Notice::NotificationNavigate {
                    window_id: target_window,
                    route,
                    thread_id: target_thread,
                    test: is_test,
                    receipt_id: clicked_receipt["id"].clone(),
                });
            });
        });
    }

    fn notification_sound(&self, p: &Value) -> Outcome {
        let scope = self.notification_scope(p)?;
        if notifications::choice(context(), &scope)? != Some(true) {
            return Err(bad("desktop.notifications_workspace_denied", "此工作台尚未允许通知提示音。"));
        }
        let thread = policy::required(p, "threadId", "desktop.notification_sound_invalid", "提示音缺少会话或事件标识。")?;
        let event = policy::required(p, "eventId", "desktop.notification_sound_invalid", "提示音缺少会话或事件标识。")?;
        let dedupe = policy::required(p, "dedupeKey", "desktop.notification_sound_invalid", "提示音缺少会话或事件标识。")?;
        let visible=self.notification_visible(&scope,thread,false);
        let status = if visible {
            "suppressed"
        } else if self.0.inner.borrow().dedupe_store.has_sound(&scope, dedupe) {
            "already-requested"
        } else {
            play_cue_sound()?;
            self.0.inner.borrow_mut().dedupe_store.record_sound(&scope, dedupe);
            "sound-requested"
        };
        Ok(json!({"connectionVersion":scope.connection_version,"serviceId":scope.service_id,"identityId":scope.identity_id,
            "threadId":thread,"eventId":event,"dedupeKey":dedupe,"status":status,"host":"desktop-client"}))
    }

    fn view(&self, p: &Value) -> Outcome {
        let route = policy::string(p, "route").filter(|route| route.starts_with('/')).ok_or_else(|| bad("desktop.route_invalid", "页面路由无效。"))?;
        let visible = p["visible"] == true;
        let focused = p.get("focused").and_then(Value::as_bool).unwrap_or(visible);
        let window_id = policy::string(p, "windowId").filter(|w| !w.is_empty()).unwrap_or("main").to_owned();
        let document_id = policy::string(p, "documentId").unwrap_or_default().to_owned();
        let prev_report_doc = self.0.inner.borrow().windows.get(&window_id)
            .filter(|owner| is_report_editor_route(&owner.route))
            .map(|owner| owner.document_id.clone());
        let now_report_editor = is_report_editor_route(route);
        if !now_report_editor && prev_report_doc.is_some() {
            native::dispose_report_preview(window_id.clone());
        }
        crate::speech::context_changed(&window_id,&document_id,route,crate::connections::scope(&window_id).scope_version);
        let explicit_threads: Vec<String> = p["visibleThreadIds"]
            .as_array()
            .map(|arr| arr.iter().filter_map(Value::as_str).map(str::to_owned).collect())
            .unwrap_or_default();

        let changed = {
            let mut inner = self.0.inner.borrow_mut();
            let changed = inner.route != route
                || inner.visible != visible
                || inner.focused != focused
                || inner.visible_threads != explicit_threads;
            if window_id == "main" || focused {
                inner.route = route.to_owned();
                inner.visible = visible;
                inner.focused = focused;
                inner.visible_threads = explicit_threads.clone();
                if !document_id.is_empty() && window_id == "main" {
                    inner.document_id = document_id.clone();
                }
            }
            if focused {
                inner.active_window_id = window_id.clone();
            }
            inner.windows.insert(
                window_id.clone(),
                WindowState {
                    window_id:window_id.clone(),
                    document_id:document_id.clone(),
                    route: route.to_owned(),
                    visible,
                    focused,
                    visible_threads: explicit_threads.clone(),
                },
            );
            changed
        };
        if now_report_editor && !document_id.is_empty() && prev_report_doc.as_deref() != Some(document_id.as_str()) {
            let shell = self.clone();
            let wid = window_id.clone();
            let doc = document_id.clone();
            self.0.owner.run(|| {
                spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    let still_report_editor = shell.0.inner.borrow().windows.get(&wid).is_some_and(|owner| {
                        owner.document_id == doc && is_report_editor_route(&owner.route)
                    });
                    if still_report_editor {
                        native::warm_report_preview(wid, doc);
                    }
                });
            });
        }
        let active=policy::route_thread(route).or_else(||explicit_threads.first().cloned());native::hide_window_preview(&window_id,&document_id,active.as_deref());
        native::refresh_focus_state();
        self.sync_dock_badge();
        if changed {
            self.emit("state", self.state());
        }
        Ok(Value::Null)
    }

    fn select_path(&self, id: &Value, p: &Value) -> Option<Outcome> {
        let dialog = match policy::string(p, "kind") {
            Some("directory") => OpenDialog::folders().title("选择目录"),
            Some("file") => OpenDialog::files().title("选择文件"),
            _ => return Some(Err(bad("desktop.path_kind_invalid", "只能选择文件或目录。"))),
        };
        let window=p["__windowId"].as_str().unwrap_or("main").to_owned();let document=p["__documentId"].as_str().unwrap_or("").to_owned();let initial_scope=crate::connections::scope(&window).scope_version;
        let shell = self.clone();
        self.later(id, async move {
            match pick_paths(dialog).await {
                Ok(Some(paths)) => match paths.into_iter().next() {
                    Some(path) => {
                        if crate::connections::scope(&window).scope_version != initial_scope {
                            return Err(bad("desktop.scope_changed", "选择期间工作台身份已改变，请重新选择。"));
                        }
                        let selected = uuid::Uuid::new_v4().to_string();
                        let result = json!({ "id": selected, "path": path.to_string_lossy(), "name": name_of(&path), "host": "desktop-client", "serverMapped": false });
                        shell.0.inner.borrow_mut().paths.insert(json!([window,document,selected]).to_string(),path);
                        Ok(result)
                    }
                    None => Ok(Value::Null),
                },
                Ok(None) => Ok(Value::Null),
                Err(error) => Err(BridgeError::new("desktop.dialog_failed", format!("无法打开选择窗口：{error}"))),
            }
        });
        None
    }

    fn open_target(&self, id: &Value, target: PathBuf, action: Option<&str>) -> Option<Outcome> {
        let reveal = match action {
            Some("reveal") => true,
            Some("open") => false,
            _ => return Some(Err(bad("desktop.path_action_invalid", "只能打开或在访达中显示。"))),
        };
        self.later(id, async move {
            let result = if reveal { reveal_path(target).await } else { open_path(target).await };
            result.map(|()| Value::Null).map_err(|error| BridgeError::new("desktop.path_open_failed", format!("无法打开：{error}")))
        });
        None
    }

    fn open_path(&self, id: &Value, p: &Value) -> Option<Outcome> {
        let path = policy::string(p, "id").and_then(|selected| self.0.inner.borrow().paths.get(&json!([p["__windowId"],p["__documentId"],selected]).to_string()).cloned());
        match path {
            Some(path) => self.open_target(id, path, policy::string(p, "action")),
            None => Some(Err(bad("desktop.path_unknown", "路径选择已失效，请重新选择。"))),
        }
    }

    fn identity_matches(&self, p: &Value) -> Result<(String, String), BridgeError> {
        let scope=crate::connections::scope(p["__windowId"].as_str().unwrap_or("main"));
        let service = policy::string(p, "serviceId").unwrap_or_default();
        let identity = policy::string(p, "identityId").unwrap_or_default();
        if scope.service_id.is_empty() || service != scope.service_id || identity != scope.identity_id {
            return Err(bad("desktop.identity_unverified", "请先确认当前服务身份，再连接本机对应目录。"));
        }
        Ok((service.to_owned(), identity.to_owned()))
    }

    fn select_workspace_root(&self, id: &Value, p: &Value) -> Option<Outcome> {
        let fields = (|| {
            let (service_id, identity_id) = self.identity_matches(p)?;
            let need = |key: &str| policy::required(p, key, "desktop.workspace_grant_invalid", "工作区映射请求无效。").map(str::to_owned);
            Ok::<_, BridgeError>((need("threadId")?, need("workspaceId")?, need("serviceRoot")?, service_id, identity_id))
        })();
        let (thread_id, workspace_id, service_root, service_id, identity_id) = match fields {
            Ok(fields) => fields,
            Err(error) => return Some(Err(error)),
        };
        let window_id=p["__windowId"].as_str().unwrap_or("main").to_owned();let document_id=p["__documentId"].as_str().unwrap_or("").to_owned();let initial_scope_version=crate::connections::scope(&window_id).scope_version;
        let shell = self.clone();
        self.later(id, async move {
            let picked = pick_paths(OpenDialog::folders().title("选择本机对应的工作区目录"))
                .await
                .map_err(|error| BridgeError::new("desktop.dialog_failed", format!("无法打开选择窗口：{error}")))?;
            let Some(path) = picked.and_then(|paths| paths.into_iter().next()) else { return Ok(Value::Null) };
            {
                let scope=crate::connections::scope(&window_id);
                let scope_ok=scope.scope_version==initial_scope_version&&scope.service_id==service_id&&scope.identity_id==identity_id;
                if !scope_ok || !shell.thread_visible_in(&window_id,&document_id,&thread_id) {
                    return Err(bad("desktop.workspace_scope_changed", "选择目录期间当前会话或服务身份已改变，请重新选择。"));
                }
            }
            let root = path.canonicalize().map_err(|error| BridgeError::new("desktop.workspace_scope_changed", format!("本机目录不可用：{error}")))?;
            let grant_id = uuid::Uuid::new_v4().to_string();
            let result = json!({
                "grantId": grant_id, "threadId": thread_id, "workspaceId": workspace_id,
                "clientRoot": root.to_string_lossy(), "serviceRoot": service_root,
                "host": "desktop-client", "mapping": "user-selected",
            });
            shell.0.inner.borrow_mut().grants.insert(grant_id, Grant { window_id,document_id,scope_version:initial_scope_version,thread_id, workspace_id, service_id, identity_id, client_root: root });
            Ok(result)
        });
        None
    }

    fn grant_root(&self, p: &Value) -> Result<PathBuf, BridgeError> {
        self.identity_matches(p)?;
        let requested_thread = policy::string(p, "threadId").unwrap_or_default();
        let window=p["__windowId"].as_str().unwrap_or("main");let document=p["__documentId"].as_str().unwrap_or("");let scope=crate::connections::scope(window);
        if !requested_thread.is_empty() && !self.thread_visible_in(window,document,requested_thread) {
            return Err(bad("desktop.workspace_scope_changed", "当前会话已切换，请在当前会话重新操作。"));
        }
        let inner = self.0.inner.borrow();
        let grant = policy::string(p, "grantId").and_then(|grant| inner.grants.get(grant));
        match grant {
            Some(grant)
                if Some(grant.thread_id.as_str()) == policy::string(p, "threadId")
                    && Some(grant.workspace_id.as_str()) == policy::string(p, "workspaceId")
                    && grant.window_id==window&&grant.document_id==document&&grant.scope_version==scope.scope_version&&grant.service_id == scope.service_id
                    && grant.identity_id == scope.identity_id =>
            {
                Ok(grant.client_root.clone())
            }
            _ => Err(bad("desktop.workspace_grant_expired", "本机目录映射已过期，请重新选择。")),
        }
    }

    fn open_workspace_file(&self, id: &Value, p: &Value) -> Option<Outcome> {
        let target = self.grant_root(p).and_then(|root| {
            let relative = policy::required(p, "relativePath", "desktop.workspace_file_invalid", "请选择工作区内的相对文件路径。")?;
            policy::resolve_in_root(&root, Some(relative))
        });
        match target {
            Ok(target) => self.open_target(id, target, policy::string(p, "action")),
            Err(error) => Some(Err(error)),
        }
    }

    fn open_in_editor(&self, id: &Value, p: &Value) -> Option<Outcome> {
        let prepared = (|| {
            let root = self.grant_root(p)?;
            let relative = policy::string(p, "relativePath").filter(|path| !path.is_empty());
            let target = policy::resolve_in_root(&root, relative)?;
            let line = match p.get("line") {
                None | Some(Value::Null) => None,
                Some(line) => Some(line.as_u64().filter(|line| *line >= 1).ok_or_else(|| bad("desktop.editor_line_invalid", "行号无效。"))?),
            };
            let preferred = policy::string(p, "editor").filter(|editor| !editor.is_empty()).unwrap_or("auto");
            let path_env = crate::shell_env::path_string();
            let editor = policy::find_editor(preferred, &path_env, executable)?;
            Ok::<_, BridgeError>((target, line, editor))
        })();
        let (target, line, editor) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => return Some(Err(error)),
        };
        let path = target.to_string_lossy().into_owned();
        let Some(editor) = editor else {
            self.later(id, async move {
                open_path(target).await.map_err(|error| BridgeError::new("desktop.path_open_failed", format!("无法打开：{error}")))?;
                Ok(json!({ "opener": "system", "label": "系统默认应用", "path": path }))
            });
            return None;
        };
        let launched = crate::shell_env::command(&editor.file)
            .args(policy::editor_args(editor.name, &target, line))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        Some(match launched {
            Ok(mut child) => {
                std::thread::spawn(move || child.wait());
                Ok(json!({ "opener": editor.name, "label": editor.label, "path": path }))
            }
            Err(error) => Err(BridgeError::new("desktop.editor_launch_failed", format!("无法启动 {}：{error}", editor.label))),
        })
    }

    fn preview_call(&self,id:&Value,method:&str,p:&Value)->Option<Outcome>{
        let window=p["__windowId"].as_str().unwrap_or("main").to_owned();let document=p["__documentId"].as_str().unwrap_or("").to_owned();
        let (active,scope)={let inner=self.0.inner.borrow();let Some(owner)=inner.windows.get(&window).filter(|owner|owner.document_id==document)else{return Some(Err(bad("desktop.document_stale","所属窗口页面已改变。")))};let active=policy::route_thread(&owner.route).or_else(||owner.visible_threads.first().cloned());(active,{let scope=crate::connections::scope(&window);json!([scope.origin,scope.service_id,scope.identity_id,scope.scope_version]).to_string()})};
        let expected=if method=="preview.open"{p["threadId"].as_str().map(str::to_owned)}else{None};
        if active.is_none()||expected.as_ref().is_some_and(|thread|Some(thread)!=active.as_ref()){return Some(Err(bad("desktop.browser_thread_inactive","预览须属于发起操作窗口当前的聊天。")));}
        let mut params=p.clone();params["__replyId"]=id.clone();let method=method.to_owned();let reply_id=id.clone();
        native::on_main(move||{if method=="preview.open"{let outcome=native::open_window_preview(&window,&document,&params,&scope);native::notify(Notice::Reply(reply_id,outcome));}else{native::invoke_window_preview(&window,&document,&method,params,move|outcome|native::notify(Notice::Reply(reply_id,outcome)));}});None
    }

    fn attachment(&self, id: &Value, method: &str, p: &Value) -> Option<Outcome> {
        let root = context().state.join("desktop-attachments");
        if method == "attachment.usage" {
            off_thread(id.clone(), move || {
                attachments::usage(&root)
                    .map(|bytes| json!({ "bytes": bytes, "limit": attachments::CACHE_LIMIT }))
                    .map_err(|error| BridgeError::new("desktop.attachment_cache_unavailable", format!("附件缓存无法读取：{error}")))
            });
            return None;
        }
        let key = {
            let identity=crate::connections::scope(p["__windowId"].as_str().unwrap_or("main"));
            let scope=attachments::Scope{origin:&identity.origin,service_id:&identity.service_id,identity_id:&identity.identity_id};
            attachments::scope_key(&scope,policy::string(p,"draftId").unwrap_or_default())
        };
        let key = match key {
            Ok(key) => key,
            Err(error) => return Some(Err(error)),
        };
        let entry = policy::string(p, "id").unwrap_or_default().to_owned();
        match method {
            "attachment.cache" => {
                let bytes = match base64::engine::general_purpose::STANDARD.decode(policy::string(p, "data").unwrap_or_default()) {
                    Ok(bytes) => bytes,
                    Err(_) => return Some(Err(bad("desktop.attachment_cache_invalid", "附件数据无效。"))),
                };
                let name = policy::string(p, "name").unwrap_or_default().to_owned();
                let kind = policy::string(p, "type").unwrap_or_default().to_owned();
                let last_modified = finite(p, "lastModified").unwrap_or(0.0);
                off_thread(id.clone(), move || attachments::save(&root, &key, attachments::Incoming { name: &name, kind: &kind, last_modified, bytes: &bytes }));
            }
            "attachment.restore" => off_thread(id.clone(), move || {
                attachments::restore(&root, &key, &entry).map(|restored| {
                    json!({
                        "data": base64::engine::general_purpose::STANDARD.encode(&restored.bytes),
                        "name": restored.name, "type": restored.kind, "lastModified": restored.last_modified,
                    })
                })
            }),
            _ => off_thread(id.clone(), move || done(attachments::remove(&root, &key, &entry))),
        }
        None
    }

    fn authorize_terminal(&self, p: &Value) -> Outcome {
        let thread = policy::string(p, "threadId").unwrap_or_default();
        if thread.is_empty() || !self.thread_visible_in(p["__windowId"].as_str().unwrap_or("main"),p["__documentId"].as_str().unwrap_or(""),thread) {
            return Err(bad("desktop.terminal_scope_invalid", "终端必须属于当前工作台会话。"));
        }
        policy::terminal_path(thread, policy::string(p, "path").unwrap_or_default())?;
        Ok(Value::Null)
    }

    /// The main page started loading: its bridge and any preview it placed
    /// are gone.
    pub fn main_loading(&self) {
        native::BRIDGE_READY.store(false, Ordering::SeqCst);
        self.cancel_window_notifications("main", true, "desktop.notification_document_changed", "通知所属的页面已刷新。");
        self.0.inner.borrow_mut().ui_ready = false;
        self.retire_window_document("main");
        crate::speech::cancel_window("main");
        native::dispose_window_preview("main");
    }

    pub fn main_loaded(&self) {
        let origin = context().origin.clone();
        native::on_main(move || native::install(origin));
    }


}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_key_comparison_is_exact() {
        assert!(equal_secret("abc", "abc"));
        assert!(!equal_secret("abc", "abd"));
        assert!(!equal_secret("abc", "abcd"));
        assert!(!equal_secret("", "a"));
    }


}
