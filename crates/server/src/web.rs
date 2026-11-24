use axum::{
    Json,
    extract::Request,
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use serde_json::json;
pub async fn serve(request: Request) -> Response {
    if request.uri().path().starts_with("/api/") {
        return (StatusCode::NOT_IMPLEMENTED,Json(json!({"error":{"code":"capability.unavailable","message":"This operation is not available in the Rust service."},"path":request.uri().path()}))).into_response();
    }
    let path = request
        .uri()
        .path_and_query()
        .map(|v| v.as_str())
        .unwrap_or("/");
    Redirect::temporary(&format!("http://127.0.0.1:3101{path}")).into_response()
}
