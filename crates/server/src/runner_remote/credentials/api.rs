use super::*;
use crate::AppState;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path as RoutePath, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::sync::Arc;
use tokio::sync::OwnedSemaphorePermit;
fn answer(value: Value) -> Response {
    ([("cache-control", "no-store")], Json(value)).into_response()
}
fn error(status: StatusCode, message: impl ToString) -> Response {
    (
        status,
        [("cache-control", "no-store")],
        Json(json!({"error":message.to_string()})),
    )
        .into_response()
}
fn acquire(state: &AppState) -> Result<OwnedSemaphorePermit, Response> {
    state
        .remote_transfer_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            error(
                StatusCode::TOO_MANY_REQUESTS,
                "Credential transfer slots are busy",
            )
        })
}
async fn reserved(
    state: Arc<AppState>,
    permit: Option<OwnedSemaphorePermit>,
    operation: impl FnOnce(Vault) -> Result<Value, String> + Send + 'static,
) -> Response {
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation(Vault::open(
            state.store.clone(),
            &crate::native_chat::state_dir(&state),
        )?)
    })
    .await
    {
        Ok(Ok(value)) => answer(value),
        Ok(Err(message)) => error(StatusCode::CONFLICT, message),
        Err(_) => error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Credential operation failed",
        ),
    }
}
async fn work(
    state: Arc<AppState>,
    operation: impl FnOnce(Vault) -> Result<Value, String> + Send + 'static,
) -> Response {
    let permit = match acquire(&state) {
        Ok(permit) => permit,
        Err(response) => return response,
    };
    reserved(state, Some(permit), operation).await
}
async fn body<T: serde::de::DeserializeOwned>(
    state: &AppState,
    request: Request,
) -> Result<(OwnedSemaphorePermit, T), Response> {
    let permit = acquire(state)?;
    let bytes = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        axum::body::to_bytes(request.into_body(), 512 * 1024),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => {
            return Err(error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Credential request is too large",
            ));
        }
        Err(_) => {
            return Err(error(
                StatusCode::REQUEST_TIMEOUT,
                "Credential body timed out",
            ));
        }
    };
    let value = serde_json::from_slice(&bytes)
        .map_err(|_| error(StatusCode::BAD_REQUEST, "Invalid credential request"))?;
    Ok((permit, value))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    expected_revision: Option<String>,
    material: Material,
}
async fn update(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    request: Request,
) -> Response {
    let (permit, body) = match body::<Update>(&state, request).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    reserved(state, Some(permit), move |vault| {
        vault.put(&id, body.expected_revision.as_deref(), &body.material)
    })
    .await
}
async fn metadata(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
) -> Response {
    work(state, move |vault| vault.metadata(&id)).await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Issue {
    material_id: String,
    scope: Scope,
    ttl_seconds: i64,
}
async fn issue(State(state): State<Arc<AppState>>, Json(body): Json<Issue>) -> Response {
    work(state, move |vault| {
        Ok(json!(vault.grant(
            &body.material_id,
            body.scope,
            body.ttl_seconds,
            chrono::Utc::now().timestamp()
        )?))
    })
    .await
}
async fn status(State(state): State<Arc<AppState>>, RoutePath(id): RoutePath<String>) -> Response {
    work(state, move |vault| Ok(json!(vault.get_grant(&id)?))).await
}
async fn revoke(State(state): State<Arc<AppState>>, RoutePath(id): RoutePath<String>) -> Response {
    reserved(state, None, move |vault| {
        Ok(json!({"grant":vault.revoke(&id)?,"execution_stop_confirmed":false}))
    })
    .await
}
async fn reveal(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
) -> Response {
    let lease = match super::super::files::lease(&state, &headers) {
        Ok(lease) => lease,
        Err(_) => return error(StatusCode::UNAUTHORIZED, "Node authorization required"),
    };
    work(state, move |vault| {
        let (grant, material) = vault.reveal(&id, &lease, chrono::Utc::now().timestamp())?;
        Ok(json!({"grant":grant,"material":material}))
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Refresh {
    expected_revision: String,
    material: Material,
}
async fn refresh(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    request: Request,
) -> Response {
    let lease = match super::super::files::lease(&state, request.headers()) {
        Ok(lease) => lease,
        Err(_) => return error(StatusCode::UNAUTHORIZED, "Node authorization required"),
    };
    let (permit, body) = match body::<Refresh>(&state, request).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    reserved(state, Some(permit), move |vault| {
        Ok(json!(vault.refresh(
            &id,
            &lease,
            &body.expected_revision,
            &body.material,
            chrono::Utc::now().timestamp()
        )?))
    })
    .await
}
async fn revoke_material(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
) -> Response {
    reserved(state, None, move |vault| {
        Ok(json!({"material":vault.revoke_material(&id)?,"execution_stop_confirmed":false}))
    })
    .await
}
async fn node_status(
    State(state): State<Arc<AppState>>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
) -> Response {
    let lease = match super::super::files::lease(&state, &headers) {
        Ok(lease) => lease,
        Err(_) => return error(StatusCode::UNAUTHORIZED, "Node authorization required"),
    };
    reserved(state, None, move |vault| {
        Ok(json!(vault.authorize(
            &id,
            &lease,
            chrono::Utc::now().timestamp()
        )?))
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusBatch { ids:Vec<String> }
async fn node_status_batch(State(state):State<Arc<AppState>>,headers:HeaderMap,Json(body):Json<StatusBatch>)->Response {
    let lease=match super::super::files::lease(&state,&headers){Ok(lease)=>lease,Err(_)=>return error(StatusCode::UNAUTHORIZED,"Node authorization required")};
    if body.ids.is_empty() || body.ids.len()>256 {return error(StatusCode::BAD_REQUEST,"Credential status batch must contain 1–256 identities");}
    reserved(state,None,move|vault|{
        let at=chrono::Utc::now().timestamp();let mut rows=Vec::with_capacity(body.ids.len());
        for id in body.ids {match vault.authorize(&id,&lease,at){
            Ok(grant)=>rows.push(json!({"id":id,"grant":grant})),
            Err(error)=>rows.push(json!({"id":id,"error":error})),
        }}Ok(json!({"grants":rows}))
    }).await
}

pub(in crate::runner_remote) fn admin_router() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/runner/credential-materials/{id}",
            get(metadata).put(update),
        )
        .route(
            "/api/runner/credential-materials/{id}/revoke",
            post(revoke_material),
        )
        .route("/api/runner/credential-grants", post(issue))
        .route("/api/runner/credential-grants/{id}", get(status))
        .route("/api/runner/credential-grants/{id}/revoke", post(revoke))
        .layer(DefaultBodyLimit::max(16 * 1024))
}
pub(in crate::runner_remote) fn node_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/v1/runner/credential-grants/{id}", get(reveal))
        .route("/v1/runner/credential-grants/{id}/status", get(node_status))
        .route("/v1/runner/credential-grants/status", post(node_status_batch))
        .route("/v1/runner/credential-grants/{id}/refresh", post(refresh))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .with_state(state)
}
