use super::*;
use crate::runner_remote::commands::{Command, Operation};
const GRANTS: &str = "runner_artifact_grants";

#[cfg(test)]
pub(in crate::runner_remote) fn prepare_grant(
    state: &AppState,
    command: &Command,
) -> Result<(), String> {
    prepare_grant_store(&state.store,command)
}
pub(in crate::runner_remote) fn prepare_grant_store(store:&crate::store::Store,command:&Command)->Result<(),String>{
    store.document_scope(||->Result<(),String>{
        if matches!(command.action,Operation::Stop|Operation::Ensure{..}) {
            for mut grant in store.list(GRANTS).map_err(|e|e.to_string())? {
                if grant["scope"]["node_id"]==command.node_id && grant["scope"]["run_id"]==command.run_id &&
                    ((matches!(command.action,Operation::Stop)&&grant["scope"]["generation"].as_u64().is_some_and(|g|g<=command.generation))||grant["scope"]["generation"].as_u64().is_some_and(|g|g<command.generation)||(grant["scope"]["generation"]==command.generation&&grant["policy_version"]!=command.policy_version)) {
                    grant["state"]=json!("revoked");
                    store.put(GRANTS,grant["id"].as_str().ok_or("Invalid artifact grant")?,&grant).map_err(|e|e.to_string())?;
                }
            }
        }
        if let Operation::TransferPush{transfer_id,path,max_bytes}=&command.action {
            transfers::validate_path(path)?;
            if transfer_id.is_empty()||transfer_id.len()>160||!transfer_id.bytes().all(|b|b.is_ascii_alphanumeric()||matches!(b,b'-'|b'_'))||*max_bytes==0||*max_bytes>DEFAULT_QUOTA {
                return Err("Invalid artifact identity or size bound".into());
            }
            let scope=Scope{node_id:command.node_id.clone(),project_id:command.project_id.clone(),run_id:command.run_id.clone(),generation:command.generation};
            let digest=command.digest()?;
            if let Some(prior)=store.get(GRANTS,transfer_id).map_err(|e|e.to_string())? {
                if prior["digest"]!=digest||prior["state"]!="active" {return Err("Artifact grant conflicts or was revoked".into());}
            }else if store.get("runner_transfers",transfer_id).map_err(|e|e.to_string())?.is_some(){
                return Err("Artifact identity already belongs to an input transfer".into());
            }
            store.put(GRANTS,transfer_id,&json!({"id":transfer_id,"scope":scope,"path":path,"max_bytes":max_bytes,
                "digest":digest,"policy_version":command.policy_version,"session_generation":command.session_generation,
                "credential_version":command.credential_version,"state":"active"})).map_err(|e|e.to_string())?;
        }
        Ok(())
    }).map_err(|e|e.to_string())?
}
pub(in crate::runner_remote) fn verified_source(
    state: &AppState,
    manifest: &Manifest,
) -> Result<(), String> {
    verified_source_store(&state.store,manifest)
}
pub(in crate::runner_remote) fn verified_source_store(store:&crate::store::Store,manifest:&Manifest)->Result<(),String>{
    if store
        .get(GRANTS, &manifest.id)
        .map_err(|e| e.to_string())?
        .is_some()
    {
        let accepted = store
            .get("runner_artifacts", &manifest.id)
            .map_err(|e| e.to_string())?
            .ok_or("Artifact has no accepted publication receipt")?;
        if accepted["state"] != "verified" || accepted["manifest"] != json!(manifest) {
            return Err("Artifact publication does not match its staged bytes".into());
        }
    }
    Ok(())
}
fn authorized(
    state: &AppState,
    lease: &identity::Lease,
    manifest: &Manifest,
) -> Result<(), String> {
    let now = chrono::Utc::now().timestamp();
    identity::authorize(&state.store, lease, &manifest.scope.project_id, false, now)?;
    let grant = state
        .store
        .get(GRANTS, &manifest.id)
        .map_err(|e| e.to_string())?
        .ok_or("No artifact upload grant")?;
    if grant["state"] != "active"
        || grant["scope"] != json!(manifest.scope)
        || manifest.scope.node_id != lease.node_id
        || grant["path"] != manifest.path
        || manifest.size > grant["max_bytes"].as_u64().unwrap_or(0)
        || grant["session_generation"] != lease.generation
        || grant["credential_version"] != lease.credential_version
    {
        return Err("Artifact upload is outside its current execution grant".into());
    }
    Ok(())
}
async fn begin_artifact(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(manifest): Json<Manifest>,
) -> Response {
    let lease = match lease(&state, &headers) {
        Ok(lease) => lease,
        Err(_) => return failure(StatusCode::UNAUTHORIZED, "Node authorization required"),
    };
    blocking(state, move |state, store| {
        authorized(state, &lease, &manifest)?;
        Ok(response(store.begin(&manifest)?))
    })
    .await
}
async fn upload(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(query): Query<Offset>,
    request: Request,
) -> Response {
    let lease = match lease(&state, request.headers()) {
        Ok(lease) => lease,
        Err(_) => return failure(StatusCode::UNAUTHORIZED, "Node authorization required"),
    };
    let hash = match request
        .headers()
        .get("x-muteki-sha256")
        .and_then(|h| h.to_str().ok())
    {
        Some(hash) => hash.to_owned(),
        None => return failure(StatusCode::BAD_REQUEST, "Missing chunk digest"),
    };
    let manifest = match state
        .store
        .get("runner_transfers", &id)
        .ok()
        .flatten()
        .and_then(|row| serde_json::from_value::<Manifest>(row["manifest"].clone()).ok())
    {
        Some(manifest) => manifest,
        None => return failure(StatusCode::NOT_FOUND, "Unknown artifact"),
    };
    if let Err(error) = authorized(&state, &lease, &manifest) {
        return failure(StatusCode::FORBIDDEN, error);
    }
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
        Ok(Err(_)) => return failure(StatusCode::PAYLOAD_TOO_LARGE, "Invalid or oversized chunk"),
        Err(_) => return failure(StatusCode::REQUEST_TIMEOUT, "Chunk upload timed out"),
    };
    blocking_reserved(state, permit, move |state, store| {
        authorized(state, &lease, &manifest)?;
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
async fn finish(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let lease = match lease(&state, &headers) {
        Ok(lease) => lease,
        Err(_) => return failure(StatusCode::UNAUTHORIZED, "Node authorization required"),
    };
    blocking(state,move |state,store|{
        let manifest=store.manifest(&id)?;
        authorized(state,&lease,&manifest)?;
        let sealed=store.seal(&id,&manifest.scope)?;
        state.store.document_scope(||->Result<(),String>{
            authorized(state,&lease,&manifest)?;
            state.store.put("runner_artifacts",&id,&json!({"id":id,"manifest":manifest,"state":"verified","accepted_at":chrono::Utc::now().to_rfc3339()})).map_err(|e|e.to_string())
        }).map_err(|e|e.to_string())??;
        Ok(response(sealed))
    }).await
}
pub(super) fn node_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/v1/runner/artifacts", post(begin_artifact))
        .route(
            "/v1/runner/artifacts/{id}/chunks",
            axum::routing::put(upload),
        )
        .route("/v1/runner/artifacts/{id}/seal", post(finish))
        .layer(DefaultBodyLimit::max(16 * 1024))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn artifact_grants_fence_scope_size_stop_and_credential_rotation() {
        let root = tempfile::tempdir().unwrap();
        let state =
            AppState::new(crate::store::Store::open(root.path().join("db.sqlite")).unwrap());
        let now = chrono::Utc::now().timestamp();
        let token = identity::issue_enrollment(
            &state.store,
            "node".into(),
            vec!["project".into()],
            60,
            now,
        )
        .unwrap();
        let credential = identity::enroll(&state.store, &token, now).unwrap();
        let lease = identity::connect(&state.store, &credential, now).unwrap();
        let mut command = Command {
            id: "export".into(),
            node_id: lease.node_id.clone(),
            project_id: "project".into(),
            run_id: "run".into(),
            generation: 2,
            session_generation: lease.generation,
            credential_version: lease.credential_version,
            policy_version: 1,
            expires_at: now + 60,
            action: Operation::TransferPush {
                transfer_id: "artifact".into(),
                path: "result.txt".into(),
                max_bytes: 32,
            },
        };
        prepare_grant(&state, &command).unwrap();
        let manifest = Manifest {
            id: "artifact".into(),
            scope: Scope {
                node_id: lease.node_id.clone(),
                project_id: "project".into(),
                run_id: "run".into(),
                generation: 2,
            },
            path: "result.txt".into(),
            size: 8,
            sha256: transfers::digest(b"artifact"),
            mode: 0o600,
        };
        authorized(&state, &lease, &manifest).unwrap();
        assert!(verified_source(&state, &manifest).is_err());
        state
            .store
            .put(
                "runner_artifacts",
                &manifest.id,
                &json!({"state":"verified","manifest":manifest}),
            )
            .unwrap();
        verified_source(&state, &manifest).unwrap();

        for changed in ["size", "run", "path", "generation"] {
            let mut changed_manifest = manifest.clone();
            match changed {
                "size" => changed_manifest.size = 33,
                "run" => changed_manifest.scope.run_id = "another".into(),
                "path" => changed_manifest.path = "other".into(),
                _ => changed_manifest.scope.generation = 1,
            }
            assert!(authorized(&state, &lease, &changed_manifest).is_err());
        }
        command.action = Operation::Stop;
        command.generation = 1;
        prepare_grant(&state, &command).unwrap();
        authorized(&state, &lease, &manifest).unwrap();
        command.generation = 2;
        prepare_grant(&state, &command).unwrap();
        assert!(authorized(&state, &lease, &manifest).is_err());
        command.action = Operation::TransferPush {
            transfer_id: "artifact".into(),
            path: "result.txt".into(),
            max_bytes: 32,
        };
        assert!(prepare_grant(&state, &command).is_err());
        command.action = Operation::TransferPush {
            transfer_id: "second".into(),
            path: "result.txt".into(),
            max_bytes: 32,
        };
        prepare_grant(&state, &command).unwrap();
        let mut second = manifest;
        second.id = "second".into();
        authorized(&state, &lease, &second).unwrap();
        identity::rotate(&state.store, &lease.node_id, now).unwrap();
        assert!(authorized(&state, &lease, &second).is_err());
    }
}
