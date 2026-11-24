mod artifacts;
pub(super) use artifacts::{prepare_grant_store, verified_source, verified_source_store};
use super::{
    identity,
    transfers::{self, CHUNK_BYTES, Manifest, Scope, Transfers},
};
use crate::AppState;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

const DEFAULT_QUOTA: u64 = 4 * 1024 * 1024 * 1024;
fn failure(status: StatusCode, error: impl ToString) -> Response {
    (
        status,
        [("cache-control", "no-store")],
        Json(json!({"error":error.to_string()})),
    )
        .into_response()
}
fn response(value: Value) -> Response {
    ([("cache-control", "no-store")], Json(value)).into_response()
}
fn storage(state: &AppState) -> Result<Transfers, String> {
    let quota = state
        .store
        .get("runner_transfer_policy", "local")
        .map_err(|e| e.to_string())?
        .and_then(|policy| policy["staging_quota_bytes"].as_u64())
        .unwrap_or(DEFAULT_QUOTA);
    Transfers::open(
        state.store.clone(),
        &crate::native_chat::state_dir(state),
        quota,
    )
}
async fn blocking(
    state: Arc<AppState>,
    work: impl FnOnce(&AppState, &Transfers) -> Result<Response, String> + Send + 'static,
) -> Response {
    let permit = match acquire(&state) {
        Ok(permit) => permit,
        Err(response) => return response,
    };
    blocking_reserved(state, permit, work).await
}
fn acquire(state: &AppState) -> Result<tokio::sync::OwnedSemaphorePermit, Response> {
    state
        .remote_transfer_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            failure(
                StatusCode::TOO_MANY_REQUESTS,
                "Bulk transfer slots are busy; retry this chunk",
            )
        })
}
async fn blocking_reserved(
    state: Arc<AppState>,
    permit: tokio::sync::OwnedSemaphorePermit,
    work: impl FnOnce(&AppState, &Transfers) -> Result<Response, String> + Send + 'static,
) -> Response {
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work(&state, &storage(&state)?)
    })
    .await
    {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => failure(StatusCode::CONFLICT, error),
        Err(_) => failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Transfer storage worker failed",
        ),
    }
}
fn staging_authorized(state: &AppState, scope: &Scope) -> Result<(), String> {
    identity::allow_staging(
        &state.store,
        &scope.node_id,
        &scope.project_id,
        chrono::Utc::now().timestamp(),
    )
}
async fn begin(State(state): State<Arc<AppState>>, Json(manifest): Json<Manifest>) -> Response {
    blocking(state, move |state, store| {
        staging_authorized(state, &manifest.scope)?;
        Ok(response(store.begin(&manifest)?))
    })
    .await
}
async fn status(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    blocking(state, move |_, store| {
        let manifest = store.manifest(&id)?;
        Ok(response(store.status(&id, &manifest.scope)?))
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Offset {
    offset: u64,
}
async fn chunk(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<Offset>,
    request: Request,
) -> Response {
    let Some(hash) = request
        .headers()
        .get("x-muteki-sha256")
        .and_then(|h| h.to_str().ok())
        .map(str::to_owned)
    else {
        return failure(StatusCode::BAD_REQUEST, "Missing chunk digest");
    };
    let permit = match acquire(&state) {
        Ok(permit) => permit,
        Err(response) => return response,
    };
    let bytes = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        axum::body::to_bytes(request.into_body(), CHUNK_BYTES),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => {
            return failure(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Upload body is invalid or exceeds the chunk limit",
            );
        }
        Err(_) => {
            return failure(
                StatusCode::REQUEST_TIMEOUT,
                "Chunk upload timed out; retry a smaller chunk",
            );
        }
    };
    blocking_reserved(state, permit, move |state, store| {
        let manifest = store.manifest(&id)?;
        staging_authorized(state, &manifest.scope)?;
        Ok(response(store.chunk(
            &id,
            &manifest.scope,
            query.offset,
            &bytes,
            &hash,
        )?))
    })
    .await
}
async fn seal(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    blocking(state, move |state, store| {
        let manifest = store.manifest(&id)?;
        staging_authorized(state, &manifest.scope)?;
        Ok(response(store.seal(&id, &manifest.scope)?))
    })
    .await
}
async fn cancel(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    blocking(state, move |_, store| {
        let manifest = store.manifest(&id)?;
        Ok(response(store.cancel(&id, &manifest.scope)?))
    })
    .await
}
async fn admin_chunk(State(state):State<Arc<AppState>>,Path(id):Path<String>,Query(range):Query<Range>)->Response{
    blocking(state,move |state,store|{
        let manifest=store.manifest(&id)?;
        verified_source(state,&manifest)?;
        let bytes=store.read_chunk(&id,&manifest.scope,range.offset,range.length)?;
        Ok(([("content-type","application/octet-stream".to_owned()),("cache-control","no-store".to_owned()),
            ("content-disposition","attachment; filename=artifact.bin".to_owned()),("x-muteki-sha256",transfers::digest(&bytes)),
            ("x-muteki-offset",range.offset.to_string()),("x-muteki-total-bytes",manifest.size.to_string())],bytes).into_response())
    }).await
}
pub fn admin_router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/runner/transfers", post(begin))
        .route("/api/runner/transfers/{id}", get(status))
        .route(
            "/api/runner/transfers/{id}/chunks",
            get(admin_chunk).put(chunk),
        )
        .route("/api/runner/transfers/{id}/seal", post(seal))
        .route("/api/runner/transfers/{id}/cancel", post(cancel))
        .layer(DefaultBodyLimit::max(16 * 1024))
}
pub(super) fn lease(state: &AppState, headers: &HeaderMap) -> Result<identity::Lease, String> {
    let encoded = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .filter(|v| v.len() <= 2048)
        .ok_or("Node authorization required")?;
    let lease: identity::Lease = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| "Invalid node authorization")?,
    )
    .map_err(|_| "Invalid node authorization".to_owned())?;
    identity::projects(&state.store, &lease, chrono::Utc::now().timestamp())?;
    Ok(lease)
}
fn node_manifest(
    state: &AppState,
    store: &Transfers,
    id: &str,
    lease: &identity::Lease,
) -> Result<Manifest, Response> {
    let now = chrono::Utc::now().timestamp();
    identity::projects(&state.store, lease, now).map_err(|_| {
        failure(
            StatusCode::UNAUTHORIZED,
            "Invalid or expired node authorization",
        )
    })?;
    let manifest = store
        .manifest(id)
        .map_err(|_| failure(StatusCode::NOT_FOUND, "Unknown transfer"))?;
    if manifest.scope.node_id != lease.node_id {
        return Err(failure(
            StatusCode::FORBIDDEN,
            "Transfer is not assigned to this node",
        ));
    }
    identity::authorize(&state.store, lease, &manifest.scope.project_id, false, now)
        .map_err(|_| failure(StatusCode::FORBIDDEN, "Transfer scope is not authorized"))?;
    Ok(manifest)
}
async fn node_status(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let lease = match lease(&state, &headers) {
        Ok(lease) => lease,
        Err(_) => return failure(StatusCode::UNAUTHORIZED, "Node authorization required"),
    };
    blocking(state, move |state, store| {
        let manifest = match node_manifest(state, store, &id, &lease) {
            Ok(manifest) => manifest,
            Err(response) => return Ok(response),
        };
        Ok(response(store.status(&id, &manifest.scope)?))
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Range {
    offset: u64,
    length: usize,
}
async fn node_chunk(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(range): Query<Range>,
    headers: HeaderMap,
) -> Response {
    let lease = match lease(&state, &headers) {
        Ok(lease) => lease,
        Err(_) => return failure(StatusCode::UNAUTHORIZED, "Node authorization required"),
    };
    blocking(state, move |state, store| {
        let manifest = match node_manifest(state, store, &id, &lease) {
            Ok(manifest) => manifest,
            Err(response) => return Ok(response),
        };
        verified_source(state,&manifest)?;
        let bytes = store.read_chunk(&id, &manifest.scope, range.offset, range.length)?;
        Ok((
            [
                ("content-type", "application/octet-stream".to_owned()),
                ("cache-control", "no-store".to_owned()),
                ("x-muteki-sha256", transfers::digest(&bytes)),
                ("x-muteki-offset", range.offset.to_string()),
                ("x-muteki-total-bytes", manifest.size.to_string()),
            ],
            bytes,
        )
            .into_response())
    })
    .await
}
pub fn node_router(state: Arc<AppState>) -> Router {
    Router::new()
        .merge(artifacts::node_routes())
        .route("/v1/runner/transfers/{id}", get(node_status))
        .route("/v1/runner/transfers/{id}/chunks", get(node_chunk))
        .with_state(state)
}
