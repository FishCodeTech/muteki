//! The pinned tcode_control boundary. Names carry Muteki's MCP namespace only.
use crate::{
    AppState, control_contract::*, control_observer, control_operate, control_reader, control_state,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::Arc;
fn decode<T: DeserializeOwned>(value: &Value) -> Result<T, Value> {
    serde_json::from_value(value.clone()).map_err(|error|json!({"code":"control.arguments_invalid","category":"validation","message":error.to_string()}))
}
fn legacy_args(value: impl serde::Serialize) -> Value {
    let mut value = serde_json::to_value(value).expect("control params serialize to JSON");
    if let Some(fields) = value.as_object_mut() {
        fields.retain(|_, value| !value.is_null());
    }
    value
}
fn error(message: impl ToString) -> Value {
    json!({"code":"control.operation_failed","message":message.to_string()})
}

/// Preserve `role` across the pinned `StartThreadParams` deserialization boundary and fill a
/// default `provider` (pinned role provider, workspace policy role, or caller runtime) when omitted,
/// while preserving explicit caller intent for `routing.source` attribution.
fn prepare_start_thread_args(s: &AppState, caller: &str, args: &Value) -> Result<Value, Value> {
    let mut candidate = args.clone();
    let non_empty = |v: &Value| -> Option<String> {
        v.as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let explicit_provider = non_empty(&candidate["provider"]);
    let explicit_profile = non_empty(&candidate["profile"]);
    let explicit_model = non_empty(&candidate["model"]);
    let explicit_effort = non_empty(&candidate["effort"])
        .or_else(|| non_empty(&candidate["options"]["reasoningEffort"]));

    let mut requested_fields = Vec::<&str>::new();
    if explicit_provider.is_some() { requested_fields.push("provider"); }
    if explicit_profile.is_some() { requested_fields.push("profile"); }
    if explicit_model.is_some() { requested_fields.push("model"); }
    if explicit_effort.is_some() { requested_fields.push("effort"); }

    let mut inherited_fields = Vec::<&str>::new();
    let mut inherited_adapter_id: Option<String> = None;
    let mut inherited_instance_id: Option<String> = None;

    let mut role_id = non_empty(&candidate["role"]);
    let mut routing_source = if role_id.is_some() {
        "orchestration_role"
    } else if !requested_fields.is_empty() {
        "explicit_request"
    } else {
        "inherited_parent"
    };

    let cfg = crate::control::settings(s).map_err(|(_, axum::Json(b))| b["error"].clone())?;
    let caller_mode = crate::control::thread_orchestration_mode(s, caller);
    let resolved_orchestration = cfg.orchestration_for_mode(caller_mode);
    if role_id.is_none()
        && explicit_provider.is_none()
        && explicit_profile.is_none()
        && explicit_model.is_none()
        && let Some(policy_role) = cfg.effective_default_role(&resolved_orchestration)
        && resolved_orchestration
            .profile
            .roles
            .iter()
            .any(|r| r.enabled && r.id.eq_ignore_ascii_case(&policy_role))
    {
        role_id = Some(policy_role);
        routing_source = "workspace_policy";
    }

    let allow_provider_inherit = candidate["subagent"].as_bool() == Some(true)
        || crate::chat::load(s, caller).is_ok_and(|p| p["thread"]["mode"] == "src");

    let inherit_from_parent = |cand: &mut Value,
                               inherited: &mut Vec<&'static str>,
                               adapter_out: &mut Option<String>,
                               instance_out: &mut Option<String>,
                               allow_provider: bool| {
        if let Ok(parent) = crate::chat::load(s, caller)
            && let Some(adapter_id) = parent["runtime"]["adapter_id"].as_str()
            && let Some((engine, _)) = crate::agent_runtime::descriptors::for_adapter(adapter_id)
        {
            let parent_engine = engine.engine();
            if cand["provider"]
                .as_str()
                .is_none_or(|p| p.trim().is_empty())
            {
                if !allow_provider {
                    return;
                }
                cand["provider"] = json!(parent_engine);
                inherited.push("provider");
                *adapter_out = Some(adapter_id.to_owned());
                if let Some(inst) = parent["runtime"]["instance_id"].as_str()
                    && !inst.is_empty()
                {
                    *instance_out = Some(inst.to_owned());
                }
            }
            let same_engine = cand["provider"]
                .as_str()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .is_some_and(|req| {
                    crate::agent_runtime::descriptors::find(req)
                        .map(|d| d.engine())
                        .unwrap_or(req)
                        .eq_ignore_ascii_case(parent_engine)
                });
            if same_engine {
                if cand["profile"]
                    .as_str()
                    .is_none_or(|p| p.trim().is_empty())
                    && let Some(cred) = parent["runtime"]["credential_id"].as_str()
                    && !cred.is_empty()
                {
                    cand["profile"] = json!(cred);
                    inherited.push("profile");
                }
                if cand["model"]
                    .as_str()
                    .is_none_or(|m| m.trim().is_empty())
                    && let Some(model) = parent["runtime"]["model"].as_str()
                    && !model.is_empty()
                {
                    cand["model"] = json!(model);
                    inherited.push("model");
                }
            }
        }
    };

    if let Some(ref role_id) = role_id {
        let Some(role) = resolved_orchestration
            .profile
            .roles
            .iter()
            .find(|r| r.enabled && r.id.eq_ignore_ascii_case(role_id))
        else {
            return Err(json!({
                "code": "control.role_unknown",
                "category": "validation",
                "message": format!("unknown or disabled orchestration role: {role_id}")
            }));
        };
        if role.mode == "pinned" && !role.provider.is_empty() {
            if candidate["provider"]
                .as_str()
                .is_none_or(|p| p.trim().is_empty())
            {
                candidate["provider"] = json!(role.provider);
            }
        } else {
            inherit_from_parent(
                &mut candidate,
                &mut inherited_fields,
                &mut inherited_adapter_id,
                &mut inherited_instance_id,
                true,
            );
        }
    } else {
        inherit_from_parent(
            &mut candidate,
            &mut inherited_fields,
            &mut inherited_adapter_id,
            &mut inherited_instance_id,
            allow_provider_inherit,
        );
    }
    if candidate
        .get("project_id")
        .is_none_or(|value| value.is_null())
    {
        candidate["project_id"] = json!("");
    }
    let parsed = decode::<StartThreadParams>(&candidate)?;
    let mut forwarded = legacy_args(parsed);
    if let Some(mode) = candidate.get("access_mode") {
        let mode: String = serde_json::from_value(mode.clone()).map_err(|e| json!({"code":"control.arguments_invalid","category":"validation","message":e.to_string()}))?;
        forwarded["access_mode"] = json!(mode);
    }
    if let Some(role_id) = role_id {
        forwarded["role"] = json!(role_id);
    }
    if let Some(adapter_id) = inherited_adapter_id {
        forwarded["inherited_adapter_id"] = json!(adapter_id);
    }
    if let Some(instance_id) = inherited_instance_id {
        forwarded["inherited_instance_id"] = json!(instance_id);
    }
    forwarded["routing_intent"] = json!({
        "source": routing_source,
        "workbench_mode": resolved_orchestration.mode,
        "orchestration_scope": resolved_orchestration.source,
        "requested_fields": requested_fields,
        "inherited_fields": inherited_fields,
    });
    for extra in [
        "todo_id",
        "allowed_refs",
        "delegation_profile",
        "allowed_tools",
        "isolation",
        "wait",
        "timeout_seconds",
        "acceptance_criteria",
        "routing_reason",
    ] {
        if let Some(value) = candidate.get(extra).filter(|v| !v.is_null()) {
            forwarded[extra] = value.clone();
        }
    }
    Ok(forwarded)
}
pub fn canonical(name: &str) -> &str {
    match name {
        "muteki_outline_thread" => "muteki_outline",
        "muteki_read_anchors" => "muteki_read",
        "muteki_reference_thread" => "muteki_reference",
        "muteki_rewind_thread" => "muteki_rewind",
        "muteki_list_runtimes" => "muteki_list_providers",
        name => name,
    }
}
pub async fn implemented(
    s: &Arc<AppState>,
    caller: &str,
    name: &str,
    args: &Value,
) -> Option<Result<Value, Value>> {
    Some(match canonical(name) {
        "muteki_list_projects" => control_observer::projects(s, caller),
        "muteki_list_threads" => match decode(args) {
            Ok(p) => control_observer::threads(s, caller, p).await,
            Err(error) => Err(error),
        },
        "muteki_search" => match decode(args) {
            Ok(p) => control_observer::search(s, caller, p).await,
            Err(error) => Err(error),
        },
        "muteki_review_candidates" => match decode(args) {
            Ok(p) => control_observer::review(s, caller, p).await,
            Err(error) => Err(error),
        },
        "muteki_get_thread" => match decode(args) {
            Ok(p) => control_observer::thread(s, caller, p).await,
            Err(error) => Err(error),
        },
        "muteki_outline" => decode(args).and_then(|p| control_reader::outline(s, caller, p)),
        "muteki_read" => decode(args).and_then(|p| control_reader::read(s, caller, p)),
        "muteki_search_in_item" => {
            decode(args).and_then(|p| control_reader::search_output(s, caller, p))
        }
        "muteki_file_history" => {
            decode(args).and_then(|p| control_reader::file_edits(s, caller, p))
        }
        "muteki_get_settings" => decode(args).and_then(|p| settings(s, caller, p)),
        "muteki_list_providers" => providers(s, caller).await,
        "muteki_start_thread" => match args["request_id"]
            .as_str()
            .filter(|id| !id.trim().is_empty())
            .map(|request_id| control_operate::find_existing_started_thread(s, caller, request_id))
        {
            Some(Ok(Some(existing))) => Ok(existing),
            Some(Err(error)) => Err(error),
            None | Some(Ok(None)) => match prepare_start_thread_args(s, caller, args) {
                Ok(forwarded) => control_operate::start_thread(s, caller, &forwarded).await,
                Err(error) => Err(error),
            },
        },
        "muteki_send_message" => match decode::<SendMessageParams>(args) {
            Ok(p) => control_operate::send_message(s, caller, &legacy_args(p)).await,
            Err(error) => Err(error),
        },
        "muteki_get_git" => match decode(args) {
            Ok(p) => crate::control_git::read(s, caller, p).await,
            Err(error) => Err(error),
        },
        "muteki_git" => match decode(args) {
            Ok(p) => crate::control_git::act(s, caller, p).await,
            Err(error) => Err(error),
        },
        "muteki_terminal" => decode(args).and_then(|p| crate::control_terminal::act(s, caller, p)),
        "muteki_read_terminal" => {
            decode(args).and_then(|p| crate::control_terminal::read(s, caller, p))
        }
        "muteki_get_pull_request" => {
            decode(args).and_then(|p| crate::control_pr::read(s, caller, p))
        }
        "muteki_pull_request" => match decode(args) {
            Ok(p) => crate::control_pr::act(s, caller, p).await,
            Err(error) => Err(error),
        },
        "muteki_update_threads" => match decode(args) {
            Ok(p) => crate::control_actions::update(s, caller, p).await,
            Err(error) => Err(error),
        },
        "muteki_control_thread" => match decode(args) {
            Ok(p) => crate::control_actions::run(s, caller, p).await,
            Err(error) => Err(error),
        },
        "muteki_record_review" => {
            decode(args).and_then(|p| crate::control_actions::record(s, caller, p))
        }
        "muteki_settle_self" => {
            decode(args).and_then(|p| crate::control_actions::arm(s, caller, p))
        }
        "muteki_reference" => {
            decode(args).and_then(|p| crate::control_actions::reference(s, caller, p))
        }
        "muteki_request_grant" => match decode::<RequestGrantParams>(args) {
            Ok(p) => crate::control::request_grant(s, caller, &legacy_args(p)).await,
            Err(error) => Err(error),
        },
        "muteki_respond_input" => match decode(args) {
            Ok(p) => crate::control_actions::respond_input(s, caller, p).await,
            Err(error) => Err(error),
        },
        "muteki_respond_approval" => match decode(args) {
            Ok(p) => crate::control_actions::respond_approval(s, caller, p).await,
            Err(error) => Err(error),
        },
        "muteki_create_project" => match decode(args) {
            Ok(p) => crate::control_actions::create_project(s, caller, p).await,
            Err(error) => Err(error),
        },
        "muteki_move_thread" => match decode(args) {
            Ok(p) => crate::control_actions::move_thread(s, caller, p).await,
            Err(error) => Err(error),
        },
        "muteki_delete_threads" => match decode(args) {
            Ok(p) => crate::control_actions::delete_threads(s, caller, p).await,
            Err(error) => Err(error),
        },
        "muteki_rewind" => match decode(args) {
            Ok(p) => crate::native_chat::control_rewind(s, caller, p).await,
            Err(error) => Err(error),
        },
        "muteki_notify_when" => match decode::<NotifyWhenParams>(args) {
            Ok(p) => crate::control_tools::notify_when(s, caller, &legacy_args(p)),
            Err(error) => Err(error),
        },
        "muteki_cancel_notify" => match decode::<CancelNotifyParams>(args) {
            Ok(p) => crate::control_tools::cancel_notify(s, caller, &legacy_args(p)),
            Err(error) => Err(error),
        },
        "muteki_patch_settings" => match decode(args) {
            Ok(p) => crate::control_actions::patch_settings(s, caller, p).await,
            Err(error) => Err(error),
        },
        "muteki_report_result" => match decode::<ReportResultParams>(args) {
            Ok(p) => control_operate::report_result(s, caller, &legacy_args(p)).await,
            Err(error) => Err(error),
        },
        _ => return None,
    })
}
fn redact(value: &mut Value) -> usize {
    match value {
        Value::String(text) => {
            let (clean, count) = crate::control_index::redact(text);
            *text = clean;
            count
        }
        Value::Array(values) => values.iter_mut().map(redact).sum(),
        Value::Object(values) => values.values_mut().map(redact).sum(),
        _ => 0,
    }
}
pub fn settings(s: &AppState, caller: &str, p: GetSettingsParams) -> Result<Value, Value> {
    let settings = control_state::settings(s)?;
    let mut value = serde_json::to_value(&settings).map_err(error)?;
    let cfg = crate::control::settings(s).map_err(|(_, body)| body.0)?;
    let resolved = cfg.orchestration_for_mode(crate::control::thread_orchestration_mode(s, caller));
    let object = value.as_object_mut().expect("Settings serialize as an object");
    object.retain(|key, _| matches!(key.as_str(),
        "source_control" | "language" | "theme_mode" | "word_wrap_diffs" |
        "skip_delete_confirmation" | "live_command_panel_disabled" | "sidebar_provider_marks" |
        "provider_update_checks_disabled" | "inactive_frame_throttle_disabled" |
        "scroll_to_latest_on_send" | "shorten_long_commands" | "resume_on_limit_reset" |
        "auto_settle_after_days" | "auto_settle_on_merge" | "project_settlement_overrides" |
        "orchestrate" | "browser" | "control" | "settlement_automation" |
        "new_project_dir" | "project_start" | "project_start_custom"));
    object.insert("browser".into(), json!(settings.browser));
    object.insert("orchestrate".into(), json!(settings.subagents));
    object.insert("control".into(), json!({
        "enabled": crate::capabilities::collab_enabled(s),
        "default_grant": cfg.default_grant,
        "project_overrides": cfg.project_overrides,
        "subagent_policy": cfg.subagent_policy,
        "orchestration": cfg.orchestration,
        "orchestration_overrides": cfg.orchestration_overrides,
    }));
    object.insert("orchestration".into(), serde_json::to_value(&resolved.profile).map_err(error)?);
    object.insert("orchestration_scope".into(), json!(resolved.source));
    object.insert("workbench_mode".into(), json!(resolved.mode));
    object.insert("orchestration_overrides".into(), serde_json::to_value(&cfg.orchestration_overrides).map_err(error)?);
    object.insert("ui".into(), s.store.get("settings", "ui").map_err(error)?
        .unwrap_or_else(|| json!({"version":0,"values":{}})));
    if let Some(keys) = p.keys {
        let values = value
            .as_object_mut()
            .ok_or_else(|| error("settings are not an object"))?;
        for key in &keys {
            if !values.contains_key(key) {
                return Err(error(format!("unknown settings key: {key}")));
            }
        }
        values.retain(|key, _| keys.contains(key));
    }
    let count = redact(&mut value);
    Ok(
        json!({"settings":value,"redacted":count,"omitted":["environment variable values","settings keys this build does not know"]}),
    )
}
pub async fn providers(s: &Arc<AppState>, caller: &str) -> Result<Value, Value> {
    let inventory =
        crate::chat_subagents::query(s, caller, "conversation.subagent.runtimes", &json!({}))
            .await
            .map_err(|(_, b)| b.0["error"].clone())?;
    let rows = inventory["runtimes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut grouped = std::collections::BTreeMap::<String, Vec<&Value>>::new();
    for row in &rows {
        let engine = row["engine"].as_str().ok_or_else(|| error("runtime inventory is missing engine"))?;
        grouped.entry(engine.to_owned()).or_default().push(row);
    }
    let mut providers = Vec::new();
    for (engine, runtimes) in grouped {
        let mut models = Vec::new();
        let mut profiles = Vec::new();
        for runtime in &runtimes {
            let mut profile = (**runtime).clone();
            profile["id"] = runtime["credential_id"].clone();
            profile["display_name"] = runtime["credential_id"].clone();
            profile["enabled"] = runtime["available"].clone();
            profiles.push(profile);
            for model in runtime["models"].as_array().into_iter().flatten() {
                let mut model = model.clone();
                model["profile"] = runtime["credential_id"].clone();
                model["adapter_id"] = runtime["adapter_id"].clone();
                model["instance_id"] = runtime["instance_id"].clone();
                model["is_default"] = json!(runtime["default_model"] == model["id"]);
                models.push(model);
            }
        }
        let descriptor = crate::agent_runtime::descriptors::find(&engine)
            .ok_or_else(|| error(format!("unknown runtime inventory engine {engine}")))?;
        let adapter = descriptor.adapter(&descriptor.identity.default_adapter_id)
            .ok_or_else(|| error(format!("missing default adapter for {engine}")))?;
        let caps = &adapter.capabilities;
        providers.push(json!({
            "kind": engine, "name": engine, "display_name": descriptor.identity.display_name,
            "caps": {"steering":caps.steer,"fork":caps.fork,"manual_compaction":caps.compaction,
                "mcp_servers":caps.mcp,"subagents":caps.subagents},
            "models":models,"profiles":profiles,"runtimes":runtimes,
            "supported_efforts":caps.supported_efforts,
        }));
    }
    Ok(json!({"providers":providers,"runtimes":rows,"current_runtime":inventory["current_runtime"],
        "note":inventory["note"]}))
}
