pub mod access_api;
pub mod activity;
pub mod account_material;
mod private_files;
pub mod attachment_delivery;
pub mod agent_runtime;
mod tool_resources;
pub mod business;
pub mod browser;
pub mod browser_headless;
pub mod capabilities;
pub mod capability_gateway;
pub mod chat;
pub mod chat_computer;
pub mod chat_subagents;
pub mod chat_core;
pub mod chat_history_api;
pub mod chat_metadata;
pub mod chat_preflight_api;
pub mod checkpoints;
pub mod compat;
pub mod composer;
pub mod control;
pub mod control_content;
pub mod control_subagents;
pub mod control_native_agents;
pub mod control_contract;
pub mod control_catalog;
pub mod control_index;
pub mod control_log;
pub mod control_reader;
pub mod control_state;
pub mod control_automation;
pub mod control_settings_api;
pub mod control_observer;
mod control_search_text;
pub mod control_api;
pub mod control_terminal;
pub mod control_git;
pub mod control_pr;
pub mod control_actions;
pub mod control_lifecycle;
pub mod control_lifecycle_api;
pub mod control_observe;
pub mod control_operate;
pub mod control_tools;
pub mod conversation_view;
pub mod conversation_policy;
pub mod extensions;
pub mod files;
mod git_api;
mod git_process;
mod github_pr;
pub mod history_api;
pub mod history_import;
pub mod host_import;
pub mod import;
mod image_validation;
mod src_resources;
pub mod legacy_chat_import;
pub mod legacy_ui;
pub mod lifecycle;
pub mod mac_apps;
pub mod memory_api;
pub mod model_settings_api;
mod model_evidence;
pub mod native_chat;
pub mod native_credentials;
pub mod network_proxy;
pub mod operations_api;
pub mod plugins;
pub mod pricing_read;
pub mod provider;
pub mod provider_history_import;
pub mod quota;
pub mod quota_resume;
pub mod runs;
pub mod runtime_api;
pub mod settings_metadata;
pub mod shares;
#[path = "../../desktop/src/shell_env.rs"]
pub mod shell_env;
pub mod host_process;
pub mod store;
pub mod system_api;
pub mod runner_resources;
pub mod runner_remote;
pub mod runner;
mod runner_execution;
pub mod terminal;
pub mod usage_history;
mod web;
pub mod workspace_read_api;
pub mod workspaces;
pub mod workbench_settings;
use axum::{
    Json, Router,
    extract::Request,
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use serde_json::json;
use std::sync::Arc;
use store::Store;
pub struct AppState {
    pub business: Arc<business::Registry>,
    pub conversation_policy: Arc<dyn conversation_policy::Policy>,
    pub conversation_run_bridge: Arc<dyn conversation_policy::RunBridge>,
    pub lifecycle:lifecycle::Service,
    pub metadata:chat_metadata::Service,
    pub run_progress:runs::progress::Service,
    pub store: Store,
    pub access: std::sync::OnceLock<Arc<access_api::Access>>,
    pub control_forge: std::sync::OnceLock<Arc<dyn tcode_control_services::forge::Forge>>,
    pub control_receipts: tokio::sync::Mutex<std::collections::HashMap<String, serde_json::Value>>,
    pub chat: chat::Runtime,
    /// Live engine sessions, one per conversation thread.
    pub agents: agent_runtime::Pool,
    /// Turn dispatch and checkpoint waits for native conversation threads.
    pub native: native_chat::Registry,
    /// Interactive workspace shells, in this process only.
    pub terminals: terminal::Terminals,
    /// Managed Agent extension packages and their MCP connections.
    pub plugins: plugins::Plugins,
    /// Gateway credentials the live engine sessions hold.
    pub capability_gateway: capability_gateway::Gateway,
    /// Right-panel browser requests waiting on a connected chat client.
    pub browser: Arc<browser::Broker>,
    /// Mac computer-control session and saved JS results for each chat Thread.
    pub computer: Arc<chat_computer::Broker>,
    /// Browser pages and computer apps each chat's running turn is driving.
    pub activity: Arc<activity::Hub>,
    /// Cached provider subscription windows.
    pub limits: quota::Limits,
    /// Write admission while a managed desktop app switches versions.
    pub admission: system_api::Admission,
    /// Active CTF / Pentest run registries, worker tasks, and SSE buses.
    pub runs: runs::RunRegistry,
    pub extension_host: extensions::Host,
    pub remote_runners: Arc<runner_remote::broker::Registry>,
    pub runner_listener: Arc<runner_remote::listener::Manager>,
    pub remote_transfer_slots: Arc<tokio::sync::Semaphore>,
    /// Node dispatch queue for a scratch state (draft or catalog checks), so its
    /// Worker processes are admitted with the node's real Runs.
    pub admission_store: Option<Store>,
}
impl AppState {
    /// Scratch state that keeps its own documents but shares this node's Worker
    /// admission queue and remote Runner connections.
    pub fn scratch_for(&self, store: Store) -> Self {
        let mut scratch = Self::new(store);
        scratch.business=self.business.clone();
        scratch.conversation_policy=self.conversation_policy.clone();
        scratch.conversation_run_bridge=self.conversation_run_bridge.clone();
        scratch.admission_store = Some(self.admission_store.clone().unwrap_or_else(|| self.store.clone()));
        scratch.remote_runners = self.remote_runners.clone();
        scratch.runner_listener = self.runner_listener.clone();
        scratch
    }
    pub fn new(store: Store) -> Self {
        Self {
            business: Arc::new(business::Registry::default()),
            conversation_policy: Arc::new(runs::src_api::native_policy::ConversationPolicy),
            conversation_run_bridge: Arc::new(runs::local_pi::ConversationRunBridge),
            lifecycle:lifecycle::Service::default(),
            store,
            access: std::sync::OnceLock::new(),
            control_forge: std::sync::OnceLock::new(),
            control_receipts: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            metadata:chat_metadata::Service::default(),
            run_progress:runs::progress::Service::default(),
            chat: chat::Runtime::default(),
            agents: agent_runtime::Pool::default(),
            native: native_chat::Registry::default(),
            terminals: terminal::Terminals::default(),
            plugins: plugins::Plugins::default(),
            capability_gateway: capability_gateway::Gateway::default(),
            browser: Arc::new(browser::Broker::default()),
            computer: Arc::new(chat_computer::Broker::default()),
            activity: Arc::default(),
            limits: quota::Limits::default(),
            admission: system_api::Admission::default(),
            runs: runs::RunRegistry::default(),
            extension_host: extensions::Host::default(),
            remote_runners: Arc::new(runner_remote::broker::Registry::default()),
            runner_listener: Arc::new(runner_remote::listener::Manager::default()),
            remote_transfer_slots: Arc::new(tokio::sync::Semaphore::new(4)),
            admission_store: None,
        }
    }
}

pub fn app(state: Arc<AppState>, access: Arc<access_api::Access>) -> Router {
    state.access.get_or_init(|| access.clone());
    compat::router()
        .merge(access_api::router(access.clone()))
        .merge(chat::router())
        .merge(workspaces::router())
        .merge(legacy_ui::router())
        .merge(mac_apps::router())
        .merge(memory_api::router())
        .merge(provider_history_import::router())
        .merge(files::router())
        .merge(git_api::router())
        .merge(github_pr::router())
        .merge(history_api::router())
        .merge(chat_history_api::router())
        .merge(workspace_read_api::router())
        .merge(model_settings_api::router())
        .merge(chat_preflight_api::router())
        .merge(chat_metadata::router())
        .merge(settings_metadata::router())
        .merge(network_proxy::router())
        .merge(agent_runtime::version_policy::router())
        .merge(shares::router())
        .merge(runtime_api::router())
        .merge(native_credentials::router())
        .merge(terminal::router())
        .merge(plugins::api::router())
        .merge(capabilities::router())
        .merge(control::router())
        .merge(capability_gateway::router())
        .merge(browser::router())
        .merge(activity::router())
        .merge(extensions::router())
        .merge(quota::router())
        .merge(host_import::router())
        .merge(system_api::router())
        .merge(runner_resources::router())
        .merge(runner_remote::router())
        .merge(operations_api::router())
        .merge(runs::router())
        .route("/api/models/catalog", axum::routing::get(model_catalog))
        .fallback(web::serve)
        .layer(middleware::from_fn_with_state(state.clone(), business::gate))
        .layer(middleware::from_fn_with_state(state.clone(), system_api::write_gate))
        .layer(axum::Extension(access.clone()))
        .layer(middleware::from_fn_with_state(access.clone(), access_api::gate))
        .layer(middleware::from_fn_with_state(access, local_origin))
        .with_state(state)
}

pub fn local_origin_accepted(origin: Option<&str>, host: Option<&str>, exposed: bool) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    let same_host = exposed && host.is_some_and(|host| origin == format!("http://{host}"));
    same_host
        || reqwest::Url::parse(origin)
            .ok()
            .is_some_and(|url| matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")))
}

async fn local_origin(
    axum::extract::State(access): axum::extract::State<Arc<access_api::Access>>,
    request: Request,
    next: Next,
) -> Response {
    if let Some(origin_header) = request.headers().get("origin") {
        let origin = origin_header.to_str().ok().unwrap_or("");
        let host = request.headers().get("host").and_then(|host| host.to_str().ok());
        if !local_origin_accepted(Some(origin), host, access.is_exposed()) {
            return (StatusCode::FORBIDDEN, Json(json!({"detail":"This local service only accepts requests from a local application."}))).into_response();
        }
    }
    next.run(request).await
}

async fn model_catalog(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
) -> Response {
    match provider::public_catalog(&state.store) {
        Ok(credentials) => Json(json!({"credentials":credentials})).into_response(),
        Err(error) => {
            tracing::error!(%error,"Model catalog failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail":"Could not load model catalog"})),
            )
                .into_response()
        }
    }
}

pub mod runtime_guardian;

pub mod src_native_hook;
