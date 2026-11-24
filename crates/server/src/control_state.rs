//! Canonical tcode_control state projected from Muteki's persisted threads.
use crate::{AppState, control};
use serde_json::{Value, json};
use tcode_control_core::{
    project::SessionMeta,
    settings::Settings,
    settlement::{SettlementBlockers, ThreadActivity},
};

pub fn millis(value: &Value) -> Option<u64> {
    value
        .as_str()
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .and_then(|date| u64::try_from(date.timestamp_millis()).ok())
}
fn seconds(value: &Value) -> Option<u64> {
    millis(value).map(|at| at / 1000)
}
pub fn activity(doc: &Value) -> ThreadActivity {
    let mut result = ThreadActivity::default();
    let has_parent = doc["thread"]["state"]["lineage"]["parent_thread_id"].is_string();
    for message in doc["messages"].as_array().into_iter().flatten() {
        let at = millis(&message["created_at"]);
        if message["role"] == "user" {
            result.last_message_at = result.last_message_at.max(at);
        }
        let origin =
            message["origin"]
                .as_str()
                .unwrap_or(if has_parent { "agent" } else { "human" });
        if message["role"] == "user" && origin == "human" {
            result.last_human_message_at = result.last_human_message_at.max(at);
        }
    }
    for turn in doc["turns"].as_array().into_iter().flatten() {
        result.last_run_started_at = result
            .last_run_started_at
            .max(millis(&turn["started_at"]).or_else(|| millis(&turn["created_at"])));
        result.last_run_completed_at = result
            .last_run_completed_at
            .max(millis(&turn["completed_at"]).or_else(|| millis(&turn["finished_at"])));
    }
    if let Some(last) = doc["turns"].as_array().and_then(|turns| turns.last()) {
        result.failed = last["status"] == "failed";
        result.interrupted = matches!(last["status"].as_str(), Some("interrupted" | "cancelled"));
    }
    result
}
fn canonical_grant(value: &Value) -> Value {
    let mut value = value.clone();
    if let Some(categories) = value.get_mut("categories").and_then(Value::as_array_mut) {
        categories.retain(|category| {
            matches!(
                category.as_str(),
                Some(
                    "answer_input"
                        | "settings_write"
                        | "terminal"
                        | "delete"
                        | "git_remote"
                        | "pr_write"
                        | "history_edit"
                )
            )
        });
    }
    value
}
pub fn settings(s: &AppState) -> Result<Settings, Value> {
    let config = control::settings(s).map_err(|(_, b)| b.0["error"].clone())?;
    let stored = s
        .store
        .get(control::SETTINGS, control::GLOBAL)
        .map_err(|e| json!({"code":"control.storage_failed","message":e.to_string()}))?
        .unwrap_or(Value::Null);
    let mut settings = Settings::default();
    settings.control.enabled = crate::capabilities::collab_enabled(s);
    settings.control.default_grant = serde_json::from_value(canonical_grant(
        &serde_json::to_value(
            config
                .default_grant
                .unwrap_or_else(control::Grant::standard),
        )
        .unwrap(),
    ))
    .map_err(|e| json!({"code":"control.settings_invalid","message":e.to_string()}))?;
    settings.control.project_overrides = config
        .project_overrides
        .into_iter()
        .map(|(id, grant)| {
            Ok((
                id,
                serde_json::from_value(canonical_grant(&serde_json::to_value(grant).unwrap()))
                    .map_err(
                        |e| json!({"code":"control.settings_invalid","message":e.to_string()}),
                    )?,
            ))
        })
        .collect::<Result<_, Value>>()?;
    settings.subagents.child_approval =
        serde_json::from_value(json!(config.subagent_policy.approval_mode))
            .map_err(|e| json!({"code":"control.settings_invalid","message":e.to_string()}))?;
    settings.subagents.child_worktrees = config.subagent_policy.child_worktrees;
    if stored.get("settlement_automation").is_some() {
        settings.settlement_automation =
            serde_json::from_value(stored["settlement_automation"].clone())
                .map_err(|e| json!({"code":"control.settings_invalid","message":e.to_string()}))?;
    }
    if let Some(days) = stored.get("auto_settle_after_days") {
        settings.auto_settle_after_days = days.as_f64();
    }
    settings.auto_settle_on_merge = stored["auto_settle_on_merge"]
        .as_bool()
        .unwrap_or(settings.auto_settle_on_merge);
    if stored["project_settlement_overrides"].is_object() {
        settings.project_settlement_overrides =
            serde_json::from_value(stored["project_settlement_overrides"].clone())
                .map_err(|e| json!({"code":"control.settings_invalid","message":e.to_string()}))?;
    }
    if stored["source_control"].is_object() {
        settings.source_control = serde_json::from_value(stored["source_control"].clone())
            .map_err(|e| json!({"code":"control.settings_invalid","message":e.to_string()}))?;
    }
    let ui = s
        .store
        .get("settings", "ui")
        .map_err(|e| json!({"code":"control.storage_failed","message":e.to_string()}))?
        .unwrap_or(Value::Null);
    if let Some(language) = ui["values"]["language"].as_str() {
        settings.language = (*language != *"system").then(|| language.to_owned());
    }
    if let Some(theme) = ui["values"]["theme"].as_str() {
        settings.theme_mode = serde_json::from_value(json!(theme))
            .map_err(|e| json!({"code":"control.settings_invalid","message":e.to_string()}))?;
    }
    settings.word_wrap_diffs = ui["values"]["diffWrap"]
        .as_bool()
        .unwrap_or(settings.word_wrap_diffs);
    settings.sidebar_provider_marks = ui["values"]["sidebarModelIcon"]
        .as_bool()
        .unwrap_or(settings.sidebar_provider_marks);
    settings.live_command_panel_disabled = ui["values"]["liveCommandPanelDisabled"]
        .as_bool()
        .unwrap_or(settings.live_command_panel_disabled);
    settings.inactive_frame_throttle_disabled = ui["values"]["inactiveFrameThrottleDisabled"]
        .as_bool()
        .unwrap_or(settings.inactive_frame_throttle_disabled);
    settings.scroll_to_latest_on_send = ui["values"]["scrollToLatestOnSend"]
        .as_bool()
        .unwrap_or(settings.scroll_to_latest_on_send);
    settings.shorten_long_commands = ui["values"]["shortenLongCommands"]
        .as_bool()
        .unwrap_or(settings.shorten_long_commands);
    settings.skip_delete_confirmation = ui["values"]["skipDeleteConfirmation"]
        .as_bool()
        .unwrap_or(settings.skip_delete_confirmation);
    settings.provider_update_checks_disabled = ui["values"]["providerUpdateChecksDisabled"]
        .as_bool()
        .unwrap_or(settings.provider_update_checks_disabled);
    let folder = |key: &str| {
        ui["values"][key]
            .as_str()
            .filter(|path| !path.trim().is_empty())
            .map(str::to_owned)
    };
    settings.new_project_dir = folder("newProjectDir");
    settings.project_start_custom = folder("projectStartCustom");
    if let Some(start) = ui["values"]["projectStart"].as_str() {
        settings.project_start = serde_json::from_value(json!(start))
            .map_err(|e| json!({"code":"control.settings_invalid","message":e.to_string()}))?;
    }
    settings.resume_on_limit_reset = crate::quota_resume::auto_resume_enabled(s)
        .map_err(|e| json!({"code":"control.storage_failed","message":e.to_string()}))?;
    let (home_url, allow_evaluate) = crate::browser::settings(s)?;
    settings.browser.enabled = crate::capabilities::browser_enabled(s);
    settings.browser.home_url = (!home_url.is_empty()).then_some(home_url);
    settings.browser.allow_evaluate = allow_evaluate;
    Ok(settings)
}
pub fn meta(s: &AppState, doc: &Value) -> Result<SessionMeta, Value> {
    let thread = &doc["thread"];
    let id = thread["thread_id"].as_str().unwrap_or("");
    let ctrl = control::thread_doc(s, id).map_err(|(_, b)| b.0["error"].clone())?;
    let life = control::read_lifecycle(&ctrl);
    let line = &thread["state"]["lineage"];
    let cwd = crate::native_chat::session_cwd(s, doc, id)
        .map_err(|error| json!({"code":error.code,"message":error.message}))?;
    let adapter = doc["runtime"]["adapter_id"].as_str().unwrap_or("");
    let engine = crate::agent_runtime::descriptors::for_adapter(adapter)
        .map(|(engine, _)| engine.engine())
        .unwrap_or("unknown");
    let provider = tcode_control_agent::ProviderKind::NATIVE
        .into_iter()
        .find(|kind| tcode_control_core::settings::provider_key(*kind) == engine)
        .unwrap_or(tcode_control_agent::ProviderKind::Acp);
    let mut value = json!({"id":id,"title":thread["title"],"provider":provider,"cwd":cwd,"project_id":thread["project_id"],"model":doc["runtime"]["model"],
        "created_at":seconds(&thread["created_at"]).unwrap_or(0),"updated_at":seconds(&thread["updated_at"]).unwrap_or(0),
        "parent_session_id":line["parent_thread_id"],"archived_at":if thread["archived"]==true{seconds(&thread["archived_at"]).or_else(||seconds(&thread["updated_at"]))}else{None},
        "settled_at":if life.settled{seconds(&json!(life.settled_at)).or_else(||seconds(&thread["updated_at"]))}else{None},
        "pinned_at":if life.pinned{seconds(&thread["pinned_at"]).or_else(||seconds(&thread["updated_at"]))}else{None},
        "settled_override":if life.settled{json!("settled")}else if ctrl["manual_unsettle"]==true{json!("active")}else{Value::Null},
        "auto_settle_disabled_at":ctrl["kept_at"],"parked_at":if life.parked{seconds(&ctrl["parked_at"]).or_else(||seconds(&thread["updated_at"]))}else{None},
        "pull_requests":ctrl["pull_requests"].as_array().cloned().unwrap_or_default(),"pull_request_watches_paused":ctrl["pull_request_watches_paused"].as_bool().unwrap_or(false),
        "control_grant":canonical_grant(&ctrl["grant"]),"references":ctrl["references"].as_array().into_iter().flatten().map(|r|json!({"thread_id":r["target_thread_id"],"by":if r["actor"]=="user"{"user"}else{"model"},"created_at":seconds(&r["created_at"]).unwrap_or(0),"boundary_notice":r["boundary_notice"].is_string()})).collect::<Vec<_>>(),"started_by":thread["started_by"],"self_settle":ctrl["canonical_self_settle"],"unsettled_at":ctrl["unsettled_at"],"settle_suggestion_dismissed_at":ctrl["settle_suggestion_dismissed_at"],"profile_id":doc["runtime"]["credential_id"].as_str().filter(|id|id.starts_with("account:")),"cancelled_at":if thread["state"]["status"]=="cancelled"{seconds(&thread["updated_at"])}else{None}});
    if life.follow_up {
        value["follow_up"] = json!({"since":seconds(&ctrl["follow_up_since"]).unwrap_or(0),"note":life.follow_up_note});
    }
    let report = &thread["state"]["subagent_report"];
    if let Some(state) = report["state"].as_str() {
        value["expectation"] = json!({"state":state,"opened_at":seconds(&report["opened_at"]).unwrap_or(0),"reminded":report["reminded"].as_bool().unwrap_or(false),"delivered_item":report["delivered_report_id"],"undelivered":crate::control_subagents::pending_text(doc)});
    }
    if line["isolation"] == "worktree" {
        if let Some(workspace) = s
            .store
            .get("workspaces", thread["workspace_id"].as_str().unwrap_or(""))
            .map_err(|e| json!({"code":"control.storage_failed","message":e.to_string()}))?
        {
            let root = crate::workspace_read_api::project_bound_root(
                s,
                thread["project_id"].as_str().unwrap_or(""),
            )
            .map_err(|(_, b)| b.0["error"].clone())?;
            value["worktree"] = json!({"root_project_path":root,"base":workspace["settings"]["base_ref"].as_str().unwrap_or("HEAD"),"branch":workspace["settings"]["branch"]});
        }
    }
    value["native_subagent"] = line["native_subagent"].clone();
    serde_json::from_value(value).map_err(
        |error| json!({"code":"control.thread_metadata_invalid","message":error.to_string()}),
    )
}
pub fn blockers(doc: &Value) -> SettlementBlockers {
    let state = &doc["thread"]["state"];
    SettlementBlockers {
        pending_approval: state["pending_approvals"]
            .as_object()
            .is_some_and(|rows| rows.values().any(|row| row["status"] == "pending")),
        pending_input: state["pending_user_input"]["status"] == "pending",
        live_run: state["status"] == "running",
        background_work: doc["native_session"]["background_tasks"]
            .as_u64()
            .unwrap_or(0)
            > 0,
        unsettled_children: state["subagents"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|child| {
                matches!(
                    child["delivery"].as_str(),
                    Some("running" | "awaiting_settle")
                )
            }),
        pending_human_message: doc["queue"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|row| row["origin"] == "human"),
        pending_agent_message: doc["queue"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|row| row["origin"] != "human")),
        scheduled_wake: doc["scheduled_tasks"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty())
            || doc["queue"]
                .as_array()
                .is_some_and(|rows| rows.iter().any(|row| row["not_before"].is_number())),
        pull_request_watches: false,
    }
}
