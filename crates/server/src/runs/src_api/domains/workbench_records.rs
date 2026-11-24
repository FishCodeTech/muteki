//! Durable SRC workbench TODOs, history, report drafts and suggestions.
//! The original workbench stores operator decisions as task records. A command
//! marker and its record revision are committed together for replay recovery.
use super::*;

pub(super) const APPLIED: &str = "src_workbench_record_applied_commands";
pub(super) const PROJECT_MEMORY: &str = "src_project_memory";

fn memory_scope_for_task(t: &Value, task_id: &str) -> (Option<String>, String) {
    let project_id = t["project_id"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned);
    let scope_key = project_id
        .as_deref()
        .map(|p| format!("project:{p}"))
        .unwrap_or_else(|| format!("task:{task_id}"));
    (project_id, scope_key)
}

fn repo_less_conv_dir(s: &AppState, thread_id: &str) -> Option<std::path::PathBuf> {
    if thread_id.is_empty() {
        return None;
    }
    let doc = s.store.get("conversation_threads", thread_id).ok().flatten()?;
    if doc["thread"]["workspace_id"]
        .as_str()
        .is_some_and(|w| !w.trim().is_empty())
    {
        return None;
    }
    let dir_name = if thread_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        thread_id.to_owned()
    } else {
        format!("{:x}", Sha256::digest(thread_id.as_bytes()))
    };
    let dir = crate::native_chat::state_dir(s).join("conversations").join(dir_name);
    dir.is_dir().then_some(dir)
}

pub(in crate::runs::src_api) fn write_memory_files_to_dir(
    dir: &std::path::Path,
    preferences: &str,
    notes: &str,
) {
    let _ = std::fs::write(dir.join("preferences.md"), preferences);
    let _ = std::fs::write(dir.join("notes.md"), notes);
    let pref_hash = format!("{:x}", Sha256::digest(preferences.as_bytes()));
    let notes_hash = format!("{:x}", Sha256::digest(notes.as_bytes()));
    let sync_meta = json!({
        "preferences_sha256": pref_hash,
        "notes_sha256": notes_hash,
    });
    let _ = std::fs::write(dir.join(".muteki_memory_sync.json"), sync_meta.to_string());
}

pub(in crate::runs::src_api) fn project_memory_get(s: &AppState, task_id: &str) -> Api<Value> {
    let t = task(s, task_id)?;
    let (project_id, scope_key) = memory_scope_for_task(&t, task_id);
    let saved = s.store.get(PROJECT_MEMORY, &scope_key).map_err(db)?;
    let doc = saved.unwrap_or_else(|| json!({}));
    Ok(json!({
        "scope_key": scope_key,
        "project_id": project_id,
        "task_id": task_id,
        "preferences": doc["preferences"].as_str().unwrap_or(""),
        "notes": doc["notes"].as_str().unwrap_or(""),
        "revision": doc["revision"].as_u64().unwrap_or(0),
        "updated_by": doc["updated_by"].as_str().unwrap_or(""),
        "updated_at": doc["updated_at"].as_str().unwrap_or(""),
    }))
}

fn merge_memory_field(existing: &str, incoming: &str, append: bool) -> String {
    if !append {
        return incoming.to_owned();
    }
    let trimmed = incoming.trim();
    if trimmed.is_empty() {
        return existing.to_owned();
    }
    if existing.trim().is_empty() {
        return incoming.to_owned();
    }
    if existing.lines().any(|line| line.trim() == trimmed) || existing.contains(trimmed) {
        return existing.to_owned();
    }
    format!("{}\n{}", existing.trim_end(), incoming)
}

pub(in crate::runs::src_api) fn project_memory_save_direct(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: Option<&str>,
    actor: &str,
) -> Api<Value> {
    let t = task(s, task_id)?;
    let (project_id, scope_key) = memory_scope_for_task(&t, task_id);
    let pref_in = field(body, "preferences").as_str();
    let notes_in = field(body, "notes").as_str();
    if pref_in.is_none() && notes_in.is_none() {
        return Err(Error::invalid("preferences or notes string is required"));
    }
    let append = field(body, "mode").as_str() == Some("append");
    let expected_rev = body
        .get("base_revision")
        .or_else(|| body.get("payload").and_then(|p| p.get("base_revision")))
        .and_then(Value::as_u64);
    let saved_out = loop {
        if let Some(cid) = command_id
            && let Some(marker) = s.store.get(APPLIED, cid).map_err(db)?
        {
            return Ok(marker["output"].clone());
        }
        let prior = s.store.get(PROJECT_MEMORY, &scope_key).map_err(db)?;
        let current_rev = prior
            .as_ref()
            .and_then(|v| v["revision"].as_u64())
            .unwrap_or(0);
        if let Some(expected) = expected_rev
            && expected != current_rev
        {
            return Err(Error::conflict("项目记忆版本已变化，请刷新后重试"));
        }
        let existing_pref = prior
            .as_ref()
            .and_then(|v| v["preferences"].as_str())
            .unwrap_or("");
        let existing_notes = prior
            .as_ref()
            .and_then(|v| v["notes"].as_str())
            .unwrap_or("");
        let next_pref = match pref_in {
            Some(val) => merge_memory_field(existing_pref, val, append),
            None => existing_pref.to_owned(),
        };
        let next_notes = match notes_in {
            Some(val) => merge_memory_field(existing_notes, val, append),
            None => existing_notes.to_owned(),
        };
        let next_rev = current_rev + 1;
        let stamp = now();
        let next_doc = json!({
            "scope_key": scope_key,
            "project_id": project_id,
            "last_task_id": task_id,
            "preferences": next_pref,
            "notes": next_notes,
            "revision": next_rev,
            "updated_by": actor,
            "updated_at": stamp,
        });
        let output = json!({
            "scope_key": scope_key,
            "project_id": project_id,
            "task_id": task_id,
            "preferences": next_pref,
            "notes": next_notes,
            "revision": next_rev,
            "updated_by": actor,
            "updated_at": stamp,
        });
        if let Some(cid) = command_id {
            let marker = json!({"command_id": cid, "output": output});
            if s.store
                .put_many_if(
                    &[
                        (PROJECT_MEMORY, scope_key.as_str(), prior.as_ref()),
                        (APPLIED, cid, None),
                    ],
                    &[
                        (PROJECT_MEMORY, scope_key.as_str(), &next_doc),
                        (APPLIED, cid, &marker),
                    ],
                )
                .map_err(db)?
            {
                break output;
            }
        } else if s
            .store
            .put_many_if(
                &[(PROJECT_MEMORY, scope_key.as_str(), prior.as_ref())],
                &[(PROJECT_MEMORY, scope_key.as_str(), &next_doc)],
            )
            .map_err(db)?
        {
            break output;
        }
        if expected_rev.is_some() {
            return Err(Error::conflict("项目记忆版本已变化，请刷新后重试"));
        }
    };
    if let Some(main_tid) = t["binding"]["thread_id"].as_str()
        && let Some(dir) = repo_less_conv_dir(s, main_tid)
    {
        write_memory_files_to_dir(
            &dir,
            saved_out["preferences"].as_str().unwrap_or(""),
            saved_out["notes"].as_str().unwrap_or(""),
        );
    }
    Ok(saved_out)
}

fn id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}

fn bad(code: &'static str, message: impl Into<String>) -> Error {
    Error::new(StatusCode::BAD_REQUEST, code, message)
}

fn stale(kind: &str, reference: &str) -> Error {
    Error::new(
        StatusCode::CONFLICT,
        "workbench_ref_stale",
        format!("proposal references unavailable {kind} {reference}"),
    )
}

fn field<'a>(body: &'a Value, name: &str) -> &'a Value {
    body.get(name)
        .or_else(|| body.get("payload").and_then(|payload| payload.get(name)))
        .unwrap_or(&Value::Null)
}

fn text<'a>(value: &'a Value) -> &'a str {
    value.as_str().unwrap_or("").trim()
}

fn object(value: &Value) -> serde_json::Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn found(s: &AppState, task_id: &str, kind: &str, key: &str) -> Api<Option<Value>> {
    match rec(s, task_id, kind, key) {
        Ok(row) => Ok(Some(row)),
        Err(error) if error.status == StatusCode::NOT_FOUND => Ok(None),
        Err(error) => Err(error),
    }
}

fn stored(s: &AppState, task_id: &str, kind: &str) -> Api<Vec<Value>> {
    records(s, task_id, kind)
}

fn commit(
    s: &AppState,
    task_id: &str,
    kind: &str,
    key: &str,
    payload: Value,
    expected_revision: Option<u64>,
    command_id: &str,
    include_revision: bool,
    result: Option<Value>,
) -> Api<Value> {
    loop {
        if let Some(marker) = s.store.get(APPLIED, command_id).map_err(db)? {
            return Ok(marker["output"].clone());
        }
        let (record_key, version, row) =
            stage_rec(s, task_id, kind, key, payload.clone(), expected_revision)?;
        let previous = s.store.get(RECORDS, &record_key).map_err(db)?;
        let mut output = result.clone().unwrap_or_else(|| row["payload"].clone());
        if include_revision {
            output["revision"] = json!(version + 1);
        }
        let next = json!({"version":version+1,"values":row});
        let marker = json!({"command_id":command_id,"output":output});
        if s.store
            .put_many_if(
                &[
                    (RECORDS, record_key.as_str(), previous.as_ref()),
                    (APPLIED, command_id, None),
                ],
                &[
                    (RECORDS, record_key.as_str(), &next),
                    (APPLIED, command_id, &marker),
                ],
            )
            .map_err(db)?
        {
            return Ok(output);
        }
        if let Some(marker) = s.store.get(APPLIED, command_id).map_err(db)? {
            return Ok(marker["output"].clone());
        }
        if expected_revision.is_some() {
            return Err(Error::conflict("SRC 记录版本已变化，请刷新后重试"));
        }
    }
}

fn todo_save(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: &str,
    actor: &str,
) -> Api<Value> {
    let data = object(&body["payload"]);
    let todo_id = text(&body["todo_id"]);
    let todo_id = if todo_id.is_empty() {
        text(&body["payload"]["todo_id"])
    } else {
        todo_id
    };
    let todo_id = if todo_id.is_empty() {
        id("wtodo")
    } else {
        todo_id.to_owned()
    };
    let existing = found(s, task_id, "workbench_todo", &todo_id)?;
    let mut merged = existing
        .as_ref()
        .map(|row| object(&row["payload"]))
        .unwrap_or_default();
    merged.extend(data);
    if text(merged.get("title").unwrap_or(&Value::Null)).is_empty() {
        return Err(bad("todo_incomplete", "todo needs a title"));
    }
    let status = text(merged.get("status").unwrap_or(&Value::Null));
    if status.is_empty() {
        let old = existing
            .as_ref()
            .map(|row| text(&row["payload"]["status"]))
            .unwrap_or("");
        merged.insert(
            "status".into(),
            json!(if old.is_empty() { "open" } else { old }),
        );
    }
    merged.insert("todo_id".into(), json!(todo_id));
    merged.insert("updated_by".into(), json!(actor));
    merged.insert("updated_at".into(), json!(now()));
    commit(
        s,
        task_id,
        "workbench_todo",
        &todo_id,
        Value::Object(merged),
        existing.as_ref().and_then(|row| row["revision"].as_u64()),
        command_id,
        false,
        None,
    )
}

fn question_status(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: &str,
    actor: &str,
) -> Api<Value> {
    let question_id = require(body, "question_id")?;
    let status = require(body, "status")?;
    let mut matches = stored(s, task_id, "research_question")?
        .into_iter()
        .filter(|row| {
            row["record_id"] == question_id
                || row["parent_id"] == question_id
                || row["payload"]["question_id"] == question_id
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|row| {
        row["payload"]["version"]
            .as_u64()
            .unwrap_or_else(|| row["revision"].as_u64().unwrap_or(0))
    });
    let row = matches.pop().ok_or_else(|| {
        Error::new(
            StatusCode::NOT_FOUND,
            "artifact_missing",
            "research question not found",
        )
    })?;
    let mut value = row["payload"].clone();
    let updated_at = now();
    value["status"] = json!(status);
    value["status_updated_by"] = json!(actor);
    value["status_updated_at"] = json!(updated_at);
    let key = row["natural_key"].as_str().unwrap_or(question_id);
    let saved = commit(
        s,
        task_id,
        "research_question",
        key,
        value.clone(),
        row["revision"].as_u64(),
        command_id,
        false,
        None,
    )?;
    if matches!(status, "refuted" | "invalidated" | "disproved" | "rejected") {
        let _ = super::coordination::enqueue_wakeup(
            s,
            task_id,
            &format!("question_refuted:{question_id}:{updated_at}"),
            "hypothesis_refuted",
            &format!("研究问题/假设 `{question_id}` 已被推翻，需重新判断关联工作"),
            value["title"]
                .as_str()
                .or_else(|| value["question"].as_str())
                .unwrap_or(status),
            json!({
                "question_id": question_id,
                "status": status,
                "updated_at": updated_at,
            }),
        );
    }
    Ok(saved)
}

fn history_record(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: &str,
    actor: &str,
) -> Api<Value> {
    if !body.get("entry").is_some_and(Value::is_object) {
        return Err(bad("workbench_history_invalid", "entry object is required"));
    }
    let entry = &body["entry"];
    let entry_id = text(&entry["id"]);
    let entry_id = if entry_id.is_empty() {
        id("wbhist")
    } else {
        entry_id.to_owned()
    };
    if let Some(row) = found(s, task_id, "workbench_history", &entry_id)? {
        return Ok(row["payload"].clone());
    }
    let at = text(&entry["at"]);
    let value = json!({"entry_id":entry_id,"task_id":task_id,"action":text(&entry["action"]),
        "summary":text(&entry["summary"]),"scope":text(&entry["scope"]),
        "refs":entry["refs"].as_array().cloned().unwrap_or_default(),"undo":entry["undo"],
        "domain":entry["domain"],"at":if at.is_empty(){now()}else{at.to_owned()},
        "actor_id":actor,"reverted_at":null});
    commit(
        s,
        task_id,
        "workbench_history",
        &entry_id,
        value,
        Some(0),
        command_id,
        false,
        None,
    )
}

fn history_revert(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: &str,
    actor: &str,
) -> Api<Value> {
    let entry_id = require(body, "entry_id")?;
    let row = found(s, task_id, "workbench_history", entry_id)?.ok_or_else(|| {
        Error::new(
            StatusCode::NOT_FOUND,
            "artifact_missing",
            format!("history entry {entry_id} is unavailable"),
        )
    })?;
    let mut value = row["payload"].clone();
    if !text(&value["reverted_at"]).is_empty() {
        return Err(Error::conflict("history entry was already reverted"));
    }
    value["reverted_at"] = json!(now());
    value["reverted_by"] = json!(actor);
    commit(
        s,
        task_id,
        "workbench_history",
        row["natural_key"].as_str().unwrap_or(entry_id),
        value,
        row["revision"].as_u64(),
        command_id,
        false,
        None,
    )
}

fn draft_save(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: &str,
    actor: &str,
) -> Api<Value> {
    let data = object(&body["payload"]);
    let draft_id = text(field(body, "draft_id"));
    let draft_id = if draft_id.is_empty() {
        id("wrptd")
    } else {
        draft_id.to_owned()
    };
    let existing = found(s, task_id, "workbench_report_draft", &draft_id)?;
    let revision = existing
        .as_ref()
        .and_then(|row| row["revision"].as_u64())
        .unwrap_or(0);
    if existing.is_some() && body["base_revision"].as_u64() != Some(revision) {
        return Err(Error::conflict("report draft revision does not match"));
    }
    let mut merged = existing
        .as_ref()
        .map(|row| object(&row["payload"]))
        .unwrap_or_default();
    merged.extend(data);
    merged.insert("draft_id".into(), json!(draft_id));
    merged.insert("lifecycle".into(), json!("draft"));
    merged.insert("updated_by".into(), json!(actor));
    merged.insert("updated_at".into(), json!(now()));
    commit(
        s,
        task_id,
        "workbench_report_draft",
        &draft_id,
        Value::Object(merged),
        Some(revision),
        command_id,
        true,
        None,
    )
}

fn draft_discard(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: &str,
    actor: &str,
) -> Api<Value> {
    let draft_id = require(body, "draft_id")?;
    let row = rec(s, task_id, "workbench_report_draft", draft_id)?;
    let revision = row["revision"].as_u64().unwrap_or(0);
    if body["base_revision"].as_u64() != Some(revision) {
        return Err(Error::conflict("report draft revision does not match"));
    }
    let mut value = row["payload"].clone();
    value["lifecycle"] = json!("discarded");
    value["discarded_by"] = json!(actor);
    value["discarded_at"] = json!(now());
    commit(
        s,
        task_id,
        "workbench_report_draft",
        row["natural_key"].as_str().unwrap_or(draft_id),
        value,
        Some(revision),
        command_id,
        false,
        Some(json!({"draft_id":draft_id,"lifecycle":"discarded"})),
    )
}

fn check_ref(s: &AppState, task_id: &str, kind: &str, reference: &str) -> Api<()> {
    rec(s, task_id, kind, reference).map_err(|_| stale(kind, reference))?;
    Ok(())
}

fn suggestion_propose(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: &str,
    actor: &str,
) -> Api<Value> {
    let kind = require(body, "kind")?;
    if !matches!(
        kind,
        "classify"
            | "new_finding"
            | "merge"
            | "conflict_resolve"
            | "plan_update"
            | "report_draft"
            | "retest_record"
            | "supplement_evidence"
            | "intel_match"
    ) {
        return Err(bad(
            "suggestion_kind_invalid",
            "unsupported suggestion kind",
        ));
    }
    let proposal = body["proposal"]
        .as_object()
        .map(|_| body["proposal"].clone())
        .unwrap_or_else(|| json!({}));
    for (field, record_kind) in [
        ("finding_id", "finding_claim"),
        ("asset_id", "asset"),
        ("report_id", "frozen_report"),
    ] {
        if let Some(reference) = proposal[field].as_str().filter(|v| !v.is_empty()) {
            check_ref(s, task_id, record_kind, reference)?;
        }
    }
    if kind == "merge" {
        let record_kind = if proposal["object_kind"] == "clue" {
            "finding_claim"
        } else {
            "asset"
        };
        for reference in proposal["primary_id"].as_str().into_iter().chain(
            proposal["member_ids"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str),
        ) {
            check_ref(s, task_id, record_kind, reference)?;
        }
    }
    let suggestion_id = id("wsugg");
    let value = json!({"suggestion_id":suggestion_id,"kind":kind,"proposal":proposal,
        "rationale":body["rationale"].as_str().unwrap_or(""),"turn_id":body["turn_id"],
        "source_message_id":body["source_message_id"],
        "evidence_refs":body["evidence_refs"].as_array().cloned().unwrap_or_default(),
        "proposed_by":actor,"proposed_at":now(),"decision":"pending",
        "decided_by":null,"decided_at":null,"applied_ref":null});
    commit(
        s,
        task_id,
        "workbench_suggestion",
        &suggestion_id,
        value,
        Some(0),
        command_id,
        false,
        None,
    )
}

fn suggestion_resolve(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: &str,
    actor: &str,
) -> Api<Value> {
    let suggestion_id = require(body, "suggestion_id")?;
    let decision = require(body, "decision")?;
    if !matches!(decision, "accepted" | "dismissed") {
        return Err(bad(
            "suggestion_decision_invalid",
            "decision must be accepted or dismissed",
        ));
    }
    let row = rec(s, task_id, "workbench_suggestion", suggestion_id)?;
    let revision = row["revision"].as_u64().unwrap_or(0);
    let mut value = row["payload"].clone();
    if value["decision"] != "pending" {
        return Ok(
            json!({"suggestion_id":suggestion_id,"decision":value["decision"],"already_resolved":true}),
        );
    }
    if body["base_revision"].as_u64() != Some(revision) {
        return Err(Error::conflict("suggestion revision does not match"));
    }
    value["decision"] = json!(decision);
    value["decided_by"] = json!(actor);
    value["decided_at"] = json!(now());
    value["applied_ref"] = body["applied_ref"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map_or(Value::Null, |v| json!(v));
    commit(
        s,
        task_id,
        "workbench_suggestion",
        row["natural_key"].as_str().unwrap_or(suggestion_id),
        value,
        Some(revision),
        command_id,
        false,
        None,
    )
}

fn feedback_record(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: &str,
    actor: &str,
) -> Api<Value> {
    let report_id = text(&body["report_id"]);
    let finding_id = text(&body["finding_id"]);
    if report_id.is_empty() && finding_id.is_empty() {
        return Err(Error::invalid("report_id or finding_id is required"));
    }
    if !report_id.is_empty() {
        report(s, task_id, report_id)?;
    }
    if !finding_id.is_empty() {
        rec(s, task_id, "finding", finding_id)?;
    }
    let kind = body["kind"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("general");
    if !matches!(
        kind,
        "supplement_request"
            | "duplicate_dispute"
            | "retest_notice"
            | "triage_question"
            | "general"
    ) {
        return Err(Error::invalid(
            "feedback kind must be supplement_request, duplicate_dispute, retest_notice, triage_question, or general",
        ));
    }
    let raw_text = body["text"].as_str().unwrap_or("");
    let notes = body["notes"].as_str().unwrap_or("");
    let links = match body.get("links") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(arr)) => arr.clone(),
        Some(_) => return Err(Error::invalid("feedback links must be an array")),
    };
    let attachments = body["attachments"].as_array().cloned().unwrap_or_default();
    for reference in &attachments {
        verify_ref(s, task_id, reference)?;
    }
    let attachment_refs = match body.get("attachment_refs") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(arr)) => {
            for item in arr {
                if item.is_object() {
                    verify_ref(s, task_id, item)?;
                } else if let Some(aid) = item.as_str().map(str::trim).filter(|v| !v.is_empty()) {
                    let stored = s
                        .store
                        .get("src_artifacts", &format!("{task_id}:{aid}"))
                        .map_err(db)?;
                    if stored.is_none() {
                        return Err(Error::new(
                            StatusCode::NOT_FOUND,
                            "artifact_missing",
                            format!("attachment artifact {aid} is unavailable"),
                        ));
                    }
                } else {
                    return Err(Error::invalid(
                        "attachment_refs entries must be artifact references or IDs",
                    ));
                }
            }
            arr.clone()
        }
        Some(_) => return Err(Error::invalid("attachment_refs must be an array")),
    };
    if raw_text.trim().is_empty()
        && notes.trim().is_empty()
        && attachments.is_empty()
        && attachment_refs.is_empty()
    {
        return Err(Error::conflict(
            "platform feedback requires text, notes, or attachments",
        ));
    }
    let effective_text = if !raw_text.trim().is_empty() {
        raw_text
    } else {
        notes
    };
    let text_ref = if effective_text.trim().is_empty() {
        Value::Null
    } else {
        put_artifact(
            s,
            task_id,
            effective_text.as_bytes(),
            "platform-feedback.txt",
            "text/plain",
        )?
    };
    let feedback_id = id("sfeedback");
    let received_at = text(&body["received_at"]);
    let value = json!({
        "feedback_id": feedback_id,
        "task_id": task_id,
        "report_id": if report_id.is_empty() { Value::Null } else { json!(report_id) },
        "finding_id": if finding_id.is_empty() { Value::Null } else { json!(finding_id) },
        "kind": kind,
        "text_ref": text_ref,
        "notes": notes,
        "links": links,
        "source": require(body, "source")?,
        "platform_reference": text(&body["platform_reference"]),
        "attachments": attachments,
        "attachment_refs": attachment_refs,
        "received_at": if received_at.is_empty() { now() } else { received_at.to_owned() },
        "recorded_by": actor,
        "trust": "untrusted_research_input_not_authorization"
    });
    commit(
        s,
        task_id,
        "platform_feedback",
        &feedback_id,
        value,
        Some(0),
        command_id,
        false,
        None,
    )
}

fn asset_reference(s: &AppState, task_id: &str, asset_id: &str) -> Api<Value> {
    let row = rec(s, task_id, "asset", asset_id).map_err(|error| {
        if error.status == StatusCode::NOT_FOUND {
            Error::new(
                StatusCode::NOT_FOUND,
                "artifact_missing",
                format!("asset {asset_id} is unavailable"),
            )
        } else {
            error
        }
    })?;
    if row["payload"]["lifecycle"] == "destroyed" {
        return Err(Error::new(
            StatusCode::NOT_FOUND,
            "artifact_missing",
            "asset was destroyed",
        ));
    }
    Ok(row)
}

fn asset_link(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: &str,
    actor: &str,
) -> Api<Value> {
    let ref_kind = require(body, "ref_kind")?;
    let ref_id = require(body, "ref_id")?;
    if !matches!(ref_kind, "task" | "run" | "report") {
        return Err(bad(
            "workbench_link_invalid",
            "ref_kind must be task, run or report",
        ));
    }
    let asset_ids = body["asset_ids"]
        .as_array()
        .filter(|ids| !ids.is_empty())
        .ok_or_else(|| bad("asset_missing", "no asset_ids given"))?;
    let mut ids = Vec::<String>::new();
    for value in asset_ids {
        let asset_id = value
            .as_str()
            .ok_or_else(|| bad("asset_missing", "invalid asset id"))?;
        asset_reference(s, task_id, asset_id)?;
        if !ids.iter().any(|id| id == asset_id) {
            ids.push(asset_id.to_owned());
        }
    }
    let key = format!("{ref_kind}:{ref_id}");
    let existing = found(s, task_id, "workbench_link", &key)?;
    let mut payload = existing
        .as_ref()
        .map(|row| row["payload"].clone())
        .unwrap_or_else(|| {
            json!({"link_id":id("wblink"),"task_id":task_id,"ref_kind":ref_kind,
            "ref_id":ref_id,"created_by":actor,"created_at":now()})
        });
    let mut all = payload["asset_ids"].as_array().cloned().unwrap_or_default();
    for asset_id in ids {
        if !all.iter().any(|value| value == &asset_id) {
            all.push(json!(asset_id));
        }
    }
    payload["asset_ids"] = json!(all);
    if existing.is_some() {
        payload["updated_by"] = json!(actor);
        payload["updated_at"] = json!(now());
    }
    commit(
        s,
        task_id,
        "workbench_link",
        &key,
        payload,
        existing.as_ref().and_then(|row| row["revision"].as_u64()),
        command_id,
        false,
        None,
    )
}

fn object_link(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: &str,
    actor: &str,
) -> Api<Value> {
    let ref_kind = require(body, "ref_kind")?;
    let ref_id = require(body, "ref_id")?;
    if !matches!(ref_kind, "task" | "run" | "report" | "finding") {
        return Err(bad("link_ref_invalid", "unsupported link target"));
    }
    let ids = body["object_ids"]
        .as_array()
        .filter(|ids| !ids.is_empty())
        .ok_or_else(|| bad("link_targets_required", "link needs at least one object"))?;
    let object_kind = body["object_kind"].as_str().unwrap_or("asset");
    let key = format!("{ref_kind}:{ref_id}:{object_kind}");
    let existing = found(s, task_id, "workbench_link", &key)?;
    let mut merged = std::collections::BTreeSet::<String>::new();
    for value in existing
        .as_ref()
        .into_iter()
        .flat_map(|row| row["payload"]["object_ids"].as_array())
        .flatten()
        .chain(ids)
    {
        merged.insert(value.as_str().unwrap_or("").to_owned());
    }
    let object_ids = merged.into_iter().collect::<Vec<_>>();
    let payload = json!({"ref_kind":ref_kind,"ref_id":ref_id,"object_kind":object_kind,
        "object_ids":object_ids,"updated_by":actor,"updated_at":now()});
    let output = json!({"ref_kind":ref_kind,"ref_id":ref_id,"object_kind":object_kind,"object_ids":object_ids});
    commit(
        s,
        task_id,
        "workbench_link",
        &key,
        payload,
        existing.as_ref().and_then(|row| row["revision"].as_u64()),
        command_id,
        false,
        Some(output),
    )
}

fn task_asset_link(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: &str,
    actor: &str,
) -> Api<Value> {
    let asset_id = require(body, "asset_id")?;
    let state = require(body, "state")?;
    if !matches!(state, "attached" | "detached") {
        return Err(bad(
            "workbench_link_invalid",
            "state must be attached or detached",
        ));
    }
    asset_reference(s, task_id, asset_id)?;
    let existing = found(s, task_id, "task_asset_link", asset_id)?;
    let mut payload = existing
        .as_ref()
        .map(|row| row["payload"].clone())
        .unwrap_or_else(|| json!({"asset_id":asset_id,"task_id":task_id}));
    payload["state"] = json!(state);
    payload["actor_id"] = json!(actor);
    payload["updated_at"] = json!(now());
    commit(
        s,
        task_id,
        "task_asset_link",
        asset_id,
        payload,
        existing.as_ref().and_then(|row| row["revision"].as_u64()),
        command_id,
        false,
        None,
    )
}

fn duplicate_verdict(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: &str,
    actor: &str,
) -> Api<Value> {
    let key = require(body, "key")?;
    let kind = require(body, "kind")?;
    let ids = body["ids"]
        .as_array()
        .filter(|ids| !ids.is_empty())
        .ok_or_else(|| {
            bad(
                "duplicate_verdict_invalid",
                "verdict key and member ids are required",
            )
        })?;
    let state = require(body, "state")?;
    if !matches!(state, "dismissed" | "undetermined" | "clear") {
        return Err(bad(
            "duplicate_verdict_invalid",
            "invalid duplicate verdict state",
        ));
    }
    let existing = found(s, task_id, "duplicate_verdict", key)?;
    if state == "clear" && existing.is_none() {
        return commit_many(
            s,
            task_id,
            &[],
            command_id,
            json!({"key":key,"state":"cleared"}),
        );
    }
    let payload = if state == "clear" {
        let mut value = existing.as_ref().unwrap()["payload"].clone();
        value["state"] = json!("cleared");
        value["cleared_by"] = json!(actor);
        value["cleared_at"] = json!(now());
        value
    } else {
        json!({"verdict_id":existing.as_ref().map(|row| row["payload"]["verdict_id"].clone())
                .unwrap_or_else(|| json!(id("sdupverdict"))),
            "task_id":task_id,"key":key,"kind":kind,"ids":ids,
            "state":state,"reason":text(&body["reason"]),"missing_note":text(&body["missing_note"]),
            "decided_by":actor,"decided_at":now()})
    };
    commit(
        s,
        task_id,
        "duplicate_verdict",
        key,
        payload,
        existing.as_ref().and_then(|row| row["revision"].as_u64()),
        command_id,
        false,
        None,
    )
}

fn finding_merge(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: &str,
    actor: &str,
) -> Api<Value> {
    let kind = require(body, "kind")?;
    if !matches!(kind, "source" | "clue") {
        return Err(bad(
            "merge_kind_invalid",
            "merge kind must be source or clue",
        ));
    }
    let primary_id = require(body, "primary_id")?;
    let member_ids = body["member_ids"]
        .as_array()
        .filter(|ids| !ids.is_empty())
        .ok_or_else(|| bad("merge_invalid", "member_ids are required"))?
        .clone();
    let merge_id = id("wmerge");
    let value = json!({"merge_id":merge_id,"kind":kind,"primary_id":primary_id,
        "member_ids":member_ids,
        "reason":text(&body["reason"]),"merged_by":actor,"merged_at":now(),
        "undone_at":null,"undone_by":null});
    commit(
        s,
        task_id,
        "workbench_merge",
        &merge_id,
        value,
        Some(0),
        command_id,
        false,
        None,
    )
}

fn finding_unmerge(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: &str,
    actor: &str,
) -> Api<Value> {
    let merge_id = require(body, "merge_id")?;
    let row = rec(s, task_id, "workbench_merge", merge_id)?;
    if !row["payload"]["undone_at"].is_null() {
        return commit_many(
            s,
            task_id,
            &[],
            command_id,
            json!({"merge_id":merge_id,"already_undone":true}),
        );
    }
    let mut payload = row["payload"].clone();
    let at = now();
    payload["undone_at"] = json!(at);
    payload["undone_by"] = json!(actor);
    commit(
        s,
        task_id,
        "workbench_merge",
        row["natural_key"].as_str().unwrap_or(merge_id),
        payload,
        row["revision"].as_u64(),
        command_id,
        false,
        Some(json!({"merge_id":merge_id,"undone_at":at})),
    )
}

fn finding_meta(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: &str,
    actor: &str,
) -> Api<Value> {
    let finding_id = require(body, "finding_id")?;
    if !body.get("meta").is_some_and(Value::is_object) {
        return Err(bad("finding_meta_invalid", "meta must be an object"));
    }
    let key = finding_id.to_owned();
    let existing = found(s, task_id, "workbench_finding", &key)?;
    let mut payload = existing
        .as_ref()
        .map(|row| object(&row["payload"]))
        .unwrap_or_default();
    for (key, value) in object(&body["meta"]) {
        if !value.is_null() {
            payload.insert(key, value);
        }
    }
    payload.insert("finding_id".into(), json!(finding_id));
    payload.insert("updated_by".into(), json!(actor));
    payload.insert("updated_at".into(), json!(now()));
    commit(
        s,
        task_id,
        "workbench_finding",
        &key,
        Value::Object(payload),
        existing.as_ref().and_then(|row| row["revision"].as_u64()),
        command_id,
        false,
        None,
    )
}

fn finding_stale(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: &str,
    actor: &str,
) -> Api<Value> {
    let finding_id = require(body, "finding_id")?;
    let reason = require(body, "reason")?;
    let marker_id = id("sfs");
    let (snapshot_id, snapshot) =
        super::finding::capture_dependency_snapshot(s, task_id, finding_id)?;
    let value = json!({
        "finding_id": finding_id,
        "reason": reason,
        "dependency_snapshot_id": snapshot_id,
        "dependency_snapshot": snapshot,
        "dependency_digest": snapshot_id,
        "source_refs": [],
        "marked_by": actor,
        "marked_at": now(),
        "cleared": false,
    });
    commit(
        s,
        task_id,
        "finding_stale",
        &marker_id,
        value,
        Some(0),
        command_id,
        false,
        Some(json!({"finding_id":finding_id,"stale":true,"reason":reason})),
    )
}

fn commit_many(
    s: &AppState,
    task_id: &str,
    updates: &[(String, String, Value, Option<u64>)],
    command_id: &str,
    output: Value,
) -> Api<Value> {
    if let Some(marker) = s.store.get(APPLIED, command_id).map_err(db)? {
        return Ok(marker["output"].clone());
    }
    let mut staged = Vec::new();
    for (kind, key, payload, expected) in updates {
        let (record_key, version, row) =
            stage_rec(s, task_id, kind, key, payload.clone(), *expected)?;
        let old = s.store.get(RECORDS, &record_key).map_err(db)?;
        staged.push((record_key, old, json!({"version":version+1,"values":row})));
    }
    let marker = json!({"command_id":command_id,"output":output});
    let mut checks = staged
        .iter()
        .map(|(key, old, _)| (RECORDS, key.as_str(), old.as_ref()))
        .collect::<Vec<_>>();
    checks.push((APPLIED, command_id, None));
    let mut writes = staged
        .iter()
        .map(|(key, _, next)| (RECORDS, key.as_str(), next))
        .collect::<Vec<_>>();
    writes.push((APPLIED, command_id, &marker));
    if s.store.put_many_if(&checks, &writes).map_err(db)? {
        return Ok(output);
    }
    if let Some(marker) = s.store.get(APPLIED, command_id).map_err(db)? {
        return Ok(marker["output"].clone());
    }
    Err(Error::conflict("SRC record revision changed"))
}

fn asset_importance(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: &str,
    actor: &str,
) -> Api<Value> {
    let importance = require(body, "importance")?;
    if !matches!(importance, "core" | "high" | "normal" | "low") {
        return Err(bad(
            "asset_importance_invalid",
            "importance must be core, high, normal or low",
        ));
    }
    let ids = body["asset_ids"]
        .as_array()
        .filter(|ids| !ids.is_empty())
        .ok_or_else(|| bad("asset_missing", "no asset_ids given"))?;
    let mut updates = Vec::new();
    let mut ordered = Vec::<String>::new();
    let mut seen = std::collections::HashSet::<String>::new();
    for value in ids {
        let asset_id = value
            .as_str()
            .ok_or_else(|| bad("asset_missing", "invalid asset id"))?;
        ordered.push(asset_id.to_owned());
        if !seen.insert(asset_id.to_owned()) {
            continue;
        }
        let row = asset_reference(s, task_id, asset_id)?;
        if row["payload"]["lifecycle"] == "merged" {
            return Err(Error::new(
                StatusCode::NOT_FOUND,
                "artifact_missing",
                "merged asset is no longer editable",
            ));
        }
        let mut payload = row["payload"].clone();
        payload["importance"] = json!(importance);
        payload["importance_by"] = json!(actor);
        payload["importance_at"] = json!(now());
        updates.push((
            "asset".to_owned(),
            row["natural_key"].as_str().unwrap_or(asset_id).to_owned(),
            payload,
            row["revision"].as_u64(),
        ));
    }
    commit_many(
        s,
        task_id,
        &updates,
        command_id,
        json!({"updated":ordered,"importance":importance}),
    )
}

fn finding_stale_clear(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: &str,
    actor: &str,
) -> Api<Value> {
    let finding_id = require(body, "finding_id")?;
    let mut updates = Vec::new();
    for row in stored(s, task_id, "finding_stale")? {
        if row["payload"]["finding_id"] != finding_id || row["payload"]["cleared"] == true {
            continue;
        }
        let mut payload = row["payload"].clone();
        payload["cleared"] = json!(true);
        payload["cleared_by"] = json!(actor);
        payload["cleared_at"] = json!(now());
        let key = row["natural_key"].as_str().unwrap_or("").to_owned();
        updates.push((
            "finding_stale".to_owned(),
            key,
            payload,
            row["revision"].as_u64(),
        ));
    }
    let count = updates.len();
    commit_many(
        s,
        task_id,
        &updates,
        command_id,
        json!({"finding_id":finding_id,"cleared":count}),
    )
}

fn pin_asset_version(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: &str,
    actor: &str,
) -> Api<Value> {
    let asset_id = require(body, "asset_id")?;
    let asset = asset_reference(s, task_id, asset_id)?["payload"].clone();
    let version = stored(s, task_id, "asset_version")?
        .into_iter()
        .filter(|row| row["payload"]["asset_id"] == asset_id)
        .count()
        + 1;
    let label = body["label"]
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("v{version}"));
    let version_id = id("sver");
    let value = json!({"version_id":version_id,"task_id":task_id,"asset_id":asset_id,
        "version":version,"label":label,"snapshot":asset,"pinned_by":actor,"pinned_at":now()});
    commit(
        s,
        task_id,
        "asset_version",
        &format!("{asset_id}:{version}"),
        value,
        Some(0),
        command_id,
        false,
        None,
    )
}

pub(super) fn apply_command(
    s: &AppState,
    task_id: &str,
    kind: &str,
    body: &Value,
    command_id: &str,
    actor: &str,
) -> Api<Value> {
    s.store.document_scope(||apply_command_inner(s,task_id,kind,body,command_id,actor)).map_err(db)?
}
pub(super) fn apply_command_inner(
    s: &AppState,
    task_id: &str,
    kind: &str,
    body: &Value,
    command_id: &str,
    actor: &str,
) -> Api<Value> {
    task(s, task_id)?;
    if let Some(marker) = s.store.get(APPLIED, command_id).map_err(db)? {
        return Ok(marker["output"].clone());
    }
    match kind {
        "src.workbench.todo.save" => todo_save(s, task_id, body, command_id, actor),
        "src.workbench.question.status" => question_status(s, task_id, body, command_id, actor),
        "src.workbench.history.record" => history_record(s, task_id, body, command_id, actor),
        "src.workbench.history.revert" => history_revert(s, task_id, body, command_id, actor),
        "src.workbench.report.draft.save" => draft_save(s, task_id, body, command_id, actor),
        "src.workbench.report.draft.discard" => draft_discard(s, task_id, body, command_id, actor),
        "src.workbench.suggestion.propose" => {
            suggestion_propose(s, task_id, body, command_id, actor)
        }
        "src.workbench.suggestion.resolve" => {
            suggestion_resolve(s, task_id, body, command_id, actor)
        }
        "src.workbench.asset.link" => asset_link(s, task_id, body, command_id, actor),
        "src.workbench.object.link" => object_link(s, task_id, body, command_id, actor),
        "src.workbench.asset.tasklink" => task_asset_link(s, task_id, body, command_id, actor),
        "src.workbench.duplicate.verdict" => duplicate_verdict(s, task_id, body, command_id, actor),
        "src.workbench.finding.merge" => finding_merge(s, task_id, body, command_id, actor),
        "src.workbench.finding.unmerge" => finding_unmerge(s, task_id, body, command_id, actor),
        "src.workbench.finding.meta.save" => finding_meta(s, task_id, body, command_id, actor),
        "src.workbench.finding.stale" => finding_stale(s, task_id, body, command_id, actor),
        "src.workbench.finding.stale.clear" => {
            finding_stale_clear(s, task_id, body, command_id, actor)
        }
        "src.workbench.asset.importance" => asset_importance(s, task_id, body, command_id, actor),
        "src.asset.version.pin" => pin_asset_version(s, task_id, body, command_id, actor),
        "src.feedback.record" => feedback_record(s, task_id, body, command_id, actor),
        "src.project.memory.save" => project_memory_save_direct(s, task_id, body, Some(command_id), actor),
        _ => Err(Error::invalid("unsupported SRC workbench command")),
    }
}

fn report_diff(s: &AppState, task_id: &str, body: &Value) -> Api<Value> {
    let from = require(body, "from_report_id")?;
    let to = require(body, "to_report_id")?;
    let a = report(s, task_id, from)?;
    let b = report(s, task_id, to)?;
    let mut keys = std::collections::BTreeSet::new();
    if let Some(document) = a["document"].as_object() {
        keys.extend(document.keys().cloned());
    }
    if let Some(document) = b["document"].as_object() {
        keys.extend(document.keys().cloned());
    }
    let mut fields = keys
        .into_iter()
        .map(|key| {
            json!({"field":key,"changed":a["document"][&key]!=b["document"][&key],
        "before":a["document"][&key],"after":b["document"][&key]})
        })
        .collect::<Vec<_>>();
    for key in ["template", "cvss", "deduplication", "target_version"] {
        if a[key] != b[key] {
            fields.push(json!({"field":key,"changed":true,"before":a[key],"after":b[key]}));
        }
    }
    let changed = fields
        .iter()
        .filter(|field| field["changed"] == true)
        .count();
    Ok(
        json!({"from":from,"to":to,"from_version":a["version"],"to_version":b["version"],
        "fields":fields,"changed_count":changed}),
    )
}

pub(super) fn query(s: &AppState, task_id: &str, kind: &str, body: &Value) -> Api<Value> {
    task(s, task_id)?;
    let payloads = |record_kind: &str| -> Api<Vec<Value>> {
        Ok(stored(s, task_id, record_kind)?
            .into_iter()
            .map(|row| row["payload"].clone())
            .collect())
    };
    match kind {
        "src.workbench.todo.list" => Ok(json!({"items":payloads("workbench_todo")?})),
        "src.workbench.report.drafts" => {
            Ok(json!({"items":stored(s, task_id, "workbench_report_draft")?
            .into_iter().filter(|row| row["payload"]["lifecycle"] != "discarded")
            .map(|row| { let mut value = row["payload"].clone(); value["revision"] = row["revision"].clone(); value })
            .collect::<Vec<_>>() }))
        }
        "src.workbench.suggestions" => Ok(json!({"items":stored(s, task_id, "workbench_suggestion")?
            .into_iter()
            .filter(|row| row["payload"]["decision"] == "pending")
            .map(|row| {
                let mut value = row["payload"].clone();
                value["_revision"] = row["revision"].clone();
                value
            })
            .collect::<Vec<_>>() })),
        "src.workbench.history" => {
            let mut rows = payloads("workbench_history")?;
            rows.sort_by(|a, b| b["at"].as_str().cmp(&a["at"].as_str()));
            let start = body["cursor"].as_u64().unwrap_or(0) as usize;
            let limit = body["limit"].as_u64().unwrap_or(500).clamp(1, 1000) as usize;
            let total = rows.len();
            let items = rows.into_iter().skip(start).take(limit).collect::<Vec<_>>();
            let next = if start + items.len() < total {
                Some(start + items.len())
            } else {
                None
            };
            Ok(json!({"items":items,"total":total,"next_cursor":next}))
        }
        "src.workbench.report.diff" => report_diff(s, task_id, body),
        "src.workbench.asset.links" => Ok(json!({"items":payloads("workbench_link")?})),
        "src.workbench.asset.tasklinks" => Ok(json!({"items":payloads("task_asset_link")?})),
        "src.workbench.finding.merges" => Ok(json!({"items":payloads("workbench_merge")?
            .into_iter().filter(|row| body["kind"].as_str().unwrap_or("").is_empty()
                || row["kind"] == body["kind"]).collect::<Vec<_>>()})),
        "src.workbench.finding.meta" => Ok(json!({"items":payloads("workbench_finding")?})),
        "src.workbench.duplicate.verdicts" => Ok(json!({"items":payloads("duplicate_verdict")?
            .into_iter().filter(|row| matches!(row["state"].as_str(),Some("dismissed"|"undetermined")))
            .collect::<Vec<_>>()})),
        "src.project.memory.get" => project_memory_get(s, task_id),
        _ => Err(Error::invalid("unsupported SRC workbench query")),
    }
}
