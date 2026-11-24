//! Settlement uses the pinned core's policy over actual host blockers.
use crate::{AppState, control, control_state};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use tcode_control_core::{
    control::SettlementAutomation,
    settlement::{SettlementBlockers, ThreadActivity, automatic_settlement_due},
};

#[derive(Default)]
pub(crate) struct SweepCache {
    inputs: HashMap<i64, SettlementInputs>,
}

struct SettlementInputs {
    metadata_document: Value,
    activity: ThreadActivity,
    blockers: SettlementBlockers,
}

impl SettlementInputs {
    fn from_document(mut doc: Value) -> Self {
        let activity = control_state::activity(&doc);
        let blockers = control_state::blockers(&doc);
        if let Some(fields) = doc.as_object_mut() {
            fields.retain(|name, _| matches!(name.as_str(), "thread" | "runtime" | "ctf_binding"));
        }
        Self { metadata_document: doc, activity, blockers }
    }
}

pub(crate) async fn sweep(s: &Arc<AppState>, cache: &mut SweepCache) -> Result<(), Value> {
    let settings = control_state::settings(s)?;
    if settings.settlement_automation == SettlementAutomation::Off {
        return Ok(());
    }
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let known_versions: Vec<_> = cache.inputs.keys().copied().collect();
    let rows = s
        .store
        .conversation_documents_for_settlement(&known_versions)
        .map_err(|error| json!({"message":error.to_string()}))?;
    let mut inputs = HashMap::with_capacity(rows.len());
    let mut order = Vec::with_capacity(rows.len());
    for (version, document) in rows {
        let input = document
            .map(SettlementInputs::from_document)
            .or_else(|| cache.inputs.remove(&version))
            .ok_or_else(|| json!({"message":"The settlement snapshot did not contain a cached document version."}))?;
        order.push(version);
        inputs.insert(version, input);
    }
    cache.inputs = inputs;

    for version in order {
        let input = &cache.inputs[&version];
        let thread = &input.metadata_document["thread"];
        if crate::chat::is_ctf_thread(thread) || crate::chat::is_side_chat(thread) {
            continue;
        }
        let meta = control_state::meta(s, &input.metadata_document)?;
        let project = meta
            .project_id
            .as_ref()
            .and_then(|id| settings.project_settlement_overrides.get(id));
        let days = project
            .and_then(|project| project.auto_settle_after_days)
            .unwrap_or(settings.auto_settle_after_days);
        let on_merge = project
            .and_then(|project| project.auto_settle_on_merge)
            .unwrap_or(settings.auto_settle_on_merge);
        let mut blockers = input.blockers;
        blockers.pull_request_watches = meta.pull_requests.iter().any(|link| link.watch.is_some());
        let due = automatic_settlement_due(
            &meta,
            &input.activity,
            blockers,
            now,
            days,
            on_merge,
        );
        if let Some(due) = due.filter(|due| due.due_at <= now) {
            if settings.settlement_automation == SettlementAutomation::ExplicitSignals {
                crate::control_lifecycle::settle_thread_now(s, "automation", &meta.id, None)
                    .await?;
            } else {
                let mut state = control::thread_doc(s, &meta.id)
                    .map_err(|(_, body)| body.0["error"].clone())?;
                if state["settle_suggestion"]["activity_at"] != due.activity_at {
                    state["settle_suggestion"] =
                        json!({"due_at":due.due_at,"activity_at":due.activity_at});
                    control::save_thread_doc(s, &meta.id, &state)
                        .map_err(|(_, body)| body.0["error"].clone())?;
                    control::emit(
                        s,
                        &meta.id,
                        "core.control.updated",
                        json!({"kind":"settle_suggestion","suggestion":state["settle_suggestion"]}),
                    )
                    .map_err(|(_, body)| body.0["error"].clone())?;
                }
            }
        }
    }
    Ok(())
}
