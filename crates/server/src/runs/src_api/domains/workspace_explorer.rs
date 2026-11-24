//! Read-only Task/Run workspace SQLite explorer & attachment auto-binding router.

use super::{graphql_grpc, import_formats, research_ingest};
use crate::AppState;
use crate::runs::src_api::{Api, Error, db, now, records, require, save_rec, task};
use axum::http::StatusCode;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::path::PathBuf;

fn allowed_roots(s: &AppState, id: &str) -> Api<Vec<PathBuf>> {
    let state_dir = crate::native_chat::state_dir(s);
    let mut roots = vec![
        state_dir.join("src-traffic").join(id),
        state_dir.join("src-vault").join(id),
        state_dir.join("src-artifacts").join(id),
    ];
    for row in records(s, id, "run_binding")? {
        if let Some(run_id) = row["payload"]["run_id"].as_str().filter(|v| !v.is_empty()) {
            roots.push(state_dir.join("runs").join(run_id));
            roots.push(state_dir.join("run-vault").join(run_id));
        }
    }
    Ok(roots
        .into_iter()
        .filter(|p| p.exists())
        .filter_map(|p| std::fs::canonicalize(p).ok())
        .collect())
}

fn validate_scoped_sqlite(s: &AppState, id: &str, raw_path: &str) -> Api<PathBuf> {
    let roots = allowed_roots(s, id)?;
    let requested = PathBuf::from(raw_path.trim());
    let resolved = if requested.is_absolute() {
        std::fs::canonicalize(&requested)
    } else {
        roots
            .iter()
            .find_map(|r| std::fs::canonicalize(r.join(&requested)).ok())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "not found"))
    }
    .map_err(|_| Error::new(StatusCode::NOT_FOUND, "artifact_missing", "SQLite path not found in Task workspace"))?;
    if !roots.iter().any(|r| resolved.starts_with(r)) {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "workspace_scope_violation",
            "Path escapes Task/Run workspace boundaries",
        ));
    }
    Ok(resolved)
}

fn discover_sqlite_files(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = roots.to_vec();
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().map_or(true, |kind| kind.is_symlink()) { continue; }
            if path.is_dir() {
                if out.len() < 64 {
                    stack.push(path);
                }
            } else if path.extension().and_then(|e| e.to_str()).is_some_and(|ext| {
                matches!(ext.to_ascii_lowercase().as_str(), "sqlite" | "sqlite3" | "db")
            }) {
                out.push(path);
                if out.len() >= 64 {
                    break;
                }
            }
        }
    }
    out
}

pub(super) fn tables(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let _ = task(s, id)?;
    let files = if let Some(path) = body["db_path"].as_str().filter(|v| !v.trim().is_empty()) {
        vec![validate_scoped_sqlite(s, id, path)?]
    } else {
        let roots = allowed_roots(s, id)?;
        discover_sqlite_files(&roots)
    };

    let mut dbs = Vec::new();
    for path in files {
        let path = validate_scoped_sqlite(s, id, &path.to_string_lossy())?;
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(db)?;
        let mut stmt = conn
            .prepare("SELECT name, type FROM sqlite_master WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .map_err(db)?;
        let rows = stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
            .map_err(db)?;
        let mut table_list = Vec::new();
        for item in rows {
            let (name, tkind) = item.map_err(db)?;
            let escaped = name.replace('"', "\"\"");
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM \"{escaped}\""), [], |r| r.get(0))
                .unwrap_or(-1);
            table_list.push(json!({
                "name": name,
                "kind": tkind,
                "row_count": count,
            }));
        }
        dbs.push(json!({
            "db_path": path.to_string_lossy(),
            "file_name": path.file_name().and_then(|n| n.to_str()).unwrap_or(""),
            "tables": table_list,
        }));
    }

    Ok(json!({
        "database_count": dbs.len(),
        "databases": dbs,
    }))
}

fn sqlite_val_to_json(val: rusqlite::types::Value) -> Value {
    match val {
        rusqlite::types::Value::Null => Value::Null,
        rusqlite::types::Value::Integer(i) => json!(i),
        rusqlite::types::Value::Real(f) => json!(f),
        rusqlite::types::Value::Text(t) => json!(t),
        rusqlite::types::Value::Blob(b) => json!(format!("<blob {} bytes>", b.len())),
    }
}

pub(super) fn sample(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let _ = task(s, id)?;
    let path = validate_scoped_sqlite(s, id, require(body, "db_path")?)?;
    let table_name = require(body, "table_name")?.trim();
    let limit = body["limit"].as_u64().unwrap_or(20).clamp(1, 100);
    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(db)?;

    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type IN ('table','view') AND name = ?1",
            [table_name],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !exists {
        return Err(Error::new(
            StatusCode::NOT_FOUND,
            "table_not_found",
            format!("Table '{table_name}' does not exist"),
        ));
    }

    let escaped = table_name.replace('"', "\"\"");
    let mut stmt = conn
        .prepare(&format!("SELECT * FROM \"{escaped}\" LIMIT ?1"))
        .map_err(db)?;
    let col_names = stmt
        .column_names()
        .into_iter()
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let mut rows_iter = stmt.query([limit as i64]).map_err(db)?;
    let mut out_rows = Vec::new();
    while let Some(row) = rows_iter.next().map_err(db)? {
        let mut obj = serde_json::Map::new();
        for (idx, col) in col_names.iter().enumerate() {
            let v = row.get::<_, rusqlite::types::Value>(idx).map_err(db)?;
            obj.insert(col.clone(), sqlite_val_to_json(v));
        }
        out_rows.push(Value::Object(obj));
    }

    Ok(json!({
        "db_path": path.to_string_lossy(),
        "table_name": table_name,
        "columns": col_names,
        "row_count": out_rows.len(),
        "rows": out_rows,
    }))
}

pub(super) fn attachment_autobind(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    use base64::Engine;
    let _ = task(s, id)?;
    let filename = require(body, "filename")?.trim();
    let lower = filename.to_ascii_lowercase();
    let content = if let Some(b64) = body["content_b64"].as_str().filter(|v| !v.is_empty()) {
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|_| Error::invalid("content_b64 must be valid base64"))?
    } else if let Some(text) = body["content_text"].as_str() {
        text.as_bytes().to_vec()
    } else {
        return Err(Error::invalid("content_b64 or content_text is required"));
    };

    let (route_kind, outcome) = if lower.ends_with(".js.map") || lower.ends_with(".map") {
        let encoded = base64::engine::general_purpose::STANDARD.encode(&content);
        let res = research_ingest::unpack_sourcemap(
            s,
            id,
            &json!({
                "source_url": body["source_url"].as_str().unwrap_or(filename),
                "source_map_b64": encoded,
            }),
        )?;
        ("sourcemap_unpack", res)
    } else if lower.ends_with(".proto") {
        let text = String::from_utf8(content.clone())
            .map_err(|_| Error::invalid(".proto file must be UTF-8"))?;
        let res = graphql_grpc::ingest(
            s,
            id,
            &json!({
                "protocol": "grpc",
                "endpoint_url": body["source_url"].as_str().unwrap_or(""),
                "proto_text": text,
            }),
        )?;
        ("grpc_proto_ingest", res)
    } else if lower.ends_with(".har") {
        let parsed = serde_json::from_slice::<Value>(&content)
            .map_err(|err| Error::invalid(format!("HAR JSON invalid: {err}")))?;
        let count = parsed["log"]["entries"].as_array().map_or(0, Vec::len);
        let parsed_count = import_formats::parse(&content, "har", filename, &Value::Null, &Value::Null)
            .map(|p| p.rows.len())
            .unwrap_or(0);
        ("har_parsed", json!({"entry_count": count, "extracted_items": parsed_count}))
    } else if lower.contains("openapi") || lower.contains("swagger") {
        let encoded = base64::engine::general_purpose::STANDARD.encode(&content);
        let res = research_ingest::ingest_openapi(
            s,
            id,
            &json!({
                "source_url": body["source_url"].as_str().unwrap_or(filename),
                "content_b64": encoded,
            }),
            None,
        )?;
        ("openapi_ingest", res)
    } else if lower.ends_with(".json") {
        if let Ok(val) = serde_json::from_slice::<Value>(&content) {
            if val.get("__schema").is_some()
                || val.get("data").and_then(|d| d.get("__schema")).is_some()
            {
                let res = graphql_grpc::ingest(
                    s,
                    id,
                    &json!({
                        "protocol": "graphql",
                        "endpoint_url": body["source_url"].as_str().unwrap_or(""),
                        "schema": val,
                    }),
                )?;
                ("graphql_introspection", res)
            } else {
                let item_count = import_formats::parse(&content, "json", filename, &Value::Null, &Value::Null)
                    .map(|p| p.rows.len())
                    .unwrap_or(0);
                ("structured_json_extract", json!({"extracted_items": item_count}))
            }
        } else {
            return Err(Error::invalid("invalid JSON attachment"));
        }
    } else {
        let item_count = import_formats::parse(&content, "generic", filename, &Value::Null, &Value::Null)
            .map(|p| p.rows.len())
            .unwrap_or(0);
        ("generic_extract", json!({"extracted_items": item_count}))
    };

    let bind_id = format!(
        "sbind_{}",
        &crate::runs::src_api::digest(&json!([filename, route_kind, content.len()]))[..20]
    );
    let record = json!({
        "bind_id": bind_id,
        "task_id": id,
        "filename": filename,
        "route_kind": route_kind,
        "byte_len": content.len(),
        "outcome": outcome,
        "bound_at": now(),
    });
    save_rec(s, id, "attachment_autobind", &bind_id, record.clone(), None)?;
    Ok(record)
}
