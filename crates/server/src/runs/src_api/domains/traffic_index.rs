//! Per-task SQLite FTS5 trigram index + 256 KiB hybrid immutable CAS vault for HTTP traffic.
use super::{Api, Error, db, now, parse_url, records, require, verify_ref};
use crate::AppState;
use axum::http::StatusCode;
use base64::Engine;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{self, Permissions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

pub(super) const INLINE_FTS_LIMIT: usize = 256 * 1024;

fn task_traffic_dir(s: &AppState, task_id: &str) -> PathBuf {
    crate::native_chat::state_dir(s).join("src-traffic").join(task_id)
}

pub(super) fn task_vault_dir(s: &AppState, task_id: &str) -> PathBuf {
    crate::native_chat::state_dir(s).join("src-vault").join(task_id)
}

fn db_path(s: &AppState, task_id: &str) -> PathBuf {
    task_traffic_dir(s, task_id).join("traffic_index.sqlite3")
}

fn open_index(s: &AppState, task_id: &str) -> Api<Connection> {
    let path = db_path(s, task_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(db)?;
    }
    let conn = Connection::open(&path).map_err(db)?;
    conn.busy_timeout(Duration::from_millis(5000)).map_err(db)?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         CREATE TABLE IF NOT EXISTS traffic_exchanges (
             exchange_id TEXT PRIMARY KEY,
             rowid_seq INTEGER,
             method TEXT NOT NULL,
             observed_url TEXT NOT NULL,
             host TEXT NOT NULL,
             path TEXT NOT NULL,
             status_code INTEGER,
             protocol TEXT NOT NULL,
             context_id TEXT NOT NULL,
             source_run_id TEXT NOT NULL,
             source_kind TEXT NOT NULL,
             mime TEXT NOT NULL,
             started_at TEXT NOT NULL,
             req_size INTEGER NOT NULL,
             req_sha256 TEXT NOT NULL,
             req_spilled INTEGER NOT NULL DEFAULT 0,
             req_vault_path TEXT NOT NULL DEFAULT '',
             resp_size INTEGER NOT NULL DEFAULT 0,
             resp_sha256 TEXT NOT NULL DEFAULT '',
             resp_spilled INTEGER NOT NULL DEFAULT 0,
             resp_vault_path TEXT NOT NULL DEFAULT '',
             bookmarked INTEGER NOT NULL DEFAULT 0,
             bookmark_label TEXT NOT NULL DEFAULT '',
             updated_at TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_traffic_filter
             ON traffic_exchanges (host, status_code, method, context_id, started_at);
         CREATE VIRTUAL TABLE IF NOT EXISTS traffic_fts USING fts5(
             exchange_id UNINDEXED,
             url,
             req_text,
             resp_text,
             tokenize='trigram'
         );",
    )
    .map_err(db)?;
    Ok(conn)
}

pub(super) fn put_vault_object(s: &AppState, task_id: &str, bytes: &[u8]) -> Api<(String, String)> {
    let sha = format!("{:x}", Sha256::digest(bytes));
    let rel = format!("objects/{}/{}", &sha[..2], sha);
    let root = task_vault_dir(s, task_id);
    let abs = root.join(&rel);
    if !abs.is_file() {
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).map_err(db)?;
        }
        let tmp = abs.with_file_name(format!(".tmp-{}-{}", std::process::id(), sha));
        {
            let mut file = fs::File::create(&tmp).map_err(db)?;
            file.write_all(bytes).map_err(db)?;
            file.sync_all().map_err(db)?;
        }
        let _ = fs::set_permissions(&tmp, Permissions::from_mode(0o444));
        if let Err(err) = fs::rename(&tmp, &abs) {
            let _ = fs::remove_file(&tmp);
            if !abs.is_file() {
                return Err(db(err));
            }
        }
    }
    Ok((sha, rel))
}

pub(super) fn read_vault_object(
    s: &AppState,
    task_id: &str,
    rel: &str,
    expected_sha256: &str,
) -> Api<Vec<u8>> {
    let root = task_vault_dir(s, task_id).canonicalize().map_err(db)?;
    let abs = root.join(rel).canonicalize().map_err(db)?;
    if !abs.starts_with(&root) {
        return Err(Error::new(StatusCode::FORBIDDEN, "src.vault_path_invalid", "vault path escapes task root"));
    }
    let bytes = fs::read(&abs).map_err(db)?;
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if !expected_sha256.is_empty() && actual != expected_sha256 {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "src.artifact_corrupt",
            "vault object SHA-256 digest mismatch",
        ));
    }
    Ok(bytes)
}

fn prepare_index_text(raw: &[u8]) -> (String, bool) {
    let too_large = raw.len() > INLINE_FTS_LIMIT;
    let binary = raw.contains(&0) || std::str::from_utf8(raw).is_err();
    let spilled = too_large || binary;
    let slice = if raw.len() > INLINE_FTS_LIMIT {
        &raw[..INLINE_FTS_LIMIT]
    } else {
        raw
    };
    let text = String::from_utf8_lossy(slice).replace('\0', " ");
    (text, spilled)
}

pub(super) fn index_exchange(
    s: &AppState,
    task_id: &str,
    exchange: &Value,
    raw_req: &[u8],
    raw_resp: Option<&[u8]>,
) -> Api<()> {
    let exchange_id = exchange["exchange_id"].as_str().unwrap_or("");
    if exchange_id.is_empty() {
        return Ok(());
    }
    let url = exchange["observed_url"].as_str().unwrap_or("");
    let parsed_url = parse_url(url).ok();
    let host = parsed_url
        .as_ref()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_default();
    let path = parsed_url
        .as_ref()
        .map(|u| u.path().to_owned())
        .unwrap_or_else(|| "/".to_owned());

    let (req_text, req_spilled) = prepare_index_text(raw_req);
    let (req_sha, req_rel) = if req_spilled {
        put_vault_object(s, task_id, raw_req)?
    } else {
        (format!("{:x}", Sha256::digest(raw_req)), String::new())
    };

    let (resp_size, resp_sha, resp_spilled, resp_rel, resp_text) = match raw_resp {
        Some(bytes) => {
            let (text, spilled) = prepare_index_text(bytes);
            let (sha, rel) = if spilled {
                put_vault_object(s, task_id, bytes)?
            } else {
                (format!("{:x}", Sha256::digest(bytes)), String::new())
            };
            (bytes.len() as i64, sha, spilled, rel, text)
        }
        None => (0_i64, String::new(), false, String::new(), String::new()),
    };

    let mut conn = open_index(s, task_id)?;
    let tx = conn.transaction().map_err(db)?;
    let existing_bookmark: Option<(i64, String)> = tx
        .query_row(
            "SELECT bookmarked, bookmark_label FROM traffic_exchanges WHERE exchange_id = ?1",
            params![exchange_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(db)?;
    let (bookmarked, bookmark_label) = existing_bookmark.unwrap_or((0, String::new()));

    tx.execute(
        "INSERT OR REPLACE INTO traffic_exchanges (
            exchange_id, method, observed_url, host, path, status_code, protocol,
            context_id, source_run_id, source_kind, mime, started_at,
            req_size, req_sha256, req_spilled, req_vault_path,
            resp_size, resp_sha256, resp_spilled, resp_vault_path,
            bookmarked, bookmark_label, updated_at
        ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23)",
        params![
            exchange_id,
            exchange["method"].as_str().unwrap_or("GET"),
            url,
            host,
            path,
            exchange["status_code"].as_i64(),
            exchange["protocol"].as_str().unwrap_or("http/1.1"),
            exchange["context_id"].as_str().unwrap_or("default"),
            exchange["source_run_id"].as_str().unwrap_or(""),
            exchange["source_kind"].as_str().unwrap_or("http_tool"),
            exchange["metadata"]["media_type"].as_str().unwrap_or(""),
            exchange["started_at"].as_str().unwrap_or(""),
            raw_req.len() as i64,
            req_sha,
            i64::from(req_spilled),
            req_rel,
            resp_size,
            resp_sha,
            i64::from(resp_spilled),
            resp_rel,
            bookmarked,
            bookmark_label,
            now(),
        ],
    )
    .map_err(db)?;
    tx.execute("DELETE FROM traffic_fts WHERE exchange_id = ?1", params![exchange_id])
        .map_err(db)?;
    tx.execute(
        "INSERT INTO traffic_fts (exchange_id, url, req_text, resp_text) VALUES (?1,?2,?3,?4)",
        params![exchange_id, url, req_text, resp_text],
    )
    .map_err(db)?;
    tx.commit().map_err(db)?;
    Ok(())
}

fn ensure_backfilled(s: &AppState, task_id: &str) -> Api<()> {
    let http_rows = records(s, task_id, "http_exchange")?;
    let import_rows = records(s, task_id, "import_exchange")?;
    let expected = http_rows.len() + import_rows.len();
    if expected == 0 {
        return Ok(());
    }
    let conn = open_index(s, task_id)?;
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM traffic_exchanges", [], |r| r.get(0))
        .map_err(db)?;
    if count as usize >= expected {
        return Ok(());
    }
    drop(conn);
    backfill_missing(s, task_id)
}

pub(super) fn backfill_missing(s: &AppState, task_id: &str) -> Api<()> {
    let mut rows = records(s, task_id, "http_exchange")?;
    rows.extend(records(s, task_id, "import_exchange")?);
    if rows.is_empty() {
        return Ok(());
    }
    let indexed = {
        let conn = open_index(s, task_id)?;
        let mut stmt = conn.prepare("SELECT exchange_id FROM traffic_exchanges").map_err(db)?;
        let ids = stmt.query_map([], |r| r.get::<_, String>(0)).map_err(db)?;
        ids.collect::<Result<std::collections::HashSet<_>, _>>().map_err(db)?
    };
    for row in rows {
        let ex = &row["payload"];
        if ex["exchange_id"].as_str().is_some_and(|id| indexed.contains(id)) {
            continue;
        }
        let Ok(req_ref) = verify_ref(s, task_id, &ex["request_raw_ref"]) else {
            continue;
        };
        let Ok(req_bytes) = base64::engine::general_purpose::STANDARD.decode(req_ref["data"].as_str().unwrap_or("")) else {
            continue;
        };
        let resp_bytes = if ex["response_raw_ref"].is_object() {
            verify_ref(s, task_id, &ex["response_raw_ref"])
                .ok()
                .and_then(|r| base64::engine::general_purpose::STANDARD.decode(r["data"].as_str().unwrap_or("")).ok())
        } else {
            None
        };
        index_exchange(s, task_id, ex, &req_bytes, resp_bytes.as_deref())?;
    }
    Ok(())
}

pub(super) fn set_bookmark(
    s: &AppState,
    task_id: &str,
    exchange_id: &str,
    bookmarked: bool,
    label: &str,
) -> Api<()> {
    ensure_backfilled(s, task_id)?;
    let conn = open_index(s, task_id)?;
    let updated = conn
        .execute(
            "UPDATE traffic_exchanges SET bookmarked = ?1, bookmark_label = ?2, updated_at = ?3 WHERE exchange_id = ?4",
            params![i64::from(bookmarked), label, now(), exchange_id],
        )
        .map_err(db)?;
    if updated == 0 {
        return Err(Error::new(
            StatusCode::NOT_FOUND,
            "src.record_not_found",
            "exchange is not in traffic index",
        ));
    }
    Ok(())
}

pub(super) fn search(s: &AppState, task_id: &str, params_val: &Value) -> Api<Value> {
    let _ = super::sync_inspection(s, task_id);
    ensure_backfilled(s, task_id)?;
    let conn = open_index(s, task_id)?;
    let query = params_val["query"].as_str().map(str::trim).unwrap_or("");
    let limit = params_val["limit"].as_u64().unwrap_or(50).clamp(1, 200) as i64;
    let offset = params_val["cursor"]
        .as_str()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0);

    let mut sql_where = vec!["1=1".to_owned()];
    let mut bind_vals: Vec<rusqlite::types::Value> = Vec::new();
    let push_eq = |col: &str, key: &str, upper: bool, where_parts: &mut Vec<String>, binds: &mut Vec<rusqlite::types::Value>| {
        if let Some(val) = params_val[key].as_str().map(str::trim).filter(|v| !v.is_empty()) {
            binds.push(rusqlite::types::Value::Text(if upper {
                val.to_ascii_uppercase()
            } else {
                val.to_ascii_lowercase()
            }));
            where_parts.push(format!("e.{col} = ?{}", binds.len()));
        }
    };
    push_eq("method", "method", true, &mut sql_where, &mut bind_vals);
    if let Some(host) = params_val["host"].as_str().map(str::trim).filter(|v| !v.is_empty()) {
        bind_vals.push(rusqlite::types::Value::Text(host.to_ascii_lowercase()));
        sql_where.push(format!("e.host = ?{}", bind_vals.len()));
    }
    if let Some(ctx) = params_val["context_id"].as_str().map(str::trim).filter(|v| !v.is_empty()) {
        bind_vals.push(rusqlite::types::Value::Text(ctx.to_owned()));
        sql_where.push(format!("e.context_id = ?{}", bind_vals.len()));
    }
    if let Some(run_id) = params_val["source_run_id"].as_str().map(str::trim).filter(|v| !v.is_empty()) {
        bind_vals.push(rusqlite::types::Value::Text(run_id.to_owned()));
        sql_where.push(format!("e.source_run_id = ?{}", bind_vals.len()));
    }
    if let Some(mime) = params_val["mime"].as_str().map(str::trim).filter(|v| !v.is_empty()) {
        bind_vals.push(rusqlite::types::Value::Text(format!("%{}%", mime.to_ascii_lowercase())));
        sql_where.push(format!("LOWER(e.mime) LIKE ?{}", bind_vals.len()));
    }
    if let Some(code) = params_val["status_code"].as_i64() {
        bind_vals.push(rusqlite::types::Value::Integer(code));
        sql_where.push(format!("e.status_code = ?{}", bind_vals.len()));
    }
    if params_val["bookmarked_only"] == true {
        sql_where.push("e.bookmarked = 1".to_owned());
    }

    let use_fts = query.chars().count() >= 3;
    let join_clause = if use_fts {
        let escaped = format!("\"{}\"", query.replace('"', "\"\""));
        bind_vals.push(rusqlite::types::Value::Text(escaped));
        sql_where.push(format!("traffic_fts MATCH ?{}", bind_vals.len()));
        "JOIN traffic_fts ON traffic_fts.exchange_id = e.exchange_id"
    } else if !query.is_empty() {
        let like = format!("%{}%", query.to_ascii_lowercase());
        bind_vals.push(rusqlite::types::Value::Text(like.clone()));
        let idx1 = bind_vals.len();
        bind_vals.push(rusqlite::types::Value::Text(like));
        let idx2 = bind_vals.len();
        sql_where.push(format!(
            "(LOWER(e.observed_url) LIKE ?{idx1} OR LOWER(f.req_text) LIKE ?{idx2} OR LOWER(f.resp_text) LIKE ?{idx2})"
        ));
        "LEFT JOIN traffic_fts f ON f.exchange_id = e.exchange_id"
    } else {
        ""
    };

    let sql = format!(
        "SELECT e.exchange_id, e.method, e.observed_url, e.host, e.path, e.status_code, e.protocol,
                e.context_id, e.source_run_id, e.source_kind, e.mime, e.started_at,
                e.req_size, e.req_sha256, e.req_spilled, e.resp_size, e.resp_sha256, e.resp_spilled,
                e.bookmarked, e.bookmark_label
         FROM traffic_exchanges e
         {join_clause}
         WHERE {}
         ORDER BY e.started_at DESC, e.rowid DESC
         LIMIT ?{} OFFSET ?{}",
        sql_where.join(" AND "),
        bind_vals.len() + 1,
        bind_vals.len() + 2
    );
    bind_vals.push(rusqlite::types::Value::Integer(limit + 1));
    bind_vals.push(rusqlite::types::Value::Integer(offset));

    let mut stmt = conn.prepare(&sql).map_err(db)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(bind_vals.iter()), |r| {
            Ok(json!({
                "exchange_id": r.get::<_, String>(0)?,
                "method": r.get::<_, String>(1)?,
                "observed_url": r.get::<_, String>(2)?,
                "host": r.get::<_, String>(3)?,
                "path": r.get::<_, String>(4)?,
                "status_code": r.get::<_, Option<i64>>(5)?,
                "protocol": r.get::<_, String>(6)?,
                "context_id": r.get::<_, String>(7)?,
                "source_run_id": r.get::<_, String>(8)?,
                "source_kind": r.get::<_, String>(9)?,
                "mime": r.get::<_, String>(10)?,
                "started_at": r.get::<_, String>(11)?,
                "request_size": r.get::<_, i64>(12)?,
                "request_sha256": r.get::<_, String>(13)?,
                "request_spilled": r.get::<_, i64>(14)? != 0,
                "response_size": r.get::<_, i64>(15)?,
                "response_sha256": r.get::<_, String>(16)?,
                "response_spilled": r.get::<_, i64>(17)? != 0,
                "bookmarked": r.get::<_, i64>(18)? != 0,
                "bookmark_label": r.get::<_, String>(19)?,
            }))
        })
        .map_err(db)?;
    let mut items = Vec::new();
    for item in rows {
        items.push(item.map_err(db)?);
    }
    let has_more = items.len() > limit as usize;
    items.truncate(limit as usize);
    let next_cursor = if has_more {
        json!((offset + items.len() as i64).to_string())
    } else {
        Value::Null
    };
    Ok(json!({
        "items": items,
        "returned": items.len(),
        "next_cursor": next_cursor,
        "fts_tokenizer": "trigram",
        "inline_limit_bytes": INLINE_FTS_LIMIT,
    }))
}

pub(super) fn read_bytes(s: &AppState, task_id: &str, params_val: &Value) -> Api<Value> {
    let exchange_id = require(params_val, "exchange_id")?;
    let part = params_val["part"].as_str().unwrap_or("both");
    let offset = params_val["offset"].as_u64().unwrap_or(0) as usize;
    let limit = params_val["limit"].as_u64().unwrap_or(8 * 1024 * 1024).clamp(1, 32 * 1024 * 1024) as usize;

    let conn = open_index(s, task_id)?;
    let row: Option<(bool, String, String, bool, String, String)> = conn
        .query_row(
            "SELECT req_spilled, req_sha256, req_vault_path, resp_spilled, resp_sha256, resp_vault_path
             FROM traffic_exchanges WHERE exchange_id = ?1",
            params![exchange_id],
            |r| Ok((r.get::<_, i64>(0)? != 0, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? != 0, r.get(4)?, r.get(5)?)),
        )
        .optional()
        .map_err(db)?;

    let ex = super::find_exchange_record(s, task_id, exchange_id)?["payload"].clone();
    let load_part = |spilled: bool, sha: &str, vpath: &str, ref_val: &Value| -> Api<Option<Value>> {
        let bytes = if spilled && !vpath.is_empty() {
            read_vault_object(s, task_id, vpath, sha)?
        } else if ref_val.is_object() {
            let art = verify_ref(s, task_id, ref_val)?;
            base64::engine::general_purpose::STANDARD
                .decode(art["data"].as_str().unwrap_or(""))
                .map_err(|_| Error::invalid("artifact encoding invalid"))?
        } else {
            return Ok(None);
        };
        let total = bytes.len();
        let start = offset.min(total);
        let end = start.saturating_add(limit).min(total);
        let slice = &bytes[start..end];
        let text = (!slice.contains(&0)).then(|| String::from_utf8(slice.to_vec()).ok()).flatten();
        Ok(Some(json!({
            "total_size": total,
            "offset": start,
            "length": slice.len(),
            "complete": start == 0 && end == total,
            "sha256": format!("{:x}", Sha256::digest(&bytes)),
            "spilled": spilled,
            "encoding": if text.is_some() { "utf-8" } else { "base64" },
            "text": text,
            "base64": base64::engine::general_purpose::STANDARD.encode(slice),
        })))
    };

    let (req_s, req_h, req_p, resp_s, resp_h, resp_p) = row.unwrap_or_default();
    Ok(json!({
        "exchange_id": exchange_id,
        "request": if matches!(part, "request" | "both") {
            load_part(req_s, &req_h, &req_p, &ex["request_raw_ref"])?.unwrap_or(Value::Null)
        } else {
            Value::Null
        },
        "response": if matches!(part, "response" | "both") {
            load_part(resp_s, &resp_h, &resp_p, &ex["response_raw_ref"])?.unwrap_or(Value::Null)
        } else {
            Value::Null
        },
    }))
}

fn format_hex_dump(bytes: &[u8]) -> String {
    let slice = &bytes[..bytes.len().min(512)];
    let mut lines = Vec::new();
    for (row_idx, chunk) in slice.chunks(16).enumerate() {
        let offset = row_idx * 16;
        let mut hex_part = String::with_capacity(48);
        let mut ascii_part = String::with_capacity(16);
        for (i, &b) in chunk.iter().enumerate() {
            if i > 0 {
                hex_part.push(' ');
            }
            use std::fmt::Write;
            let _ = write!(&mut hex_part, "{b:02x}");
            ascii_part.push(if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' });
        }
        lines.push(format!("{offset:04x}  {hex_part:<47}  |{ascii_part}|"));
    }
    if bytes.len() > 512 {
        lines.push(format!("... (+{} bytes total)", bytes.len() - 512));
    }
    lines.join("\n")
}

pub(super) fn wire_diagnostics(s: &AppState, task_id: &str, params_val: &Value) -> Api<Value> {
    let exchange_id = require(params_val, "exchange_id")?;
    let ex = super::find_exchange_record(s, task_id, exchange_id)?["payload"].clone();
    let raw = read_bytes(s, task_id, &json!({"exchange_id": exchange_id, "part": "both"}))?;

    let preview_of = |part: &Value| -> (String, String) {
        let b64 = part["base64"].as_str().unwrap_or("");
        if b64.is_empty() {
            return (String::new(), String::new());
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap_or_default();
        let sha = part["sha256"].as_str().unwrap_or("").to_owned();
        (sha, format_hex_dump(&bytes))
    };

    let (req_sha256, req_hex_preview) = preview_of(&raw["request"]);
    let (resp_sha256, resp_hex_preview) = preview_of(&raw["response"]);

    Ok(json!({
        "exchange_id": exchange_id,
        "protocol": ex["protocol"],
        "method": ex["method"],
        "observed_url": ex["observed_url"],
        "status_code": ex["status_code"],
        "duration_ms": ex["duration_ms"],
        "context_id": ex["context_id"],
        "source_run_id": ex["source_run_id"],
        "worker_id": ex["worker_id"],
        "proxy_node_id": ex["proxy_node_id"],
        "request_sha256": req_sha256,
        "response_sha256": resp_sha256,
        "request_hex_preview": req_hex_preview,
        "response_hex_preview": resp_hex_preview,
        "request": raw["request"],
        "response": raw["response"],
    }))
}

pub(super) fn mcp_circuits(s: &AppState) -> Api<Value> {
    let circuits = s
        .plugins
        .opened()
        .map(|reg| reg.mcp_circuit_snapshots())
        .unwrap_or_default();
    let cooldowns = crate::agent_runtime::credentials::quota_cooldown_snapshots();
    Ok(json!({
        "mcp_circuits": circuits,
        "quota_cooldowns": cooldowns,
    }))
}

pub(super) fn mcp_reset(s: &AppState, body: &Value) -> Api<Value> {
    let filter = body["server"].as_str();
    let reset_mcp_count = s
        .plugins
        .opened()
        .map(|reg| reg.reset_mcp_circuits(filter))
        .unwrap_or(0);
    let reset_quota_count = crate::agent_runtime::credentials::clear_quota_cooldowns(filter);
    Ok(json!({
        "reset_mcp_count": reset_mcp_count,
        "reset_quota_count": reset_quota_count,
        "reset_at": now(),
    }))
}
