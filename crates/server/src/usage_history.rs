//! Import only persisted numerical usage. Never scan native sessions or accounts.
use crate::store::Store;
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};
fn string<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}
fn number(v: &Value) -> Option<f64> {
    v.as_f64().filter(|n| n.is_finite() && *n >= 0.0)
}
pub fn engine(raw: &str) -> String {
    let text = raw.trim().to_lowercase();
    let name = text.split(':').next().unwrap_or("");
    let name = name
        .strip_prefix("cli.")
        .unwrap_or(name)
        .split('.')
        .next()
        .unwrap_or("");
    match name {
        "" => "unknown",
        "claude-code" | "claude_code" => "claude",
        "open-code" | "open_code" => "opencode",
        "cursor-agent" => "cursor",
        "codex-cli" => "codex",
        other => other,
    }
    .into()
}
fn numeric(row: &mut Value, k: &str) {
    row[k] = number(&row[k]).map_or(Value::Null, |n| json!(n));
}
/// Persist the same numerical shape that the public ledger projects.
pub(crate) fn capture(record: &mut Value, usage: &Value) {
    for (field,paths) in [
        ("input_tokens",vec!["/input_tokens","/prompt_tokens"]),
        ("output_tokens",vec!["/output_tokens","/completion_tokens"]),
        ("cache_read_tokens",vec!["/cache_read_tokens","/cached_input_tokens","/prompt_tokens_details/cached_tokens","/input_tokens_details/cached_tokens"]),
        ("cache_write_tokens",vec!["/cache_write_tokens","/cache_creation_input_tokens"]),
        ("reasoning_tokens",vec!["/reasoning_tokens","/completion_tokens_details/reasoning_tokens","/output_tokens_details/reasoning_tokens"]),
        ("reported_cost",vec!["/reported_cost","/cost_usd"]),
        ("estimated_cost",vec!["/estimated_cost"]),
    ] {
        record[field] = paths.iter().find_map(|path|usage.pointer(path).and_then(number)).map_or(Value::Null,|value|json!(value));
    }
    record["measurement_scope"] = usage.get("measurement_scope").cloned().unwrap_or_else(||json!(
        if record["input_tokens"].is_number() || record["output_tokens"].is_number(){"turn"}else{"context_only"}));
    record["source"] = json!("runtime");
    record["token_schema_version"] = json!(3);
    record["quality"] = json!("reported");
    for key in ["currency","price_source","cost_status","amount_source","provider_id","speed"] {
        if let Some(value) = usage.get(key) { record[key] = value.clone(); }
    }
    if record["reported_cost"].is_number() {
        record["currency"] = json!("USD");
        if record["price_source"].is_null() { record["price_source"] = json!("provider_reported"); }
        if record["cost_status"].is_null() { record["cost_status"] = json!("reported"); }
    }
}
fn project(raw: &Value) -> Value {
    let mut row = json!({});
    for field in [
        "id",
        "at",
        "thread_id",
        "turn_id",
        "run_id",
        "competition_id",
        "challenge_id",
        "worker_id",
        "role",
        "actor_kind",
        "billing_scope",
        "model",
        "workspace_kind",
        "credential_id",
        "provider_id",
        "speed",
        "catalog_provider",
        "quality",
        "source",
        "status",
        "currency",
        "measurement_scope",
        "price_source",
        "cost_status",
        "amount_source",
        "normalization_repair",
    ] {
        if let Some(v) = raw.get(field) {
            if v.is_string() || v.is_number() {
                row[field] = v.clone();
            }
        }
    }
    for field in [
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "reasoning_tokens",
        "reported_cost",
        "estimated_cost",
        "cost",
    ] {
        row[field] = raw[field].clone();
        numeric(&mut row, field);
    }
    for field in ["generation","token_schema_version"] {
        row[field]=raw[field].as_u64().or_else(||number(&raw[field]).filter(|value|value.fract()==0. && *value<u64::MAX as f64).map(|value|value as u64)).map_or(Value::Null,|value|json!(value));
    }
    row["engine"] = json!(engine(string(raw, "engine")));
    row
}
fn ledger_row(raw: &Value) -> Option<Value> {
    let amount = ["reported_cost", "estimated_cost"]
        .iter()
        .any(|k| number(&raw[*k]).is_some());
    if (string(raw, "source") == "context-only"
        || string(raw, "measurement_scope") == "context_only")
        && !amount
    {
        return None;
    }
    let mut row = project(raw);
    if string(raw, "engine") == "cursor.sdk"
        && string(raw, "source") == "runtime"
        && number(&raw["token_schema_version"]).unwrap_or(0.0) < 3.0
    {
        row["input_tokens"] = number(&raw["input_tokens"]).map_or(Value::Null, |n| {
            json!(
                n + number(&raw["cache_read_tokens"]).unwrap_or(0.0)
                    + number(&raw["cache_write_tokens"]).unwrap_or(0.0)
            )
        });
        row["input_lower_bound"] = json!(
            number(&raw["cache_read_tokens"]).is_none()
                || number(&raw["cache_write_tokens"]).is_none()
        );
        row["normalization_repair"] = json!("cursor_sdk_disjoint_v3_view");
        row["token_schema_version"] = json!(3);
    }
    let eng = string(&row, "engine").to_owned();
    let source = string(raw, "source");
    let trusted = ["model_price", "driver_estimate"].contains(&string(raw, "price_source"));
    let legacy = source == "legacy-snapshot"
        || [
            "legacy_pi_estimate_unverified",
            "legacy_estimate_unverified",
        ]
        .contains(&string(raw, "cost_status"))
        || (["pi", "omp"].contains(&eng.as_str()) && source == "cli" && !trusted);
    let unverified = legacy
        || (number(&raw["estimated_cost"]).is_some()
            && !trusted
            && !["deepseek-v4-pro", "deepseek-v4-flash", "codex", "gpt-5"]
                .contains(&string(raw, "model")));
    let driver = string(raw, "price_source") == "driver_estimate"
        || (["pi.rpc", "omp.rpc", "claude.sdk", "claude.agent_sdk"]
            .contains(&string(raw, "engine"))
            && source == "runtime");
    row["captured_cost"] = json!({"reported_cost":number(&raw["reported_cost"]),"estimated_cost":number(&raw["estimated_cost"]),"price_source":raw["price_source"].as_str(),"cost_status":raw["cost_status"].as_str()});
    let mut reported = if legacy || driver {
        None
    } else {
        number(&raw["reported_cost"])
    };
    if eng == "opencode" && reported == Some(0.0) {
        reported = None;
    }
    let driver_amount = if ["pi.rpc", "omp.rpc"].contains(&string(raw, "engine"))
        && number(&raw["reported_cost"]) == Some(0.0)
    {
        None
    } else {
        number(&raw["reported_cost"])
    };
    let estimate = if unverified {
        None
    } else {
        number(&raw["estimated_cost"]).or(if driver { driver_amount } else { None })
    };
    row["reported_cost"] = reported.map_or(Value::Null, |n| json!(n));
    row["estimated_cost"] = estimate.map_or(Value::Null, |n| json!(n));
    row["cost"] = reported.or(estimate).map_or(Value::Null, |n| json!(n));
    row["amount_source"] = json!(if reported.is_some() {
        "provider_reported"
    } else if estimate.is_some() {
        if string(raw,"price_source")=="model_price" && raw["price_snapshot"].is_object() {
            if raw["price_snapshot"]["status"]=="custom" { "custom" } else { "catalog" }
        } else if driver {
            "driver_estimate"
        } else {
            "captured_estimate"
        }
    } else {
        "unpriced"
    });
    if unverified {
        row["cost_status"] = json!("saved_estimate_unverified");
    }
    if raw["source"]=="runtime" && raw["invocation_id"].is_string() && row["quality"].is_null() {
        row["quality"]=json!(if raw["cost_status"]=="partial" {"partial"} else if reported.is_some() {"reported"} else if estimate.is_some() {"estimated"} else {"missing"});
    }
    normalize_local_pi_ctf_row(&mut row);
    Some(row)
}

pub fn normalize_local_pi_ctf_row(row: &mut Value) {
    let tid = string(row, "thread_id").to_owned();
    let Some(real_run_id) = tid.strip_prefix("ctf-pi-").filter(|s| !s.is_empty()) else {
        return;
    };
    let rid = string(row, "run_id");
    if rid.is_empty() || rid == tid {
        row["run_id"] = json!(real_run_id);
    }
    if row["actor_kind"].is_null() {
        row["actor_kind"] = json!("solve");
    }
    if row["billing_scope"].is_null() {
        row["billing_scope"] = json!("solve");
    }
    if row["worker_id"].is_null() {
        row["worker_id"] = json!("cli-pi#1");
    }
    if row["workspace_kind"].is_null() || row["workspace_kind"] == "conversation" {
        row["workspace_kind"] = json!("ctf");
    }
}
fn saved_db(path: &Path) -> Result<Connection> {
    Ok(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?)
}
fn event_time(stamp: &Value) -> Option<f64> {
    if let Some(value) = stamp.as_f64() {
        return Some(value);
    }
    let text = stamp.as_str()?;
    if let Ok(value) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(value.timestamp_micros() as f64 / 1_000_000.0);
    }
    use chrono::TimeZone;
    let naive = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f").ok()?;
    chrono::Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|value| value.timestamp_micros() as f64 / 1_000_000.0)
}
fn legacy_key(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::String(value) => value.clone(),
        other => other.to_string(),
    }
}
fn legacy_token(value: &Value) -> Value {
    match number(value) {
        Some(value) if value <= (2u64.pow(53) - 1) as f64 => json!(value as u64),
        _ => Value::Null,
    }
}
/// Rebuild old cost.update snapshots from the copied run history. The cutoff is
/// the original Python usage ledger's coverage boundary, copied at import time.
pub fn import_run_history(store: &Store) -> Result<Value> {
    let root = store
        .get("system", "history_root")?
        .and_then(|value| value.as_str().map(PathBuf::from))
        .context("Import legacy run history before supplementing usage")?;
    let db = Connection::open_with_flags(
        root.join("state/usage.sqlite3"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let coverage: i64 = db.query_row(
        "SELECT value FROM usage_meta WHERE key='coverage_start'",
        [],
        |row| row.get(0),
    )?;
    let cutoff = coverage as f64 / 1_000_000.0;
    let mut imported = 0usize;
    let mut skipped = 0usize;
    let mut paths = fs::read_dir(root.join("state"))?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry.file_type().is_ok_and(|kind| kind.is_file())
                && entry.path().extension().is_some_and(|ext| ext == "jsonl")
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        let stream = match fs::File::open(&path) {
            Ok(file) => file,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let mut latest = BTreeMap::<(String, String, String, String), (f64, Value, Value)>::new();
        let mut generation = json!(0);
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else {
                skipped += 1;
                continue;
            };
            let Ok(event) = serde_json::from_str::<Value>(&line) else {
                skipped += 1;
                continue;
            };
            if !event.is_object() {
                skipped += 1;
                continue;
            }
            let stamp = event.get("ts").cloned().unwrap_or(json!(0));
            let Some(stamp) = event_time(&stamp) else {
                skipped += 1;
                continue;
            };
            if stamp >= cutoff {
                continue;
            }
            let payload = event
                .get("payload")
                .filter(|value| match value {
                    Value::Null | Value::Bool(false) => false,
                    Value::Number(value) => value.as_f64() != Some(0.0),
                    Value::String(value) => !value.is_empty(),
                    Value::Array(value) => !value.is_empty(),
                    Value::Object(_) | Value::Bool(true) => true,
                })
                .cloned()
                .unwrap_or_else(|| json!({}));
            if !payload.is_object() {
                skipped += 1;
                continue;
            }
            if let Some(value) = payload.get("execution_generation") {
                generation = value.clone();
            }
            if event["event_type"] != "cost.update" {
                continue;
            }
            let run = event["run_id"].as_str().unwrap_or("").to_owned();
            let scope = legacy_key(&payload["scope"]);
            let actor = event["solver_id"]
                .as_str()
                .filter(|value| !value.is_empty())
                .or_else(|| payload["solver_id"].as_str().filter(|value| !value.is_empty()))
                .or_else(|| payload["challenge_id"].as_str().filter(|value| !value.is_empty()))
                .unwrap_or("")
                .to_owned();
            latest.insert(
                (run, legacy_key(&generation), scope, actor),
                (stamp, payload, generation.clone()),
            );
        }
        let solver_runs = latest
            .keys()
            .filter(|key| matches!(key.2.as_str(),"solver"|"solve"))
            .map(|key| (key.0.clone(), key.1.clone()))
            .collect::<BTreeSet<_>>();
        let global_runs = latest
            .keys()
            .filter(|key| key.2 == "global")
            .map(|key| (key.0.clone(), key.1.clone()))
            .collect::<BTreeSet<_>>();
        for ((run, generation_key, scope, actor), (stamp, payload, generation)) in latest {
            let run_generation = (run.clone(), generation_key.clone());
            if run.is_empty()
                || (solver_runs.contains(&run_generation) && scope=="global")
                || (!solver_runs.contains(&run_generation)
                    && global_runs.contains(&run_generation)
                    && !matches!(scope.as_str(),"global"|"auxiliary"))
            {
                continue;
            }
            let auxiliary = scope=="auxiliary" || matches!(actor.as_str(),"btw"|"respond"|"standby"|"ask"|"writeup"|"followup"|"report"|"report_review");
            let id = format!("legacy:{run}:{generation_key}:{scope}:{actor}");
            if store.get("usage_history", &id)?.is_some() {
                continue;
            }
            let raw = json!({
                "id":id,"at":stamp,"run_id":run,"generation":generation,
                "workspace_kind":"single-security-task","role":if auxiliary {&actor} else {"worker"},
                "worker_id":actor,"actor_kind":if auxiliary {"auxiliary"} else {"solve"},
                "input_tokens":legacy_token(&payload["input_tokens"]),
                "output_tokens":legacy_token(&payload["output_tokens"]),
                "estimated_cost":number(&payload["usd"]),
                "quality":"estimated","source":"legacy-snapshot",
                "status":"historical_quality_unknown","measurement_scope":"solver-snapshot",
                "currency":"USD","token_schema_version":2,
            });
            let mut row = ledger_row(&raw).context("Project legacy usage snapshot")?;
            row["historical"] = json!(true);
            store.put("usage_history", &id, &row)?;
            imported += 1;
        }
    }
    Ok(json!({"imported":imported,"skipped":skipped,"coverage_start":cutoff}))
}
pub fn import(source: &Path, store: &Store) -> Result<(usize, usize)> {
    let mut ledger_count = 0;
    let mut provider_count = 0;
    let path = source.join("usage.sqlite3");
    if path.exists() {
        let db = saved_db(&path)?;
        let mut owners = BTreeMap::new();
        let mut stmt =
            db.prepare("SELECT run_id,competition_id,challenge_id FROM run_ownership")?;
        for row in stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })? {
            let (run, competition, challenge) = row?;
            owners.insert(run, (competition, challenge));
        }
        let mut stmt = db.prepare("SELECT id,at,data FROM usage ORDER BY id")?;
        for row in stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })? {
            let (id, at, data) = row?;
            let raw: Value =
                serde_json::from_str(&data).context("Decode saved usage observation")?;
            if let Some(mut row) = ledger_row(&raw) {
                row["id"] = json!(id);
                row["at"] = json!(at);
                if let Some((competition, challenge)) = owners.get(string(&row, "run_id")) {
                    row["competition_id"] = json!(competition);
                    if !challenge.is_empty() {
                        row["challenge_id"] = json!(challenge);
                    }
                    row["workspace_kind"] = json!("competition");
                }
                row["historical"] = json!(true);
                if store.get("usage_history", &id)?.is_none() {
                    store.put("usage_history", &id, &row)?;
                    ledger_count += 1;
                }
            }
        }
    }
    let path = source.join("_provider_usage_cache.sqlite3");
    if path.exists() {
        let db = saved_db(&path)?;
        let mut stmt = db.prepare("SELECT data FROM files ORDER BY path")?;
        let mut seen = BTreeSet::new();
        for data in stmt.query_map([], |r| r.get::<_, String>(0))? {
            let data: Value =
                serde_json::from_str(&data?).context("Decode saved numerical provider cache")?;
            for raw in data["records"].as_array().into_iter().flatten() {
                let key = string(raw, "key");
                if key.is_empty() {
                    continue;
                }
                let eng = engine(string(raw, "engine"));
                let id = format!("{eng}:{key}");
                if !seen.insert(id.clone()) {
                    continue;
                }
                let Some(at) = number(&raw["at_ms"]) else {
                    continue;
                };
                let mut row = project(raw);
                row["id"] = json!(key);
                row["at"] = json!(at / 1000.0);
                row["historical"] = json!(true);
                row["source"] = json!("saved_provider_cache");
                row["session_id"] = json!(format!(
                    "{:x}",
                    Sha256::digest(string(raw, "session").as_bytes())
                ));
                for (to, from) in [
                    ("cache_read_tokens", "cache_read"),
                    ("cache_write_tokens", "cache_write"),
                    ("output_tokens", "output"),
                    ("reasoning_tokens", "reasoning"),
                    ("uncached_input", "uncached_input"),
                    ("unclassified_input", "unclassified_input"),
                ] {
                    row[to] = number(&raw[from]).map_or(Value::Null, |n| json!(n));
                }
                row["input_tokens"] = match (
                    number(&raw["uncached_input"]),
                    number(&raw["unclassified_input"]),
                    number(&raw["cache_read"]),
                    number(&raw["cache_write"]),
                ) {
                    (a, b, c, d) if [a, b, c, d].iter().any(Option::is_some) => json!(
                        a.unwrap_or(0.0) + b.unwrap_or(0.0) + c.unwrap_or(0.0) + d.unwrap_or(0.0)
                    ),
                    _ => Value::Null,
                };
                if eng == "claude" && !row["reported_cost"].is_null() {
                    row["estimated_cost"] = row["reported_cost"].clone();
                    row["reported_cost"] = Value::Null;
                }
                row["cost"] = row["reported_cost"]
                    .as_f64()
                    .or(row["estimated_cost"].as_f64())
                    .map_or(Value::Null, |n| json!(n));
                row["amount_source"] = json!(if !row["reported_cost"].is_null() {
                    "provider_reported"
                } else if !row["estimated_cost"].is_null() {
                    "captured_estimate"
                } else {
                    "unpriced"
                });
                if store.get("provider_usage_history", &id)?.is_none() {
                    store.put("provider_usage_history", &id, &row)?;
                    provider_count += 1;
                }
            }
        }
    }
    store.put("import_metadata","usage_history",&json!({"ledger_records":store.list("usage_history")?.len(),"provider_cache_records":store.list("provider_usage_history")?.len(),"historical":true,"refresh_supported":false}))?;
    Ok((ledger_count, provider_count))
}
pub fn rows(store: &Store, scope: &str) -> rusqlite::Result<Vec<Value>> {
    rows_internal(store, scope, None)
}

pub fn rows_for_runs(
    store: &Store,
    run_ids: &std::collections::HashSet<String>,
) -> rusqlite::Result<Vec<Value>> {
    rows_internal(store, "muteki", Some(run_ids))
}

fn rows_internal(
    store: &Store,
    scope: &str,
    run_ids: Option<&std::collections::HashSet<String>>,
) -> rusqlite::Result<Vec<Value>> {
    let live: Vec<_> = match run_ids {
        Some(ids) => {
            let mut expanded = ids.clone();
            for id in ids {
                expanded.insert(format!("ctf-pi-{id}"));
            }
            store.list_for_run_ids("usage", &expanded)?
        }
        None => store.list("usage")?,
    }
        .into_iter()
        .filter_map(|r| ledger_row(&r))
        .collect();
    let ids: BTreeSet<_> = live.iter().map(|r| string(r, "id").to_owned()).collect();
    let turns: BTreeSet<_> = live
        .iter()
        .filter(|r| !string(r, "thread_id").is_empty() && !string(r, "turn_id").is_empty())
        .map(|r| {
            (
                string(r, "thread_id").to_owned(),
                string(r, "turn_id").to_owned(),
            )
        })
        .collect();
    let mut ledger = live;
    let run_bindings=store.list("competition_bindings_live")?.into_iter()
        .filter_map(|binding|Some((binding["run_id"].as_str()?.to_owned(),(binding["competition_id"].clone(),binding["competition_challenge_id"].clone()))))
        .collect::<BTreeMap<_,_>>();
    let history = match run_ids {
        Some(ids) => store.list_for_run_ids("usage_history", ids)?,
        None => store.list("usage_history")?,
    };
    for raw in history {
        let row = if raw["source"]=="runtime" && raw["invocation_id"].is_string() {
            let auxiliary=raw["actor_kind"]=="auxiliary"||matches!(raw["role"].as_str(),Some("btw"|"respond"|"standby"|"ask"|"writeup"|"followup"|"report"|"report_review"));
            let mut row=ledger_row(&raw).unwrap_or(raw);
            row["actor_kind"]=json!(if auxiliary{"auxiliary"}else{"solve"});row["billing_scope"]=row["actor_kind"].clone();
            if let Some((competition,challenge))=run_bindings.get(string(&row,"run_id")) {
                if row["competition_id"].is_null(){row["competition_id"]=competition.clone();}
                if row["challenge_id"].is_null(){row["challenge_id"]=challenge.clone();}
            }
            row
        } else { raw };
        if !ids.contains(string(&row, "id"))
            && !turns.contains(&(
                string(&row, "thread_id").into(),
                string(&row, "turn_id").into(),
            ))
        {
            ledger.push(row);
        }
    }
    let dims = [
        "thread_id",
        "turn_id",
        "run_id",
        "generation",
        "worker_id",
        "role",
        "actor_kind",
        "billing_scope",
        "engine",
        "model",
        "credential_id",
        "provider_id",
    ];
    let mut groups = BTreeMap::<String, Vec<usize>>::new();
    for (index, row) in ledger.iter().enumerate() {
        if !string(row, "thread_id").is_empty() && !string(row, "turn_id").is_empty() {
            groups
                .entry(
                    serde_json::to_string(
                        &dims.iter().map(|key| row[*key].clone()).collect::<Vec<_>>(),
                    )
                    .unwrap(),
                )
                .or_default()
                .push(index);
        }
    }
    let mut removed = BTreeSet::new();
    for indexes in groups.values() {
        let tokens: Vec<_> = indexes
            .iter()
            .copied()
            .filter(|i| {
                number(&ledger[*i]["input_tokens"]).is_some()
                    || number(&ledger[*i]["output_tokens"]).is_some()
            })
            .collect();
        let amounts: Vec<_> = indexes
            .iter()
            .copied()
            .filter(|i| {
                let row = &ledger[*i];
                row["source"] == "runtime-cumulative"
                    && row["measurement_scope"] == "interval"
                    && row["price_source"] == "driver_estimate"
                    && [
                        "input_tokens",
                        "output_tokens",
                        "cache_read_tokens",
                        "cache_write_tokens",
                        "reasoning_tokens",
                    ]
                    .iter()
                    .all(|k| number(&row[*k]).is_none())
                    && (number(&row["reported_cost"]).is_some()
                        || number(&row["estimated_cost"]).is_some())
            })
            .collect();
        if tokens.len() == 1 && !amounts.is_empty() {
            let target = tokens[0];
            if ledger[target]["reported_cost"].is_null() {
                let values: Vec<_> = amounts
                    .iter()
                    .filter_map(|i| number(&ledger[*i]["estimated_cost"]))
                    .collect();
                if !values.is_empty() {
                    ledger[target]["estimated_cost"] = json!(values.iter().sum::<f64>());
                    ledger[target]["cost"] = ledger[target]["estimated_cost"].clone();
                    ledger[target]["amount_source"] = json!("driver_estimate");
                }
            }
            ledger[target]["amount_observation_ids"] = json!(
                amounts
                    .iter()
                    .map(|i| ledger[*i]["id"].clone())
                    .collect::<Vec<_>>()
            );
            removed.extend(amounts);
        }
    }
    let ledger: Vec<_> = ledger
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !removed.contains(i))
        .map(|(_, row)| row)
        .collect();
    if scope == "history" {
        let mut rows = store.list("provider_usage_history")?;
        rows.extend(ledger.into_iter().filter(|r| {
            !["codex", "claude", "grok", "opencode", "cursor"].contains(&string(r, "engine"))
        }));
        Ok(rows)
    } else {
        Ok(ledger)
    }
}
pub fn aggregate(rows: &[Value]) -> Value {
    let sum = |key: &str| {
        let values: Vec<_> = rows.iter().filter_map(|r| number(&r[key])).collect();
        if values.is_empty() {
            Value::Null
        } else {
            json!(values.iter().sum::<f64>())
        }
    };
    let mut out = json!({});
    for key in [
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "reasoning_tokens",
        "reported_cost",
        "estimated_cost",
        "cost",
    ] {
        out[key] = sum(key);
    }
    out["total_tokens"] = match (number(&out["input_tokens"]), number(&out["output_tokens"])) {
        (Some(a), Some(b)) => json!(a + b),
        _ => Value::Null,
    };
    out["records"] = json!(rows.len());
    out["unpriced"] = json!(rows.iter().filter(|r| r["cost"].is_null()).count());
    out["quality"] = json!({});
    out["observed"] = json!({});
    out["pricing_coverage"] = json!({});
    for field in [
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "reasoning_tokens",
    ] {
        out["observed"][field] = json!(rows.iter().filter(|r| !r[field].is_null()).count());
    }
    for kind in ["reported", "estimated", "partial", "missing"] {
        out["quality"][kind] = json!(rows.iter().filter(|r| r["quality"] == kind).count());
    }
    for kind in [
        "provider_reported",
        "driver_estimate",
        "captured_estimate",
        "catalog",
        "custom",
        "unpriced",
    ] {
        out["pricing_coverage"][kind] =
            json!(rows.iter().filter(|r| r["amount_source"] == kind).count());
    }
    out["token_coverage"] = json!(if rows.is_empty()
        || out["input_tokens"].is_null() && out["output_tokens"].is_null()
    {
        "missing"
    } else if rows.iter().all(|r| !r["input_tokens"].is_null()
        && !r["output_tokens"].is_null()
        && r["input_lower_bound"] != true)
    {
        "complete"
    } else {
        "partial"
    });
    out["historical_scope_unknown"] = json!(
        rows.iter()
            .filter(|r| r["measurement_scope"] == "historical_scope_unknown")
            .count()
    );
    out["input_includes_cache"] = json!(true);
    out["pricing_issues"] = json!(rows.iter().filter_map(|row| row.get("pricing_issue")).collect::<Vec<_>>());
    out
}

/// Price observations without an amount using the same current catalog and
/// scoped overrides as the model picker. Captured charges/estimates stay intact;
/// this read projection never rewrites the numerical usage ledger.
pub fn price_rows(store: &Store, rows: &mut [Value]) -> Result<(), String> {
    let mut identities = BTreeMap::<String, Value>::new();
    let mut keys = Vec::with_capacity(rows.len());
    for row in rows.iter() {
        if number(&row["cost"]).is_some() {
            keys.push(None);
            continue;
        }
        let mut identity = json!({"engine":row["engine"],"model":row["model"]});
        for field in ["credential_id", "provider_id", "speed", "catalog_provider"] {
            if !string(row, field).is_empty() { identity[field] = row[field].clone(); }
        }
        if !string(row, "catalog_provider").is_empty() {
            identity["route_provenance"] = json!({"source":"saved_usage"});
        }
        if string(row, "engine").is_empty() || string(row, "model").is_empty() {
            keys.push(None);
            continue;
        }
        let key = identity.to_string();
        identities.entry(key.clone()).or_insert(identity);
        keys.push(Some(key));
    }
    let items = identities.values().cloned().collect::<Vec<_>>();
    let resolved = if items.is_empty() { json!({"items":[]}) } else { crate::pricing_read::resolve(store, &items)? };
    let prices = identities.keys().zip(resolved["items"].as_array().ok_or("Invalid pricing response")?)
        .collect::<BTreeMap<_, _>>();
    for (row, key) in rows.iter_mut().zip(keys) {
        if number(&row["cost"]).is_some() { continue; }
        row["amount_source"] = json!("unpriced");
        let price = key.as_ref().and_then(|key| prices.get(key));
        let mut reason = match price.map(|p| string(p, "status")) {
            Some("catalog" | "custom") => "",
            Some("ambiguous") => "ambiguous_model",
            Some("unknown-tier") => "unknown_tier",
            Some("subscription_unknown") => "subscription_unknown",
            _ => "missing_price",
        };
        if let Some(price) = price.filter(|_| reason.is_empty()) {
            let estimate = (|| {
                if row["measurement_scope"] == "historical_scope_unknown" || row["input_lower_bound"] == true { return None; }
                let input = number(&row["input_tokens"])?;
                let output = number(&row["output_tokens"])?;
                let read = number(&row["cache_read_tokens"]).unwrap_or(0.0);
                let write = number(&row["cache_write_tokens"]).unwrap_or(0.0);
                let exclusive = row["source"] == "runtime"
                    && (row["workspace_kind"] == "conversation"
                        || string(row, "thread_id").starts_with("ctf-pi-"))
                    && matches!(string(row,"engine"), "claude" | "pi" | "omp" | "opencode" | "droid");
                if !exclusive && read + write > input { return None; }
                let regular = if exclusive { input } else { input - read - write };
                let mut total = 0.0;
                for (field, tokens) in [("input",regular),("output",output),("cache_read",read),("cache_write",write)] {
                    if tokens > 0.0 { total += tokens * number(&price["rates"][field])? / 1e6; }
                }
                total.is_finite().then_some(total)
            })();
            if let Some(amount) = estimate {
                row["cost"] = json!(amount);
                row["estimated_cost"] = json!(amount);
                row["amount_source"] = price["status"].clone();
                row["cost_status"] = json!("estimated");
                row["price_source"] = json!("model_price");
                continue;
            }
            reason = if number(&row["input_tokens"]).is_none() || number(&row["output_tokens"]).is_none()
                || row["measurement_scope"] == "historical_scope_unknown" || row["input_lower_bound"] == true {
                "missing_tokens"
            } else { "missing_price" };
        }
        let mut issue = json!({"engine":row["engine"],"model":row["model"],"reason":reason});
        for field in ["credential_id", "provider_id", "speed"] {
            if let Some(value) = row.get(field) { issue[field] = value.clone(); }
        }
        row["pricing_issue"] = issue;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_db_reads_committed_rows_from_an_active_wal() {
        let dir = std::env::temp_dir().join(format!("muteki-usage-wal-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("usage.sqlite3");
        let writer = Connection::open(&path).unwrap();
        writer
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; \
                 CREATE TABLE usage_meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL); \
                 INSERT INTO usage_meta VALUES ('coverage_start',1788633098226038);",
            )
            .unwrap();
        assert!(path.with_file_name("usage.sqlite3-wal").exists());
        let reader = saved_db(&path).unwrap();
        let coverage: i64 = reader
            .query_row(
                "SELECT value FROM usage_meta WHERE key='coverage_start'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(coverage, 1788633098226038);
        drop(reader);
        drop(writer);
        fs::remove_dir_all(dir).unwrap();
    }
}
