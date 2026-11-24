//! Shared settlement rules and code-host credential settings.
use crate::{AppState, control};
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::{delete, get, post},
};
use serde_json::{Value, json};
use std::sync::Arc;
use tcode_control_core::{
    control::SettlementAutomation,
    pull_request::{HostKind, HostRefusal},
    settings::{ProjectSettlementSettings, SourceControlSettings, TokenCheck},
};

fn fail(error: impl ToString) -> crate::chat::Failure {
    control::failure(
        axum::http::StatusCode::BAD_REQUEST,
        "control.settings_invalid",
        error.to_string(),
    )
}
/// Control helpers fail with `{code, message}`; keep both instead of nesting JSON in a message.
fn coded(error: Value) -> crate::chat::Failure {
    match (error["code"].as_str(), error["message"].as_str()) {
        (Some(code), Some(message)) => {
            control::failure(axum::http::StatusCode::BAD_REQUEST, code, message)
        }
        _ => fail(error),
    }
}
fn host_refused(kind: HostKind, refusal: HostRefusal) -> crate::chat::Failure {
    let name = kind.terms().name;
    let message = match refusal {
        HostRefusal::Blank if kind.organizations() => "请输入组织名称。".to_owned(),
        HostRefusal::Invalid if kind.organizations() => {
            "这不是有效的 Azure DevOps 组织，请输入其名称，例如 fabrikam。".to_owned()
        }
        HostRefusal::Blank => "请输入域名。".to_owned(),
        HostRefusal::Invalid => "这不是有效的域名，请输入类似 git.example.com 的域名。".to_owned(),
        HostRefusal::PortOrPath => format!("{name} 只能按域名添加，不能带端口或路径。"),
        HostRefusal::Path => format!("{name} 按域名添加，可带端口，但不能带路径。"),
        HostRefusal::Fixed => format!("{name} 只位于 {}。", kind.public_host()),
    };
    control::failure(
        axum::http::StatusCode::BAD_REQUEST,
        "control.host_invalid",
        message,
    )
}
pub fn secret_store(
    s: &AppState,
) -> Result<tcode_control_services::settings::SettingsStore, crate::chat::Failure> {
    let root = crate::host_process::configured_state_dir(s)
        .ok_or_else(|| fail("Host state directory is missing"))?
        .join("control-hosts");
    std::fs::create_dir_all(&root).map_err(fail)?;
    Ok(tcode_control_services::settings::SettingsStore::new(root))
}
pub(crate) async fn read(State(s): State<Arc<AppState>>) -> crate::chat::Api {
    let settings = crate::control_state::settings(&s).map_err(coded)?;
    let host = crate::control_pr::host(&s).map_err(coded)?;
    let status = tokio::task::spawn_blocking(move || host.credential_status())
        .await
        .map_err(fail)?;
    Ok(Json(
        json!({"settlement_automation":settings.settlement_automation,"auto_settle_after_days":settings.auto_settle_after_days,"auto_settle_on_merge":settings.auto_settle_on_merge,"project_settlement_overrides":settings.project_settlement_overrides,"source_control":{"hosts":settings.source_control.hosts,"status":status}}),
    ))
}
pub(crate) async fn write(State(s): State<Arc<AppState>>, Json(body): Json<Value>) -> crate::chat::Api {
    save(s, body, "user").await
}

pub(crate) async fn save(s: Arc<AppState>, body: Value, actor: &str) -> crate::chat::Api {
    let mut doc = s
        .store
        .get(control::SETTINGS, control::GLOBAL)
        .map_err(control::storage)?
        .unwrap_or_else(|| json!({}));
    if let Some(value) = body.get("settlement_automation") {
        let value: SettlementAutomation = serde_json::from_value(value.clone()).map_err(fail)?;
        doc["settlement_automation"] = json!(value);
    }
    if let Some(value) = body.get("auto_settle_after_days") {
        let value: Option<f64> = serde_json::from_value(value.clone()).map_err(fail)?;
        if value.is_some_and(|days| !days.is_finite() || !(1.0..=90.0).contains(&days)) {
            return Err(fail("auto_settle_after_days must be null or 1 to 90"));
        }
        doc["auto_settle_after_days"] = json!(value);
    }
    if let Some(value) = body.get("auto_settle_on_merge") {
        let value: bool = serde_json::from_value(value.clone()).map_err(fail)?;
        doc["auto_settle_on_merge"] = json!(value);
    }
    if let Some(value) = body.get("project_settlement_overrides") {
        let value: std::collections::HashMap<String, ProjectSettlementSettings> =
            serde_json::from_value(value.clone()).map_err(fail)?;
        doc["project_settlement_overrides"] = json!(value);
    }
    if let Some(value) = body.get("source_control") {
        let value: SourceControlSettings = serde_json::from_value(value.clone()).map_err(fail)?;
        let mut hosts = std::collections::BTreeMap::new();
        for (typed, choice) in value.hosts {
            let host = choice
                .kind
                .authority(&typed)
                .map_err(|refusal| host_refused(choice.kind, refusal))?;
            if hosts.insert(host.clone(), choice).is_some() {
                return Err(control::failure(
                    axum::http::StatusCode::BAD_REQUEST,
                    "control.host_exists",
                    format!("{host} 已在列表中。"),
                ));
            }
        }
        doc["source_control"] = json!({"hosts":hosts});
    }
    s.store
        .put(control::SETTINGS, control::GLOBAL, &doc)
        .map_err(control::storage)?;
    let host = crate::control_pr::host(&s).map_err(coded)?;
    host.configure(
        crate::control_state::settings(&s)
            .map_err(coded)?
            .source_control
            .hosts,
    );
    control::audit(
        &s,
        actor,
        "",
        "settings_changed",
        json!({"fields":body.as_object().map(|fields|fields.keys().collect::<Vec<_>>())}),
    );
    read(State(s)).await
}
pub(crate) async fn credential(State(s): State<Arc<AppState>>, Json(body): Json<Value>) -> crate::chat::Api {
    let kind = serde_json::from_value(body["kind"].clone()).map_err(fail)?;
    let name = body["host"]
        .as_str()
        .ok_or_else(|| fail("host is required"))?
        .to_owned();
    let host = crate::control_pr::host(&s).map_err(coded)?;
    let store = secret_store(&s)?;
    match body["action"].as_str() {
        Some("connect") => {
            let token = body["token"]
                .as_str()
                .filter(|token| !token.is_empty())
                .ok_or_else(|| fail("token is required"))?
                .to_owned();
            let email = body["email"]
                .as_str()
                .filter(|email| !email.is_empty())
                .map(str::to_owned);
            if body["force"].as_bool() == Some(true) {
                store
                    .set_credential(kind, &name, Some(&token), email.as_deref())
                    .map_err(fail)?;
                host.forget_credential(&name);
                return Ok(Json(
                    json!({"connected":true,"check":null,"saved_unchecked":true}),
                ));
            }
            let checking = host.clone();
            let target = name.clone();
            let secret = token.clone();
            let mail = email.clone();
            let check = tokio::task::spawn_blocking(move || {
                checking.check_token(&target, &secret, mail.as_deref())
            })
            .await
            .map_err(fail)?;
            match &check {
                TokenCheck::Accepted { account } => store
                    .set_accepted_credential(
                        kind,
                        &name,
                        &token,
                        email.as_deref(),
                        account.as_ref().map(|a| a.login.as_str()),
                    )
                    .map_err(fail)?,
                TokenCheck::Unconfirmed => store
                    .set_credential(kind, &name, Some(&token), email.as_deref())
                    .map_err(fail)?,
                _ => return Ok(Json(json!({"connected":false,"check":check}))),
            }
            host.forget_credential(&name);
            Ok(Json(json!({"connected":true,"check":check})))
        }
        Some("remove") => {
            store.set_token(kind, &name, None).map_err(fail)?;
            let mut doc = s
                .store
                .get(control::SETTINGS, control::GLOBAL)
                .map_err(control::storage)?
                .unwrap_or_else(|| json!({}));
            if let Some(hosts) = doc["source_control"]["hosts"].as_object_mut() {
                hosts.remove(&name);
            }
            s.store
                .put(control::SETTINGS, control::GLOBAL, &doc)
                .map_err(control::storage)?;
            host.forget_credential(&name);
            host.configure(
                crate::control_state::settings(&s)
                    .map_err(coded)?
                    .source_control
                    .hosts,
            );
            Ok(Json(json!({"removed":true})))
        }
        Some("disconnect") => {
            store.set_token(kind, &name, None).map_err(fail)?;
            host.forget_credential(&name);
            Ok(Json(json!({"disconnected":true})))
        }
        Some("identify") => {
            tokio::task::spawn_blocking(move || host.identify(&name))
                .await
                .map_err(fail)?;
            read(State(s)).await
        }
        _ => Err(fail(
            "action must be connect, remove, disconnect or identify",
        )),
    }
}
async fn thread_prs(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> crate::chat::Api {
    let _ = crate::chat::load(&s, &id)?;
    let paused = control::thread_doc(&s, &id)
        .map(|ctrl| ctrl["pull_request_watches_paused"] == true)
        .unwrap_or(false);
    Ok(Json(json!({
        "pull_requests": crate::control_pr::links(&s, &id).map_err(coded)?.into_iter().filter(|link| link.visible()).collect::<Vec<_>>(),
        "pull_request_watches_paused": paused,
    })))
}
async fn thread_pr_action(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(mut body): Json<Value>,
) -> crate::chat::Api {
    body["thread_id"] = json!(id);
    let params = serde_json::from_value(body).map_err(fail)?;
    let result = crate::control_pr::act_as_user(&s, params)
        .await
        .map_err(coded)?;
    Ok(Json(result))
}
async fn thread_pr_read(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> crate::chat::Api {
    let _ = crate::chat::load(&s, &id)?;
    let key = body["key"].as_str().ok_or_else(|| fail("key is required"))?;
    let read: tcode_control_protocol::PullRequestRead =
        serde_json::from_value(body["read"].clone()).map_err(fail)?;
    let result = crate::control_pr::read_as_user(&s, &id, key, read)
        .await
        .map_err(coded)?;
    Ok(Json(result))
}
async fn thread_pr_stack(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> crate::chat::Api {
    let _ = crate::chat::load(&s, &id)?;
    let key = body["key"].as_str().ok_or_else(|| fail("key is required"))?;
    let result = crate::control_pr::stack_as_user(&s, &id, key)
        .await
        .map_err(coded)?;
    Ok(Json(result))
}
async fn thread_pr_viewed(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> crate::chat::Api {
    let _ = crate::chat::load(&s, &id)?;
    let key = body["key"].as_str().ok_or_else(|| fail("key is required"))?;
    let paths: Vec<String> = serde_json::from_value(body["paths"].clone()).map_err(fail)?;
    let viewed = body["viewed"].as_bool().unwrap_or(true);
    let result = crate::control_pr::set_viewed_as_user(&s, &id, key, paths, viewed)
        .await
        .map_err(coded)?;
    Ok(Json(result))
}
async fn thread_pr_review(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> crate::chat::Api {
    let _ = crate::chat::load(&s, &id)?;
    let key = body["key"].as_str().ok_or_else(|| fail("key is required"))?;
    let verdict: tcode_control_protocol::PullRequestReviewVerdict =
        serde_json::from_value(body["verdict"].clone()).map_err(fail)?;
    let head = body["head"].as_str().ok_or_else(|| fail("head is required"))?.to_owned();
    let comment_body = body["body"].as_str().unwrap_or("").to_owned();
    let comments: Vec<tcode_control_core::pull_request::PullRequestReviewDraftComment> =
        serde_json::from_value(body.get("comments").cloned().unwrap_or_else(|| json!([])))
            .map_err(fail)?;
    let result = crate::control_pr::submit_review_as_user(
        &s,
        &id,
        key,
        verdict,
        head,
        comment_body,
        comments,
    )
    .await
    .map_err(coded)?;
    Ok(Json(result))
}
async fn thread_pr_host_action(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> crate::chat::Api {
    let _ = crate::chat::load(&s, &id)?;
    let key = body["key"].as_str().ok_or_else(|| fail("key is required"))?;
    let action: tcode_control_protocol::PullRequestAction =
        serde_json::from_value(body["action"].clone()).map_err(fail)?;
    let result = crate::control_pr::host_action_as_user(&s, &id, key, action)
        .await
        .map_err(coded)?;
    Ok(Json(result))
}
async fn thread_pr_watches_paused(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> crate::chat::Api {
    let _ = crate::chat::load(&s, &id)?;
    let paused = body["paused"].as_bool().ok_or_else(|| fail("paused bool is required"))?;
    crate::control_pr::set_watches_paused(&s, &id, paused).map_err(coded)?;
    Ok(Json(json!({ "pull_request_watches_paused": paused })))
}
async fn review_records(State(s): State<Arc<AppState>>) -> crate::chat::Api {
    let mut records = s
        .store
        .list("control_reviews_v2")
        .map_err(control::storage)?;
    records.sort_by(|a, b| b["recorded_at"].as_u64().cmp(&a["recorded_at"].as_u64()));
    Ok(Json(json!({"records":records})))
}
async fn dismiss_suggestion(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> crate::chat::Api {
    let mut doc = control::thread_doc(&s, &id)?;
    doc["settle_suggestion_dismissed_at"] = json!(chrono::Utc::now().timestamp());
    doc["settle_suggestion"] = Value::Null;
    control::save_thread_doc(&s, &id, &doc)?;
    control::emit(
        &s,
        &id,
        "core.control.updated",
        json!({"kind":"settle_suggestion_dismissed"}),
    )?;
    Ok(Json(json!({"dismissed":true})))
}
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/control/threads/{id}/settle-suggestion",
            delete(dismiss_suggestion),
        )
        .route("/api/control/review-records", get(review_records))
        .route(
            "/api/control/threads/{id}/pull-requests",
            get(thread_prs).post(thread_pr_action),
        )
        .route(
            "/api/control/threads/{id}/pull-requests/read",
            post(thread_pr_read),
        )
        .route(
            "/api/control/threads/{id}/pull-requests/stack",
            post(thread_pr_stack),
        )
        .route(
            "/api/control/threads/{id}/pull-requests/viewed",
            post(thread_pr_viewed),
        )
        .route(
            "/api/control/threads/{id}/pull-requests/review",
            post(thread_pr_review),
        )
        .route(
            "/api/control/threads/{id}/pull-requests/host-action",
            post(thread_pr_host_action),
        )
        .route(
            "/api/control/threads/{id}/pull-requests/watches-paused",
            post(thread_pr_watches_paused),
        )
        .route("/api/control/policy", get(read).put(write))
        .route("/api/control/host-credential", post(credential))
}
