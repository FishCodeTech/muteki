//! Import durable history only. No legacy service is initialized or recovered.
use crate::store::Store;
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

#[derive(Default, Debug, Serialize)]
pub struct Summary {
    pub databases: usize,
    pub runs: usize,
    pub competitions: usize,
    pub src_tasks: usize,
    pub records: usize,
    pub files: usize,
    pub bytes: u64,
    pub chat_artifacts: usize,
    pub missing_chat_artifacts: usize,
    pub legacy_chat_artifacts_skipped: usize,
}
fn directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn private(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
pub(crate) fn read_db(path: &Path) -> Result<Connection> {
    Ok(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?)
}
fn snapshot(source: &Path, target: &Path, summary: &mut Summary) -> Result<()> {
    if !source.is_file() {
        return Ok(());
    }
    if let Some(parent) = target.parent() {
        directory(parent)?;
    }
    if !target.exists() {
        let conn = read_db(source)?;
        conn.execute("VACUUM INTO ?1", [target.to_string_lossy().as_ref()])
            .context("Snapshot historical SQLite database")?;
        private(target)?;
    }
    summary.databases += 1;
    summary.bytes += target.metadata()?.len();
    Ok(())
}
fn copy_file(source: &Path, target: &Path, summary: &mut Summary) -> Result<()> {
    if !source.symlink_metadata()?.is_file() {
        return Ok(());
    }
    if let Some(parent) = target.parent() {
        directory(parent)?;
    }
    if !target.exists() {
        fs::copy(source, target)?;
        private(target)?;
    }
    summary.files += 1;
    summary.bytes += target.metadata()?.len();
    Ok(())
}
fn copy_tree(source: &Path, target: &Path, summary: &mut Summary) -> Result<()> {
    if !source.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        let n = name.to_string_lossy();
        if n.starts_with('.')
            || [
                "node_modules",
                "__pycache__",
                ".venv",
                "venv",
                "cache",
                "tmp",
                "temp",
                "secrets",
            ]
            .contains(&n.as_ref())
        {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            copy_tree(&entry.path(), &target.join(&name), summary)?;
        } else if kind.is_file() {
            copy_file(&entry.path(), &target.join(&name), summary)?;
        }
    }
    Ok(())
}
pub(crate) fn table_rows(conn: &Connection, table: &str) -> Result<Vec<Value>> {
    let mut stmt = conn.prepare(&format!("SELECT * FROM \"{table}\""))?;
    let names: Vec<String> = stmt.column_names().iter().map(|x| x.to_string()).collect();
    let rows = stmt.query_map([], |r| {
        let mut obj = serde_json::Map::new();
        for (i, name) in names.iter().enumerate() {
            use rusqlite::types::ValueRef;
            let v = match r.get_ref(i)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(n) => json!(n),
                ValueRef::Real(n) => json!(n),
                ValueRef::Text(s) => {
                    let s = String::from_utf8_lossy(s);
                    if name == "payload" || name.ends_with("_json") {
                        serde_json::from_str(&s).unwrap_or_else(|_| json!(s))
                    } else {
                        json!(s)
                    }
                }
                ValueRef::Blob(_) => Value::Null,
            };
            obj.insert(name.clone(), v);
        }
        Ok(Value::Object(obj))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}
pub(crate) fn payload(row: &Value) -> Value {
    row.get("payload")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or_else(|| row.clone())
}
fn json_file(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
/// Capture public credential metadata without reading or transferring credential values.
pub fn import_platform_metadata(
    source_root: &Path,
    state_dir: &Path,
    store: &Store,
) -> Result<usize> {
    let secret_root = source_root.join("state/_platform/competition/secrets");
    let mut records = Vec::new();
    if secret_root.is_dir() {
        for entry in fs::read_dir(&secret_root)? {
            let entry = entry?;
            let connection = entry.file_name().to_string_lossy().into_owned();
            if !entry.file_type()?.is_dir() || !metadata_segment(&connection) {
                continue;
            }
            for file in fs::read_dir(entry.path())? {
                let file = file?;
                let name = file.file_name().to_string_lossy().into_owned();
                let Some(key) = name.strip_suffix(".secret") else {
                    continue;
                };
                if !file.file_type()?.is_file() || !metadata_segment(key) {
                    continue;
                }
                let metadata_path = entry.path().join(format!("{key}.meta.json"));
                let raw = if metadata_path.symlink_metadata().is_ok_and(|v| v.is_file()) {
                    json_file(&metadata_path)?
                } else {
                    json!({})
                };
                let mut record = json!({"reference":format!("secret://platform/{connection}/{key}"),"connection_id":connection,"key":key,"created_at":"","updated_at":"","expires_at":"","rotation":1,"status":"not_imported","original_status":raw["status"].as_str().unwrap_or("active"),"source_present":true,"present":false,"available":false,"historical":true,"read_only":true});
                for name in ["created_at", "updated_at", "expires_at"] {
                    if let Some(value) = raw[name].as_str() {
                        record[name] = json!(value);
                    }
                }
                if let Some(rotation) = raw["rotation"].as_u64() {
                    record["rotation"] = json!(rotation);
                }
                records.push(record);
            }
        }
    }
    records.sort_by(|a, b| a["reference"].as_str().cmp(&b["reference"].as_str()));
    let target = state_dir.join("legacy");
    directory(&target)?;
    let path = target.join("platform-secret-metadata.json");
    fs::write(&path, serde_json::to_vec(&records)?)?;
    private(&path)?;
    store.put(
        "system",
        "history_platform_secret_metadata",
        &json!(records),
    )?;
    Ok(records.len())
}
fn metadata_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|v| v.is_ascii_alphanumeric() || b"_-".contains(&v))
}
pub fn run(source_root: &Path, state_dir: &Path, store: &Store) -> Result<Summary> {
    let source = source_root.canonicalize()?;
    let source_state = source.join("state");
    let target = state_dir.join("legacy");
    directory(&target)?;
    let mut summary = Summary::default();
    for name in ["platform.db", "competition.db", "memory.db"] {
        snapshot(
            &source_state.join("_platform").join(name),
            &target.join("_platform").join(name),
            &mut summary,
        )?;
    }
    snapshot(
        &source_state.join("usage.sqlite3"),
        &target.join("state/usage.sqlite3"),
        &mut summary,
    )?;
    let platform = read_db(&target.join("_platform/platform.db"))?;
    directory(&state_dir.join("uploads"))?;
    for row in table_rows(&platform, "artifacts")? {
        let digest = row["sha256"].as_str().context("Legacy artifact digest missing")?;
        anyhow::ensure!(digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()), "Invalid legacy artifact digest");
        let original = source_state.join("_platform/conversation_artifacts").join(digest);
        if !original.is_file() { summary.missing_chat_artifacts += 1; continue; }
        anyhow::ensure!(original.symlink_metadata()?.is_file(), "Legacy artifact must be a regular file");
        use sha2::{Digest, Sha256};
        let bytes = fs::read(&original)?;
        anyhow::ensure!(format!("{:x}", Sha256::digest(&bytes)) == digest, "Legacy artifact integrity check failed");
        copy_file(&original, &target.join("conversation_artifacts").join(digest), &mut summary)?;
        summary.chat_artifacts += 1;
    }
    for (table, collection, id_key) in [
        ("projects", "history_projects", "project_id"),
        ("workspaces", "history_workspaces", "workspace_id"),
        ("threads", "history_threads", "thread_id"),
        ("tasks", "history_tasks", "task_id"),
        ("src_records", "history_src_records", "record_id"),
    ] {
        for (row_index, row) in table_rows(&platform, table)?.into_iter().enumerate() {
            let id = row[id_key].as_str().context("Historical record ID")?;
            let value = if table == "src_records" {
                let mut record = row.clone();
                record["_legacy_order"] = json!(row_index);
                record
            } else {
                payload(&row)
            };
            store.put(collection, id, &value)?;
            summary.records += 1;
        }
    }
    let folders_source = source_state.join("_folders.json");
    if folders_source.is_file() {
        let folders = json_file(&folders_source)?;
        if let Some(rows) = folders.as_object() {
            for (id, value) in rows {
                if value.is_object() {
                    store.put("folders", id, value)?;
                }
            }
        }
        copy_file(
            &folders_source,
            &target.join("state/_folders.json"),
            &mut summary,
        )?;
    }
    let bindings_source = source_state.join("_bound_runs.json");
    if bindings_source.is_file() {
        let bindings = json_file(&bindings_source)?;
        if let Some(rows) = bindings.as_object() {
            for (id, value) in rows {
                store.put("history_run_bindings", id, value)?;
            }
        }
        copy_file(
            &bindings_source,
            &target.join("state/_bound_runs.json"),
            &mut summary,
        )?;
    }
    let mut summaries = BTreeMap::<String, Value>::new();
    let summary_source = source_state.join("_run_summaries.json");
    if summary_source.is_file() {
        let index = json_file(&summary_source)?;
        if let Some(rows) = index["runs"].as_object() {
            summaries.extend(rows.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        copy_file(
            &summary_source,
            &target.join("state/_run_summaries.json"),
            &mut summary,
        )?;
    }
    let meta_source = source_state.join("_rail_meta.json");
    let metadata = if meta_source.is_file() {
        copy_file(
            &meta_source,
            &target.join("state/_rail_meta.json"),
            &mut summary,
        )?;
        json_file(&meta_source)?
    } else {
        json!({})
    };
    for entry in fs::read_dir(&source_state)? {
        let entry = entry?;
        if entry.file_type()?.is_file() && entry.path().extension().is_some_and(|s| s == "jsonl") {
            let mut first = None;
            let mut count = 0;
            let mut last = 0.0f64;
            let mut started = false;
            let mut finished = false;
            let mut solved = false;
            let mut mode = "ctf".to_owned();
            let mut name = String::new();
            for line in BufReader::new(fs::File::open(entry.path())?).lines() {
                let line = line?;
                let Ok(event) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if first.is_none() {
                    first = event["run_id"].as_str().map(str::to_owned);
                }
                count += 1;
                last = event["ts"].as_f64().unwrap_or(last);
                let p = &event["payload"];
                match event["event_type"].as_str().unwrap_or("") {
                    "run.preparing" | "run.started" => {
                        started = true;
                        finished = false;
                        if let Some(m) = p["mode"]
                            .as_str()
                            .or_else(|| p["challenge"]["mode"].as_str())
                        {
                            mode = m.into();
                        }
                        if let Some(n) = p["challenge"]["name"].as_str() {
                            name = n.into();
                        }
                    }
                    "run.finished" => {
                        finished = true;
                        solved = p["solved"].as_bool().unwrap_or(false)
                    }
                    "run.reopened" => finished = false,
                    "run.titled" => {
                        if let Some(n) = p["name"].as_str().or_else(|| p["title"].as_str()) {
                            name = n.into();
                        }
                    }
                    _ => {}
                }
            }
            if let Some(id) = first {
                let value=summaries.entry(id.clone()).or_insert_with(||json!({"run_id":id,"name":name,"mode":mode,"started":started,"finished":finished,"solved":solved,"events":count,"ts":last}));
                if let Some(meta) = metadata[&id].as_object() {
                    for (k, v) in meta {
                        value[k] = v.clone();
                    }
                    if let Some(n) = meta.get("custom_name").and_then(Value::as_str) {
                        value["name"] = json!(n);
                    }
                }
                value["original_status"] = json!(if value["solved"] == true {
                    "solved"
                } else if value["finished"] == true {
                    "finished"
                } else if value["started"] == true {
                    "running"
                } else {
                    "idle"
                });
                value["historical"] = json!(true);
                value["read_only"] = json!(true);
                value["status"] = json!(if value["started"] == true && value["finished"] != true {
                    "interrupted"
                } else {
                    value["original_status"].as_str().unwrap_or("history")
                });
                value["updated_at"] = value["ts"].clone();
                value["paused"] = json!(false);
                value["awaiting_help"] = json!(false);
                value["runtime_incomplete"] = json!(false);
                value["history_event_file"] =
                    json!(format!("state/{}", entry.file_name().to_string_lossy()));
            }
            copy_file(
                &entry.path(),
                &target.join("state").join(entry.file_name()),
                &mut summary,
            )?;
        }
    }
    for (id, value) in &summaries {
        store.put("runs", id, value)?;
        summary.runs += 1;
    }
    let competitions = read_db(&target.join("_platform/competition.db"))?;
    for row in table_rows(&competitions, "competitions")? {
        let mut v = payload(&row);
        v["historical"] = json!(true);
        v["read_only"] = json!(true);
        v["original_scheduler_state"] = v["scheduler_state"].clone();
        v["scheduler_state"] = json!("paused");
        store.put(
            "competitions",
            v["competition_id"].as_str().context("Competition ID")?,
            &v,
        )?;
        summary.competitions += 1;
    }
    for task in store.list("history_tasks")? {
        if task["kind"] == "src.bounty" || task["input"]["product_mode"] == "src" {
            store.put(
                "history_src_tasks",
                task["task_id"].as_str().context("SRC task ID")?,
                &task,
            )?;
            summary.src_tasks += 1;
        }
    }
    let graphs = source_state.join("control/graphs");
    if graphs.is_dir() {
        for entry in fs::read_dir(graphs)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                snapshot(
                    &entry.path().join("shared_graph.db"),
                    &target
                        .join("graphs")
                        .join(entry.file_name())
                        .join("shared_graph.db"),
                    &mut summary,
                )?;
            }
        }
    }
    copy_tree(
        &source.join("sessions"),
        &target.join("sessions"),
        &mut summary,
    )?;
    copy_tree(
        &source_state.join("_platform/src/tasks"),
        &target.join("_platform/src/tasks"),
        &mut summary,
    )?;
    for row in table_rows(&competitions, "artifact_objects")? {
        let v = payload(&row);
        if let Some(path) = v["local_path"].as_str() {
            let p = PathBuf::from(path);
            let p = if p.is_absolute() { p } else { source.join(p) };
            if p.is_file() && p.canonicalize()?.starts_with(&source) {
                if let Some(sha) = v["sha256"]
                    .as_str()
                    .filter(|s| s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit()))
                {
                    copy_file(
                        &p,
                        &target.join("competition_artifacts").join(sha),
                        &mut summary,
                    )?;
                }
            }
        }
    }
    import_platform_metadata(&source, state_dir, store)?;
    store.put("system", "history_root", &json!(target.canonicalize()?))?;
    store.put("system", "history_import", &serde_json::to_value(&summary)?)?;
    Ok(summary)
}
