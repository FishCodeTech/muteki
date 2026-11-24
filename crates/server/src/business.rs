//! Business availability is independent of shared storage and process ownership.
//! CTF (including competitions), conversations, pentest and SRC recover separately.
use crate::AppState;
use axum::{
    Json,
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use futures_util::FutureExt;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    future::Future,
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
};

tokio::task_local! { static RECOVERING: (usize,Module); }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Module {
    Chat,
    Ctf,
    Pentest,
    Src,
}
impl Module {
    pub const ALL: [Self; 4] = [Self::Chat, Self::Ctf, Self::Pentest, Self::Src];
    pub fn id(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Ctf => "ctf",
            Self::Pentest => "pentest",
            Self::Src => "src",
        }
    }
    pub fn title(self) -> &'static str {
        match self {
            Self::Chat => "聊天",
            Self::Ctf => "CTF",
            Self::Pentest => "渗透",
            Self::Src => "SRC",
        }
    }
    fn index(self) -> usize {
        match self {
            Self::Chat => 0,
            Self::Ctf => 1,
            Self::Pentest => 2,
            Self::Src => 3,
        }
    }
    pub fn from_descriptor(id: &str) -> Option<Self> {
        match id {
            "builtin.conversation" => Some(Self::Chat),
            "builtin.single-security-task" | "builtin.competition" => Some(Self::Ctf),
            "builtin.pentest" => Some(Self::Pentest),
            "builtin.src" => Some(Self::Src),
            _ => None,
        }
    }
    pub fn for_run(run: &Value) -> Self {
        if run["bound_to_src"] == true
            || run["src_task_id"].as_str().is_some_and(|id| !id.is_empty())
        {
            Self::Src
        } else if run["mode"] == "pentest" {
            Self::Pentest
        } else {
            Self::Ctf
        }
    }
    pub fn descriptor(self) -> &'static str {
        match self {
            Self::Chat => "builtin.conversation",
            Self::Ctf => "builtin.single-security-task",
            Self::Pentest => "builtin.pentest",
            Self::Src => "builtin.src",
        }
    }
}

pub struct Registry {
    status: [Mutex<Value>; 4],
    tasks: Mutex<HashSet<(Module, &'static str)>>,
    recovery: [tokio::sync::Mutex<()>; 4],
    dispatch: [tokio::sync::Mutex<()>; 4],
    shared_dispatch: tokio::sync::Mutex<()>,
}
impl Default for Registry {
    fn default() -> Self {
        let initial_state = if cfg!(test) {
            "ready"
        } else {
            "waiting_activation"
        };
        Self {
            status: std::array::from_fn(|i| {
                Mutex::new(
                    json!({"id":Module::ALL[i].id(),"state":initial_state,"error":null}),
                )
            }),
            tasks: Mutex::default(),
            recovery: std::array::from_fn(|_| tokio::sync::Mutex::new(())),
            dispatch: std::array::from_fn(|_| tokio::sync::Mutex::new(())),
            shared_dispatch: tokio::sync::Mutex::new(()),
        }
    }
}
impl Registry {
    pub fn status(&self, module: Module) -> Value {
        self.status[module.index()]
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn snapshot(&self) -> Value {
        json!(Module::ALL.map(|m| self.status(m)))
    }
    pub fn disable(&self, module: Module) {
        self.set(module, "disabled", None);
    }
    fn set(&self, module: Module, phase: &str, error: Option<String>) {
        let mut status = self.status[module.index()]
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        status["state"] = json!(phase);
        status["error"] = error.map_or(
            Value::Null,
            |message| json!({"code":"module.recovery_failed","message":message}),
        );
        status["updated_at"] = json!(chrono::Utc::now().to_rfc3339());
    }
}

#[derive(Debug)]
pub struct Unavailable {
    pub code: &'static str,
    pub module: Module,
    pub message: String,
}
impl Unavailable {
    pub fn body(&self) -> Value {
        json!({"error":{"code":self.code,"message":self.message,"category":"availability","retryable":true,"detail":{"module":self.module.id()}},"detail":self.message})
    }
    pub fn failure(self) -> (StatusCode, Json<Value>) {
        (StatusCode::SERVICE_UNAVAILABLE, Json(self.body()))
    }
}
impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl IntoResponse for Unavailable {
    fn into_response(self) -> Response {
        self.failure().into_response()
    }
}

pub fn require(state: &AppState, module: Module) -> Result<(), Unavailable> {
    require_descriptor(state, module, module.descriptor())
}
pub fn require_descriptor(state: &AppState, module: Module, id: &str) -> Result<(), Unavailable> {
    let _ = id;
    if !configured_enabled(state, module).map_err(|e| Unavailable {
        code: "storage.unavailable",
        module,
        message: e.to_string(),
    })? {
        return Err(Unavailable {
            code: "module.disabled",
            module,
            message: format!("{}模块已停用", module.title()),
        });
    }
    let status = state.business.status(module);
    let recovery = RECOVERING
        .try_with(|owner| *owner == (Arc::as_ptr(&state.business) as usize, module))
        .unwrap_or(false);
    if status["state"] == "ready" || recovery {
        return Ok(());
    }
    Err(Unavailable {
        code: "module.not_ready",
        module,
        message: format!(
            "{}模块暂不可用：{}",
            module.title(),
            status["error"]["message"]
                .as_str()
                .unwrap_or("模块正在恢复，可在模块管理中查看状态并重试")
        ),
    })
}
pub fn configured_enabled(state: &AppState, module: Module) -> rusqlite::Result<bool> {
    for id in descriptor_ids(module) {
        if state
            .store
            .get("domain_modules", id)?
            .is_some_and(|flag| flag["enabled"] == false)
        {
            return Ok(false);
        }
    }
    Ok(true)
}
pub fn descriptor_ids(module: Module) -> &'static [&'static str] {
    match module {
        Module::Chat => &["builtin.conversation"],
        Module::Ctf => &["builtin.single-security-task", "builtin.competition"],
        Module::Pentest => &["builtin.pentest"],
        Module::Src => &["builtin.src"],
    }
}
pub fn run_available(state: &AppState, run: &Value) -> bool {
    require(state, Module::for_run(run)).is_ok()
}

/// Commands and tools use the same admission as their direct HTTP counterparts.
/// Names here are protocol namespaces, never semantic guesses about user input.
pub fn require_operation(
    state: &AppState,
    kind: &str,
    envelope: &Value,
) -> Result<(), Unavailable> {
    if safety_command(kind) {
        return Ok(());
    }
    if kind.starts_with("src.") {
        return require(state, Module::Src);
    }
    if crate::runs::competition_api::supports_command(kind)
        || crate::runs::competition_api::supports_query(kind)
    {
        return require_descriptor(state, Module::Ctf, "builtin.competition");
    }
    if kind.starts_with("conversation.") || kind.starts_with("thread.") {
        return require(state, Module::Chat);
    }
    let values = if envelope["payload"].is_object() {
        &envelope["payload"]
    } else {
        &envelope["params"]
    };
    if kind.starts_with("run.") || kind == "graph.shared.read" {
        let id = values["run_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .or_else(|| envelope["aggregate_id"].as_str())
            .unwrap_or("");
        if !id.is_empty() {
            require_run(state, id)?;
        }
        let mode = values["mode"]
            .as_str()
            .or_else(|| values["challenge"]["mode"].as_str())
            .or_else(|| (values["task_kind"] == "pentest.target").then_some("pentest"));
        if kind == "run.create" || mode.is_some() {
            require(
                state,
                if mode == Some("pentest") {
                    Module::Pentest
                } else {
                    Module::Ctf
                },
            )?;
        }
    }
    if kind == "task.create" {
        match values["kind"].as_str() {
            Some("ctf.challenge" | "ctf") => require(state, Module::Ctf)?,
            Some("pentest.target" | "pentest") => require(state, Module::Pentest)?,
            Some("src.bounty") => require(state, Module::Src)?,
            _ => {}
        }
    }
    Ok(())
}
pub fn safety_command(kind: &str) -> bool {
    matches!(
        kind,
        "run.stop"
            | "run.pause"
            | "run.freeze"
            | "run.cancel_worker"
            | "conversation.turn.interrupt"
            | "conversation.subagent.cancel"
    )
}
pub fn require_run(state: &AppState, id: &str) -> Result<(), Unavailable> {
    match state.store.get("runs", id) {
        Ok(Some(run)) => require(state, Module::for_run(&run)),
        Ok(None) => Ok(()),
        Err(error) => Err(Unavailable {
            code: "storage.unavailable",
            module: Module::Ctf,
            message: error.to_string(),
        }),
    }
}

/// Serialization belongs to a business domain, never a network call in another.
pub fn operation_module(
    state: &AppState,
    kind: &str,
    envelope: &Value,
) -> Result<Option<Module>, Unavailable> {
    if kind.starts_with("src.") {
        return Ok(Some(Module::Src));
    }
    if crate::runs::competition_api::supports_command(kind)
        || crate::runs::competition_api::supports_query(kind)
    {
        return Ok(Some(Module::Ctf));
    }
    if kind.starts_with("conversation.") || kind.starts_with("thread.") {
        return Ok(Some(Module::Chat));
    }
    if kind.starts_with("run.") || kind == "graph.shared.read" {
        let data = if envelope["payload"].is_object() {
            &envelope["payload"]
        } else {
            &envelope["params"]
        };
        let id = data["run_id"]
            .as_str()
            .or_else(|| envelope["aggregate_id"].as_str())
            .unwrap_or("");
        if !id.is_empty() {
            if let Some(run) = state.store.get("runs", id).map_err(|e| Unavailable {
                code: "storage.unavailable",
                module: Module::Ctf,
                message: e.to_string(),
            })? {
                return Ok(Some(Module::for_run(&run)));
            }
        }
        return Ok(Some(
            if data["mode"] == "pentest"
                || data["challenge"]["mode"] == "pentest"
                || data["task_kind"] == "pentest.target"
            {
                Module::Pentest
            } else {
                Module::Ctf
            },
        ));
    }
    if kind == "task.create" {
        return Ok(match envelope["payload"]["kind"].as_str() {
            Some("ctf" | "ctf.challenge") => Some(Module::Ctf),
            Some("pentest" | "pentest.target") => Some(Module::Pentest),
            Some("src.bounty") => Some(Module::Src),
            _ => None,
        });
    }
    Ok(None)
}
pub fn dispatch_lock(state: &AppState, module: Option<Module>) -> &tokio::sync::Mutex<()> {
    match module {
        Some(Module::Ctf) => crate::operations_api::command_dispatch_lock(),
        Some(module) => &state.business.dispatch[module.index()],
        None => &state.business.shared_dispatch,
    }
}

/// A failed or stalled module never delays core readiness or another module.
pub fn activate(state: Arc<AppState>) {
    for module in Module::ALL {
        retry(state.clone(), module);
    }
}
pub fn retry(state: Arc<AppState>, module: Module) {
    tokio::spawn(async move {
        let Ok(_recovery) = state.business.recovery[module.index()].try_lock() else {
            return;
        };
        match configured_enabled(&state, module) {
            Ok(false) => {
                state.business.disable(module);
                return;
            }
            Err(error) => {
                state
                    .business
                    .set(module, "failed", Some(error.to_string()));
                return;
            }
            Ok(true) => {}
        }
        if state.business.status(module)["state"] == "ready" {
            return;
        }
        state.business.set(module, "initializing", None);
        {
            let _dispatch = dispatch_lock(&state, Some(module)).lock().await;
        }
        let result = RECOVERING
            .scope(
                (Arc::as_ptr(&state.business) as usize, module),
                AssertUnwindSafe(recover(&state, module)).catch_unwind(),
            )
            .await;
        let result = match result {
            Ok(result) => result,
            Err(panic) => Err(panic_message(panic)),
        };
        match result {
            Ok(()) => {
                state.business.set(module, "ready", None);
                crate::operations_api::spawn_effect_loop_for(state.clone(), Some(module));
                match module {
                    Module::Chat => crate::native_chat::spawn_control_recovery(state.clone()),
                    Module::Ctf => crate::runs::competition_api::start_loops(state.clone()),
                    Module::Src => {
                        crate::runs::src_api::spawn_monitor_loop(state.clone());
                        crate::runs::src_api::spawn_inspection_bridge(state.clone());
                    }
                    Module::Pentest => {}
                }
            }
            Err(error) => {
                tracing::error!(module=module.id(),%error,"Business module recovery failed");
                state.business.set(module, "failed", Some(error));
            }
        }
        if let Err(error) = state.store.put(
            "system",
            &format!("module_recovery:{}", module.id()),
            &state.business.status(module),
        ) {
            state.business.set(
                module,
                "failed",
                Some(format!("Could not persist module recovery: {error}")),
            );
        }
    });
}
fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "Module task panicked without a text payload".into())
}
async fn recover(state: &Arc<AppState>, module: Module) -> Result<(), String> {
    match module {
        Module::Chat => {
            crate::agent_runtime::ledger::settle_previous_epochs(&state.store, state.chat.epoch())
                .map_err(|e| e.to_string())?;
            crate::native_chat::recover_control_receipts(state).map_err(|e| e.to_string())?;
            crate::chat_subagents::recover(state.clone()).await?;
        }
        Module::Ctf => {
            crate::runs::recover_module(state, module).await?;
            crate::runs::competition_api::initialize(state).await?;
        }
        Module::Pentest => crate::runs::recover_module(state, module).await?,
        Module::Src => {
            crate::runs::recover_module(state, module).await?;
            crate::runs::src_api::initialize(state).await?;
        }
    }
    Ok(())
}

/// Supervision keeps unexpected loop termination observable; recovery is explicit.
pub fn spawn(
    state: Arc<AppState>,
    module: Module,
    name: &'static str,
    work: impl Future<Output = ()> + Send + 'static,
) {
    if !state
        .business
        .tasks
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((module, name))
    {
        return;
    }
    tokio::spawn(async move {
        match AssertUnwindSafe(work).catch_unwind().await {
            Ok(()) => state.business.set(
                module,
                "failed",
                Some(format!("Background service {name} exited")),
            ),
            Err(panic) => state.business.set(
                module,
                "failed",
                Some(format!(
                    "Background service {name}: {}",
                    panic_message(panic)
                )),
            ),
        }
        state
            .business
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(module, name));
        tracing::error!(
            module = module.id(),
            service = name,
            "Business background service stopped"
        );
    });
}

/// Direct HTTP paths have explicit business ownership. Shared facilities stay available.
pub async fn gate(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    let owner = if path == "/api/threads" || path.starts_with("/api/threads/") {
        Some((Module::Chat, Module::Chat.descriptor()))
    } else if path == "/api/src" || path.starts_with("/api/src/") {
        Some((Module::Src, Module::Src.descriptor()))
    } else if path == "/api/competitions"
        || path.starts_with("/api/competitions/")
        || matches!(
            path,
            "/api/platform-connections" | "/api/platform-kinds" | "/api/platform-secrets"
        )
        || path.starts_with("/api/platform-connections/")
    {
        Some((Module::Ctf, "builtin.competition"))
    } else {
        None
    };
    let command_path = path.starts_with("/api/threads/") && path.ends_with("/commands");
    if !command_path {
        if let Some((module, id)) = owner {
            if let Err(error) = require_descriptor(&state, module, id) {
                return error.into_response();
            }
        }
    }
    if path.starts_with("/api/runs/") {
        let control = path.ends_with("/control")
            || path.ends_with("/hitl")
            || request.method() == axum::http::Method::DELETE
                && path.ends_with("/workers");
        if control {
            return next.run(request).await;
        }
        use axum::extract::FromRequestParts;
        let (mut parts, body) = request.into_parts();
        if let Ok(params) = axum::extract::RawPathParams::from_request_parts(&mut parts, &()).await
        {
            if let Some((_, id)) = params
                .iter()
                .find(|(key, _)| matches!(*key, "id" | "run_id"))
            {
                match state.store.get("runs",id) {
                    Ok(Some(run))=>if let Err(error)=require(&state,Module::for_run(&run)){return error.into_response();},
                    Err(error)=>return (StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"error":{"code":"storage.unavailable","message":error.to_string()}}))).into_response(),
                    Ok(None)=>{},
                }
            }
        }
        return next.run(Request::from_parts(parts, body)).await;
    }
    next.run(request).await
}
