use super::*;
use std::io::{Read, Seek, SeekFrom};
const NAMES: [&str; 4] = [
    "board_query",
    "events_query",
    "artifact_read",
    "workspace_read",
];
pub(super) fn schemas() -> Value {
    json!(NAMES.map(|name|json!({"type":"function","function":{"name":name,"description":match name{"board_query"=>"Page graph: type facts/steps/goals/dead_ends/operator_notes, id/status/search/cursor/limit","events_query"=>"Page events by after_seq/before_seq/kind/query/limit; view evidence (default) excludes assistant notes, conversation includes them","artifact_read"=>"Read run artifact path or artifact_id, offset and limit (bytes); no mutations",_=>"Read workspace with mode list/read, relative path, offset and limit; pages include next_offset"},"parameters":{"type":"object","properties":{"type":{"type":"string"},"id":{"type":"string"},"status":{"type":"string"},"search":{"type":"string"},"cursor":{"type":"integer"},"limit":{"type":"integer"},"after_seq":{"type":"integer"},"before_seq":{"type":"integer"},"kind":{"type":"string"},"query":{"type":"string"},"view":{"type":"string","enum":["evidence","conversation"]},"artifact_id":{"type":"string"},"mode":{"type":"string","enum":["list","read"]},"path":{"type":"string"},"offset":{"type":"integer"}},"additionalProperties":false}}})))
}
fn page(rows: Vec<Value>, args: &Value) -> Value {
    let offset = args["cursor"].as_u64().unwrap_or(0) as usize;
    let limit = args["limit"].as_u64().unwrap_or(50).clamp(1, 200) as usize;
    let total = rows.len();
    let rows = rows
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    json!({"items":rows,"total":total,"next_cursor":if offset+rows.len()<total{Some(offset+rows.len())}else{None}})
}
pub(super) fn query(
    state: &AppState,
    run: &str,
    name: &str,
    args: &Value,
) -> Result<Value, Failure> {
    if !NAMES.contains(&name) {
        return Err(Failure::new(
            "tool_forbidden",
            "Only the four read-only tools are allowed",
        ));
    }
    match name {
        "board_query" => {
            let path = graph::graph_dir(state, run).join("shared_graph.db");
            if !path.is_file() {
                return Ok(page(Vec::new(), args));
            }
            let conn = rusqlite::Connection::open_with_flags(
                path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )
            .map_err(storage)?;
            let sql = match args["type"].as_str().unwrap_or("facts") {
                "facts" => {
                    "SELECT e.seq AS id,e.kind,e.payload,e.verified,COALESCE(s.status,'active') AS status FROM events e LEFT JOIN fact_states s ON s.fact_seq=e.seq WHERE e.kind='fact_added'"
                }
                "steps" => "SELECT * FROM intents",
                "goals" => "SELECT * FROM sub_goals",
                "dead_ends" => "SELECT seq AS id,kind,payload FROM events WHERE kind='dead_end'",
                "operator_notes" => "SELECT * FROM operator_directives",
                _ => return Err(Failure::new("tool_arguments", "Unknown board type")),
            };
            let mut stmt = conn.prepare(sql).map_err(storage)?;
            let names = stmt
                .column_names()
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>();
            let rows = stmt
                .query_map([], |row| {
                    let mut obj = serde_json::Map::new();
                    for (i, key) in names.iter().enumerate() {
                        use rusqlite::types::ValueRef;
                        let v = match row.get_ref(i)? {
                            ValueRef::Null => Value::Null,
                            ValueRef::Integer(n) => json!(n),
                            ValueRef::Real(n) => json!(n),
                            ValueRef::Text(t) => {
                                let s = String::from_utf8_lossy(t);
                                if key == "payload" {
                                    serde_json::from_str(&s).unwrap_or(json!(s))
                                } else {
                                    json!(s)
                                }
                            }
                            ValueRef::Blob(_) => json!({"binary":true}),
                        };
                        obj.insert(key.clone(), v);
                    }
                    Ok(Value::Object(obj))
                })
                .map_err(storage)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(storage)?;
            let rows = rows
                .into_iter()
                .filter(|r| {
                    args["status"].as_str().is_none_or(|s| r["status"] == s)
                        && args["search"]
                            .as_str()
                            .is_none_or(|s| r.to_string().contains(s))
                        && args["id"].as_str().is_none_or(|s| {
                            ["id", "directive_id", "intent_id", "goal_id", "sub_goal_id"]
                                .iter()
                                .any(|key| {
                                    r[*key].as_str() == Some(s)
                                        || r[*key].as_i64().is_some_and(|id| id.to_string() == s)
                                })
                        })
                })
                .collect();
            Ok(page(rows, args))
        }
        "events_query" => {
            let limit = args["limit"].as_u64().unwrap_or(50).clamp(1, 200) as usize;
            let mut rows = Vec::new();
            let mut more = false;
            for e in persisted_event_scan(state, run, args["after_seq"].as_u64().unwrap_or(0))?
                .filter_map(|(_, e)| e)
            {
                let seq = e["seq"].as_u64().unwrap_or(0);
                let kind = e["event_type"].as_str().unwrap_or("");
                if args["view"] != "conversation"
                    && (kind.starts_with("followup.")
                        || e["payload"]["provenance"] == "non_evidence_assistant_note")
                {
                    continue;
                }
                if args["after_seq"].as_u64().is_some_and(|s| seq <= s)
                    || args["before_seq"].as_u64().is_some_and(|s| seq >= s)
                    || args["kind"].as_str().is_some_and(|s| kind != s)
                    || args["query"]
                        .as_str()
                        .is_some_and(|s| !e.to_string().contains(s))
                {
                    continue;
                }
                if rows.len() == limit {
                    more = true;
                    break;
                }
                rows.push(e);
            }
            Ok(
                json!({"items":rows,"has_more":more,"next_after_seq":rows.last().map(|r|r["seq"].clone())}),
            )
        }
        "artifact_read" => {
            if let Some(id) = args["artifact_id"].as_str() {
                if let Some(row) = state
                    .store
                    .get(ARTIFACTS, id)
                    .map_err(storage)?
                    .filter(|r| r["run_id"] == run)
                {
                    return Ok(text_page(row["markdown"].as_str().unwrap_or(""), args));
                }
                let (artifact, metadata) = evidence::artifact(state, run, id)?;
                let mut page = text_page(
                    artifact["content"]
                        .as_str()
                        .ok_or_else(|| Failure::new("artifact_encoding", "Artifact is not text"))?,
                    args,
                );
                page["metadata"] = metadata;
                return Ok(page);
            }
            read_path(&shared_pools::real_workspace(state, run), args, false)
        }
        _ => read_path(&shared_pools::real_workspace(state, run), args, true),
    }
}
fn denied(path: &std::path::Path) -> bool {
    path.components().any(|p| {
        let s = p.as_os_str().to_string_lossy();
        s.starts_with(".muteki")
            || s.starts_with(".secret")
            || s.ends_with(".db")
            || s.ends_with(".db-wal")
            || s.ends_with(".db-shm")
            || s.ends_with(".sqlite")
            || s.ends_with(".sqlite3")
            || s.ends_with(".wal")
            || matches!(p, std::path::Component::ParentDir)
    })
}
fn text_page(text: &str, args: &Value) -> Value {
    let mut offset = (args["offset"].as_u64().unwrap_or(0) as usize).min(text.len());
    while !text.is_char_boundary(offset) {
        offset += 1;
    }
    let mut end =
        (offset + args["limit"].as_u64().unwrap_or(16384).clamp(1, 65536) as usize).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    json!({"text":&text[offset..end],"offset":offset,"bytes":text.len(),"next_offset":if end<text.len(){Some(end)}else{None}})
}
fn read_path(root: &std::path::Path, args: &Value, list: bool) -> Result<Value, Failure> {
    let root = root.canonicalize()?;
    let relative = std::path::Path::new(args["path"].as_str().unwrap_or("."));
    if relative.is_absolute() || denied(relative) {
        return Err(Failure::new(
            "path_forbidden",
            "Path is outside readable workspace",
        ));
    }
    let target = root.join(relative).canonicalize()?;
    if !target.starts_with(&root) || denied(target.strip_prefix(&root).unwrap()) {
        return Err(Failure::new(
            "path_forbidden",
            "Path escapes readable workspace",
        ));
    }
    // Open every component relative to an already opened directory: no symlink race.
    #[cfg(unix)]
    let mut file = {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};
        let mut options = fs::OpenOptions::new();
        options.read(true);
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY);
        let mut fd = options.open(&root)?;
        for part in target.strip_prefix(&root).unwrap().components() {
            let name = CString::new(part.as_os_str().as_encoded_bytes())
                .map_err(|e| Failure::from(e.to_string()))?;
            let raw = unsafe {
                libc::openat(
                    fd.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if raw < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            fd = unsafe { fs::File::from_raw_fd(raw) };
        }
        fd
    };
    #[cfg(not(unix))]
    let mut file = fs::File::open(&target)?;
    let meta = file.metadata()?;
    if meta.is_dir() {
        if !list || args["mode"] == "read" {
            return Err(Failure::new(
                "tool_arguments",
                "Path is a directory; pass a file path or use workspace_read with mode=list",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let current = fs::symlink_metadata(&target)?;
            if (current.dev(), current.ino()) != (meta.dev(), meta.ino()) {
                return Err(Failure::new(
                    "path_forbidden",
                    "Directory changed during read",
                ));
            }
        }
        let mut entries = Vec::new();
        for e in fs::read_dir(&target)? {
            let e = e?;
            if denied(std::path::Path::new(&e.file_name())) {
                continue;
            }
            let ft = e.file_type()?;
            if ft.is_symlink() {
                continue;
            }
            entries.push(
                json!({"name":e.file_name().to_string_lossy(),"directory":ft.is_dir()}),
            );
        }
        entries.sort_by_key(|e| e["name"].as_str().unwrap_or("").to_owned());
        return Ok(page(
            entries,
            &json!({"cursor":args["offset"],"limit":args["limit"]}),
        ));
    }
    let total = meta.len();
    let offset = args["offset"].as_u64().unwrap_or(0);
    let limit = args["limit"].as_u64().unwrap_or(16384).clamp(1, 65536);
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes)?;
    let text = String::from_utf8(bytes.clone()).map_err(|_| {
        Failure::new(
            "artifact_encoding",
            "Requested byte range is not UTF-8; align offset/limit to character boundaries",
        )
    })?;
    let end = offset + bytes.len() as u64;
    Ok(
        json!({"text":text,"offset":offset,"bytes":total,"next_offset":if end<total{Some(end)}else{None}}),
    )
}
