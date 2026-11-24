//! Competition handlers for the shared command/query HTTP envelopes.
use super::*;

pub(crate) fn canonical_command_type(kind: &str) -> &str {
    match kind {
        "competition.challenge.select" => "challenge.select",
        "competition.challenge.skip" => "challenge.skip",
        "competition.scheduler.start" => "scheduler.start",
        "competition.scheduler.pause" => "scheduler.pause",
        "competition.scheduler.resume" => "scheduler.resume",
        "competition.instance.ensure" => "instance.ensure",
        "competition.instance.stop" => "instance.stop",
        "competition.platform_submission.approve" => "platform_submission.approve",
        "competition.platform_submission.retry" => "platform_submission.retry",
        "projection.rebuild" => "competition.projection.rebuild",
        "projection.rebuild_all" => "competition.projection.rebuild_all",
        other => other,
    }
}

pub(crate) fn supports_command(kind: &str) -> bool {
    let canonical = canonical_command_type(kind);
    binding_outbox::is_binding_command(canonical)
        || matches!(
            canonical,
            "connection.create"
                | "competition.connection.create"
                | "connection.update"
                | "connection.test"
                | "connection.unregister"
                | "connection.credential.set"
                | "connection.credential.revoke"
                | "connection.browser_session.import"
                | "connection.browser_session.renew"
                | "connection.browser_session.revoke"
                | "competition.sync"
                | "competition.update"
                | "competition.unregister"
                | "policy.update"
                | "competition.policy.update"
                | "scheduler.start"
                | "scheduler.pause"
                | "scheduler.resume"
                | "challenge.select"
                | "challenge.queue"
                | "competition.challenge.queue"
                | "challenge.skip"
                | "instance.ensure"
                | "instance.stop"
                | "platform_submission.approve"
                | "platform_submission.retry"
                | "competition.submission.submit"
                | "competition.submission.manual_override"
                | "competition.message"
                | "competition.projection.rebuild"
                | "competition.projection.rebuild_all"
        )
}
pub(crate) fn supports_query(kind: &str) -> bool {
    matches!(
        kind,
        "connection.list"
            | "competition.list"
            | "competition.snapshot"
            | "competition.challenges"
            | "competition.submissions"
            | "competition.leases"
            | "competition.projections"
            | "competition.projection.status"
            | "competition.projection.watermarks"
    )
}
fn scoped_id(payload: &Value, envelope: &Value, key: &str) -> ApiResult<String> {
    let from_payload = field(payload, key);
    let from_aggregate = field(envelope, "aggregate_id");
    if !from_payload.is_empty() && !from_aggregate.is_empty() && from_payload != from_aggregate {
        return Err(ApiError::invalid(format!(
            "{key} conflicts with aggregate_id"
        )));
    }
    let result = if !from_payload.is_empty() {
        from_payload
    } else {
        from_aggregate
    };
    if result.is_empty() {
        return Err(ApiError::invalid(format!("{key} is required")));
    }
    Ok(result.to_owned())
}
fn with_command_id(mut payload: Value, envelope: &Value) -> Value {
    payload["command_id"] = envelope["command_id"].clone();
    if envelope.get("idempotency_key").is_some() && payload.get("idempotency_key").is_none() {
        payload["idempotency_key"] = envelope["idempotency_key"].clone();
    }
    payload
}
pub(crate) async fn dispatch_command_envelope(state: Arc<AppState>, envelope: Value) -> Response {
    let result: ApiResult<Value> = async {
        let raw_kind = if field(&envelope, "command_type").is_empty() {
            field(&envelope, "kind")
        } else {
            field(&envelope, "command_type")
        };
        let kind = canonical_command_type(raw_kind);
        let payload = envelope
            .get("payload")
            .cloned()
            .filter(Value::is_object)
            .ok_or_else(|| ApiError::invalid("payload must be an object"))?;
        let mut body = with_command_id(payload.clone(), &envelope);
        let aggregate_id = field(&envelope, "aggregate_id").to_owned();
        let response = match kind {
            "connection.create" | "competition.connection.create" => {
                body["command_type"] = json!(kind);
                body["expected_version"] = envelope["expected_version"].clone();
                body["_planned_admission"] = envelope["_planned_admission"].clone();
                connection_refs::create(&state, &body)?
            }
            "connection.update" => {
                let id = scoped_id(&body, &envelope, "connection_id")?;
                body["expected_version"] = envelope["expected_version"].clone();
                body["_planned_admission"] = envelope["_planned_admission"].clone();
                patch_connection(State(state.clone()), Path(id), Json(body))
                    .await?
                    .0
            }
            "connection.test" => {
                let id = scoped_id(&body, &envelope, "connection_id")?;
                body["expected_version"] = envelope["expected_version"].clone();
                body["_planned_admission"] = envelope["_planned_admission"].clone();
                connection_outbox::test(&state,&id,&body)?
            }
            "connection.unregister" => {
                let id = scoped_id(&body, &envelope, "connection_id")?;
                body["expected_version"] = envelope["expected_version"].clone();
                body["_planned_admission"] = envelope["_planned_admission"].clone();
                connection_refs::unregister(&state,&id,&body)?
            }
            "connection.credential.set" => {
                let id = scoped_id(&body, &envelope, "connection_id")?;
                body["expected_version"] = envelope["expected_version"].clone();
                body["_planned_admission"] = envelope["_planned_admission"].clone();
                connection_refs::link_credential(&state,&id,&body)?
            }
            "connection.credential.revoke" => {
                let id = scoped_id(&body, &envelope, "connection_id")?;
                body["expected_version"] = envelope["expected_version"].clone();
                body["_planned_admission"] = envelope["_planned_admission"].clone();
                connection_refs::revoke_credential(&state,&id,&body)?
            }
            "connection.browser_session.import" => {
                let id = scoped_id(&body, &envelope, "connection_id")?;
                body["expected_version"] = envelope["expected_version"].clone();
                body["_planned_admission"] = envelope["_planned_admission"].clone();
                connection_refs::import_browser_state(&state,&id,body).await?
            }
            "connection.browser_session.renew" => {
                let id = scoped_id(&body, &envelope, "connection_id")?;
                body["expected_version"] = envelope["expected_version"].clone();
                body["_planned_admission"] = envelope["_planned_admission"].clone();
                connection_refs::browser_session(&state, &id, &body, true).await?
            }
            "connection.browser_session.revoke" => {
                let id = scoped_id(&body, &envelope, "connection_id")?;
                body["expected_version"] = envelope["expected_version"].clone();
                body["_planned_admission"] = envelope["_planned_admission"].clone();
                connection_refs::browser_session(&state, &id, &body, false).await?
            }
            "competition.sync" => {
                body["expected_version"] = envelope["expected_version"].clone();
                body["_planned_admission"] = envelope["_planned_admission"].clone();
                connection_outbox::sync(&state,&body)?
            }
            "competition.update" => {
                let id = scoped_id(&body, &envelope, "competition_id")?;
                body["expected_version"] = envelope["expected_version"].clone();
                body["_planned_admission"] = envelope["_planned_admission"].clone();
                patch_competition(State(state.clone()), Path(id), Json(body))
                    .await?
                    .0
            }
            "competition.unregister" => {
                let id = scoped_id(&body, &envelope, "competition_id")?;
                body["expected_version"] = envelope["expected_version"].clone();
                body["_planned_admission"] = envelope["_planned_admission"].clone();
                body["_cursor_scope"] = envelope["_verified_scope"].clone();
                archive_commands::unregister(&state,&id,&body)?
            }
            "competition.message" => {
                let id = scoped_id(&body, &envelope, "competition_id")?;
                let mut message = payload;
                message["competition_id"] = json!(id);
                let actor = field(&envelope["actor"],"id");
                let fallback = if actor.is_empty() { "operator" } else { actor };
                message["author"] = json!(field(&message,"author").to_owned().if_empty(fallback));
                let scope = envelope["_verified_scope"]
                    .as_str()
                    .unwrap_or("operator:local-user");
                let receipt = messages::execute(
                    &state,
                    &id,
                    field(&envelope, "command_id"),
                    field(&envelope, "idempotency_key"),
                    envelope["expected_version"].as_u64(),
                    scope,
                    &envelope["_planned_admission"],
                    &message,
                )?;
                json!({"receipt":receipt})
            }
            "competition.projection.rebuild" => {
                let name = field(&body, "projection_name")
                    .to_owned()
                    .if_empty(field(&body, "name"));
                let replayed = projections::rebuild(&state, &name)?;
                let wm = projections::watermark(&state, &name)?;
                let receipt = save_receipt(
                    &state,
                    field(&envelope, "command_id"),
                    "competition.projection.rebuild",
                    "competition",
                    &name,
                    json!({"projection_name": name, "replayed": replayed, "watermark": wm}),
                )?;
                json!({"receipt":receipt})
            }
            "competition.projection.rebuild_all" => {
                let replayed = projections::rebuild_all(&state)?;
                let wms = projections::watermarks(&state)?;
                let receipt = save_receipt(
                    &state,
                    field(&envelope, "command_id"),
                    "competition.projection.rebuild_all",
                    "competition",
                    "all",
                    json!({"replayed": replayed, "watermarks": wms}),
                )?;
                json!({"receipt":receipt})
            }
            _ if binding_outbox::is_binding_command(kind) => {
                if envelope["aggregate_type"] == "run_binding"
                    && !aggregate_id.is_empty()
                    && !field(&body,"binding_id").is_empty()
                    && aggregate_id != field(&body,"binding_id") {
                    return Err(ApiError::invalid("binding_id conflicts with aggregate_id"));
                }
                if envelope["aggregate_type"] == "run_binding" && !aggregate_id.is_empty() {
                    body["binding_id"] = json!(aggregate_id);
                }
                body["expected_version"] = envelope["expected_version"].clone();
                body["cursor_scope"] = json!(
                    envelope["_verified_scope"]
                        .as_str()
                        .unwrap_or("operator:local-user")
                );
                let hint = field(&body, "competition_id").to_owned();
                let hint = if !hint.is_empty() {
                    Some(hint.as_str())
                } else if envelope["aggregate_type"] == "competition" && !aggregate_id.is_empty() {
                    Some(aggregate_id.as_str())
                } else {
                    None
                };
                let receipt = binding_outbox::command(
                    &state,
                    hint,
                    kind,
                    field(&envelope, "command_id"),
                    &body,
                    &envelope["_planned_admission"],
                )
                .await?;
                json!({"receipt":receipt})
            }
            _ => {
                let id = if !field(&body, "competition_id").is_empty() {
                    field(&body, "competition_id").to_owned()
                } else if !aggregate_id.is_empty() && envelope["aggregate_type"] == "competition" {
                    aggregate_id
                } else {
                    return Err(ApiError::invalid("competition_id is required"));
                };
                let command = if matches!(kind,"challenge.select" | "challenge.queue"
                    | "competition.challenge.queue" | "challenge.skip"
                    | "policy.update" | "competition.policy.update"
                    | "scheduler.start" | "scheduler.pause" | "scheduler.resume"
                    | "instance.ensure" | "instance.stop"
                    | "competition.submission.submit" | "platform_submission.approve"
                    | "platform_submission.retry" | "competition.submission.manual_override") {
                    json!({"command_type":kind,"command_id":envelope["command_id"],
                        "idempotency_key":envelope["idempotency_key"],
                        "expected_version":envelope["expected_version"],
                        "cursor_scope":envelope["_verified_scope"].as_str().unwrap_or("operator:local-user"),
                        "actor_id":if envelope["actor"]["kind"] == "operator" {
                            envelope["actor"]["id"].as_str().unwrap_or("")
                        } else {
                            envelope["_verified_scope"].as_str()
                                .and_then(|scope|scope.strip_prefix("operator:")).unwrap_or("")
                        },
                        "_planned_admission":envelope["_planned_admission"],
                        "payload":payload})
                } else {
                    json!({"command_type":kind,"command_id":envelope["command_id"],"payload":body})
                };
                let receipt = execute_command(&state, &id, &command).await?;
                json!({"receipt":receipt})
            }
        };
        Ok(response)
    }
    .await;
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.into_response(),
    }
}

pub(crate) fn query_envelope(state: &AppState, envelope: &Value) -> Response {
    let result: ApiResult<Value> = (|| {
        let kind = field(envelope, "query_type");
        let query_id = field(envelope, "query_id");
        let params = envelope
            .get("params")
            .filter(|v| v.is_object())
            .unwrap_or(&Value::Null);
        let competition_id = field(params, "competition_id");
        let aggregate_id = field(envelope, "aggregate_id");
        if !competition_id.is_empty() && !aggregate_id.is_empty() && competition_id != aggregate_id
        {
            return Err(ApiError::invalid(
                "competition_id conflicts with aggregate_id",
            ));
        }
        let id = if competition_id.is_empty() {
            aggregate_id
        } else {
            competition_id
        };
        let projection = match kind {
            "connection.list" => {
                let mut rows = state
                    .store
                    .list_inserted(CONNECTIONS)
                    .map_err(ApiError::internal)?;
                rows.retain(|row| row["archived"] != true);
                rows.sort_by(|a, b| field(a, "created_at").cmp(field(b, "created_at")));
                json!(
                    rows.into_iter()
                        .map(query_connection_view)
                        .collect::<Vec<_>>()
                )
            }
            "competition.list" => {
                let mut rows = state
                    .store
                    .list_inserted(COMPETITIONS)
                    .map_err(ApiError::internal)?;
                rows.retain(|row| row["archived"] != true);
                rows.sort_by(|a, b| field(a, "created_at").cmp(field(b, "created_at")));
                json!(rows)
            }
            "competition.projections"
            | "competition.projection.status"
            | "competition.projection.watermarks" => {
                json!({
                    "names": projections::names(state)?,
                    "watermarks": projections::watermarks(state)?,
                    "lagging": projections::lagging(state)?,
                })
            }
            _ => {
                if id.is_empty() {
                    return Err(ApiError::new(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "competition.id_required",
                        format!("{kind} requires params.competition_id"),
                    ));
                }
                let competition = state
                    .store
                    .get(COMPETITIONS, id)
                    .map_err(ApiError::internal)?
                    .ok_or_else(|| {
                        ApiError::new(
                            StatusCode::NOT_FOUND,
                            "competition.competition.not_found",
                            format!("unknown competition {id:?}"),
                        )
                    })?;
                if competition["archived"] == true {
                    return Err(ApiError::new(
                        StatusCode::NOT_FOUND,
                        "competition.competition.archived",
                        format!("competition {id:?} was removed from the inventory"),
                    ));
                }
                match kind {
                    "competition.snapshot" => live_snapshot(state, id)
                        .map_err(ApiError::internal)?
                        .ok_or_else(|| ApiError::missing("competition"))?,
                    "competition.challenges" => {
                        let snapshot = live_snapshot(state, id)
                            .map_err(ApiError::internal)?
                            .ok_or_else(|| ApiError::missing("competition"))?;
                        json!(snapshot["challenges"].as_array().into_iter().flatten().map(|row|json!({
                            "challenge":row["challenge"],"current_revision":row["current_revision"],
                            "active_binding":row["active_binding"],"run_id":row["run_id"],
                            "active_lease":row["active_lease"],"queue_entry":row["queue_entry"]
                        })).collect::<Vec<_>>())
                    }
                    "competition.submissions" => {
                        let mut submissions = ordered_scoped(state, SUBMISSIONS, id)?;
                        let mut candidates = ordered_scoped(state, CANDIDATES, id)?;
                        submissions.sort_by(|a, b| {
                            field(a, "created_at")
                                .cmp(field(b, "created_at"))
                                .then_with(|| {
                                    field(a, "submission_id").cmp(field(b, "submission_id"))
                                })
                        });
                        candidates.sort_by(|a, b| {
                            field(a, "created_at")
                                .cmp(field(b, "created_at"))
                                .then_with(|| {
                                    field(a, "candidate_id").cmp(field(b, "candidate_id"))
                                })
                        });
                        let submissions = submissions
                            .into_iter()
                            .map(safe_submission)
                            .collect::<Vec<_>>();
                        let candidates = candidates
                            .into_iter()
                            .map(safe_candidate)
                            .collect::<Vec<_>>();
                        json!({"competition_id":id,"submissions":submissions,"candidates":candidates})
                    }
                    "competition.leases" => {
                        let mut leases = ordered_scoped(state, LEASES, id)?;
                        leases.sort_by(|a, b| {
                            field(a, "created_at")
                                .cmp(field(b, "created_at"))
                                .then_with(|| field(a, "lease_id").cmp(field(b, "lease_id")))
                        });
                        json!(leases.into_iter().map(query_lease_view).collect::<Vec<_>>())
                    }
                    _ => return Err(ApiError::invalid("Unknown competition query")),
                }
            }
        };
        Ok(
            json!({"result":{"query_id":query_id,"query_type":kind,"result":projection,"cursor":null}}),
        )
    })();
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.into_response(),
    }
}
