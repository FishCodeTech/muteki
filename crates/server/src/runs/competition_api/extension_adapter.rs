use super::*;
use base64::Engine;
use sha2::{Digest, Sha256};

pub(super) const FIRST_SYNC_NOTICES: &str = "competition_extension_first_sync_notices";
#[cfg(test)]
pub(super) const TEST_ADAPTERS: &str = "test_extension_platform_adapters";

pub(super) fn supported(state: &AppState, kind: &str) -> ApiResult<bool> {
    #[cfg(test)]
    if state
        .store
        .get(TEST_ADAPTERS, kind)
        .map_err(ApiError::internal)?
        .is_some()
    {
        return Ok(true);
    }
    Ok(
        crate::extensions::ready_capabilities(state, "platform-adapter")
            .map_err(ApiError::internal)?
            .iter()
            .any(|entry| entry["id"] == kind),
    )
}

fn extension_id_for(state: &AppState, kind: &str) -> String {
    #[cfg(test)]
    if let Ok(Some(entry)) = state.store.get(TEST_ADAPTERS, kind) {
        let ext_id = field(&entry, "extension_id");
        if !ext_id.is_empty() {
            return ext_id.to_owned();
        }
    }
    crate::extensions::ready_capabilities(state, "platform-adapter")
        .ok()
        .and_then(|entries| {
            entries
                .into_iter()
                .find(|entry| entry["id"] == kind)
                .and_then(|entry| entry["extension_id"].as_str().map(str::to_owned))
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| kind.to_owned())
}

pub(super) fn sanitize_platform_value(node: &Value) -> Value {
    const FORBIDDEN: [&str; 5] = [
        "token",
        "credential",
        "cookie",
        "vpn_config",
        "private_key",
    ];
    match node {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, value) in map {
                let lower = key.to_ascii_lowercase();
                if FORBIDDEN.contains(&lower.as_str()) {
                    continue;
                }
                out.insert(key.clone(), sanitize_platform_value(value));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(100)
                .map(sanitize_platform_value)
                .collect(),
        ),
        Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null => node.clone(),
    }
}

pub(super) fn parse_clock_timestamp(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let normalized = if trimmed.ends_with('Z') {
        format!("{}+00:00", &trimmed[..trimmed.len() - 1])
    } else {
        trimmed.to_owned()
    };
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(&normalized) {
        return Some(parsed.with_timezone(&chrono::Utc).to_rfc3339());
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S") {
        return Some(naive.and_utc().to_rfc3339());
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(naive.and_utc().to_rfc3339());
    }
    None
}

pub(super) fn apply_remote_sync_metadata(
    state: &AppState,
    connection: &Value,
    competition: &mut Value,
    remote: &RemoteFetch,
    command_id: &str,
) -> ApiResult<(bool, Vec<Value>)> {
    let strip_transient = |value: &Value| -> Value {
        let mut copy = value.clone();
        if let Some(obj) = copy.as_object_mut() {
            for key in [
                "updated_at",
                "last_sync_attempt_at",
                "last_synced_at",
                "last_sync_error",
                "first_sync_notices",
            ] {
                obj.remove(key);
            }
        }
        copy
    };
    let before = strip_transient(competition);
    if let Some(platform_status) = remote.platform_status.as_ref() {
        competition["platform_status"] = sanitize_platform_value(platform_status);
    }
    if let Some(clock) = remote.competition_clock.as_ref() {
        let safe_clock = sanitize_platform_value(clock);
        competition["competition_clock"] = safe_clock.clone();
        for (source, target) in [
            ("started_at", "starts_at"),
            ("starts_at", "starts_at"),
            ("ends_at", "ends_at"),
        ] {
            if let Some(raw) = safe_clock.get(source).and_then(Value::as_str)
                && let Some(parsed) = parse_clock_timestamp(raw)
            {
                competition[target] = json!(parsed);
            }
        }
    }
    let after = strip_transient(competition);
    let platform_status_changed = before != after;

    let connection_id = field(connection, "connection_id");
    let competition_id = field(competition, "competition_id");
    let external_competition_id = field(competition, "external_competition_id");
    let extension_id = if remote.extension_id.is_empty() {
        extension_id_for(state, field(connection, "platform_kind"))
    } else {
        remote.extension_id.clone()
    };

    for raw_notice in &remote.notices {
        let sanitized = sanitize_platform_value(raw_notice);
        let explicit_id = field(&sanitized, "notice_id")
            .to_owned()
            .if_empty(field(&sanitized, "id"))
            .if_empty(field(&sanitized, "code"));
        let notice_id = if explicit_id.is_empty() {
            let digest = format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&sanitized).unwrap_or_default())
            );
            format!("notice-{}", &digest[..16])
        } else {
            explicit_id
        };
        let key = serde_json::to_string(&json!([
            extension_id,
            connection_id,
            external_competition_id,
            notice_id
        ]))
        .map_err(ApiError::internal)?;
        if state
            .store
            .get(FIRST_SYNC_NOTICES, &key)
            .map_err(ApiError::internal)?
            .is_some()
        {
            continue;
        }
        let record = json!({
            "notice_key": key,
            "extension_id": extension_id,
            "connection_id": connection_id,
            "competition_id": competition_id,
            "external_competition_id": external_competition_id,
            "notice_id": notice_id,
            "notice": sanitized,
            "created_at": now(),
        });
        let event = leases::planned_event(
            competition_id,
            "competition",
            competition_id,
            "competition.sync.notice",
            command_id,
            json!({
                "competition_id": competition_id,
                "connection_id": connection_id,
                "external_competition_id": external_competition_id,
                "extension_id": extension_id,
                "notice_id": notice_id,
                "notice": sanitized,
            }),
        );
        let _ = put_records_with_events(
            state,
            competition_id,
            &[(FIRST_SYNC_NOTICES, key.as_str(), None)],
            &[(FIRST_SYNC_NOTICES, key.as_str(), &record)],
            &[event],
        )?;
    }

    let mut notices = state
        .store
        .list(FIRST_SYNC_NOTICES)
        .map_err(ApiError::internal)?
        .into_iter()
        .filter(|row| {
            row["connection_id"] == connection_id
                && row["external_competition_id"] == external_competition_id
        })
        .collect::<Vec<_>>();
    notices.sort_by(|a, b| {
        field(a, "created_at")
            .cmp(field(b, "created_at"))
            .then_with(|| field(a, "notice_id").cmp(field(b, "notice_id")))
    });
    if !notices.is_empty() {
        competition["first_sync_notices"] = json!(notices);
    }
    Ok((platform_status_changed, notices))
}

fn connection_ref(connection: &Value) -> Value {
    json!({"schema_version":1,"connection_id":field(connection,"connection_id"),
        "platform_kind":field(connection,"platform_kind"),
        "endpoint":field(connection,"canonical_base_url"),"competition_id":null})
}

async fn invoke(
    state: &Arc<AppState>,
    connection: &Value,
    action: &str,
    mut body: Value,
) -> ApiResult<Value> {
    let kind = field(connection, "platform_kind");
    if !supported(state, kind)? {
        return Err(ApiError::unsupported("Platform adapter is unavailable"));
    }
    #[cfg(test)]
    if let Some(mock) = state
        .store
        .get(TEST_ADAPTERS, kind)
        .map_err(ApiError::internal)?
    {
        if let Some(action_resp) = mock.get(action) {
            return Ok(action_resp.clone());
        }
    }
    body["connection"] = json!({"connection_id":field(connection,"connection_id"),
        "platform_kind":kind,
        "endpoint":field(connection,"canonical_base_url"),
        "account_key":field(connection,"account_key"),
        "credential_ref":field(connection,"credential_ref")});
    if let Some(reference) = connection["credential_ref"]
        .as_str()
        .filter(|s| !s.is_empty())
    {
        if let Ok(secret) = read_secret(state, reference) {
            body["credential"] = json!(secret);
        }
    }
    crate::extensions::invoke_capability(state, "platform-adapter", kind, action, body)
        .await
        .map_err(|error| ApiError::new(StatusCode::BAD_GATEWAY, "competition.platform_error", error))
}

fn invalid_response(message: &'static str) -> ApiError {
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        "competition.platform_response",
        message,
    )
}

fn platform_error(response: &Value) -> ApiResult<()> {
    let Some(error) = response["error"].as_object() else {
        return Ok(());
    };
    let code = error
        .get("code")
        .or_else(|| error.get("category"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Extension platform error");
    let (status, category) = match code {
        "auth_required" | "authentication_required" => {
            (StatusCode::CONFLICT, "competition.auth_required")
        }
        "not_found" => (StatusCode::NOT_FOUND, "competition.platform_not_found"),
        "permission" => (StatusCode::FORBIDDEN, "competition.platform_permission"),
        "rate_limited" => (
            StatusCode::TOO_MANY_REQUESTS,
            "competition.platform_rate_limited",
        ),
        "timeout" => (StatusCode::GATEWAY_TIMEOUT, "competition.platform_timeout"),
        _ => (StatusCode::BAD_GATEWAY, "competition.platform_error"),
    };
    Err(ApiError::new(status, category, message))
}

pub(super) async fn probe(state: &Arc<AppState>, connection: &Value) -> ApiResult<Value> {
    let response = invoke(
        state,
        connection,
        "probe",
        json!({"request":connection_ref(connection)}),
    )
    .await?;
    platform_error(&response)?;
    let capabilities = response.get("capabilities").unwrap_or(&response);
    if !capabilities.is_object() {
        return Err(invalid_response(
            "Extension probe capabilities must be an object",
        ));
    }
    let mut result = capabilities.clone();
    if result["platform_kind"].as_str().is_none_or(str::is_empty) {
        result["platform_kind"] = json!(field(connection, "platform_kind"));
    }
    Ok(result)
}

pub(super) fn parse_sync_response(
    default_extension_id: &str,
    response: &Value,
) -> ApiResult<RemoteFetch> {
    platform_error(response)?;
    let result = response.get("result").unwrap_or(response);
    let detail = &result["detail"];
    let snapshots = detail["snapshots"].as_array().cloned().unwrap_or_default();
    let mut challenges = Vec::new();
    for snapshot in &snapshots {
        if !snapshot.is_object() {
            return Err(invalid_response(
                "Extension challenge snapshot must be an object",
            ));
        }
        let external_id = field(snapshot, "external_challenge_id");
        if external_id.is_empty() {
            continue;
        }
        let raw = snapshot
            .get("raw_payload")
            .cloned()
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        let mut challenge = json!({"external_challenge_id":external_id,
            "name":field(snapshot,"name"),"category":field(snapshot,"category"),
            "remote_state":snapshot["remote_state"].as_str().unwrap_or("open"),
            "points":snapshot.get("points").cloned().unwrap_or_else(||json!(0)),
            "description":field(snapshot,"description"),
            "target":field(snapshot,"target"),"flag_format":field(snapshot,"flag_format"),
            "hints":snapshot.get("hints").cloned().unwrap_or_else(||json!([])),
            "prerequisites":snapshot.get("prerequisites").cloned().unwrap_or_else(||json!([])),
            "artifacts":snapshot.get("artifacts").cloned().unwrap_or_else(||json!([])),
            "multi_flag":snapshot["multi_flag"].as_bool().unwrap_or(false),
            "expected_flags":snapshot["expected_flags"].as_u64().unwrap_or(1),"raw":raw});
        if snapshot["hidden"] == true {
            challenge["remote_state"] = json!("hidden");
        }
        challenges.push(challenge);
    }
    let cursor = result["cursor"]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let snapshot_complete = detail["snapshot_complete"].as_bool().unwrap_or(true);
    let platform_status = detail
        .get("platform_status")
        .or_else(|| result.get("platform_status"))
        .filter(|v| v.is_object())
        .map(sanitize_platform_value);
    let competition_clock = detail
        .get("competition_clock")
        .or_else(|| result.get("competition_clock"))
        .filter(|v| v.is_object())
        .map(sanitize_platform_value);
    let raw_notices = detail
        .get("first_sync_notices")
        .or_else(|| detail.get("notices"))
        .or_else(|| result.get("first_sync_notices"))
        .or_else(|| result.get("notices"))
        .or_else(|| detail["platform_status"].get("first_sync_notices"))
        .or_else(|| detail["platform_status"].get("notices"));
    let notices = match raw_notices {
        Some(Value::Array(items)) => items
            .iter()
            .take(100)
            .map(sanitize_platform_value)
            .collect(),
        Some(obj @ Value::Object(_)) => vec![sanitize_platform_value(obj)],
        _ => Vec::new(),
    };
    let extension_id = field(detail, "extension_id")
        .to_owned()
        .if_empty(field(result, "extension_id"))
        .if_empty(default_extension_id);
    Ok(RemoteFetch {
        challenges,
        cursor,
        snapshot_complete,
        platform_status,
        competition_clock,
        notices,
        extension_id,
    })
}

#[cfg(test)]
#[tokio::test]
async fn explicit_extension_prerequisites_create_a_new_revision() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::store::Store::open(dir.path().join("muteki.sqlite")).unwrap();
    store.put("system", "state_dir", &json!(dir.path())).unwrap();
    let state = Arc::new(AppState::new(store));
    let connection = json!({"connection_id":"extension-local","platform_kind":"fixture"});
    let snapshot = |prerequisites: Vec<&str>| json!({"result":{"detail":{"snapshots":[
        {"external_challenge_id":"a","name":"A","category":"web","remote_state":"open",
            "description":"Find A","points":100,"prerequisites":[]},
        {"external_challenge_id":"b","name":"B","category":"web","remote_state":"open",
            "description":"Find B","points":100,"prerequisites":prerequisites}
    ]}}});
    let first = parse_sync_response("fixture", &snapshot(vec![])).unwrap();
    save_challenges(&state, &connection, "comp-test", "sync-first", &first.challenges, true)
        .await.unwrap();
    let second = parse_sync_response("fixture", &snapshot(vec!["a"])).unwrap();
    save_challenges(&state, &connection, "comp-test", "sync-second", &second.challenges, true)
        .await.unwrap();
    let revisions = scoped(&state, REVISIONS, "comp-test").unwrap();
    let a: Vec<_> = revisions.iter().filter(|revision| revision["name"] == "A").collect();
    let b: Vec<_> = revisions.iter().filter(|revision| revision["name"] == "B").collect();
    assert_eq!(a.len(), 1);
    assert_eq!(b.len(), 2);
    assert!(b.iter().any(|revision| revision["revision_seq"] == 2
        && revision["prerequisites"] == json!(["a"])));
}

pub(super) async fn sync(
    state: &Arc<AppState>,
    connection: &Value,
    external_competition_id: &str,
) -> ApiResult<RemoteFetch> {
    let connection_id = field(connection, "connection_id");
    let previous = state
        .store
        .list(COMPETITIONS)
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|row| {
            row["connection_id"] == connection_id
                && row["external_competition_id"] == external_competition_id
        });
    let cursor = if let Some(competition) = previous {
        state
            .store
            .get(SYNC_CURSORS, field(&competition, "competition_id"))
            .map_err(ApiError::internal)?
            .and_then(|row| row["cursor"].as_str().map(str::to_owned))
    } else {
        None
    };
    let request = json!({"schema_version":1,"connection_id":connection_id,
        "competition_id":external_competition_id,"cursor":cursor});
    let response = invoke(
        state,
        connection,
        "sync_competition",
        json!({"request":request}),
    )
    .await?;
    let default_ext_id = extension_id_for(state, field(connection, "platform_kind"));
    parse_sync_response(&default_ext_id, &response)
}

pub(super) async fn submit(
    state: &Arc<AppState>,
    connection: &Value,
    challenge: &Value,
    answer: &str,
    submission_id:&str,
) -> ApiResult<(String, String)> {
    let request = json!({"schema_version":1,"connection_id":field(connection,"connection_id"),
        "challenge_key":field(challenge,"external_challenge_id"),"flag":answer,
        "artifact_sha256":null,"idempotency_key":submission_id,"payload":{}});
    let response = invoke(state, connection, "submit", json!({"request":request})).await?;
    platform_error(&response)?;
    let result = response.get("result").unwrap_or(&response);
    let status = result["status"].as_str().unwrap_or("pending");
    let local = match status {
        "correct" => "correct",
        "incorrect" => "wrong",
        "duplicate"|"duplicate_or_solved"=>"duplicate_or_solved",
        "pending" => "pending",
        "unknown" => "unknown",
        other => other,
    };
    let remote_id = result["submission_id"].as_str().unwrap_or("").to_owned();
    Ok((local.to_owned(), remote_id))
}

pub(super) async fn fetch_artifact(
    state: &Arc<AppState>,
    connection: &Value,
    remote_id: &str,
    url: &str,
    expected_sha256:Option<&str>,
) -> ApiResult<(Vec<u8>, String)> {
    let artifact = json!({"schema_version":1,"connection_id":field(connection,"connection_id"),
        "remote_id":remote_id,"url":url,"sha256":expected_sha256});
    let response = invoke(
        state,
        connection,
        "fetch_artifact",
        json!({"artifact":artifact}),
    )
    .await?;
    platform_error(&response)?;
    let result = response.get("artifact").unwrap_or(&response);
    let encoded = result["content"].as_str().unwrap_or("");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| invalid_response("Extension artifact content is invalid base64"))?;
    let actual=format!("{:x}",Sha256::digest(&bytes));
    for expected in [expected_sha256,result["sha256"].as_str()].into_iter().flatten(){if !expected.eq_ignore_ascii_case(&actual){return Err(invalid_response("Extension attachment digest differs from the expected content"));}}
    let media_type = result["media_type"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or("application/octet-stream")
        .to_owned();
    Ok((bytes, media_type))
}

pub(super) async fn instance(
    state: &Arc<AppState>,
    connection: &Value,
    challenge: &Value,
    existing: Option<&Value>,
    stop: bool,
) -> ApiResult<Value> {
    let external = field(challenge, "external_challenge_id");
    let (action, payload) = if let Some(lease) = existing {
        let reference = json!({"schema_version":1,"lease_id":field(lease,"platform_instance_id"),
            "connection_id":field(connection,"connection_id"),"challenge_key":external,
            "fencing_token":lease["fencing_token"],"expires_at":lease["expires_at"]});
        (
            if stop {
                "release_instance"
            } else {
                "renew_instance"
            },
            json!({"lease":reference}),
        )
    } else {
        let reference = json!({"schema_version":1,"connection_id":field(connection,"connection_id"),
            "challenge_key":external,"revision":1});
        ("acquire_instance", json!({"challenge":reference}))
    };
    let response = invoke(state, connection, action, payload).await?;
    platform_error(&response)?;
    if stop {
        return Ok(json!({}));
    }
    let result = response.get("result").unwrap_or(&response);
    let lease = &result["lease"];
    let endpoints = &result["endpoints"];
    let address = endpoints["http"]
        .as_str().or_else(||endpoints["https"].as_str()).or_else(||endpoints["tcp"].as_str())
        .or_else(|| {
            endpoints
                .as_object()
                .and_then(|values| values.values().find_map(Value::as_str))
        })
        .unwrap_or("");
    Ok(json!({"url":address,"expiresAt":lease["expires_at"],"lease":lease}))
}

pub(super) async fn reconcile_submission(state:&Arc<AppState>,connection:&Value,challenge:&Value,submission:&Value)->ApiResult<(String,String)>{
    let request=json!({"schema_version":1,"connection_id":connection["connection_id"],"challenge_key":challenge["external_challenge_id"],"submission_id":submission["submission_id"],"remote_submission_id":submission["remote_receipt"],"idempotency_key":submission["submission_id"],"digest":submission["digest"]});
    let response=invoke(state,connection,"reconcile_submission",json!({"request":request})).await?;platform_error(&response)?;let result=response.get("result").unwrap_or(&response);
    let status=match result["status"].as_str().unwrap_or("unknown"){"correct"|"partial"=>"correct","wrong"|"incorrect"=>"wrong","duplicate"|"duplicate_or_solved"=>"duplicate_or_solved",_=>"unknown"};
    Ok((status.into(),result["submission_id"].as_str().unwrap_or(field(submission,"remote_receipt")).into()))
}
