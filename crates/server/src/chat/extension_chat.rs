use super::*;
use base64::Engine;
mod event_contract;

#[derive(Clone)]
pub(crate) struct Plan {
    pub adapter_id: String,
    pub extension_version: String,
    pub instance_id: String,
    pub model: String,
    pub effort: String,
    pub access_mode: String,
    pub interaction_mode: String,
    pub plan_mode_per_turn: bool,
}

pub(super) enum Progress {
    Completed(Option<Value>),
    Waiting,
    Failed(Value),
}

fn ready_entry(state: &AppState, adapter_id: &str) -> Option<Value> {
    crate::extensions::ready_capabilities(state, "external-agent-adapter")
        .ok()
        .and_then(|rows| rows.into_iter().find(|entry| entry["id"] == adapter_id))
}

pub(super) fn registered(state: &AppState, adapter_id: &str) -> bool {
    ready_entry(state, adapter_id).is_some()
}

pub(super) fn plan(state: &AppState, runtime: &Value) -> Result<Option<Plan>, Failure> {
    let adapter_id = runtime["adapter_id"].as_str().unwrap_or("").trim();
    let Some(entry) = ready_entry(state, adapter_id) else {
        return Ok(None);
    };
    let instance_id = runtime["instance_id"].as_str().unwrap_or("default");
    if instance_id != "default" {
        return Err(fail_code(
            StatusCode::NOT_FOUND,
            "runtime.instance.not_found",
            "Extension adapter has no such instance",
        ));
    }
    let interaction_mode = runtime["interaction_mode"].as_str().unwrap_or("default").trim();
    let interaction_mode = if interaction_mode.is_empty() { "default" } else { interaction_mode };
    if !matches!(interaction_mode, "default" | "plan") {
        return Err(fail_code(
            StatusCode::UNPROCESSABLE_ENTITY,
            "conversation.interaction_mode_invalid",
            "interaction_mode must be default or plan",
        ));
    }
    let key = format!("{adapter_id}:{instance_id}");
    let health = state.store.get(crate::runtime_api::HEALTH, &key)
        .ok().flatten().unwrap_or(Value::Null);
    let capabilities = &health["capabilities"];
    if interaction_mode == "plan" && (capabilities["plan_mode"] != true
        || health["extension_version"] != entry["extension_version"]) {
        return Err(fail_code(
            StatusCode::UNPROCESSABLE_ENTITY,
            "conversation.interaction_mode_unsupported",
            "This extension adapter has not reported plan_mode support",
        ));
    }
    Ok(Some(Plan {
        adapter_id: adapter_id.to_owned(),
        extension_version: entry["extension_version"].as_str().unwrap_or("").to_owned(),
        instance_id: instance_id.to_owned(),
        model: runtime["model"].as_str().unwrap_or("").to_owned(),
        effort: runtime["effort"]
            .as_str()
            .or_else(|| runtime["reasoning_effort"].as_str())
            .unwrap_or("")
            .to_owned(),
        access_mode: runtime["access_mode"].as_str().unwrap_or("").to_owned(),
        interaction_mode: interaction_mode.to_owned(),
        plan_mode_per_turn: capabilities["plan_mode_per_turn"] != false,
    }))
}

async fn call(
    s: &Arc<AppState>,
    plan: &Plan,
    action: &str,
    payload: Value,
) -> Result<Value, String> {
    let response = crate::extensions::invoke_capability(
        s,
        "external-agent-adapter",
        &plan.adapter_id,
        action,
        payload,
    )
    .await?;
    if response["error"].is_object() {
        return Err(response["error"]["code"]
            .as_str()
            .unwrap_or("extension.error")
            .to_owned());
    }
    Ok(response)
}

async fn injection(
    s: &Arc<AppState>,
    tid: &str,
    plan: &Plan,
    agent_session_id: &str,
) -> Result<(Value, Option<String>), String> {
    if !crate::capability_gateway::enabled(s) {
        return Ok((Value::Null, None));
    }
    let probe = call(
        s,
        plan,
        "probe",
        json!({"request":{"schema_version":1,"runtime_instance_id":plan.instance_id,
        "include_models":false,"options":{}}}),
    )
    .await?;
    let caps = probe.get("capabilities").unwrap_or(&probe);
    let kind = if caps["mcp"] == true {
        "mcp"
    } else if caps["acp_mcp_config"] == true {
        "acp_mcp_config"
    } else {
        return Ok((Value::Null, None));
    };
    let mode = crate::capabilities::thread_mode(&s.store, tid);
    let enabled_tools = crate::capabilities::effective_tool_set(&s.store, tid);
    let mut tools: Vec<Value> = crate::capabilities::catalog()
        .into_iter()
        .filter(|tool| {
            let name = tool["name"].as_str().unwrap_or("");
            if !enabled_tools.iter().any(|allowed| allowed == name) {
                return false;
            }
            if mode == "src" {
                crate::capabilities::is_collab_tool(name)
                    || crate::capability_gateway::is_src_platform_core_tool(name)
                    || crate::browser::handles(name)
                    || crate::chat_computer::handles(name)
            } else {
                true
            }
        })
        .map(|tool| {
            json!({"schema_version":1,"name":tool["name"],"description":tool["description"],
            "input_schema":tool["input_schema"]})
        })
        .collect();
    if mode == "src" {
        if enabled_tools
            .iter()
            .any(|name| crate::capabilities::is_platform_tool(name))
        {
            for helper in [
                crate::capability_gateway::platform_discover_tool(),
                crate::capability_gateway::platform_call_tool(),
            ] {
                tools.push(json!({
                    "schema_version": 1,
                    "name": helper["name"],
                    "description": helper["description"],
                    "input_schema": helper["inputSchema"],
                }));
            }
        }
        if enabled_tools
            .iter()
            .any(|name| crate::capabilities::is_collab_tool(name))
        {
            for helper in [
                crate::capability_gateway::collab_discover_tool(),
                crate::capability_gateway::collab_call_tool(),
            ] {
                tools.push(json!({
                    "schema_version": 1,
                    "name": helper["name"],
                    "description": helper["description"],
                    "input_schema": helper["inputSchema"],
                }));
            }
        }
    }
    if tools.is_empty() {
        return Ok((Value::Null, None));
    }
    let port = s
        .store
        .get("system", "port")
        .map_err(|_| "Control gateway port unavailable".to_owned())?
        .and_then(|row| row.as_u64())
        .ok_or("Control gateway port unavailable")?;
    let token = s
        .capability_gateway
        .issue(tid, &plan.adapter_id)
        .ok_or("Control gateway token unavailable")?;
    let session = json!({"agent_session_id":agent_session_id,"instance_id":plan.instance_id,"adapter_id":plan.adapter_id});
    let grant = match crate::capabilities::issue_grant(
        s,
        tid,
        &session,
        &crate::capability_gateway::digest(&token),
    ) {
        Ok(grant) => grant,
        Err(_) => {
            s.capability_gateway.release(tid);
            return Err("Could not issue extension capability grant".to_owned());
        }
    };
    let endpoint = format!("http://127.0.0.1:{port}/api/capability");
    let placeholder = "${MUTEKI_CAPABILITY_BEARER_TOKEN}";
    let runtime_config = if kind == "mcp" {
        json!({"mcpServers":{"muteki-control":{"type":"http","url":endpoint,
            "headers":{"Authorization":format!("Bearer {placeholder}")}}}})
    } else {
        json!({"mcpServers":[{"type":"http","name":"muteki-control","url":endpoint,
            "headers":[{"name":"Authorization","value":format!("Bearer {placeholder}")}] }],
            "session_methods":{"new":"session/new","load":"session/load","resume":"session/resume"},
            "requires_capability":"mcpCapabilities.http"})
    };
    let injection_plan = json!({"schema_version":1,"injection_kind":kind,"gateway_endpoint":endpoint,
        "tool_descriptions":tools,"credential_ref":grant["credential_reference"],"audience":grant["audience"],
        "runtime_config":runtime_config,"binding_id":grant["binding_id"],"grant_id":grant["grant_id"]});
    Ok((injection_plan, Some(token)))
}

fn release(s: &AppState, tid: &str, agent_session_id: &str) {
    if let Err(error) = crate::capabilities::revoke_session_grants(s, agent_session_id) {
        tracing::warn!(?error, "Could not revoke extension session grants");
    }
    s.capability_gateway.release(tid);
}

pub(super) fn close(s: &Arc<AppState>, tid: &str, session: &Value) -> crate::native_chat::Post {
    let (state, thread, reference) = (s.clone(), tid.to_owned(), session.clone());
    Box::pin(async move {
        let adapter_id = reference["adapter_id"].as_str().unwrap_or("");
        if !adapter_id.is_empty() {
            if let Err(error) = crate::extensions::invoke_capability(
                &state,
                "external-agent-adapter",
                adapter_id,
                "close",
                json!({"session":reference}),
            )
            .await
            {
                tracing::warn!(adapter_id,%error,"Could not close extension agent session");
            }
        }
        release(
            &state,
            &thread,
            reference["agent_session_id"].as_str().unwrap_or(""),
        );
        let _guard = state.chat.active.lock().await;
        if let Ok(mut document) = load(&state, &thread) {
            if document["extension_session"]["reference"]["agent_session_id"]
                == reference["agent_session_id"]
            {
                document["thread"]["state"]["agent_session_id"] = Value::Null;
                emit(
                    &state,
                    &thread,
                    &mut document,
                    "core.session.closed",
                    json!({"agent_session_id":reference["agent_session_id"],
                    "external_session_id":reference["external_session_id"],"reason":"thread_closed"}),
                );
                let _ = save(&state, &thread, &mut document);
            }
        }
    })
}

fn attachment_payloads(state: &AppState, document: &Value, references: &Value) -> Result<Vec<Value>, String> {
    if references.is_null() {
        return Ok(Vec::new());
    }
    references.as_array().ok_or("attachments must be an array")?.iter().map(|reference| {
        let reference = reference.as_str().ok_or("attachments must contain uploaded artifact references")?;
        let file = crate::files::read_attachment(state, document, reference)?;
        Ok(attachment_payload(file))
    }).collect()
}

fn attachment_payload((meta, bytes): (Value, Vec<u8>)) -> Value {
    let media = meta["media_type"].as_str().unwrap_or("application/octet-stream");
    json!({"schema_version":1,"sha256":meta["sha256"],"name":meta["name"],
        "media_type":media,"size":bytes.len(),"cas_path":"","path":"","workspace_path":"",
        "delivery":if media.starts_with("image/"){"native_image"}else{"workspace_file"},
        "content_base64":base64::engine::general_purpose::STANDARD.encode(bytes)})
}

pub(super) async fn control(s: &Arc<AppState>, tid: &str, body: &Value, command_key: &str) -> Api {
    crate::capabilities::renew_session_grant(s,tid)?;
    let kind = body["command_type"].as_str().unwrap_or("");
    if kind == "conversation.queue.steer" {
        return Err(fail_code(StatusCode::UNPROCESSABLE_ENTITY,
            "conversation.queue.steer_unsupported", "扩展 Agent 暂不支持将队列消息转为引导，消息仍在队列中。"));
    }
    let (session, adapter_id, turn_id, steer_attachments) = {
        let mut active = s.chat.active.lock().await;
        let mut document = load(s, tid)?;
        let session = document["extension_session"]["reference"].clone();
        let adapter_id = document["extension_session"]["adapter_id"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        if session["agent_session_id"].as_str().is_none() || !registered(s, &adapter_id) {
            return Err(fail_code(
                StatusCode::CONFLICT,
                "conversation.session.unavailable",
                "Extension session is unavailable",
            ));
        }
        if kind == "conversation.turn.interrupt" {
            if let Some(task) = active.remove(tid) {
                task.abort();
            }
            if document["thread"]["state"]["status"] == "running" {
                let turn = document["thread"]["state"]["running_turn_id"].clone();
                finish_value(&mut document, &turn, "cancelled", None);
                document["thread"]["state"]["pending_approval"] = Value::Null;
                document["thread"]["state"]["pending_user_input"] = Value::Null;
                if let Some(rows) = document["thread"]["state"]["pending_approvals"].as_object_mut()
                {
                    for row in rows.values_mut().filter(|row| row["status"] == "pending") {
                        row["status"] = json!("expired");
                        row["expired_reason"] = json!("turn_interrupted");
                    }
                }
                if queue_len(&document) > 0 {
                    pause_queue(&mut document, "turn_interrupted");
                }
                emit(
                    s,
                    tid,
                    &mut document,
                    "core.turn.interrupted",
                    json!({"turn_id":turn,"status":"cancelled"}),
                );
                save(s, tid, &mut document)?;
            }
        }
        let steer_attachments = if kind == "conversation.turn.steer" {
            attachment_payloads(s, &document, &body["payload"]["attachments"]).map_err(|error|
                fail_code(StatusCode::BAD_REQUEST, "conversation.attachment_invalid", &error))?
        } else {
            Vec::new()
        };
        (
            session,
            adapter_id,
            document["thread"]["state"]["running_turn_id"]
                .as_str()
                .unwrap_or("")
                .to_owned(),
            steer_attachments,
        )
    };
    let action = if kind == "conversation.turn.interrupt" {
        "interrupt"
    } else if matches!(
        kind,
        "conversation.approval.resolve" | "conversation.user_input.resolve"
    ) {
        "send"
    } else {
        "steer"
    };
    let payload = if action == "interrupt" {
        json!({"session":session})
    } else if kind == "conversation.approval.resolve" {
        json!({"session":session,"input":{"schema_version":1,"kind":"approval_response","text":"",
            "payload":{"schema_version":1,"approval_id":body["payload"]["approval_id"],
                "decision":body["payload"]["decision"],"scope":body["payload"]["scope"].as_str().unwrap_or("once"),
                "note":body["payload"]["note"].as_str().unwrap_or(""),"option_id":body["payload"]["option_id"].as_str().unwrap_or(""),
                "updated_input":body["payload"]["updated_input"]}}})
    } else if kind == "conversation.user_input.resolve" {
        json!({"session":session,"input":{"schema_version":1,"kind":"user_input_response",
            "text":body["payload"]["text"].as_str().unwrap_or(""),"payload":{"schema_version":1,
                "request_id":body["payload"]["request_id"],"decision":body["payload"]["decision"].as_str().unwrap_or("submit"),
                "answers":body["payload"]["answers"].as_object().cloned().unwrap_or_default()}}})
    } else {
        json!({"session":session,"input":{"schema_version":1,"kind":"steer",
            "text":body["payload"]["text"].as_str().unwrap_or(""),"payload":{"schema_version":1,
                "expected_turn_id":body["payload"]["expected_turn_id"].as_str().unwrap_or(""),
                "attachments":steer_attachments,
                "client_user_message_id":body["payload"]["client_user_message_id"].as_str()
                    .or_else(|| body["payload"]["client_message_id"].as_str()).unwrap_or("")}}})
    };
    let result = crate::extensions::invoke_capability(
        s,
        "external-agent-adapter",
        &adapter_id,
        action,
        payload,
    )
    .await
    .map_err(|error| fail_code(StatusCode::BAD_GATEWAY, "extension.command_failed", &error))?;
    if result["error"].is_object() {
        return Err(fail_code(
            StatusCode::BAD_GATEWAY,
            "extension.command_failed",
            result["error"]["code"]
                .as_str()
                .unwrap_or("Extension command failed"),
        ));
    }
    if action == "send" {
        let events = result["events"].as_array().ok_or_else(|| {
            fail_code(
                StatusCode::BAD_GATEWAY,
                "extension.events_invalid",
                "Extension response must contain events[]",
            )
        })?;
        let mut completed = false;
        let mut failed = false;
        let mut stop_reason: Option<String> = None;
        let mut usage = None;
        let fallback_message_id = id();
        for raw in events {
            let event = event_contract::validate(raw).map_err(|error| {
                fail_code(
                    StatusCode::BAD_GATEWAY,
                    "external_agent.contract.invalid_payload",
                    &error,
                )
            })?;
            match event["event_type"].as_str().unwrap_or("") {
                "message.delta" | "message.completed" if event["payload"]["role"] != "user" => {
                    message_event(s, tid, &turn_id, &event, &fallback_message_id)
                        .await
                        .map_err(|error| {
                            fail_code(
                                StatusCode::CONFLICT,
                                "conversation.turn.interrupted",
                                &error,
                            )
                        })?
                }
                "turn.completed" => {
                    completed = true;
                    stop_reason = event["payload"]["stop_reason"]
                        .as_str()
                        .or_else(|| event["payload"]["finish_reason"].as_str())
                        .map(ToOwned::to_owned);
                }
                "usage.updated" => {
                    usage = turn_usage(&event["payload"]).or(usage);
                    record_event(s, tid, &turn_id, &session, &event)
                        .await
                        .map_err(|error| {
                            fail_code(StatusCode::BAD_GATEWAY, "extension.events_invalid", &error)
                        })?;
                }
                "turn.failed" => {
                    failed = true;
                    let _guard = s.chat.active.lock().await;
                    let mut document = load(s, tid)?;
                    finish_value(
                        &mut document,
                        &json!(turn_id),
                        "failed",
                        event["payload"]["error"]["message"].as_str(),
                    );
                    emit(
                        s,
                        tid,
                        &mut document,
                        "core.turn.failed",
                        json!({"turn_id":turn_id,"error":event["payload"]["error"]}),
                    );
                    if queue_len(&document) > 0 {
                        pause_queue(&mut document, "turn_failed");
                    }
                    save(s, tid, &mut document)?;
                }
                _ => record_event(s, tid, &turn_id, &session, &event)
                    .await
                    .map_err(|error| {
                        fail_code(StatusCode::BAD_GATEWAY, "extension.events_invalid", &error)
                    })?,
            }
        }
        if completed && !failed {
            let _guard = s.chat.active.lock().await;
            let mut document = load(s, tid)?;
            completed_messages(s, tid, &mut document, &turn_id);
            finish_value(&mut document, &json!(turn_id), "completed", None);
            if let Some(usage) = usage {
                store_turn_usage(s, &mut document, &turn_id, &adapter_id, &usage);
            }
            emit(
                s,
                tid,
                &mut document,
                "core.turn.completed",
                json!({"turn_id":turn_id}),
            );
            let next = promote_queue(s, tid, &mut document);
            save(s, tid, &mut document)?;
            let cwd_after = workspace_root(s, &document);
            drop(_guard);
            if crate::capabilities::thread_mode(&s.store, tid) == "src" {
                crate::runs::src_api::sync_src_session_files_after_turn(
                    s,
                    tid,
                    std::path::Path::new(&cwd_after),
                );
            }
            if matches!(stop_reason.as_deref(), Some("refusal" | "content_filter")) {
                if let Ok(task_row) = crate::runs::src_api::task_for_thread(s, tid) {
                    if let Some(src_task_id) = task_row["task_id"].as_str() {
                        let reason_str = stop_reason.as_deref().unwrap_or("refusal");
                        if let Ok((should_clarify, clarify_opt)) =
                            crate::runs::src_api::guardrail_refusal_clarify(
                                s,
                                src_task_id,
                                tid,
                                reason_str,
                                &adapter_id,
                            )
                        {
                            if should_clarify {
                                if let Some(clarify_text) = clarify_opt {
                                    let _guard2 = s.chat.active.lock().await;
                                    if let Ok(mut doc2) = load(s, tid) {
                                        emit(
                                            s,
                                            tid,
                                            &mut doc2,
                                            "core.audit.refusal_clarification",
                                            json!({"turn_id":turn_id,"stop_reason":reason_str,"attempt":1}),
                                        );
                                        let _ = save(s, tid, &mut doc2);
                                    }
                                    drop(_guard2);
                                    let clarify_cmd_id = format!("clarify-{turn_id}");
                                    let send_body = json!({
                                        "command_id": clarify_cmd_id,
                                        "idempotency_key": clarify_cmd_id,
                                        "command_type": "conversation.turn.send",
                                        "actor": {"kind":"system","id":"src.guardrails"},
                                        "payload": {"text": clarify_text, "client_message_id": clarify_cmd_id}
                                    });
                                    let _ = Box::pin(super::command_from_platform(
                                        s.clone(),
                                        tid.to_owned(),
                                        send_body,
                                        None,
                                    ))
                                    .await;
                                    return Ok(receipt(
                                        body,
                                        tid,
                                        result.get("receipt").cloned().unwrap_or(result),
                                    ));
                                }
                            } else {
                                let _guard2 = s.chat.active.lock().await;
                                if let Ok(mut doc2) = load(s, tid) {
                                    emit(
                                        s,
                                        tid,
                                        &mut doc2,
                                        "core.audit.refusal_halted",
                                        json!({"turn_id":turn_id,"stop_reason":reason_str,"halted_for_human_switch":true}),
                                    );
                                    let _ = save(s, tid, &mut doc2);
                                }
                            }
                        }
                    }
                }
            }
            if let Some((next_turn, dispatch)) = next {
                super::start(s.clone(), tid.to_owned(), next_turn, dispatch).await;
            }
        }
    }
    let output = result.get("receipt").cloned().unwrap_or(result);
    let response = receipt(body, tid, output);
    s.store
        .put(
            "conversation_commands",
            command_key,
            &json!({"request":body,"response":response.0}),
        )
        .map_err(db)?;
    Ok(response)
}

fn turn_usage(payload: &Value) -> Option<Value> {
    if !matches!(payload["scope"].as_str(), Some("turn" | "invocation")) {
        return None;
    }
    Some(
        json!({"prompt_tokens":payload["input_tokens"].as_u64().unwrap_or(0),
        "completion_tokens":payload["output_tokens"].as_u64().unwrap_or(0),
        "total_tokens":payload["total_tokens"]}),
    )
}

fn store_turn_usage(
    s: &AppState,
    document: &mut Value,
    turn: &str,
    adapter_id: &str,
    usage: &Value,
) {
    if let Some(row) = document["turns"]
        .as_array_mut()
        .and_then(|rows| rows.iter_mut().find(|row| row["turn_id"] == turn))
    {
        row["usage"] = usage.clone();
    }
    document["thread"]["state"]["usage"] = usage.clone();
    let record = json!({"id":turn,"at":chrono::Utc::now().timestamp_millis() as f64 / 1000.0,
        "engine":adapter_id,"role":"assistant","workspace_kind":"conversation","run_id":document["thread"]["thread_id"],
        "thread_id":document["thread"]["thread_id"],"turn_id":turn,"credential_id":"",
        "model":document["runtime"]["model"],"provider":adapter_id,"created_at":now(),
        "input_tokens":usage["prompt_tokens"],"output_tokens":usage["completion_tokens"],
        "total_tokens":usage["total_tokens"],"usage":usage,"token_coverage":"reported"});
    if s.store.put("usage", turn, &record).is_err() {
        document["thread"]["state"]["last_error"] =
            json!({"message":"Response completed, but usage could not be saved"});
    }
}

async fn message_event(
    s: &Arc<AppState>,
    tid: &str,
    turn: &str,
    event: &Value,
    fallback_message_id: &str,
) -> Result<(), String> {
    let payload = &event["payload"];
    let source_message_id = payload["message_id"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback_message_id)
        .to_owned();
    let message_id = format!("{turn}:{source_message_id}");
    let text = payload["text"].as_str().unwrap_or("");
    let is_delta = event["event_type"] == "message.delta";
    let replacing = payload["replace"] == true || !is_delta;
    let _guard = s.chat.active.lock().await;
    let mut document = load(s, tid).map_err(|_| "Conversation unavailable".to_owned())?;
    if document["thread"]["state"]["running_turn_id"] != turn {
        return Err("Turn interrupted".to_owned());
    }
    let existing = document["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|row| row["message_id"] == message_id)
        .and_then(|row| row["text"].as_str())
        .map(str::to_owned);
    let changed = if is_delta {
        !text.is_empty() || payload["replace"] == true
    } else {
        existing.as_deref() != Some(text)
    };
    if changed {
        let next = if replacing {
            text.to_owned()
        } else {
            format!("{}{text}", existing.as_deref().unwrap_or(""))
        };
        let seq = stream_head(&document) + 1;
        let rows = document["messages"]
            .as_array_mut()
            .ok_or("Messages unavailable")?;
        if let Some(row) = rows.iter_mut().find(|row| row["message_id"] == message_id) {
            row["text"] = json!(next);
            if payload["phase"].is_string() {
                row["phase"] = payload["phase"].clone();
            }
        } else {
            rows.push(
                json!({"message_id":message_id,"turn_id":turn,"role":"assistant","text":next,
                "stream_seq":seq,"status":"streaming","phase":payload["phase"],"created_at":now()}),
            );
        }
        emit(
            s,
            tid,
            &mut document,
            "core.message.delta",
            json!({"role":"assistant","text":text,"message_id":message_id,"turn_id":turn,
                "phase":payload["phase"],"replace":replacing && existing.is_some()}),
        );
    }
    if !is_delta {
        if let Some(row) = document["messages"]
            .as_array_mut()
            .and_then(|rows| rows.iter_mut().find(|row| row["message_id"] == message_id))
        {
            row["status"] = json!("completed");
        }
        emit(
            s,
            tid,
            &mut document,
            "core.message.completed",
            json!({"role":"assistant","text":text,"message_id":message_id,"turn_id":turn,"phase":payload["phase"]}),
        );
    }
    save(s, tid, &mut document).map_err(|_| "Could not save extension message".to_owned())
}

fn completed_messages(s: &AppState, tid: &str, document: &mut Value, turn: &str) {
    let rows: Vec<Value> = document["messages"].as_array().into_iter().flatten()
        .filter(|row| row["turn_id"] == turn && row["role"] == "assistant" && row["status"] == "streaming")
        .map(|row| json!({"turn_id":turn,"role":"assistant","message_id":row["message_id"],"text":row["text"]}))
        .collect();
    for row in rows {
        emit(s, tid, document, "core.message.completed", row);
    }
}

async fn record_event(
    s: &Arc<AppState>,
    tid: &str,
    turn: &str,
    session: &Value,
    event: &Value,
) -> Result<(), String> {
    let source = event["event_type"].as_str().unwrap_or("");
    let target = match source {
        "reasoning.summary" => "core.reasoning.summary",
        "tool.started" => "core.tool.started",
        "tool.progress" => "core.tool.progress",
        "tool.completed" => "core.tool.completed",
        "approval.requested" => "core.approval.requested",
        "approval.resolved" => "core.approval.resolved",
        "user_input.requested" => "core.user_input.requested",
        "user_input.resolved" => "core.user_input.resolved",
        "artifact.created" => "core.artifact.created",
        "workspace.changed" => "core.workspace.changed",
        "usage.updated" => "core.usage.updated",
        "compaction.updated" => "core.context.compaction",
        "plan.updated" => "core.plan.updated",
        "agent.updated" => "core.agent.updated",
        "runtime.capabilities_updated" => "core.runtime.capabilities_updated",
        "runtime.warning" => "core.runtime.warning",
        "runtime.error" => "core.runtime.error",
        "runtime.exited" => "core.runtime.exited",
        "session.closed" => "core.session.closed",
        _ => return Ok(()),
    };
    let _guard = s.chat.active.lock().await;
    let mut current = load(s, tid).map_err(|_| "Conversation unavailable".to_owned())?;
    if current["thread"]["state"]["running_turn_id"] != turn {
        return Err("Turn interrupted".to_owned());
    }
    let mut payload = event["payload"].clone();
    if !payload.is_object() {
        payload = json!({});
    }
    payload["turn_id"] = json!(turn);
    payload["agent_session_id"] = session["agent_session_id"].clone();
    if let Some(native) = event["native_type"].as_str() {
        payload["native_type"] = json!(native);
    }
    if let Some(seq) = event["seq"].as_u64() {
        payload["native_seq"] = json!(seq);
    }
    if source == "reasoning.summary" && payload["reasoning_summary"].is_null() {
        payload["reasoning_summary"] = payload["text"].clone();
    }
    if source.starts_with("tool.") {
        let call_id = payload["tool_call_id"]
            .as_str()
            .or_else(|| payload["call_id"].as_str())
            .unwrap_or("")
            .to_owned();
        let name = payload["name"]
            .as_str()
            .or_else(|| payload["tool"].as_str())
            .unwrap_or("")
            .to_owned();
        payload["call_id"] = json!(call_id);
        payload["tool"] = json!(name);
        if source == "tool.completed" {
            let saved = crate::tool_resources::tool_payload(&mut payload).save(s)?;
            if let Some(warning) = saved.warning(&json!(turn), &call_id) {
                emit(s, tid, &mut current, "core.runtime.warning", warning);
            }
            let crate::tool_resources::SavedFiles { mut resources, artifacts, .. } = saved;
            for artifact in current["artifacts"].as_array().into_iter().flatten().filter(|artifact|
                artifact["tool_call_id"] == call_id && artifact["turn_id"] == turn)
            {
                resources.as_array_mut().unwrap().push(json!({
                    "kind":"artifact","artifact_id":artifact["artifact_id"],"name":artifact["name"],"media_type":artifact["media_type"]
                }));
            }
            if resources.as_array().is_some_and(|rows| !rows.is_empty()) {
                payload["resources"] = resources;
            }
            crate::tool_resources::append_artifacts(&mut current, &json!(turn), artifacts);
        }
        if !current["tools"].is_array() {
            current["tools"] = json!([]);
        }
        if !call_id.is_empty() {
            let status = if source == "tool.completed" {
                "completed"
            } else {
                "running"
            };
            let output = payload["output"]
                .as_str()
                .or_else(|| payload["text"].as_str())
                .unwrap_or("");
            let seq = stream_head(&current) + 1;
            let rows = current["tools"].as_array_mut().unwrap();
            if let Some(row) = rows
                .iter_mut()
                .find(|row| row["tool_id"] == call_id && row["turn_id"] == turn)
            {
                row["status"] = json!(status);
                row["output_complete"] = json!(source == "tool.completed");
                if !output.is_empty() {
                    row["output_summary"] = json!(output);
                }
            } else {
                rows.push(json!({"tool_id":call_id,"turn_id":turn,"name":name,"status":status,
                    "output_summary":output,"output_complete":source == "tool.completed","stream_seq":seq}));
            }
        }
    }
    if source == "approval.requested" {
        payload["status"] = json!("pending");
        payload["requested_at"] = json!(now());
        payload["response_capability"] =
            json!({"answerable":true,"agent_session_id":session["agent_session_id"]});
        let approval_id = payload["approval_id"].as_str().unwrap_or("").to_owned();
        if !approval_id.is_empty() {
            if !current["thread"]["state"]["pending_approvals"].is_object() {
                current["thread"]["state"]["pending_approvals"] = json!({});
            }
            current["thread"]["state"]["pending_approvals"][&approval_id] = payload.clone();
        }
        current["thread"]["state"]["pending_approval"] = payload.clone();
    }
    if source == "approval.resolved" {
        let approval_id = payload["approval_id"].as_str().unwrap_or("").to_owned();
        if !approval_id.is_empty()
            && current["thread"]["state"]["pending_approvals"][&approval_id].is_object()
        {
            current["thread"]["state"]["pending_approvals"][&approval_id]["status"] =
                json!("resolved");
        }
        current["thread"]["state"]["pending_approval"] = Value::Null;
    }
    if source == "user_input.requested" {
        payload["status"] = json!("pending");
        payload["requested_at"] = json!(now());
        payload["response_capability"] =
            json!({"answerable":true,"agent_session_id":session["agent_session_id"]});
        current["thread"]["state"]["pending_user_input"] = payload.clone();
    }
    if source == "user_input.resolved" {
        current["thread"]["state"]["pending_user_input"] = Value::Null;
    }
    if source == "session.closed" {
        current["extension_session"]["open"] = json!(false);
        current["thread"]["state"]["agent_session_id"] = Value::Null;
        release(s, tid, session["agent_session_id"].as_str().unwrap_or(""));
    }
    if source == "artifact.created" {
        let bytes = if let Some(encoded) = payload["content_base64"].as_str() {
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| "Extension artifact content is invalid base64".to_owned())?
        } else if let Some(path) = payload["path"].as_str() {
            let expanded = if let Some(suffix) = path.strip_prefix("~/") {
                std::env::var_os("HOME")
                    .map(|home| std::path::PathBuf::from(home).join(suffix))
                    .unwrap_or_else(|| std::path::PathBuf::from(path))
            } else {
                std::path::PathBuf::from(path)
            };
            std::fs::read(expanded)
                .map_err(|error| format!("Could not read extension artifact: {error}"))?
        } else {
            return Err("Extension artifact has no content".to_owned());
        };
        let mut meta = crate::files::store_agent_artifact(
            s,
            payload["name"].as_str().unwrap_or("artifact"),
            payload["kind"].as_str().unwrap_or("agent"),
            payload["media_type"]
                .as_str()
                .unwrap_or("application/octet-stream"),
            &bytes,
        )?;
        meta["turn_id"] = json!(turn);
        if payload["tool_call_id"].is_string() {
            meta["tool_call_id"] = payload["tool_call_id"].clone();
        }
        if !current["artifacts"].is_array() {
            current["artifacts"] = json!([]);
        }
        current["artifacts"]
            .as_array_mut()
            .unwrap()
            .push(meta.clone());
        payload["artifact_id"] = meta["artifact_id"].clone();
        payload["size"] = meta["size"].clone();
        payload.as_object_mut().unwrap().remove("content_base64");
    }
    if source == "workspace.changed" {
        let diff = payload["unified_diff"]
            .as_str()
            .or_else(|| payload["diff"].as_str())
            .unwrap_or("");
        if !diff.is_empty() {
            let meta = crate::files::store_agent_artifact(
                s,
                &format!("{turn}.diff"),
                "conversation.diff",
                "text/x-diff",
                diff.as_bytes(),
            )?;
            if !current["artifacts"].is_array() {
                current["artifacts"] = json!([]);
            }
            if !current["artifacts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["artifact_id"] == meta["artifact_id"])
            {
                current["artifacts"]
                    .as_array_mut()
                    .unwrap()
                    .push(meta.clone());
                emit(
                    s,
                    tid,
                    &mut current,
                    "core.artifact.created",
                    json!({"turn_id":turn,"agent_session_id":session["agent_session_id"],
                    "artifact_id":meta["artifact_id"],"name":meta["name"],"kind":meta["kind"],
                    "media_type":meta["media_type"],"size":meta["size"]}),
                );
            }
            payload["diff"] = json!(diff);
            payload["has_diff"] = json!(true);
        }
    }
    emit(s, tid, &mut current, target, payload);
    save(s, tid, &mut current).map_err(|_| "Could not save extension event".to_owned())
}

pub(super) async fn provider(
    s: &Arc<AppState>,
    tid: &str,
    turn: &str,
    plan: &Plan,
) -> Result<Progress, String> {
    crate::chat::wait_for_human_burst_settle(s, tid, turn).await;
    let document = {
        let _guard = s.chat.active.lock().await;
        let mut doc = load(s, tid).map_err(|_| "Conversation unavailable".to_owned())?;
        if crate::chat::absorb_queued_human_burst(
            s,
            tid,
            &mut doc,
            turn,
            &crate::chat::Dispatch::Extension(plan.clone()),
        ) > 0
        {
            save(s, tid, &mut doc).map_err(|_| "Could not save coalesced turn".to_owned())?;
        }
        doc
    };
    if document["thread"]["state"]["running_turn_id"] != turn {
        return Err("Turn interrupted".to_owned());
    }
    if document["thread"]["side_chat"]["tools_disabled"] == true {
        return Err("runtime.tools_disabled_unsupported: extension adapter cannot enforce a zero-tool session".to_owned());
    }
    let turn_row = document["turns"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|row| row["turn_id"] == turn);
    let user_message = document["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .rev()
        .find(|message| message["role"] == "user" && message["turn_id"] == turn)
        .ok_or("Turn has no user message")?;
    let mut text = turn_row
        .and_then(|row| row["text"].as_str())
        .or_else(|| user_message["text"].as_str())
        .unwrap_or("")
        .to_owned();
    let attachment_refs = turn_row
        .map(|row| &row["attachments"])
        .filter(|arr| arr.is_array())
        .unwrap_or(&user_message["attachments"]);
    let mut attachments = attachment_payloads(s, &document, attachment_refs)?;
    let prior = &document["extension_session"];
    let previous_mode = prior["interaction_mode"].as_str().unwrap_or("default");
    let version_change = prior["adapter_id"] == plan.adapter_id
        && prior["extension_version"] != plan.extension_version;
    let mode_change = prior["adapter_id"] == plan.adapter_id
        && previous_mode != plan.interaction_mode
        && !plan.plan_mode_per_turn;
    let rebuild = mode_change || version_change;
    let side_chat = crate::chat::is_side_chat(&document["thread"]);
    if side_chat {
        if let Some(block) = turn_row.and_then(|row| crate::composer::prefix(&row["reference_context"])) {
            text = format!("{block}\n\n{text}");
        }
    }
    let mode = crate::capabilities::thread_mode(&s.store, tid);
    let cwd = workspace_root(s, &document);
    let src_instructions = if mode == "src" {
        crate::runs::src_api::sync_src_session_files_before_turn(s, tid, std::path::Path::new(&cwd));
        crate::runs::src_api::src_thread_instructions(s, tid)
    } else {
        String::new()
    };
    let (session, needs_handoff) = if prior["adapter_id"] == plan.adapter_id
        && prior["open"] == true
        && prior["epoch"] == s.chat.epoch()
        && !rebuild
    {
        (prior["reference"].clone(), prior["handoff_pending"] == true)
    } else {
        let agent_session_id = format!("asess-{}", uuid::Uuid::new_v4().simple());
        let resume_handle = if rebuild || prior["adapter_id"] != plan.adapter_id {
            Value::Null
        } else {
            prior["reference"]["resume_handle"].clone()
        };
        let resumed = resume_handle.as_str().is_some_and(|value| !value.is_empty());
        let handoff_pending = !resumed || prior["handoff_pending"] == true;
        if mode_change && !version_change && prior["open"] == true && prior["epoch"] == s.chat.epoch() {
            call(s, plan, "close", json!({"session":prior["reference"]})).await?;
        }
        if prior["epoch"] == s.chat.epoch() {
            if let Some(previous) = prior["reference"]["agent_session_id"].as_str() {
                release(s, tid, previous);
            }
        }
        let (injection_plan, bearer_token) = injection(s, tid, plan, &agent_session_id).await?;
        let mut options = json!({"schema_version":1,"thread_mode":mode,"cwd":cwd});
        if !src_instructions.is_empty() {
            options["instructions"] = json!(src_instructions);
        }
        let request = json!({"schema_version":1,"agent_session_id":agent_session_id,
            "thread_id":tid,"workspace_id":document["thread"]["workspace_id"],
            "resume_handle":resume_handle,
            "model":if plan.model.is_empty(){Value::Null}else{json!(plan.model)},
            "effort":if plan.effort.is_empty(){Value::Null}else{json!(plan.effort)},
            "access_mode":if plan.access_mode.is_empty(){Value::Null}else{json!(plan.access_mode)},
            "interaction_mode":plan.interaction_mode,
            "options":options});
        let mut payload = json!({"request":request,"injection_plan":injection_plan});
        if let Some(token) = bearer_token {
            payload["bearer_token"] = json!(token);
        }
        let response = match call(s, plan, "start", payload).await {
            Ok(value) => value,
            Err(error) => {
                release(s, tid, &agent_session_id);
                return Err(error);
            }
        };
        if !response.is_object() {
            release(s, tid, &agent_session_id);
            return Err("Extension start response must be an object".to_owned());
        }
        let reference = json!({"schema_version":1,"agent_session_id":agent_session_id,
            "adapter_id":plan.adapter_id,"external_session_id":response["external_session_id"],
            "runtime_instance_id":plan.instance_id,"resume_handle":response["resume_handle"]});
        let _guard = s.chat.active.lock().await;
        let mut current = load(s, tid).map_err(|_| "Conversation unavailable".to_owned())?;
        if current["thread"]["state"]["running_turn_id"] != turn {
            return Err("Turn interrupted".to_owned());
        }
        current["extension_session"] = json!({"adapter_id":plan.adapter_id,"extension_version":plan.extension_version,
            "reference":reference,"epoch":s.chat.epoch(),"open":true,"interaction_mode":plan.interaction_mode,
            "handoff_pending":handoff_pending});
        current["agent_session"] = reference.clone();
        current["thread"]["state"]["agent_session_id"] = json!(agent_session_id);
        emit(
            s,
            tid,
            &mut current,
            if resumed {
                "core.session.resumed"
            } else {
                "core.session.started"
            },
            json!({"agent_session_id":agent_session_id,
            "adapter_id":plan.adapter_id,"external_session_id":response["external_session_id"],
            "reason":if mode_change {"interaction_mode_change"} else if version_change {"extension_version_change"} else {""},
            "interaction_mode_from":if mode_change {previous_mode} else {""},
            "interaction_mode_to":plan.interaction_mode}),
        );
        save(s, tid, &mut current).map_err(|_| "Could not save extension session".to_owned())?;
        (reference, handoff_pending)
    };
    let carry_history = |meta: &Value| {
        crate::attachment_delivery::handoff_files(s, &document, meta, attachment_refs)
            .map(|files| files.into_iter().map(attachment_payload))
    };
    if crate::chat::run_side::owned_by_run(&document["thread"]) {
        let mut working = document.clone();
        working["extension_session"]["reference"] = session.clone();
        let update = if needs_handoff {
            crate::chat::run_side::session_handoff(s, &mut working, turn, false)
        } else {
            crate::chat::run_side::incremental_delta(s, &mut working)
        }
        .map_err(|(_, Json(err))| {
            err["error"]["message"]
                .as_str()
                .unwrap_or("Could not assemble Run side chat context")
                .to_owned()
        })?;
        let next_seq = working["thread"]["side_chat"]["delivered_seq"].clone();
        let next_sid = working["thread"]["side_chat"]["delivered_session_id"].clone();
        let mut has_handoff = false;
        let mut handoff_meta = Value::Null;
        if let Some((block, meta)) = update {
            text = format!("{block}\n\n{text}");
            if needs_handoff {
                attachments.splice(..0, carry_history(&meta)?);
                has_handoff = true;
                handoff_meta = meta;
            }
        }
        if next_seq != document["thread"]["side_chat"]["delivered_seq"] || has_handoff {
            let _guard = s.chat.active.lock().await;
            let mut current = load(s, tid).map_err(|_| "Conversation unavailable".to_owned())?;
            current["thread"]["side_chat"]["delivered_seq"] = next_seq;
            if !next_sid.is_null() {
                current["thread"]["side_chat"]["delivered_session_id"] = next_sid;
            }
            if has_handoff
                && let Some(row) = current["turns"]
                    .as_array_mut()
                    .and_then(|rows| rows.iter_mut().find(|row| row["turn_id"] == turn))
            {
                row["handoff"] = handoff_meta;
                row["session_resolution"] = json!("structured_handoff");
            }
            save(s, tid, &mut current).map_err(|_| "Could not save extension handoff".to_owned())?;
        }
    } else if needs_handoff {
        if let Some((block, meta)) = crate::native_chat::handoff(&document, turn) {
            attachments.splice(..0, carry_history(&meta)?);
            text = format!("{block}\n\n{text}");
            let _guard = s.chat.active.lock().await;
            let mut current = load(s, tid).map_err(|_| "Conversation unavailable".to_owned())?;
            if let Some(row) = current["turns"].as_array_mut()
                .and_then(|rows| rows.iter_mut().find(|row| row["turn_id"] == turn)) {
                row["handoff"] = meta;
                row["session_resolution"] = json!("structured_handoff");
            }
            save(s, tid, &mut current).map_err(|_| "Could not save extension handoff".to_owned())?;
        }
    }
    let input = json!({"schema_version":1,"kind":"message","text":text,
        "payload":{"schema_version":1,"attachments":attachments,"interaction_mode":plan.interaction_mode,
            "model":if plan.model.is_empty(){Value::Null}else{json!(plan.model)},
            "effort":if plan.effort.is_empty(){Value::Null}else{json!(plan.effort)}}});
    crate::capabilities::renew_session_grant(s,tid).map_err(|error|error.1.0.to_string())?;
    let response = call(s, plan, "send", json!({"session":session,"input":input})).await?;
    let events = response["events"]
        .as_array()
        .ok_or("Extension send response must contain events[]")?;
    let fallback_message_id = id();
    let mut completed = false;
    let mut waiting = false;
    let mut usage = None;
    for raw in events {
        let event = event_contract::validate(raw)?;
        let payload = &event["payload"];
        match event["event_type"].as_str().unwrap_or("") {
            "message.delta" | "message.completed" if payload["role"] != "user" => {
                message_event(s, tid, turn, &event, &fallback_message_id).await?
            }
            "usage.updated" => {
                usage = turn_usage(payload).or(usage);
                record_event(s, tid, turn, &session, &event).await?;
            }
            "turn.completed" => completed = true,
            "approval.requested" | "user_input.requested" => {
                waiting = true;
                record_event(s, tid, turn, &session, &event).await?;
            }
            "turn.failed" => {
                return Ok(Progress::Failed(payload["error"].clone()));
            }
            "session.closed" => {
                record_event(s, tid, turn, &session, &event).await?;
            }
            _ => record_event(s, tid, turn, &session, &event).await?,
        }
    }
    {
        let _guard = s.chat.active.lock().await;
        let mut current = load(s, tid).map_err(|_| "Conversation unavailable".to_owned())?;
        if current["extension_session"]["reference"]["agent_session_id"] == session["agent_session_id"] {
            current["extension_session"]["interaction_mode"] = json!(plan.interaction_mode);
            current["extension_session"]["handoff_pending"] = json!(false);
            save(s, tid, &mut current).map_err(|_| "Could not save extension interaction mode".to_owned())?;
        }
    }
    if completed {
        if mode == "src" {
            crate::runs::src_api::sync_src_session_files_after_turn(
                s,
                tid,
                std::path::Path::new(&cwd),
            );
        }
        return Ok(Progress::Completed(usage));
    }
    if waiting {
        return Ok(Progress::Waiting);
    }
    Err("Extension send ended before turn.completed".to_owned())
}

/// Isolated metadata inference uses the selected extension's own protocol.
pub(super) async fn prompt_model(s:&Arc<AppState>,runtime:&Value,prompt:&str)->Result<Value,String>{
    let plan=plan(s,runtime).map_err(|error|error.1.0.to_string())?.ok_or("Extension adapter unavailable")?;
    let directory=tempfile::tempdir().map_err(|error|error.to_string())?;
    let id=format!("title-{}",uuid::Uuid::new_v4().simple());
    let start=call(s,&plan,"start",json!({"request":{"schema_version":1,"agent_session_id":id,"thread_id":id,"workspace_id":null,"resume_handle":null,"model":plan.model,"effort":plan.effort,"access_mode":"supervised","interaction_mode":"default","options":{"schema_version":1,"thread_mode":"conversation","cwd":directory.path(),"metadata_only":true}},"injection_plan":null})).await?;
    let session=json!({"schema_version":1,"agent_session_id":id,"adapter_id":plan.adapter_id,"external_session_id":start["external_session_id"],"runtime_instance_id":plan.instance_id,"resume_handle":start["resume_handle"]});
    struct Close{state:Arc<AppState>,plan:Plan,session:Value,directory:Option<tempfile::TempDir>}
    impl Drop for Close{fn drop(&mut self){let(state,plan,session,directory)=(self.state.clone(),self.plan.clone(),self.session.clone(),self.directory.take());if let Ok(runtime)=tokio::runtime::Handle::try_current(){runtime.spawn(async move{let _directory=directory;let _=call(&state,&plan,"close",json!({"session":session})).await;});}}}
    let _close=Close{state:s.clone(),plan:plan.clone(),session:session.clone(),directory:Some(directory)};
    let response=tokio::time::timeout(Duration::from_secs(60),call(s,&plan,"send",json!({"session":session,"input":{"schema_version":1,"kind":"message","text":prompt,"payload":{"schema_version":1,"attachments":[],"interaction_mode":"default","model":plan.model,"effort":plan.effort}}}))).await.map_err(|_|"Extension title invocation timed out")??;
    let mut reply=String::new();let mut usage=Value::Null;let mut completed=false;
    for raw in response["events"].as_array().ok_or("Extension title response omitted events")?{
        let event=event_contract::validate(raw)?;let payload=&event["payload"];
        match event["event_type"].as_str().unwrap_or(""){
            "message.delta" if payload["role"]!="user"=>reply.push_str(payload["delta"].as_str().or_else(||payload["text"].as_str()).unwrap_or("")),
            "message.completed" if payload["role"]!="user"=>{if let Some(text)=payload["text"].as_str(){reply=text.into();}},
            "usage.updated"=>usage=turn_usage(payload).unwrap_or(Value::Null),
            "turn.completed"=>completed=true,
            "turn.failed"=>return Err(payload["error"].to_string()),
            "approval.requested"|"user_input.requested"=>return Err("Title invocation requested interactive input".into()),_=>{}
        }
    }
    if !completed||reply.trim().is_empty(){return Err("Extension title invocation has no completed text".into())}
    Ok(json!({"reply":reply,"model":plan.model,"usage":usage}))
}
