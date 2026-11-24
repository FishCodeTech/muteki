//! Durable SuperAgent coordination for a SRC Task: main-thread Chief Researcher
//! contract, coalesced idempotent wakeups from background Runs/Monitors/Blockers,
//! non-preemptive idle turn dispatch, provenance-linked context recall, and
//! traceable research decisions.
use super::{Api, Error, coverage, db, now, policy, rec, records, require, save_rec, task};
use crate::{AppState, chat};
use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashSet};
use std::sync::{Arc, OnceLock};
use uuid::Uuid;

fn default_policy() -> Value {
    json!({
        "enabled": true,
        "auto_recon": true,
        "auto_reproduce": true,
        "merge_window_seconds": 5,
        "max_wakeups_per_hour": 24,
        "max_concurrent_runs": 3,
        "max_coordination_tokens": 0,
        "sources": {
            "run_milestone": true,
            "monitor_change": true,
            "target_blocker": true,
            "external_event": true,
            "retest": true
        },
        "updated_at": now()
    })
}

pub(super) fn policy_get(s: &AppState, id: &str) -> Api<Value> {
    let mut base = default_policy();
    if let Ok(row) = rec(s, id, "coordination_policy", "default") {
        if let (Some(dst), Some(src)) = (base.as_object_mut(), row["payload"].as_object()) {
            for (k, v) in src {
                if k == "sources"
                    && let (Some(ds), Some(ss)) = (dst.get_mut("sources").and_then(Value::as_object_mut), v.as_object())
                {
                    for (sk, sv) in ss {
                        ds.insert(sk.clone(), sv.clone());
                    }
                } else {
                    dst.insert(k.clone(), v.clone());
                }
            }
        }
    }
    Ok(base)
}

pub(super) fn policy_save(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let prior = rec(s, id, "coordination_policy", "default").ok();
    let mut current = policy_get(s, id)?;
    for key in ["enabled", "auto_recon", "auto_reproduce"] {
        if let Some(val) = body.get(key).and_then(Value::as_bool) {
            current[key] = json!(val);
        }
    }
    if let Some(window) = body.get("merge_window_seconds").and_then(Value::as_u64) {
        current["merge_window_seconds"] = json!(window.clamp(1, 600));
    }
    if let Some(limit) = body.get("max_wakeups_per_hour").and_then(Value::as_u64) {
        current["max_wakeups_per_hour"] = json!(limit.clamp(1, 240));
    }
    if let Some(runs) = body.get("max_concurrent_runs").and_then(Value::as_u64) {
        current["max_concurrent_runs"] = json!(runs.clamp(1, 16));
    }
    if let Some(tokens) = body.get("max_coordination_tokens").and_then(Value::as_u64) {
        current["max_coordination_tokens"] = json!(if tokens == 0 { 0 } else { tokens.clamp(1, 50_000_000) });
    }
    if let Some(sources) = body.get("sources").and_then(Value::as_object) {
        for key in ["run_milestone", "monitor_change", "target_blocker", "external_event", "retest"] {
            if let Some(val) = sources.get(key).and_then(Value::as_bool) {
                current["sources"][key] = json!(val);
            }
        }
    }
    current["updated_at"] = json!(now());
    let rev = prior.and_then(|r| r["revision"].as_u64()).unwrap_or(0);
    Ok(save_rec(s, id, "coordination_policy", "default", current, Some(rev))?["payload"].clone())
}

fn bump_stats(s: &AppState, id: &str, field: &str) {
    for _ in 0..4 {
        let prior = rec(s, id, "coordination_stats", "default").ok();
        let rev = prior.as_ref().and_then(|r| r["revision"].as_u64()).unwrap_or(0);
        let mut payload = prior
            .as_ref()
            .map(|r| r["payload"].clone())
            .unwrap_or_else(|| json!({"duplicate_events_suppressed":0,"duplicate_runs_prevented":0}));
        payload[field] = json!(payload[field].as_u64().unwrap_or(0) + 1);
        payload["updated_at"] = json!(now());
        match save_rec(s, id, "coordination_stats", "default", payload, Some(rev)) {
            Ok(_) => return,
            Err(err) if err.status == StatusCode::CONFLICT => continue,
            Err(_) => return,
        }
    }
}

pub(super) fn record_duplicate_run_prevented(s: &AppState, id: &str) {
    bump_stats(s, id, "duplicate_runs_prevented");
}

pub(super) fn enqueue_wakeup(
    s: &AppState,
    id: &str,
    source_key: &str,
    source_kind: &str,
    title: &str,
    summary: &str,
    refs: Value,
) -> Api<bool> {
    if source_key.is_empty() {
        return Ok(false);
    }
    let cfg = policy_get(s, id)?;
    if cfg["sources"].get(source_kind).and_then(Value::as_bool) == Some(false) {
        return Ok(false);
    }
    if records(s, id, "coordination_wakeup")?
        .iter()
        .any(|row| row["natural_key"].as_str() == Some(source_key))
    {
        bump_stats(s, id, "duplicate_events_suppressed");
        return Ok(false);
    }
    let wakeup_id = format!(
        "swake_{}",
        &format!("{:x}", Sha256::digest(format!("{id}:{source_key}").as_bytes()))[..20]
    );
    let payload = json!({
        "wakeup_id": wakeup_id,
        "task_id": id,
        "source_key": source_key,
        "source_kind": source_kind,
        "title": title,
        "summary": summary,
        "refs": refs,
        "recalled_refs": {},
        "state": "pending",
        "attempts": 0,
        "wakeup_turn_id": null,
        "decision_id": null,
        "effect_state": null,
        "last_error": "",
        "created_at": now(),
        "updated_at": now()
    });
    match save_rec(s, id, "coordination_wakeup", source_key, payload, Some(0)) {
        Ok(_) => Ok(true),
        Err(err) if err.status == StatusCode::CONFLICT => {
            bump_stats(s, id, "duplicate_events_suppressed");
            Ok(false)
        }
        Err(err) => Err(err),
    }
}

pub(super) fn enqueue_run_milestone(
    s: &AppState,
    id: &str,
    run_id: &str,
    seq: u64,
    record_kind: &str,
    record_id: &str,
    data: &Value,
    candidate_record_id: Option<&str>,
) {
    let event_type = data["event_type"].as_str().unwrap_or("");
    let payload_kind = data["payload"]["kind"].as_str().unwrap_or("");
    let is_milestone = matches!(record_kind, "report_version" | "review_event" | "run_error")
        || event_type.contains("finding")
        || matches!(payload_kind, "finding_found" | "finding_reviewed");
    if !is_milestone {
        return;
    }
    let title = format!("后台 Run `{run_id}` 产生关键进展（{record_kind} #{seq}）");
    let detail = data["payload"]["title"]
        .as_str()
        .or_else(|| data["payload"]["summary"].as_str())
        .or_else(|| data["payload"]["detail"].as_str())
        .unwrap_or(event_type);
    let mut refs = json!({
        "run_id": run_id,
        "seq": seq,
        "record_kind": record_kind,
        "record_id": record_id
    });
    if let Some(cid) = candidate_record_id.filter(|v| !v.is_empty()) {
        refs["candidate_record_id"] = json!(cid);
    }
    let _ = enqueue_wakeup(
        s,
        id,
        &format!("run_event:{run_id}:{seq}"),
        "run_milestone",
        &title,
        detail,
        refs,
    );
}

fn validate_decision_refs(s: &AppState, id: &str, action: &str, body: &Value) -> Api<()> {
    let target_run_id = body["target_run_id"].as_str().map(str::trim).unwrap_or("");
    if action == "follow_up_run" && target_run_id.is_empty() {
        return Err(Error::invalid("follow_up_run requires target_run_id"));
    }
    if !target_run_id.is_empty() {
        let valid_run = records(s, id, "run_binding")?.iter().any(|row| {
            row["payload"]["destroyed_at"].is_null()
                && row["payload"]["run_id"].as_str() == Some(target_run_id)
        });
        if !valid_run {
            return Err(Error::new(
                StatusCode::NOT_FOUND,
                "src.run_not_bound",
                format!("Run `{target_run_id}` is not bound to Task `{id}`"),
            ));
        }
    }
    let finding_id = body["finding_id"].as_str().map(str::trim).unwrap_or("");
    if matches!(action, "dispatch_reproduction" | "freeze_report") && finding_id.is_empty() {
        return Err(Error::invalid(format!("{action} requires finding_id")));
    }
    if !finding_id.is_empty() {
        let has_finding = records(s, id, "finding_claim")?
            .iter()
            .chain(records(s, id, "workbench_finding")?.iter())
            .chain(records(s, id, "worker_finding_candidate")?.iter())
            .any(|row| row["payload"]["finding_id"].as_str() == Some(finding_id));
        if !has_finding {
            return Err(Error::new(
                StatusCode::NOT_FOUND,
                "artifact_missing",
                format!("Finding `{finding_id}` is unavailable in Task `{id}`"),
            ));
        }
    }
    if let Some(report_id) = body["report_id"].as_str().filter(|v| !v.trim().is_empty()) {
        rec(s, id, "frozen_report", report_id)?;
    }
    if let Some(retest_id) = body["retest_id"].as_str().filter(|v| !v.trim().is_empty()) {
        rec(s, id, "finding_retest", retest_id)?;
    }
    if let Some(repro_id) = body["reproduction_id"].as_str().filter(|v| !v.trim().is_empty()) {
        rec(s, id, "report_reproduction", repro_id)?;
    }
    Ok(())
}

fn evaluate_decision_effect(s: &AppState, id: &str, decision: &Value) -> Api<(&'static str, Value)> {
    let action = decision["action"].as_str().unwrap_or("");
    let created_at = decision["created_at"].as_str().unwrap_or("");
    let target_run_id = decision["target_run_id"].as_str().unwrap_or("");
    let finding_id = decision["finding_id"].as_str().unwrap_or("");
    let report_id = decision["report_id"].as_str().unwrap_or("");
    let retest_id = decision["retest_id"].as_str().unwrap_or("");
    let repro_id = decision["reproduction_id"].as_str().unwrap_or("");
    let receipt_id = decision["command_receipt_id"].as_str().unwrap_or("");

    if matches!(action, "ignore" | "defer" | "notify_operator") {
        return Ok(("not_required", json!({})));
    }
    if !receipt_id.is_empty()
        && let Ok(Some(cmd)) = s.store.get("platform_commands", receipt_id).map_err(db)
    {
        let st = cmd["response"]["receipt"]["state"].as_str().unwrap_or("");
        if st == "failed" {
            return Ok(("effect_failed", json!({"command_receipt_id": receipt_id})));
        }
        if st == "completed" {
            return Ok(("effect_confirmed", json!({"command_receipt_id": receipt_id})));
        }
    }
    match action {
        "follow_up_run" => {
            if !retest_id.is_empty()
                && let Ok(rt) = rec(s, id, "finding_retest", retest_id)
            {
                let p = &rt["payload"];
                if p["recovery_required"] == true {
                    return Ok(("effect_unknown", json!({"retest_id": retest_id})));
                }
                if p["status"] == "dispatch_failed" {
                    return Ok(("effect_failed", json!({"retest_id": retest_id})));
                }
                if matches!(p["status"].as_str(), Some("dispatched" | "completed")) {
                    return Ok(("effect_confirmed", json!({"retest_id": retest_id})));
                }
            }
            if !target_run_id.is_empty() {
                if let Some(run) = s.store.get("runs", target_run_id).map_err(db)? {
                    let active = matches!(run["status"].as_str(), Some("starting" | "running" | "planning"));
                    let has_recent_directive = records(s, id, "operator_directive")?
                        .iter()
                        .chain(records(s, id, "run_event")?.iter())
                        .any(|r| {
                            r["payload"]["source_run_id"].as_str() == Some(target_run_id)
                                && r["created_at"].as_str().unwrap_or("") >= created_at
                        });
                    if active || has_recent_directive {
                        return Ok(("effect_confirmed", json!({"run_id": target_run_id, "run_status": run["status"]})));
                    }
                }
            }
            Ok(("pending_confirmation", json!({})))
        }
        "start_new_run" => {
            let mut matched_run: Option<(String, Value)> = None;
            if !target_run_id.is_empty() {
                if let Some(run) = s.store.get("runs", target_run_id).map_err(db)? {
                    matched_run = Some((target_run_id.to_owned(), run));
                }
            } else {
                for b in records(s, id, "run_binding")? {
                    if b["payload"]["destroyed_at"].is_null()
                        && b["created_at"].as_str().unwrap_or("") >= created_at
                        && let Some(rid) = b["payload"]["run_id"].as_str()
                        && let Some(run) = s.store.get("runs", rid).map_err(db)?
                    {
                        matched_run = Some((rid.to_owned(), run));
                        break;
                    }
                }
            }
            if let Some((rid, run)) = matched_run {
                let st = run["status"].as_str().unwrap_or("draft");
                if st == "failed" {
                    return Ok(("effect_failed", json!({"run_id": rid, "run_status": st})));
                }
                if st != "draft" {
                    return Ok(("effect_confirmed", json!({"run_id": rid, "run_status": st})));
                }
            }
            Ok(("pending_confirmation", json!({})))
        }
        "dispatch_reproduction" => {
            if !repro_id.is_empty() && rec(s, id, "report_reproduction", repro_id).is_ok() {
                return Ok(("effect_confirmed", json!({"reproduction_id": repro_id})));
            }
            if !retest_id.is_empty()
                && let Ok(rt) = rec(s, id, "finding_retest", retest_id)
            {
                let st = rt["payload"]["status"].as_str().unwrap_or("");
                if rt["payload"]["recovery_required"] == true {
                    return Ok(("effect_unknown", json!({"retest_id": retest_id})));
                }
                if st == "dispatch_failed" {
                    return Ok(("effect_failed", json!({"retest_id": retest_id})));
                }
                if matches!(st, "dispatched" | "completed") {
                    return Ok(("effect_confirmed", json!({"retest_id": retest_id})));
                }
            }
            if !finding_id.is_empty() {
                if let Some(r) = records(s, id, "report_reproduction")?.into_iter().rev().find(|row| {
                    row["payload"]["finding_id"].as_str() == Some(finding_id)
                        && row["created_at"].as_str().unwrap_or("") >= created_at
                }) {
                    return Ok(("effect_confirmed", json!({"reproduction_id": r["payload"]["reproduction_id"]})));
                }
            }
            Ok(("pending_confirmation", json!({})))
        }
        "freeze_report" => {
            if !report_id.is_empty() && rec(s, id, "frozen_report", report_id).is_ok() {
                return Ok(("effect_confirmed", json!({"report_id": report_id})));
            }
            if !finding_id.is_empty() {
                if let Some(r) = records(s, id, "frozen_report")?.into_iter().rev().find(|row| {
                    row["payload"]["finding_id"].as_str() == Some(finding_id)
                        && row["created_at"].as_str().unwrap_or("") >= created_at
                }) {
                    return Ok(("effect_confirmed", json!({"report_id": r["payload"]["report_id"]})));
                }
            }
            Ok(("pending_confirmation", json!({})))
        }
        _ => Ok(("pending_confirmation", json!({}))),
    }
}

pub(super) fn decide(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let action = require(body, "action")?;
    if !matches!(
        action,
        "ignore"
            | "defer"
            | "follow_up_run"
            | "start_new_run"
            | "dispatch_reproduction"
            | "freeze_report"
            | "notify_operator"
    ) {
        return Err(Error::invalid("unsupported coordination decision action"));
    }
    let rationale = require(body, "rationale")?;
    validate_decision_refs(s, id, action, body)?;

    let wakeup_ids: Vec<String> = body["wakeup_ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .collect();
    let wakeups = records(s, id, "coordination_wakeup")?;
    for wid in &wakeup_ids {
        if !wakeups
            .iter()
            .any(|r| r["payload"]["wakeup_id"].as_str() == Some(wid.as_str()))
        {
            return Err(Error::new(
                StatusCode::NOT_FOUND,
                "artifact_missing",
                format!("coordination wakeup `{wid}` is unavailable in Task `{id}`"),
            ));
        }
    }

    let decision_id = format!("scdec_{}", Uuid::new_v4().simple());
    let stamp = now();
    let stamp_dt = parse_ts(Some(&stamp)).unwrap_or_else(Utc::now);
    let mut earliest_wakeup_dt: Option<DateTime<Utc>> = None;
    for wid in &wakeup_ids {
        if let Some(row) = wakeups
            .iter()
            .find(|r| r["payload"]["wakeup_id"].as_str() == Some(wid.as_str()))
            && let Some(dt) = parse_ts(row["payload"]["created_at"].as_str())
        {
            earliest_wakeup_dt = Some(match earliest_wakeup_dt {
                Some(cur) if cur <= dt => cur,
                _ => dt,
            });
        }
    }
    let decision_latency_ms = earliest_wakeup_dt
        .map(|dt| (stamp_dt - dt).num_milliseconds().max(0))
        .map(Value::from)
        .unwrap_or(Value::Null);

    let quiet = body["quiet"]
        .as_bool()
        .unwrap_or(!matches!(action, "notify_operator" | "freeze_report"));
    let mut decision = json!({
        "decision_id": decision_id,
        "task_id": id,
        "wakeup_ids": wakeup_ids,
        "action": action,
        "rationale": rationale,
        "target_run_id": body["target_run_id"].as_str().unwrap_or(""),
        "finding_id": body["finding_id"].as_str().unwrap_or(""),
        "report_id": body["report_id"].as_str().unwrap_or(""),
        "retest_id": body["retest_id"].as_str().unwrap_or(""),
        "reproduction_id": body["reproduction_id"].as_str().unwrap_or(""),
        "command_receipt_id": body["command_receipt_id"].as_str().unwrap_or(""),
        "quiet": quiet,
        "decision_latency_ms": decision_latency_ms,
        "effect_state": "not_required",
        "effect_refs": {},
        "effect_confirmed_at": Value::Null,
        "execution_latency_ms": Value::Null,
        "created_at": stamp
    });
    let (effect_state, effect_refs) = evaluate_decision_effect(s, id, &decision)?;
    decision["effect_state"] = json!(effect_state);
    decision["effect_refs"] = effect_refs;
    if effect_state == "effect_confirmed" {
        decision["effect_confirmed_at"] = json!(stamp);
        if let Some(dt) = earliest_wakeup_dt {
            decision["execution_latency_ms"] = json!((stamp_dt - dt).num_milliseconds().max(0));
        }
    }

    let next_wakeup_state = if effect_state == "effect_confirmed" {
        "effect_confirmed"
    } else {
        "decided"
    };
    let mut consumed = Vec::new();
    for wid in &wakeup_ids {
        if let Some(row) = wakeups
            .iter()
            .find(|r| r["payload"]["wakeup_id"].as_str() == Some(wid.as_str()))
        {
            let mut next = row["payload"].clone();
            next["state"] = json!(next_wakeup_state);
            next["decision_id"] = json!(decision_id);
            next["decided_action"] = json!(action);
            next["effect_state"] = json!(effect_state);
            next["updated_at"] = json!(stamp);
            let natural = row["natural_key"].as_str().unwrap_or("");
            save_rec(
                s,
                id,
                "coordination_wakeup",
                natural,
                next,
                row["revision"].as_u64(),
            )?;
            consumed.push(wid.clone());
        }
    }
    decision["wakeup_ids"] = json!(consumed);

    save_rec(
        s,
        id,
        "coordination_decision",
        &decision_id,
        decision.clone(),
        Some(0),
    )?;

    if !quiet {
        let notice_id = format!("snotice_{}", Uuid::new_v4().simple());
        let notice = json!({
            "notice_id": notice_id,
            "task_id": id,
            "decision_id": decision_id,
            "action": action,
            "rationale": rationale,
            "target_run_id": decision["target_run_id"],
            "finding_id": decision["finding_id"],
            "report_id": decision["report_id"],
            "status": "unread",
            "created_at": stamp
        });
        save_rec(s, id, "coordination_notice", &decision_id, notice, Some(0))?;
    }
    Ok(decision)
}

pub(super) fn ack_notice(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let key = body["decision_id"]
        .as_str()
        .filter(|v| !v.is_empty())
        .or_else(|| body["notice_id"].as_str().filter(|v| !v.is_empty()))
        .ok_or_else(|| Error::invalid("decision_id or notice_id is required"))?;
    let row = records(s, id, "coordination_notice")?
        .into_iter()
        .find(|r| {
            r["natural_key"].as_str() == Some(key)
                || r["payload"]["decision_id"].as_str() == Some(key)
                || r["payload"]["notice_id"].as_str() == Some(key)
        })
        .ok_or_else(|| {
            Error::new(
                StatusCode::NOT_FOUND,
                "artifact_missing",
                "coordination notice is unavailable",
            )
        })?;
    let mut payload = row["payload"].clone();
    payload["status"] = json!("read");
    payload["read_at"] = json!(now());
    Ok(save_rec(
        s,
        id,
        "coordination_notice",
        row["natural_key"].as_str().unwrap_or(key),
        payload,
        row["revision"].as_u64(),
    )?["payload"]
        .clone())
}

fn coordination_usage(s: &AppState, thread_doc: Option<&Value>, wakeups: &[Value]) -> (u64, usize) {
    let mut seen_turns = HashSet::new();
    let mut tokens_used = 0u64;
    let mut unreported_turns = 0usize;
    for w in wakeups {
        let Some(turn_id) = w["wakeup_turn_id"].as_str().filter(|v| !v.is_empty()) else {
            continue;
        };
        if !seen_turns.insert(turn_id.to_owned()) {
            continue;
        }
        let store_usage = s.store.get("usage", turn_id).ok().flatten();
        let turn_row = thread_doc.and_then(|doc| {
            doc["turns"]
                .as_array()
                .and_then(|turns| turns.iter().find(|t| t["turn_id"] == turn_id))
        });
        let extract_tokens = |u: &Value| -> Option<u64> {
            if let Some(t) = u["total_tokens"].as_u64() {
                return Some(t);
            }
            let input = u["input_tokens"]
                .as_u64()
                .or_else(|| u["prompt_tokens"].as_u64());
            let output = u["output_tokens"]
                .as_u64()
                .or_else(|| u["completion_tokens"].as_u64());
            match (input, output) {
                (Some(i), Some(o)) => Some(i + o),
                (Some(i), None) => Some(i),
                (None, Some(o)) => Some(o),
                (None, None) => None,
            }
        };
        let found = store_usage
            .as_ref()
            .and_then(extract_tokens)
            .or_else(|| turn_row.and_then(|t| extract_tokens(&t["usage"])));
        if let Some(toks) = found {
            tokens_used = tokens_used.saturating_add(toks);
        } else if turn_row.is_some_and(|t| t["status"] == "completed") {
            unreported_turns += 1;
        }
    }
    (tokens_used, unreported_turns)
}

pub(super) fn status(s: &AppState, id: &str) -> Api<Value> {
    let t = task(s, id)?;
    let pol = policy_get(s, id)?;
    let mut wakeups: Vec<Value> = records(s, id, "coordination_wakeup")?
        .into_iter()
        .map(|r| r["payload"].clone())
        .collect();
    wakeups.sort_by(|a, b| b["created_at"].as_str().cmp(&a["created_at"].as_str()));
    let pending: Vec<Value> = wakeups
        .iter()
        .filter(|w| w["state"] == "pending")
        .cloned()
        .collect();
    let dispatched_count = wakeups
        .iter()
        .filter(|w| matches!(w["state"].as_str(), Some("dispatched" | "queued")))
        .count();
    let queued_count = wakeups.iter().filter(|w| w["state"] == "queued").count();
    let decided_count = wakeups
        .iter()
        .filter(|w| {
            matches!(
                w["state"].as_str(),
                Some("decided" | "effect_confirmed" | "acknowledged")
            )
        })
        .count();
    let effect_confirmed_count = wakeups
        .iter()
        .filter(|w| w["state"] == "effect_confirmed")
        .count();
    let invalid_wakeups = wakeups.iter().filter(|w| w["state"] == "unactioned").count();
    let delivery_failures = wakeups
        .iter()
        .filter(|w| w["state"] == "delivery_failed")
        .count();

    let mut decisions: Vec<Value> = records(s, id, "coordination_decision")?
        .into_iter()
        .map(|r| r["payload"].clone())
        .collect();
    decisions.sort_by(|a, b| b["created_at"].as_str().cmp(&a["created_at"].as_str()));
    let quiet_decisions = decisions.iter().filter(|d| d["quiet"] == true).count();

    let decision_latencies: Vec<i64> = decisions
        .iter()
        .filter_map(|d| d["decision_latency_ms"].as_i64())
        .collect();
    let avg_decision_latency_ms = if decision_latencies.is_empty() {
        Value::Null
    } else {
        json!(decision_latencies.iter().sum::<i64>() / decision_latencies.len() as i64)
    };
    let exec_latencies: Vec<i64> = decisions
        .iter()
        .filter_map(|d| d["execution_latency_ms"].as_i64())
        .collect();
    let avg_execution_latency_ms = if exec_latencies.is_empty() {
        Value::Null
    } else {
        json!(exec_latencies.iter().sum::<i64>() / exec_latencies.len() as i64)
    };

    let stats = rec(s, id, "coordination_stats", "default")
        .ok()
        .map(|r| r["payload"].clone())
        .unwrap_or_else(|| json!({}));
    let thread_doc = t["binding"]["thread_id"]
        .as_str()
        .or_else(|| t["thread_id"].as_str())
        .filter(|v| !v.is_empty())
        .and_then(|tid| chat::load(s, tid).ok());
    let (tokens_used, unreported_usage_turns) =
        coordination_usage(s, thread_doc.as_ref(), &wakeups);
    let max_tokens = pol["max_coordination_tokens"].as_u64().unwrap_or(0);

    let mut unread_notices: Vec<Value> = records(s, id, "coordination_notice")?
        .into_iter()
        .map(|r| r["payload"].clone())
        .filter(|n| n["status"] == "unread")
        .collect();
    unread_notices.sort_by(|a, b| b["created_at"].as_str().cmp(&a["created_at"].as_str()));

    let active_monitors = records(s, id, "monitor")?
        .into_iter()
        .filter(|m| m["payload"]["status"] == "active")
        .count();
    let active_runs = records(s, id, "run_binding")?
        .into_iter()
        .filter(|b| b["payload"]["destroyed_at"].is_null())
        .filter_map(|b| b["payload"]["run_id"].as_str().map(str::to_owned))
        .filter(|rid| {
            s.store
                .get("runs", rid)
                .ok()
                .flatten()
                .is_some_and(|r| matches!(r["status"].as_str(), Some("starting" | "running" | "planning")))
        })
        .count();

    let recent_wakeups: Vec<Value> = wakeups.into_iter().take(20).collect();
    let recent_decisions: Vec<Value> = decisions.into_iter().take(20).collect();
    Ok(json!({
        "task_lifecycle": t["lifecycle"],
        "policy": pol,
        "pending_wakeups": pending,
        "recent_wakeups": recent_wakeups,
        "recent_decisions": recent_decisions,
        "unread_notices": unread_notices,
        "metrics": {
            "pending_count": pending.len(),
            "queued_count": queued_count,
            "dispatched_count": dispatched_count,
            "decided_count": decided_count,
            "effect_confirmed_count": effect_confirmed_count,
            "invalid_wakeups": invalid_wakeups,
            "quiet_decisions": quiet_decisions,
            "delivery_failures": delivery_failures,
            "duplicate_events_suppressed": stats["duplicate_events_suppressed"].as_u64().unwrap_or(0),
            "duplicate_runs_prevented": stats["duplicate_runs_prevented"].as_u64().unwrap_or(0),
            "coordination_tokens_used": tokens_used,
            "max_coordination_tokens": max_tokens,
            "unreported_usage_turns": unreported_usage_turns,
            "avg_decision_latency_ms": avg_decision_latency_ms,
            "avg_execution_latency_ms": avg_execution_latency_ms,
            "active_monitors": active_monitors,
            "active_runs": active_runs
        }
    }))
}

pub(crate) fn src_thread_instructions(s: &AppState, tid: &str) -> String {
    match super::super::resolve_authorized_src_caller(s, tid) {
        Ok(Some(caller)) => match caller.delegation.as_ref() {
            None => chief_researcher_instructions(s, &caller.task_id, tid),
            Some(delegation) => delegated_worker_instructions(s, &caller.task_id, tid, delegation),
        },
        _ => String::new(),
    }
}

pub(crate) fn chief_researcher_instructions(s: &AppState, id: &str, thread_id: &str) -> String {
    let Ok(t) = task(s, id) else {
        return String::new();
    };
    let goal = t["input"]["goal"].as_str().unwrap_or("");
    let targets = policy(s, id)
        .ok()
        .and_then(|p| p["authorized_targets"].as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    let runs: Vec<String> = records(s, id, "run_binding")
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r["payload"]["destroyed_at"].is_null())
        .filter_map(|r| {
            let rid = r["payload"]["run_id"].as_str()?;
            let target = r["payload"]["target_url"].as_str().unwrap_or("");
            let state = s
                .store
                .get("runs", rid)
                .ok()
                .flatten()
                .and_then(|run| run["status"].as_str().map(str::to_owned))
                .unwrap_or_else(|| "unknown".to_owned());
            Some(format!("{rid} ({state} · {target})"))
        })
        .collect();
    let effective_tid = if thread_id.is_empty() {
        t["binding"]["thread_id"].as_str().unwrap_or("")
    } else {
        thread_id
    };
    let subagents: Vec<String> = if effective_tid.is_empty() {
        Vec::new()
    } else {
        chat::load(s, effective_tid)
            .ok()
            .and_then(|doc| doc["thread"]["state"]["subagents"].as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .rev()
            .take(12)
            .filter_map(|node| {
                let sid = node["thread_id"].as_str().or_else(|| node["agent_id"].as_str())?;
                let title = node["title"].as_str().unwrap_or(sid);
                let status = node["status"].as_str().unwrap_or("unknown");
                let delivery = node["delivery"]
                    .as_str()
                    .filter(|v| !v.is_empty())
                    .unwrap_or("unknown");
                let todo = node["todo_id"].as_str().filter(|v| !v.is_empty()).unwrap_or("-");
                let profile = node["delegation_profile"]
                    .as_str()
                    .filter(|v| !v.is_empty())
                    .unwrap_or("readonly_analysis");
                Some(format!("{sid} [status={status} · delivery={delivery} · {profile} · todo={todo}]: {title}"))
            })
            .collect()
    };
    let todos: Vec<String> = records(s, id, "workbench_todo")
        .unwrap_or_default()
        .into_iter()
        .filter(|r| !matches!(r["payload"]["status"].as_str(), Some("done" | "deleted")))
        .take(12)
        .filter_map(|r| {
            let p = &r["payload"];
            let tid = p["todo_id"].as_str()?;
            let title = p["title"].as_str().unwrap_or("");
            let status = p["status"].as_str().unwrap_or("open");
            let exec = p["executor"]["id"]
                .as_str()
                .filter(|v| !v.is_empty())
                .unwrap_or("unassigned");
            Some(format!("{tid} ({status} · owner={exec}): {title}"))
        })
        .collect();
    let memory = super::workbench_records::project_memory_get(s, id)
        .unwrap_or_else(|_| json!({"preferences": "", "notes": ""}));
    let pref = memory["preferences"].as_str().unwrap_or("").trim();
    let notes = memory["notes"].as_str().unwrap_or("").trim();
    format!(
        "## Muteki SRC 首席研究协调官（Pure Auto-Planning Coordinator）契约\n\
         当前研究项目 Task ID：`{id}`（主协调线程：`{effective_tid}`）\n\
         研究目标：{}\n\
         当前已授权目标范围：{}\n\
         活跃或近期委派子线程（Subthreads）：\n{}\n\
         已关联后台 Run：{}\n\
         当前未完结待办（Todos）：\n{}\n\
         项目级长期偏好（preferences）：\n{}\n\
         项目级长期笔记（notes）：\n{}\n\n\
         ### 铁律 1：纯协调官定位（主线程严禁亲自下场执行或阻塞等待）\n\
         - 你是 SRC 主会话协调官。**禁止**在主线程自己跑 Bash/`curl`/Python/`web_fetch` 去探测目标 URL，也**禁止**传 `wait=true` 阻塞等待子线程或 Run。\n\
         - 主线程只做四件事：**自动解析范围与增量判别 → 维护待办与项目偏好/笔记 → 并行派发/精准转向或止损子线程/Run → 几秒内简明回复用户**。\n\
         - **自然语言范围自动解析与更新**：当用户在消息中给出目标域名、URL、通配符、排除路径（`excluded_targets`/`exclusions`）或授权备注时，立即提取并在本轮直接调用 `muteki_renew_src_authorization`（传入 `targets`、`excluded_targets`、`notes`、`raw_policy_text` 等；`base_policy_version` 与 `idempotency_key` 均可省略；可按需配合 `muteki_save_src_research_authorization` 与 `muteki_preview_src_policy`），**不要让用户再去填写范围表单**。\n\
         - 哪怕用户只是随手发一句需求、没提“开子代理”，只要涉及具体目标探测、JS 逆向、浏览器操作、外部 MCP 插件调用或越权/竞态验证，**你必须自动调用 `muteki_src_todo_save` + `muteki_start_thread(subagent=true, wait=false, delegation_profile=\"scoped_execution\", todo_id=..., message=...)` 派发并行子线程执行**（`scoped_execution` 子代理自动拥有 `muteki-browser`、`muteki-computer`、`muteki-packages` MCP 插件及 `muteki_exchange_race_probe`、`muteki_research_graphql_grpc_ingest`、`muteki_identity_session_import` 等主动探测能力）；仅做代码/档案静态分析则派发 `delegation_profile=\"readonly_analysis\"`；若适合重型全站扫描则调用 `muteki_ensure_src_run` + `muteki_start_src_run`。`delegation_profile` 控制子线程工具权限边界，与当前 SRC 编排花名册中的 `role`（决定模型、推理深度与守则）正交，可按需在 `muteki_start_thread` 中同时传入。\n\n\
         ### 铁律 2：连续消息四路增量路由（每一轮只做增量，绝不推倒重来）\n\
         用户连续轰炸发来的多条消息会在同一回合内合并送达。请逐句拆解每条新意图，并按以下四类自动并行消化：\n\
         1. **独立新任务** → 先调 `muteki_src_todo_save` 记待办拿 `todo_id`，立刻为每个独立任务分别调用一次 `muteki_start_thread`（必须设 `subagent=true`、`wait=false`、`delegation_profile=\"scoped_execution\"` 或只读分析设 `\"readonly_analysis\"`、`todo_id` 绑定对应待办、`request_id` 唯一），与已在跑的 worker **并行**推进；有 N 条独立线就立刻并发开 N 个子线程。\n\
         2. **对同一条线的补充 / 追问 / 细化** → 对照上方 `Subthreads` / `Run` / `Todos` 清单（或调用 `muteki_get_subagents`）定位原 owner，调用 `muteki_send_message(thread_ids=[<owner_thread_id>], request_id=..., text=..., mode=\"steer\")`（若该线已是后台 Run 则调用 `muteki_src_run_intervene(run_id=..., action=\"steer\", note=...)`）将补充要求塞给原 owner，**绝不另起炉灶重复建 worker**。\n\
         3. **改口 / 否定 / 冲突** → 只对受影响那条线的 owner 调用 `muteki_control_thread(changes=[{{\"thread_id\": <owner_thread_id>, \"op\": \"cancel\"}}])`（或 `\"interrupt\"` 后改派；后台 Run 用 `muteki_src_run_intervene(action=\"stop\")`），**其余无关路线照常继续，绝不全部推倒重来**。\n\
         4. **无实质任务变化的确认 / 偏好 / 备注** → 安静调用 `muteki_src_project_memory_save`（或在 Repo-less 当前目录编辑 `preferences.md` / `notes.md`；`mode=\"append\"` 可增量追加）记住偏好与笔记，主聊天里极简确认一句话即可，**绝不为了“好的/记住这个习惯”之类消息额外开 worker 或刷屏**。\n\n\
         ### 铁律 3：结果汇总、子线程收尾与工作台闭环（零 AI 味专业赏金标准）\n\
         - 收到子线程回传报告（`[Muteki] Subagent ... reported`）或 `[SRC_COORDINATOR_WAKEUP]` 时：若属觉醒事件请调用 `muteki_src_coordination_decide` 落盘决策；对已结束且 `delivery=awaiting_settle` 的委派子线程，整合结论后及时调用 `muteki_control_thread(changes=[{{\"thread_id\": <child_thread_id>, \"op\": \"settle\"}}])` 完成收尾，避免子线程长期停留在待收尾状态；核实确凿证据 ID（`exchange_id`、`asset_id`、`finding_id`）后，用极简要点向用户汇报已证实的事实、端点、请求/响应差异与代码级修复建议。\n\
         - 多模型与环境只读感知：可通过 `muteki_list_providers`、`muteki_get_settings`、`muteki_list_chat_plugins`、`muteki_list_platform_extensions`、`muteki_get_capabilities_overview` 查看可用模型、配置与插件，再按需委派。\n\
         - 未独立复现严禁晋升 `GREEN`；严禁空泛套话与无依据猜测。工作台快照、线索标注/合并/判重、报告草稿、资产研判与连接器状态请通过下方 10 个 SRC 渐进式披露子域工具直连操作。\n\n\
         ### MCP 网关直连与 10 个 SRC 渐进式披露子域速查\n\
         - `muteki-platform__muteki_platform_discover`: 传 `domain` 为以下 10 个 SRC 子域之一按需取 Schema（传 `\"src\"` 仅返回子域索引）：`src-auth` / `src-assets` / `src-findings` / `src-reports` / `src-traffic` / `src-connectors` / `src-monitors` / `src-identity` / `src-research` / `src-workbench`。\n\
         - `muteki-platform__muteki_platform_call`: `{{\"tool\": \"muteki_renew_src_authorization\", \"arguments\": {{\"targets\": [...], \"excluded_targets\": [...], \"notes\": \"...\"}}}}` / `{{\"tool\": \"muteki_src_todo_save\", \"arguments\": {{\"idempotency_key\": \"...\", \"title\": \"...\", \"delegated\": true}}}}` / `{{\"tool\": \"muteki_src_project_memory_save\", \"arguments\": {{\"preferences\": \"...\", \"notes\": \"...\", \"mode\": \"append\"}}}}` / `{{\"tool\": \"muteki_src_workbench_snapshot\", \"arguments\": {{}}}}` / `{{\"tool\": \"muteki_src_run_intervene\", \"arguments\": {{\"run_id\": \"...\", \"action\": \"steer\", \"note\": \"...\"}}}}`\n\
         - `muteki-collab__muteki_collab_call`: `{{\"tool\": \"muteki_start_thread\", \"arguments\": {{\"title\": \"...\", \"message\": \"...\", \"subagent\": true, \"wait\": false, \"delegation_profile\": \"scoped_execution\", \"todo_id\": \"...\", \"request_id\": \"...\"}}}}` / `muteki_send_message` / `muteki_control_thread` / `muteki_list_providers` / `muteki_get_settings` / `muteki_notify_when`",
        if goal.is_empty() { "（按用户对话确定）" } else { goal },
        if targets.is_empty() {
            "（尚未配置主动授权目标，仅做只读资料整理；主动探测需先确认目标范围）".to_owned()
        } else {
            targets.join(", ")
        },
        if subagents.is_empty() {
            "- 暂无".to_owned()
        } else {
            subagents.into_iter().map(|line| format!("- {line}")).collect::<Vec<_>>().join("\n")
        },
        if runs.is_empty() { "暂无".to_owned() } else { runs.join("; ") },
        if todos.is_empty() {
            "- 暂无".to_owned()
        } else {
            todos.into_iter().map(|line| format!("- {line}")).collect::<Vec<_>>().join("\n")
        },
        if pref.is_empty() { "（暂无记录）" } else { pref },
        if notes.is_empty() { "（暂无记录）" } else { notes },
    )
}

pub(crate) fn delegated_worker_instructions(
    s: &AppState,
    id: &str,
    child_tid: &str,
    delegation: &Value,
) -> String {
    if task(s, id).is_err() {
        return String::new();
    }
    let targets = policy(s, id)
        .ok()
        .and_then(|p| p["authorized_targets"].as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    let profile = delegation["profile"].as_str().unwrap_or("readonly_analysis");
    let todo_id = delegation["todo_id"].as_str().unwrap_or("");
    let memory = super::workbench_records::project_memory_get(s, id)
        .unwrap_or_else(|_| json!({"preferences": ""}));
    let pref = memory["preferences"].as_str().unwrap_or("").trim();
    format!(
        "## Muteki SRC 委派执行子线程（Scoped Worker）契约\n\
         当前 Task ID：`{id}` · 子线程 ID：`{child_tid}` · 委派档案：`{profile}` · 绑定待办：`{}`\n\
         已授权目标范围：{}\n\
         项目偏好（请严格遵守输出与工作规范）：{}\n\n\
         ### 执行规范\n\
         1. 你只负责完成父协调官委派的当前有界子任务。**严禁**派生嵌套子线程或越界访问非授权目标。\n\
         2. 若你的委派档案为 `scoped_execution`，你已自动获配 `muteki-browser`、`muteki-computer`、已启用的外部聊天插件（`muteki-packages`）及并发竞态/GraphQL/gRPC/会话导入等主动探测工具；实际主动 HTTP 验证请优先通过 `muteki_exchange_draft_save` + `muteki_exchange_draft_execute`（或 `muteki_asset_observe` / `muteki_finding_claim_create`）将真实请求响应与资产写入 SRC 持久库；若处于 `readonly_analysis` 档案，则仅使用只读查询并可提出工作台建议。\n\
         3. 任务完成后，**必须**在结束回合前调用一次 `muteki_report_result`，将结构化结果、精确端点/状态码、关键证据 ID（`exchange_id`、`asset_id`、`finding_id`）及未解决项完整写入 `text`。",
        if todo_id.is_empty() { "（未指定）" } else { todo_id },
        if targets.is_empty() {
            "（无主动目标授权）".to_owned()
        } else {
            targets.join(", ")
        },
        if pref.is_empty() { "精炼准确，以实证为准" } else { pref },
    )
}

fn canonical_conv_dir(s: &AppState, tid: &str, cwd: &std::path::Path) -> Option<(String, bool, std::path::PathBuf)> {
    let doc = chat::load(s, tid).ok()?;
    if doc["thread"]["workspace_id"]
        .as_str()
        .is_some_and(|w| !w.trim().is_empty())
    {
        return None;
    }
    let root = crate::native_chat::state_dir(s).join("conversations");
    let canon_root = std::fs::canonicalize(&root).unwrap_or(root);
    let canon_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    if !canon_cwd.starts_with(&canon_root) || !canon_cwd.is_dir() {
        return None;
    }
    let caller = super::super::resolve_authorized_src_caller(s, tid).ok().flatten()?;
    Some((caller.task_id, caller.delegation.is_none(), canon_cwd))
}

pub(crate) fn sync_session_files_before_turn(s: &AppState, tid: &str, cwd: &std::path::Path) {
    let Some((task_id, is_coordinator, dir)) = canonical_conv_dir(s, tid, cwd) else {
        return;
    };
    let instructions = src_thread_instructions(s, tid);
    if !instructions.is_empty() {
        let _ = std::fs::write(dir.join("AGENTS.md"), &instructions);
        let _ = std::fs::write(dir.join("CLAUDE.md"), &instructions);
    }
    if is_coordinator
        && let Ok(mem) = super::workbench_records::project_memory_get(s, &task_id)
    {
        let pref = mem["preferences"].as_str().unwrap_or("");
        let notes = mem["notes"].as_str().unwrap_or("");
        super::workbench_records::write_memory_files_to_dir(&dir, pref, notes);
    }
}

pub(crate) fn sync_session_files_after_turn(s: &AppState, tid: &str, cwd: &std::path::Path) {
    let Some((task_id, is_coordinator, dir)) = canonical_conv_dir(s, tid, cwd) else {
        return;
    };
    if !is_coordinator {
        return;
    }
    let sync_meta: Value = std::fs::read_to_string(dir.join(".muteki_memory_sync.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| json!({}));
    let base_pref_sha = sync_meta["preferences_sha256"].as_str().unwrap_or("");
    let base_notes_sha = sync_meta["notes_sha256"].as_str().unwrap_or("");
    let mut patch = serde_json::Map::new();
    if let Ok(pref_content) = std::fs::read_to_string(dir.join("preferences.md")) {
        let sha = format!("{:x}", Sha256::digest(pref_content.as_bytes()));
        if sha != base_pref_sha {
            patch.insert("preferences".to_owned(), json!(pref_content));
        }
    }
    if let Ok(notes_content) = std::fs::read_to_string(dir.join("notes.md")) {
        let sha = format!("{:x}", Sha256::digest(notes_content.as_bytes()));
        if sha != base_notes_sha {
            patch.insert("notes".to_owned(), json!(notes_content));
        }
    }
    if patch.is_empty() {
        return;
    }
    patch.insert("mode".to_owned(), json!("replace"));
    let _ = super::workbench_records::project_memory_save_direct(
        s,
        &task_id,
        &Value::Object(patch),
        None,
        &format!("thread:{tid}"),
    );
}

fn parse_ts(raw: Option<&str>) -> Option<DateTime<Utc>> {
    raw.and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc))
}

fn thread_busy_or_awaiting_human(doc: &Value) -> bool {
    if doc["thread"]["archived"] == true {
        return true;
    }
    if doc["thread"]["state"]["status"] == "running"
        || chat::queue_len(doc) > 0
        || doc["thread"]["state"]["queue_paused"] == true
    {
        return true;
    }
    let has_pending_approvals = doc["thread"]["state"]["pending_approvals"]
        .as_object()
        .is_some_and(|map| map.values().any(|row| row["status"] == "pending"));
    let has_pending_input = doc["thread"]["state"]["pending_user_input"]
        .as_object()
        .is_some_and(|map| !map.is_empty());
    has_pending_approvals || has_pending_input
}

fn host_of(raw: &str) -> Option<String> {
    let candidate = if raw.contains("://") {
        raw.to_owned()
    } else {
        format!("https://{raw}")
    };
    reqwest::Url::parse(&candidate)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
}

fn recall_context_for_batch(s: &AppState, id: &str, batch: &[Value]) -> Api<(Vec<String>, Value)> {
    let mut recalled_runs = BTreeSet::new();
    let mut change_ids = BTreeSet::new();
    let mut candidate_ids = BTreeSet::new();
    let mut finding_ids = BTreeSet::new();
    let mut report_ids = BTreeSet::new();
    let mut retest_ids = BTreeSet::new();
    let mut event_ids = BTreeSet::new();
    let mut target_hints = BTreeSet::new();

    for item in batch {
        let r = &item["refs"];
        if let Some(rid) = r["run_id"].as_str().filter(|v| !v.is_empty()) {
            recalled_runs.insert(rid.to_owned());
        }
        if let Some(rid) = r["source_run_id"].as_str().filter(|v| !v.is_empty()) {
            recalled_runs.insert(rid.to_owned());
        }
        for cid in r["change_ids"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .chain(r["change_id"].as_str())
            .filter(|v| !v.is_empty())
        {
            change_ids.insert(cid.to_owned());
        }
        for cid in r["candidate_ids"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .chain(r["candidate_id"].as_str())
            .chain(r["candidate_record_id"].as_str())
            .filter(|v| !v.is_empty())
        {
            candidate_ids.insert(cid.to_owned());
        }
        if let Some(fid) = r["finding_id"].as_str().filter(|v| !v.is_empty()) {
            finding_ids.insert(fid.to_owned());
        }
        if let Some(rid) = r["report_id"].as_str().filter(|v| !v.is_empty()) {
            report_ids.insert(rid.to_owned());
        }
        if let Some(rt) = r["retest_id"].as_str().filter(|v| !v.is_empty()) {
            retest_ids.insert(rt.to_owned());
        }
        if let Some(ev) = r["event_id"].as_str().filter(|v| !v.is_empty()) {
            event_ids.insert(ev.to_owned());
        }
        for t in r["target_refs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .chain(r["target_url"].as_str())
            .chain(r["target"].as_str())
            .filter(|v| !v.is_empty())
        {
            target_hints.insert(t.to_owned());
        }
    }

    let mut asset_ids = BTreeSet::new();
    let mut recalled_changes = Vec::new();
    for row in records(s, id, "monitor_change")? {
        let p = &row["payload"];
        let cid = p["change_id"].as_str().or_else(|| row["record_id"].as_str()).unwrap_or("");
        if !change_ids.contains(cid) && !change_ids.contains(row["natural_key"].as_str().unwrap_or("")) {
            continue;
        }
        for aid in p["affected_asset_ids"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            asset_ids.insert(aid.to_owned());
        }
        for fid in p["affected_finding_ids"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            finding_ids.insert(fid.to_owned());
        }
        for rid in p["affected_report_ids"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            report_ids.insert(rid.to_owned());
        }
        if let Some(url) = p["target_url"].as_str().or_else(|| p["target"].as_str()).filter(|v| !v.is_empty()) {
            target_hints.insert(url.to_owned());
        }
        recalled_changes.push(json!({
            "change_id": cid,
            "title": p["title"].as_str().or_else(|| p["summary"].as_str()).unwrap_or(""),
            "before_version": p["before_version"],
            "after_version": p["after_version"],
            "artifact": p["artifact"],
            "affected_asset_ids": p["affected_asset_ids"],
            "affected_finding_ids": p["affected_finding_ids"],
            "affected_report_ids": p["affected_report_ids"]
        }));
    }

    let hint_hosts: BTreeSet<String> = target_hints.iter().filter_map(|t| host_of(t)).collect();
    let mut recalled_assets = Vec::new();
    for row in records(s, id, "asset")? {
        let p = &row["payload"];
        let aid = p["asset_id"].as_str().or_else(|| row["record_id"].as_str()).unwrap_or("");
        let canonical = p["canonical_key"].as_str().or_else(|| p["url"].as_str()).unwrap_or("");
        let matches = asset_ids.contains(aid)
            || (!canonical.is_empty()
                && (target_hints.iter().any(|h| canonical.contains(h) || h.contains(canonical))
                    || host_of(canonical).is_some_and(|ch| hint_hosts.contains(&ch))));
        if matches {
            if let Some(u) = p["url"].as_str().filter(|v| !v.is_empty()) {
                target_hints.insert(u.to_owned());
            }
            recalled_assets.push(json!({
                "asset_id": aid,
                "canonical_key": canonical,
                "version": p["version"],
                "status": p["status"]
            }));
        }
    }

    let latest_plan = records(s, id, "research_plan")?
        .into_iter()
        .max_by_key(|r| r["payload"]["version"].as_u64().unwrap_or(0))
        .map(|r| {
            json!({
                "record_id": r["record_id"],
                "version": r["payload"]["version"],
                "question": r["payload"]["question"],
                "current_judgement": r["payload"]["current_judgement"],
                "gaps": r["payload"]["gaps"]
            })
        });

    for binding in records(s, id, "run_binding")? {
        if !binding["payload"]["destroyed_at"].is_null() {
            continue;
        }
        let Some(rid) = binding["payload"]["run_id"].as_str() else {
            continue;
        };
        let r_target = binding["payload"]["target_url"].as_str().unwrap_or("");
        if host_of(r_target).is_some_and(|h| hint_hosts.contains(&h)) {
            recalled_runs.insert(rid.to_owned());
        }
    }

    let reports = records(s, id, "report_version")?;
    let facts = records(s, id, "fact")?;
    let dead_ends = records(s, id, "dead_end")?;
    let worker_candidates = records(s, id, "worker_finding_candidate")?;
    let mut lines = Vec::new();
    if !recalled_changes.is_empty() {
        lines.push("## 变化对象与差异引用".to_owned());
        for c in &recalled_changes {
            lines.push(format!("- {}", serde_json::to_string(c).unwrap_or_default()));
        }
        lines.push(String::new());
    }
    if !recalled_assets.is_empty() {
        lines.push("## 关联资产".to_owned());
        for a in recalled_assets.iter().take(12) {
            lines.push(format!("- {}", serde_json::to_string(a).unwrap_or_default()));
        }
        lines.push(String::new());
    }
    if let Some(plan) = &latest_plan {
        lines.push(format!("## 当前研究计划：{}", serde_json::to_string(plan).unwrap_or_default()));
        lines.push(String::new());
    }
    if !recalled_runs.is_empty() {
        lines.push("## 关联 Run 召回摘要（完整报文与产物可通过稳定 ID 按需读取）".to_owned());
        for rid in &recalled_runs {
            let state = s
                .store
                .get("runs", rid)
                .ok()
                .flatten()
                .map(|r| r["status"].as_str().unwrap_or("unknown").to_owned())
                .unwrap_or_else(|| "unknown".to_owned());
            let r_reports: Vec<Value> = reports
                .iter()
                .filter(|r| r["payload"]["source_run_id"].as_str() == Some(rid.as_str()))
                .map(|r| {
                    json!({
                        "record_id": r["record_id"],
                        "artifact_id": r["payload"]["artifact"]["artifact_id"],
                        "payload": r["payload"]["source"]["payload"]
                    })
                })
                .collect();
            let r_candidates: Vec<Value> = worker_candidates
                .iter()
                .filter(|r| r["payload"]["source_run_id"].as_str() == Some(rid.as_str()))
                .map(|r| r["payload"].clone())
                .collect();
            let r_facts_count = facts
                .iter()
                .filter(|r| r["payload"]["source_run_id"].as_str() == Some(rid.as_str()))
                .count();
            let r_dead_ends_count = dead_ends
                .iter()
                .filter(|r| r["payload"]["source_run_id"].as_str() == Some(rid.as_str()))
                .count();
            lines.push(format!(
                "- Run `{rid}`：状态 `{state}`，Fact {r_facts_count} 条，DeadEnd {r_dead_ends_count} 条，候选发现 {} 条，报告事件 {} 条：{} {}",
                r_candidates.len(),
                r_reports.len(),
                serde_json::to_string(&r_candidates).unwrap_or_default(),
                serde_json::to_string(&r_reports).unwrap_or_default()
            ));
        }
        lines.push(String::new());
    }

    let frozen_reports = records(s, id, "frozen_report")?;
    let mut recalled_reports = Vec::new();
    for r in &frozen_reports {
        let p = &r["payload"];
        let rid = p["report_id"].as_str().unwrap_or("");
        let fid = p["finding_id"].as_str().unwrap_or("");
        let src_run = p["source_run_id"].as_str().unwrap_or("");
        let target = p["document"]["target"].as_str().unwrap_or("");
        if report_ids.contains(rid)
            || finding_ids.contains(fid)
            || recalled_runs.contains(src_run)
            || host_of(target).is_some_and(|h| hint_hosts.contains(&h))
        {
            if !fid.is_empty() {
                finding_ids.insert(fid.to_owned());
            }
            recalled_reports.push(json!({
                "report_id": rid,
                "finding_id": fid,
                "readiness": p["review"]["readiness"],
                "effective_severity": p["effective_severity"],
                "evidence_refs": p["evidence_refs"]
            }));
        }
    }
    let mut recalled_findings = Vec::new();
    for wf in records(s, id, "workbench_finding")? {
        let p = &wf["payload"];
        let fid = p["finding_id"].as_str().unwrap_or("");
        let target = p["target"].as_str().unwrap_or("");
        if finding_ids.contains(fid) || host_of(target).is_some_and(|h| hint_hosts.contains(&h)) {
            let st = super::finding::status(s, id, &json!({"finding_id": fid})).ok();
            recalled_findings.push(json!({
                "finding_id": fid,
                "title": p["title"],
                "state": st.as_ref().map(|v| &v["state"]),
                "stale": st.as_ref().map(|v| &v["stale"])
            }));
        }
    }
    if !recalled_findings.is_empty() || !recalled_reports.is_empty() {
        lines.push("## 关联 Finding 与冻结报告".to_owned());
        for f in &recalled_findings {
            lines.push(format!("- Finding: {}", serde_json::to_string(f).unwrap_or_default()));
        }
        for r in &recalled_reports {
            lines.push(format!("- Report: {}", serde_json::to_string(r).unwrap_or_default()));
        }
        lines.push(String::new());
    }
    let recalled_retests: Vec<Value> = records(s, id, "finding_retest")?
        .into_iter()
        .map(|r| r["payload"].clone())
        .filter(|p| {
            let rtid = p["retest_id"].as_str().unwrap_or("");
            let fid = p["finding_id"].as_str().unwrap_or("");
            let rid = p["report_id"].as_str().unwrap_or("");
            retest_ids.contains(rtid) || finding_ids.contains(fid) || report_ids.contains(rid)
        })
        .collect();
    if !recalled_retests.is_empty() {
        lines.push("## 关联复测记录".to_owned());
        for rt in &recalled_retests {
            lines.push(format!(
                "- `{}` finding=`{}` status=`{}` result=`{}`",
                rt["retest_id"].as_str().unwrap_or(""),
                rt["finding_id"].as_str().unwrap_or(""),
                rt["status"].as_str().unwrap_or(""),
                rt["result"].as_str().unwrap_or("pending")
            ));
        }
        lines.push(String::new());
    }

    let recalled_refs = json!({
        "run_ids": recalled_runs.into_iter().collect::<Vec<_>>(),
        "change_ids": change_ids.into_iter().collect::<Vec<_>>(),
        "candidate_ids": candidate_ids.into_iter().collect::<Vec<_>>(),
        "event_ids": event_ids.into_iter().collect::<Vec<_>>(),
        "asset_ids": recalled_assets.iter().filter_map(|a| a["asset_id"].as_str()).collect::<Vec<_>>(),
        "finding_ids": finding_ids.into_iter().collect::<Vec<_>>(),
        "report_ids": recalled_reports.iter().filter_map(|r| r["report_id"].as_str()).collect::<Vec<_>>(),
        "retest_ids": recalled_retests.iter().filter_map(|r| r["retest_id"].as_str()).collect::<Vec<_>>()
    });
    Ok((lines, recalled_refs))
}

fn build_wakeup_prompt(s: &AppState, id: &str, batch: &[Value]) -> Api<(String, Value)> {
    let t = task(s, id)?;
    let p = policy(s, id)?;
    let cov = coverage::audit(s, id)?;
    let (recalled_lines, recalled_refs) = recall_context_for_batch(s, id, batch)?;
    let mut lines = vec![
        "[SRC_COORDINATOR_WAKEUP] 后台事件已合并送达，请作为 SRC 首席研究协调官完成研判并记录决策。".to_owned(),
        String::new(),
        format!("- Task：`{id}`（{}）", t["title"].as_str().unwrap_or("")),
        format!("- 目标：{}", t["input"]["goal"].as_str().unwrap_or("")),
        format!("- 授权范围：{}", p["authorized_targets"]),
        format!(
            "- 当前覆盖与阻断概览：状态 `{}`，已验证 {}/{}，开放阻断 {} 项，高优待验 {} 项",
            cov["status"].as_str().unwrap_or("unmodeled"),
            cov["verified_items"],
            cov["applicable_items"],
            cov["open_blockers"].as_array().map_or(0, Vec::len),
            cov["critical_unresolved_items"].as_array().map_or(0, Vec::len),
        ),
        String::new(),
        "## 本次合并唤醒事件（请在 `muteki_src_coordination_decide` 中传入下列 `wakeup_ids`）".to_owned(),
    ];
    for item in batch {
        let wid = item["wakeup_id"].as_str().unwrap_or("");
        let kind = item["source_kind"].as_str().unwrap_or("");
        let title = item["title"].as_str().unwrap_or("");
        let summary = item["summary"].as_str().unwrap_or("");
        lines.push(format!("- `{wid}` [{kind}] {title}：{summary}（refs: {}）", item["refs"]));
    }
    if !recalled_lines.is_empty() {
        lines.push(String::new());
        lines.extend(recalled_lines);
    }
    let open_blockers = cov["open_blockers"].as_array().cloned().unwrap_or_default();
    if !open_blockers.is_empty() {
        lines.push(format!("## 当前开放环境阻断（非漏洞）：{}", Value::Array(open_blockers)));
        lines.push(String::new());
    }
    lines.push(
        "请基于真实证据选择处理动作（忽略无价值噪音、跟进已有 Run、新开专项/独立复现 Run、整理 Finding Claim 并冻结报告、或仅在确需人工介入/交付结论时简明告知用户），并务必调用 `muteki_src_coordination_decide` 落盘本次决策。".to_owned(),
    );
    Ok((lines.join("\n"), recalled_refs))
}

pub(super) async fn reconcile_and_wake(s: Arc<AppState>, id: &str) -> Api<Value> {
    static WAKE_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let _guard = WAKE_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let t = task(&s, id)?;
    if t["lifecycle"]["state"] != "active" {
        return Ok(json!({"dispatched": false, "reason": "task_inactive"}));
    }

    let existing_keys: HashSet<String> = records(&s, id, "coordination_wakeup")?
        .into_iter()
        .filter_map(|row| row["natural_key"].as_str().map(str::to_owned))
        .collect();

    for binding in records(&s, id, "run_binding")? {
        if !binding["payload"]["destroyed_at"].is_null() {
            continue;
        }
        let Some(run_id) = binding["payload"]["run_id"].as_str() else {
            continue;
        };
        let Some(run) = s.store.get("runs", run_id).map_err(db)? else {
            continue;
        };
        let status = run["status"].as_str().unwrap_or("");
        if !matches!(
            status,
            "completed" | "finished" | "solved" | "failed" | "stopped" | "paused"
        ) {
            continue;
        }
        let generation = run["execution_generation"].as_u64().unwrap_or(1);
        let source_key = format!("run_state:{run_id}:g{generation}:{status}");
        if existing_keys.contains(&source_key) {
            continue;
        }
        let target = binding["payload"]["target_url"].as_str().unwrap_or("");
        let question = binding["payload"]["contract"]["question"].as_str().unwrap_or("");
        let _ = enqueue_wakeup(
            &s,
            id,
            &source_key,
            "run_milestone",
            &format!("后台 Run `{run_id}` 状态变更为 `{status}`"),
            &format!("目标 {target} · 问题：{question}"),
            json!({"run_id": run_id, "generation": generation, "status": status, "target_url": target}),
        )?;
    }

    for row in records(&s, id, "target_blocker")? {
        let b = &row["payload"];
        if b["state"] != "open" {
            continue;
        }
        let bid = b["blocker_id"].as_str().unwrap_or("");
        if bid.is_empty() {
            continue;
        }
        let source_key = format!("blocker:{bid}");
        if existing_keys.contains(&source_key) {
            continue;
        }
        let _ = enqueue_wakeup(
            &s,
            id,
            &source_key,
            "target_blocker",
            &format!("检测到环境阻断 `{}` ({})", bid, b["category"].as_str().unwrap_or("")),
            b["detail"].as_str().unwrap_or(""),
            json!({"blocker_id": bid, "category": b["category"], "target": b["target"]}),
        )?;
    }

    for row in records(&s, id, "coordination_decision")? {
        let mut decision = row["payload"].clone();
        if decision["effect_state"] != "pending_confirmation" {
            continue;
        }
        let (next_state, refs) = evaluate_decision_effect(&s, id, &decision)?;
        if next_state == "pending_confirmation" {
            continue;
        }
        let stamp = now();
        decision["effect_state"] = json!(next_state);
        decision["effect_refs"] = refs;
        if next_state == "effect_confirmed" {
            decision["effect_confirmed_at"] = json!(stamp);
            let stamp_dt = parse_ts(Some(&stamp)).unwrap_or_else(Utc::now);
            let mut earliest: Option<DateTime<Utc>> = None;
            let all_wakeups = records(&s, id, "coordination_wakeup")?;
            for wid in decision["wakeup_ids"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                if let Some(wrow) = all_wakeups.iter().find(|r| r["payload"]["wakeup_id"].as_str() == Some(wid))
                    && let Some(dt) = parse_ts(wrow["payload"]["created_at"].as_str())
                {
                    earliest = Some(match earliest {
                        Some(cur) if cur <= dt => cur,
                        _ => dt,
                    });
                }
            }
            if let Some(dt) = earliest.or_else(|| parse_ts(decision["created_at"].as_str())) {
                decision["execution_latency_ms"] = json!((stamp_dt - dt).num_milliseconds().max(0));
            }
        }
        let did = row["natural_key"].as_str().unwrap_or("");
        let _ = save_rec(&s, id, "coordination_decision", did, decision.clone(), row["revision"].as_u64());
        for wrow in records(&s, id, "coordination_wakeup")? {
            if wrow["payload"]["decision_id"] == decision["decision_id"] {
                let mut wp = wrow["payload"].clone();
                wp["effect_state"] = json!(next_state);
                if next_state == "effect_confirmed" {
                    wp["state"] = json!("effect_confirmed");
                }
                wp["updated_at"] = json!(stamp);
                let _ = save_rec(
                    &s,
                    id,
                    "coordination_wakeup",
                    wrow["natural_key"].as_str().unwrap_or(""),
                    wp,
                    wrow["revision"].as_u64(),
                );
            }
        }
    }

    let Some(thread_id) = t["binding"]["thread_id"]
        .as_str()
        .or_else(|| t["thread_id"].as_str())
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
    else {
        return Ok(json!({"dispatched": false, "reason": "no_bound_thread"}));
    };
    let Ok(doc) = chat::load(&s, &thread_id) else {
        return Ok(json!({"dispatched": false, "reason": "thread_unavailable"}));
    };

    for row in records(&s, id, "coordination_wakeup")? {
        let state_str = row["payload"]["state"].as_str().unwrap_or("");
        if state_str == "queued" {
            let queue_id = row["payload"]["wakeup_queue_id"].as_str().unwrap_or("");
            let cmd_id = row["payload"]["wakeup_command_id"].as_str().unwrap_or("");
            let matched_turn = doc["turns"].as_array().and_then(|turns| {
                turns.iter().find(|t| {
                    (!queue_id.is_empty() && t["queue_id"].as_str() == Some(queue_id))
                        || (!cmd_id.is_empty() && t["command_id"].as_str() == Some(cmd_id))
                })
            });
            if let Some(turn) = matched_turn {
                let resolved_turn_id = turn["turn_id"].as_str().unwrap_or("");
                let t_status = turn["status"].as_str().unwrap_or("");
                let mut next = row["payload"].clone();
                next["wakeup_turn_id"] = json!(resolved_turn_id);
                if t_status == "completed" {
                    next["state"] = json!(if next["decision_id"].is_string() { "decided" } else { "unactioned" });
                } else if matches!(t_status, "failed" | "interrupted" | "cancelled") {
                    let attempts = next["attempts"].as_u64().unwrap_or(1);
                    next["last_error"] = json!(format!("wakeup turn {resolved_turn_id} ended as {t_status}"));
                    next["state"] = json!(if attempts < 2 { "pending" } else { "delivery_failed" });
                } else {
                    next["state"] = json!("dispatched");
                }
                next["updated_at"] = json!(now());
                let _ = save_rec(
                    &s,
                    id,
                    "coordination_wakeup",
                    row["natural_key"].as_str().unwrap_or(""),
                    next,
                    row["revision"].as_u64(),
                );
            } else {
                let still_queued = doc["queue"].as_array().is_some_and(|queue| {
                    queue.iter().any(|item| {
                        (!queue_id.is_empty() && item["queue_id"].as_str() == Some(queue_id))
                            || (!cmd_id.is_empty() && item["command_id"].as_str() == Some(cmd_id))
                    })
                });
                if !still_queued {
                    let attempts = row["payload"]["attempts"].as_u64().unwrap_or(1);
                    let mut next = row["payload"].clone();
                    next["last_error"] = json!("queued wakeup dropped before turn execution");
                    next["state"] = json!(if attempts < 2 { "pending" } else { "delivery_failed" });
                    next["updated_at"] = json!(now());
                    let _ = save_rec(
                        &s,
                        id,
                        "coordination_wakeup",
                        row["natural_key"].as_str().unwrap_or(""),
                        next,
                        row["revision"].as_u64(),
                    );
                }
            }
            continue;
        }
        if state_str != "dispatched" {
            continue;
        }
        let Some(turn_id) = row["payload"]["wakeup_turn_id"].as_str().filter(|v| !v.is_empty()) else {
            continue;
        };
        let Some(turn) = doc["turns"]
            .as_array()
            .and_then(|turns| turns.iter().find(|t| t["turn_id"] == turn_id))
        else {
            continue;
        };
        let t_status = turn["status"].as_str().unwrap_or("");
        if t_status == "completed" {
            let mut next = row["payload"].clone();
            // If the model finished the turn without calling decide(), flag as unactioned (invalid wakeup).
            next["state"] = json!(if next["decision_id"].is_string() {
                "decided"
            } else {
                "unactioned"
            });
            next["updated_at"] = json!(now());
            let _ = save_rec(
                &s,
                id,
                "coordination_wakeup",
                row["natural_key"].as_str().unwrap_or(""),
                next,
                row["revision"].as_u64(),
            );
        } else if matches!(t_status, "failed" | "interrupted" | "cancelled") {
            let attempts = row["payload"]["attempts"].as_u64().unwrap_or(1);
            let mut next = row["payload"].clone();
            next["last_error"] = json!(format!("wakeup turn {turn_id} ended as {t_status}"));
            next["state"] = json!(if attempts < 2 { "pending" } else { "delivery_failed" });
            next["updated_at"] = json!(now());
            let _ = save_rec(
                &s,
                id,
                "coordination_wakeup",
                row["natural_key"].as_str().unwrap_or(""),
                next,
                row["revision"].as_u64(),
            );
        }
    }

    let cfg = policy_get(&s, id)?;
    if cfg["enabled"] != true {
        return Ok(json!({"dispatched": false, "reason": "auto_followup_disabled"}));
    }

    let mut auto_actions = Vec::new();
    let max_concurrent_runs = cfg["max_concurrent_runs"].as_u64().unwrap_or(3) as usize;
    let active_bindings = records(&s, id, "run_binding")?
        .into_iter()
        .filter(|b| b["payload"]["destroyed_at"].is_null())
        .filter(|b| {
            let rid = b["payload"]["run_id"].as_str().unwrap_or("");
            s.store
                .get("runs", rid)
                .ok()
                .flatten()
                .is_some_and(|run| {
                    matches!(
                        run["status"].as_str(),
                        Some("running" | "starting" | "stopping" | "paused" | "quota_paused" | "frozen")
                    )
                })
        })
        .count();
    let existing_bound_targets: HashSet<String> = records(&s, id, "run_binding")?
        .into_iter()
        .filter(|b| b["payload"]["destroyed_at"].is_null())
        .filter_map(|b| b["payload"]["target_url"].as_str().map(str::to_owned))
        .collect();
    let current_policy = policy(&s, id).ok();

    let has_active_auto_recon_wakeup = records(&s, id, "coordination_wakeup")?.into_iter().any(|w| {
        let p = &w["payload"];
        let active_state = matches!(
            p["state"].as_str(),
            Some("pending" | "queued" | "dispatched")
        );
        let is_auto_recon = p["refs"]["auto_action"].as_str() == Some("auto_recon")
            || w["natural_key"]
                .as_str()
                .is_some_and(|k| k.starts_with("auto_recon:"));
        active_state && is_auto_recon
    });

    if cfg["auto_recon"] == true
        && !has_active_auto_recon_wakeup
        && active_bindings < max_concurrent_runs
        && let Some(p) = current_policy.as_ref()
    {
        for row in records(&s, id, "asset")? {
            let asset = &row["payload"];
            if asset["lifecycle"] != "active" {
                continue;
            }
            let at = asset["type"].as_str().unwrap_or("");
            if !matches!(
                at,
                "web_origin" | "http_route" | "api_endpoint" | "http_endpoint"
            ) {
                continue;
            }
            let candidate_url = super::asset_query_url_of(asset);
            if candidate_url.is_empty()
                || existing_bound_targets.contains(&candidate_url)
                || !crate::runs::src_api::in_policy(p, &candidate_url)
            {
                continue;
            }
            let marker_key = format!(
                "auto_recon:{}",
                &format!("{:x}", Sha256::digest(candidate_url.as_bytes()))[..16]
            );
            if rec(&s, id, "coordination_auto_action", &marker_key).is_ok() {
                continue;
            }
            let _ = save_rec(
                &s,
                id,
                "coordination_auto_action",
                &marker_key,
                json!({
                    "kind": "auto_recon",
                    "target_url": candidate_url,
                    "asset_id": asset["asset_id"],
                    "created_at": now()
                }),
                Some(0),
            );
            let _ = enqueue_wakeup(
                &s,
                id,
                &marker_key,
                "run_milestone",
                &format!("自动侦察触发：发现授权范围内新 Web 目标 `{candidate_url}`"),
                "在授权范围与并发预算内建议/发起增量侦察验证",
                json!({
                    "auto_action": "auto_recon",
                    "target_url": candidate_url,
                    "asset_id": asset["asset_id"],
                    "target_refs": [candidate_url]
                }),
            );
            auto_actions.push(json!({
                "kind": "auto_recon",
                "target_url": candidate_url,
                "asset_id": asset["asset_id"]
            }));
            break;
        }
    }

    if cfg["auto_reproduce"] == true && let Some(p) = current_policy.as_ref() {
        let existing_repro_findings: HashSet<String> = records(&s, id, "report_reproduction")?
            .into_iter()
            .filter_map(|r| r["payload"]["finding_id"].as_str().map(str::to_owned))
            .collect();
        for frow in records(&s, id, "finding")? {
            let f = &frow["payload"];
            if !f["destroyed_at"].is_null() || f["state"] == "retracted" {
                continue;
            }
            let sev = f["severity"].as_str().unwrap_or("");
            if !matches!(sev, "high" | "critical") {
                continue;
            }
            let fid = f["finding_id"].as_str().unwrap_or("");
            if fid.is_empty() || existing_repro_findings.contains(fid) {
                continue;
            }
            let marker_key = format!("auto_reproduce:{fid}");
            if rec(&s, id, "coordination_auto_action", &marker_key).is_ok() {
                continue;
            }
            let mut ex_ids = Vec::new();
            let mut all_in_scope = false;
            if let Some(arr) = f["exchange_ids"].as_array() {
                let mut valid = !arr.is_empty();
                for v in arr {
                    if let Some(eid) = v.as_str()
                        && let Ok(ex_row) = rec(&s, id, "http_exchange", eid)
                        && let Some(obs_url) = ex_row["payload"]["observed_url"].as_str()
                        && crate::runs::src_api::in_policy(p, obs_url)
                    {
                        ex_ids.push(json!(eid));
                    } else {
                        valid = false;
                        break;
                    }
                }
                all_in_scope = valid && !ex_ids.is_empty();
            }
            if !all_in_scope {
                continue;
            }
            let _ = save_rec(
                &s,
                id,
                "coordination_auto_action",
                &marker_key,
                json!({
                    "kind": "auto_reproduce",
                    "finding_id": fid,
                    "exchange_ids": ex_ids,
                    "created_at": now()
                }),
                Some(0),
            );
            if p["defaults"]["effect"] == "allow" {
                let _ = super::auto_reproduce_finding(
                    s.clone(),
                    id,
                    &json!({"finding_id": fid, "exchange_ids": ex_ids}),
                )
                .await;
            }
            let _ = enqueue_wakeup(
                &s,
                id,
                &marker_key,
                "run_milestone",
                &format!("自动复现触发：中高危发现 `{fid}` 已调度独立验证"),
                "已在策略授权范围和预算内触发独立复现或排队等待确认",
                json!({
                    "auto_action": "auto_reproduce",
                    "finding_id": fid,
                    "exchange_ids": ex_ids
                }),
            );
            auto_actions.push(json!({
                "kind": "auto_reproduce",
                "finding_id": fid,
                "exchange_ids": ex_ids
            }));
            break;
        }
    }

    let all_rows = records(&s, id, "coordination_wakeup")?;
    let pending_rows: Vec<Value> = all_rows
        .iter()
        .filter(|r| r["payload"]["state"] == "pending")
        .cloned()
        .collect();
    if pending_rows.is_empty() {
        return Ok(json!({
            "dispatched": false,
            "reason": "no_pending_wakeups",
            "auto_actions": auto_actions
        }));
    }

    let window_secs = cfg["merge_window_seconds"].as_i64().unwrap_or(5).max(1);
    let now_utc = Utc::now();
    let oldest_ready = pending_rows.iter().any(|r| {
        parse_ts(r["payload"]["created_at"].as_str())
            .is_none_or(|created| now_utc - created >= Duration::seconds(window_secs))
    });
    if !oldest_ready {
        return Ok(json!({"dispatched": false, "reason": "coalescing_window"}));
    }

    let max_per_hour = cfg["max_wakeups_per_hour"].as_u64().unwrap_or(24) as usize;
    let recent_dispatched = all_rows
        .iter()
        .filter(|r| {
            r["payload"]["wakeup_turn_id"].is_string()
                && parse_ts(r["payload"]["dispatched_at"].as_str())
                    .is_some_and(|at| now_utc - at < Duration::hours(1))
        })
        .count();
    if recent_dispatched >= max_per_hour {
        return Ok(json!({"dispatched": false, "reason": "hourly_wakeup_budget_exhausted"}));
    }
    let max_coord_tokens = cfg["max_coordination_tokens"].as_u64().unwrap_or(0);
    if max_coord_tokens > 0 {
        let all_payloads: Vec<Value> = all_rows.iter().map(|r| r["payload"].clone()).collect();
        let (tokens_used, _) = coordination_usage(&s, Some(&doc), &all_payloads);
        if tokens_used >= max_coord_tokens {
            return Ok(json!({
                "dispatched": false,
                "reason": "coordination_token_budget_exhausted",
                "coordination_tokens_used": tokens_used,
                "max_coordination_tokens": max_coord_tokens
            }));
        }
    }

    if thread_busy_or_awaiting_human(&doc) {
        return Ok(json!({"dispatched": false, "reason": "main_thread_busy"}));
    }
    if chat::resolve_runtime(&s, &doc["runtime"]).is_err() {
        return Ok(json!({"dispatched": false, "reason": "runtime_unconfigured"}));
    }

    let batch_payloads: Vec<Value> = pending_rows.iter().map(|r| r["payload"].clone()).collect();
    let batch_ids: Vec<String> = batch_payloads
        .iter()
        .filter_map(|p| p["wakeup_id"].as_str().map(str::to_owned))
        .collect();
    let batch_hash = &format!("{:x}", Sha256::digest(batch_ids.join(",").as_bytes()))[..16];
    let total_attempts: u64 = batch_payloads
        .iter()
        .map(|p| p["attempts"].as_u64().unwrap_or(0))
        .sum();
    let cmd_id = format!("src-wakeup-{id}-{batch_hash}-a{total_attempts}");
    let (prompt, recalled_refs) = build_wakeup_prompt(&s, id, &batch_payloads)?;

    let send_body = json!({
        "command_id": cmd_id,
        "idempotency_key": cmd_id,
        "command_type": "conversation.turn.send",
        "actor": { "kind": "system", "id": "src.coordinator" },
        "payload": {
            "text": prompt,
            "client_message_id": cmd_id
        }
    });

    match chat::command_from_platform(s.clone(), thread_id.clone(), send_body, None).await {
        Ok(axum::Json(resp)) => {
            let output = &resp["receipt"]["output"];
            let turn_id = output["turn_id"]
                .as_str()
                .unwrap_or("")
                .to_owned();
            let queued = output["queued"] == true || turn_id.is_empty();
            let queue_id = output["queue_id"]
                .as_str()
                .unwrap_or("")
                .to_owned();
            let stamp = now();
            for row in &pending_rows {
                let mut next = row["payload"].clone();
                if turn_id.is_empty() && queued {
                    next["state"] = json!("queued");
                    next["wakeup_turn_id"] = Value::Null;
                    next["wakeup_queue_id"] = json!(queue_id);
                    next["wakeup_command_id"] = json!(cmd_id);
                } else {
                    next["state"] = json!("dispatched");
                    next["wakeup_turn_id"] = json!(turn_id);
                    next["wakeup_command_id"] = json!(cmd_id);
                }
                next["dispatched_at"] = json!(stamp);
                next["recalled_refs"] = recalled_refs.clone();
                next["attempts"] = json!(next["attempts"].as_u64().unwrap_or(0) + 1);
                next["updated_at"] = json!(stamp);
                let _ = save_rec(
                    &s,
                    id,
                    "coordination_wakeup",
                    row["natural_key"].as_str().unwrap_or(""),
                    next,
                    row["revision"].as_u64(),
                );
            }
            Ok(json!({
                "dispatched": !turn_id.is_empty(),
                "queued": turn_id.is_empty() && queued,
                "turn_id": turn_id,
                "queue_id": queue_id,
                "command_id": cmd_id,
                "wakeup_ids": batch_ids,
                "recalled_refs": recalled_refs
            }))
        }
        Err((_, axum::Json(err_body))) => Ok(json!({
            "dispatched": false,
            "reason": "turn_dispatch_deferred",
            "detail": err_body
        })),
    }
}

pub(super) async fn run_intervene(
    s: std::sync::Arc<AppState>,
    id: &str,
    body: &Value,
) -> Api<Value> {
    let _ = task(&s, id)?;
    let run_id = require(body, "run_id")?.trim();
    if !records(&s, id, "run_binding")?
        .into_iter()
        .any(|row| row["payload"]["run_id"] == run_id)
    {
        return Err(Error::new(
            StatusCode::NOT_FOUND,
            "run.not_found",
            format!("Run {run_id:?} is not bound to Task {id}"),
        ));
    }
    let action = match require(body, "action")?.trim() {
        "pause" => "pause",
        "resume" => "resume",
        "freeze" => "freeze",
        "thaw" => "thaw",
        "stop" => "stop",
        "steer" => "steer",
        other => {
            return Err(Error::invalid(format!(
                "action must be 'pause', 'resume', 'freeze', 'thaw', 'stop', or 'steer', got '{other}'"
            )))
        }
    };
    let note = body["note"].as_str().unwrap_or("").trim().to_owned();
    let exclude_paths = body["exclude_paths"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let focus_targets = body["focus_targets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();

    if matches!(action, "pause" | "resume" | "freeze" | "thaw" | "stop") {
        let control_action = match action {
            "freeze" => "freeze",
            "thaw" => "thaw",
            "stop" => "stop",
            "pause" => "pause",
            _ => "resume",
        };
        let response = crate::runs::control_run_trusted(
            axum::extract::State(s.clone()),
            axum::extract::Path(run_id.to_owned()),
            axum::Json(json!({
                "action": control_action,
                "scope": "global",
                "command_id": uuid::Uuid::new_v4().to_string()
            })),
        )
        .await;
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 2 * 1024 * 1024)
            .await
            .map_err(db)?;
        let result: Value = serde_json::from_slice(&bytes).map_err(db)?;
        if !status.is_success()
            || result["ok"] == false
            || !matches!(
                result["status"].as_str(),
                Some("applied" | "effect_observed")
            )
        {
            return Err(Error::new(
                StatusCode::CONFLICT,
                "run_intervention_failed",
                result.to_string(),
            ));
        }
    }
    let prior = rec(&s, id, "run_intervention", run_id).ok();
    let rev = prior.as_ref().and_then(|r| r["revision"].as_u64());
    let next_state = match action {
        "pause" => "paused",
        "freeze" => "frozen",
        "resume" | "thaw" => "active",
        "stop" => "stopped",
        _ => prior
            .as_ref()
            .and_then(|r| r["payload"]["state"].as_str())
            .unwrap_or("active"),
    };
    let doc = json!({
        "run_id": run_id,
        "task_id": id,
        "state": next_state,
        "last_action": action,
        "note": note,
        "exclude_paths": exclude_paths,
        "focus_targets": focus_targets,
        "updated_at": now(),
    });
    save_rec(&s, id, "run_intervention", run_id, doc.clone(), rev)?;

    if let Ok(graph) = crate::runs::graph::SharedGraph::open(&s, run_id) {
        let _ = graph.append_event(
            "operator_intervene",
            "operator",
            &doc,
            None,
            true,
            1.0,
            None,
        );
    }

    let mut steered_workers = (0usize, 0usize);
    if action == "steer" {
        let msg = if note.is_empty() {
            format!("[OPERATOR INTERVENTION] action={action}; exclude_paths={exclude_paths:?}; focus_targets={focus_targets:?}")
        } else {
            format!("[OPERATOR INTERVENTION] action={action}: {note}")
        };
        steered_workers = crate::runs::steer_src_workers(&s, run_id, &msg).await;
    }

    Ok(json!({
        "intervention": doc,
        "steered_delivered": steered_workers.0,
        "steered_live_workers": steered_workers.1,
    }))
}
