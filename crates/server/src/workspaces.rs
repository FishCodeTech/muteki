//! Durable ordinary project/workspace bindings and sidebar organization.
//! Explicit worktree commands manage Git checkouts; metadata-only removal keeps files.
use crate::AppState;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashSet, sync::Arc};
use tokio::sync::Mutex;
use uuid::Uuid;
type Error = (StatusCode, Json<Value>);
type Api = Result<Json<Value>, Error>;
const PROJECTS: &str = "projects";
const WORKSPACES: &str = "workspaces";
const REMOVALS: &str = "workspace_removals";
const REMOVAL_HISTORY: &str = "workspace_removal_history";
static WRITES: Mutex<()> = Mutex::const_new(());
fn error(status: StatusCode, code: &str, message: &str) -> Error {
    (
        status,
        Json(json!({"detail":message,"error":{"code":code,"message":message}})),
    )
}
fn invalid(message: &str) -> Error {
    error(
        StatusCode::UNPROCESSABLE_ENTITY,
        "validation.invalid",
        message,
    )
}
fn db(_: rusqlite::Error) -> Error {
    error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "storage.failed",
        "Could not access metadata",
    )
}
fn missing(kind: &str) -> Error {
    error(
        StatusCode::NOT_FOUND,
        "metadata.not_found",
        &format!("{kind} not found"),
    )
}
fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}
fn load(s: &AppState, collection: &str, id: &str) -> Result<Value, Error> {
    s.store
        .get(collection, id)
        .map_err(db)?
        .ok_or_else(|| missing(collection))
}
fn object(body: &Value) -> Result<(), Error> {
    if body.is_object() {
        Ok(())
    } else {
        Err(invalid("Expected a JSON object"))
    }
}
fn text<'a>(body: &'a Value, key: &str, default: &'a str) -> Result<&'a str, Error> {
    match body.get(key) {
        None => Ok(default),
        Some(Value::String(v)) => Ok(v),
        _ => Err(invalid(&format!("{key} must be a string"))),
    }
}
fn settings(body: &Value) -> Result<Value, Error> {
    match body.get("settings") {
        None => Ok(json!({})),
        Some(v) if v.is_object() => Ok(v.clone()),
        _ => Err(invalid("settings must be an object")),
    }
}
fn canonical_directory(raw: &str) -> Result<String, Error> {
    if raw.trim().is_empty() {
        return Err(invalid("root_path is required"));
    }
    let expanded = if raw == "~" || raw.starts_with("~/") {
        std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .map(|p| p.join(raw.strip_prefix("~/").unwrap_or("")))
            .ok_or_else(|| invalid("Could not expand home directory"))?
    } else {
        std::path::PathBuf::from(raw)
    };
    let path = expanded
        .canonicalize()
        .map_err(|_| invalid("Workspace directory does not exist or is inaccessible"))?;
    if !path.is_dir() {
        return Err(invalid("Workspace root must be a directory"));
    }
    Ok(path.to_string_lossy().into_owned())
}
fn receipt(body: &Value, kind: &str, id: &str, output: Value) -> Value {
    json!({"receipt":{"command_id":body.get("command_id").cloned().unwrap_or_else(||json!(new_id("cmd"))),"state":"completed","aggregate":{"type":kind,"id":id},"output":output}})
}
fn replay(s: &AppState, scope: &str, body: &Value) -> Result<Option<Value>, Error> {
    for field in ["command_id", "idempotency_key"] {
        let key = text(body, field, "")?.trim();
        if key.is_empty() {
            continue;
        }
        if let Some(old) = s
            .store
            .get("workspace_commands", &format!("{field}:{key}"))
            .map_err(db)?
        {
            let mut request = body.clone();
            let mut prior = old["request"].clone();
            if field == "idempotency_key" {
                request.as_object_mut().unwrap().remove("command_id");
                prior.as_object_mut().unwrap().remove("command_id");
            }
            if old["scope"] != scope || prior != request {
                return Err(error(
                    StatusCode::CONFLICT,
                    "command.conflict",
                    "Command identity was already used for another request",
                ));
            }
            return Ok(Some(old["response"].clone()));
        }
    }
    Ok(None)
}
fn commit_metadata(
    s: &AppState,
    scope: &str,
    body: &Value,
    response: &mut Value,
    expected: &[(&str, &str, Option<&Value>)],
    records: &[(&str, &str, &Value)],
    changes: &[(&str, &str, &str, Value)],
) -> Result<(), Error> {
    use crate::operations_api::{command_fingerprint, domain_event_cursor, current_aggregate_version, domain_events};
    let command_id = response["receipt"]["command_id"].as_str().unwrap().to_owned();
    let kind = response["receipt"]["aggregate"]["type"].as_str().unwrap().to_owned();
    let id = response["receipt"]["aggregate"]["id"].as_str().unwrap().to_owned();
    let envelope = if body["_platform_envelope"].is_object() {
        body["_platform_envelope"].clone()
    } else {
        json!({"command_id":command_id,"command_type":format!("conversation.{}",scope.split(':').next().unwrap_or(scope)),
            "aggregate_type":kind,"aggregate_id":id,"actor":body["actor"],"idempotency_key":body["idempotency_key"],"payload":body})
    };
    let principal = envelope["_verified_scope"].as_str().unwrap_or("operator:local-user");
    let version = current_aggregate_version(s,&kind,&id).map_err(|message| error(StatusCode::INTERNAL_SERVER_ERROR,"storage.failed",&message))?;
    if let Some(expected) = envelope["expected_version"].as_u64() {
        if expected != version {
            return Err(error(StatusCode::CONFLICT,"command.version_conflict","Metadata version changed before commit"));
        }
    }
    let count = changes.iter().filter(|(ty,aid,_,_)| *ty == kind && *aid == id).count() as u64;
    response["receipt"]["event_cursor"] = json!(domain_event_cursor(s,&kind,&id,version+count,principal)
        .map_err(|message| error(StatusCode::INTERNAL_SERVER_ERROR,"storage.failed",&message))?);
    response["receipt"]["schema_version"] = json!(1);
    response["receipt"]["completed_at"] = json!(now());
    let previous = s.store.get("platform_commands",&command_id).map_err(db)?;
    let fingerprint = command_fingerprint(&envelope);
    if previous.as_ref().is_some_and(|prior| prior["request_fingerprint"].as_str().is_some_and(|value| value != fingerprint)) {
        return Err(error(StatusCode::CONFLICT,"command.conflict","Command identity was used for another request"));
    }
    let platform = json!({"request":envelope,"request_fingerprint":fingerprint,"response":response,"status":200});
    let entry = json!({"scope":scope,"request":body,"response":response});
    let mut keys = vec![(format!("command_id:{command_id}"),entry.clone())];
    let key = text(body,"idempotency_key","")?.trim();
    if !key.is_empty() { keys.push((format!("idempotency_key:{key}"),entry)); }
    let mut conditions = expected.to_vec();
    let mut writes = records.to_vec();
    conditions.push(("platform_commands",command_id.as_str(),previous.as_ref()));
    writes.push(("platform_commands",command_id.as_str(),&platform));
    for (key,value) in &keys {
        conditions.push(("workspace_commands",key.as_str(),None));
        writes.push(("workspace_commands",key.as_str(),value));
    }
    let mut cursor_version = version;
    let events: Vec<Value> = changes.iter().map(|(event_kind,event_id,event_type,payload)| {
        let mut event = domain_events::event(&envelope,event_kind,event_id,event_type,"builtin.conversation",payload.clone());
        if *event_kind == kind && *event_id == id {
            cursor_version += 1;
            event["stream_seq"] = json!(cursor_version);
        }
        event
    }).collect();
    if domain_events::commit_checked(s,&conditions,&writes,&events,&[],|transaction| {
        if scope == "project.create" {
            if let Some(root) = records.iter().find(|(collection,_,value)| *collection == WORKSPACES && value["kind"] == "local")
                .and_then(|(_,_,value)|value["root_path"].as_str()) {
                let occupied: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM documents w
                    JOIN documents p ON p.collection='projects' AND p.id=json_extract(w.value,'$.project_id')
                    WHERE w.collection='workspaces' AND json_extract(w.value,'$.kind')='local'
                    AND json_extract(w.value,'$.root_path')=?1)",[root],|row|row.get(0))?;
                if occupied { return Ok(false); }
            }
        }
        Ok(true)
    })?.is_none() {
        return Err(error(StatusCode::CONFLICT,"command.version_conflict","Metadata changed during command commit; read current state and retry"));
    }
    Ok(())
}
pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/projects", get(list_projects).post(create_project))
        .route(
            "/api/projects/{id}",
            get(get_project)
                .patch(update_project)
                .delete(delete_project),
        )
        .route("/api/workspaces", get(list_workspaces).post(bind_workspace))
        .route(
            "/api/workspaces/{id}",
            get(get_workspace)
                .patch(update_workspace)
                .delete(delete_workspace),
        )
        .route("/api/workspaces/delete-worktree", post(delete_worktree))
        .route(
            "/api/directories/select",
            post(select_directory),
        )
        .route(
            "/api/projects/{id}/git",
            get(crate::workspace_read_api::project_git_status),
        )
        .route(
            "/api/projects/{id}/git/worktrees",
            get(list_project_worktrees),
        )
        .route(
            "/api/sidebar-preferences",
            get(get_sidebar).put(put_sidebar),
        )
        .route("/api/folders", get(list_folders).post(create_folder))
        .route(
            "/api/folders/{id}",
            axum::routing::patch(update_folder).delete(delete_folder),
        )
        .route("/api/runs/{id}", axum::routing::patch(update_run))
}
const DIRECTORY_PICKER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

fn picker_command(start: Option<&std::path::Path>) -> Option<(String, Vec<String>)> {
    let found = |name: &str| {
        std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
    };
    let start = start.map(|path| path.to_string_lossy().into_owned());
    if cfg!(target_os = "macos") {
        let mut args = vec!["-e".to_owned()];
        if let Some(start) = start {
            args.push("on run argv\ntry\n  tell application \"System Events\" to activate\n  set chosenFolder to choose folder with prompt \"选择工作目录\" default location (POSIX file (item 1 of argv))\n  return POSIX path of chosenFolder\non error number -128\n  return \"\"\nend try\nend run".to_owned());
            args.push(start);
        } else {
            args.push("try\n  tell application \"System Events\" to activate\n  set chosenFolder to choose folder with prompt \"选择工作目录\"\n  return POSIX path of chosenFolder\non error number -128\n  return \"\"\nend try".to_owned());
        }
        Some(("osascript".into(), args))
    } else if cfg!(windows) {
        let preset = if start.is_some() { "$dialog.SelectedPath = $env:MUTEKI_PICKER_START; " } else { "" };
        Some(("powershell".into(), vec!["-NoProfile".into(), "-STA".into(), "-Command".into(), format!("Add-Type -AssemblyName System.Windows.Forms; $dialog = New-Object System.Windows.Forms.FolderBrowserDialog; $dialog.Description = '选择工作目录'; {preset}if ($dialog.ShowDialog() -eq 'OK') {{ $dialog.SelectedPath }}")]))
    } else if found("zenity") {
        let mut args = ["--file-selection", "--directory", "--title=选择工作目录"].map(String::from).to_vec();
        if let Some(start) = start {
            args.push(format!("--filename={}/", start.trim_end_matches('/')));
        }
        Some(("zenity".into(), args))
    } else if found("kdialog") {
        Some(("kdialog".into(), vec!["--getexistingdirectory".into(), start.unwrap_or_else(|| ".".into())]))
    } else {
        None
    }
}

/// Where the folder chooser opens, from 设置 → 常规 → 项目. A folder that does not exist leaves the
/// chooser at the system default instead of failing the pick.
fn picker_start(s: &AppState) -> Result<Option<std::path::PathBuf>, String> {
    let ui = s.store.get("settings", "ui").map_err(|error| error.to_string())?;
    let values = ui.as_ref().map(|doc| &doc["values"]).unwrap_or(&Value::Null);
    let text = |key: &str| values[key].as_str().map(str::trim).filter(|path| !path.is_empty());
    let chosen = match values["projectStart"].as_str() {
        Some("new_project_dir") => text("newProjectDir").unwrap_or("~/MutekiProjects"),
        Some("custom") => text("projectStartCustom").unwrap_or("~"),
        _ => "~",
    };
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(std::path::PathBuf::from);
    let path = match chosen.strip_prefix('~') {
        Some(rest) => match home {
            Some(home) => home.join(rest.trim_start_matches(['/', '\\'])),
            None => return Ok(None),
        },
        None => std::path::PathBuf::from(chosen),
    };
    Ok(path.is_dir().then_some(path))
}

/// Open the host's own folder chooser; the service host is the machine the
/// agents run on, so the chosen path is directly usable as a workspace.
async fn pick_directory(start: Option<std::path::PathBuf>) -> Result<Value, String> {
    let (program, args) = picker_command(start.as_deref()).ok_or("当前系统没有可用的目录选择器")?;
    let mut command = crate::host_process::command(&program);
    if let Some(start) = &start {
        command.env("MUTEKI_PICKER_START", start);
    }
    let child = command
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("目录选择器启动失败：{e}"))?;
    let output = tokio::time::timeout(DIRECTORY_PICKER_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| "目录选择器等待超时".to_string())?
        .map_err(|e| format!("目录选择器启动失败：{e}"))?;
    let selected = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let code = output.status.code();
    if !matches!(code, Some(0 | 1)) && selected.is_empty() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if detail.is_empty() { "目录选择器启动失败".into() } else { detail });
    }
    if selected.is_empty() {
        return Ok(json!({"cancelled":true}));
    }
    let path = std::fs::canonicalize(&selected).map_err(|_| "选择的工作目录已经不存在".to_string())?;
    if !path.is_dir() {
        return Err("选择的工作目录已经不存在".into());
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
    Ok(json!({"cancelled":false,"path":path.display().to_string(),"name":name}))
}

async fn select_directory(State(s): State<Arc<AppState>>) -> Api {
    let start = picker_start(&s).map_err(|message| error(StatusCode::INTERNAL_SERVER_ERROR, "conversation.directory_picker.settings_unreadable", &message))?;
    pick_directory(start).await.map(Json).map_err(|message| {
        error(StatusCode::SERVICE_UNAVAILABLE, "conversation.directory_picker.unavailable", &message)
    })
}
#[derive(Default, Deserialize)]
pub(crate) struct ProjectListQuery {
    #[serde(default)]
    pub include_unbound: bool,
    #[serde(default)]
    pub scope: Option<String>,
}

fn effective_project_scope(project: &Value, has_local_ws: bool) -> &'static str {
    match project["scope"].as_str() {
        Some("src") if !has_local_ws => "src",
        Some("chat") => "chat",
        _ if has_local_ws => "chat",
        _ => "src",
    }
}

pub(crate) async fn list_projects(
    State(s): State<Arc<AppState>>,
    Query(q): Query<ProjectListQuery>,
) -> Api {
    let requested_scope = match q.scope.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => None,
        Some(scope @ ("chat" | "src")) => Some(scope),
        Some(_) => return Err(invalid("Invalid project scope")),
    };
    let mut workspaces = s.store.list(WORKSPACES).map_err(db)?;
    workspaces.sort_by_key(|v| v["created_at"].as_str().unwrap_or("").to_owned());
    let mut rows = Vec::new();
    for mut project in s.store.list(PROJECTS).map_err(db)? {
        let local_ws = workspaces
            .iter()
            .rev()
            .find(|ws| ws["project_id"] == project["project_id"] && ws["kind"] == "local");
        let scope = effective_project_scope(&project, local_ws.is_some());
        match requested_scope {
            Some("src") => {
                if local_ws.is_none() && scope == "src" {
                    project["scope"] = json!("src");
                    project["workspace_id"] = Value::Null;
                    project["workspace_kind"] = Value::Null;
                    project["root_path"] = Value::Null;
                    rows.push(project);
                }
            }
            Some("chat") => {
                if let Some(ws) = local_ws
                    && scope == "chat"
                {
                    project["scope"] = json!("chat");
                    project["workspace_id"] = ws["workspace_id"].clone();
                    project["workspace_kind"] = ws["kind"].clone();
                    project["root_path"] = ws["root_path"].clone();
                    rows.push(project);
                }
            }
            _ => {
                if let Some(ws) = local_ws
                    && scope == "chat"
                {
                    project["scope"] = json!(scope);
                    project["workspace_id"] = ws["workspace_id"].clone();
                    project["workspace_kind"] = ws["kind"].clone();
                    project["root_path"] = ws["root_path"].clone();
                    rows.push(project);
                } else if local_ws.is_none() && q.include_unbound {
                    project["scope"] = json!(scope);
                    project["workspace_id"] = Value::Null;
                    project["workspace_kind"] = Value::Null;
                    project["root_path"] = Value::Null;
                    rows.push(project);
                }
            }
        }
    }
    Ok(Json(json!({"count":rows.len(),"projects":rows})))
}
async fn get_project(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Api {
    Ok(Json(load(&s, PROJECTS, &id)?))
}
pub(crate) async fn create_project(State(s): State<Arc<AppState>>, Json(body): Json<Value>) -> Api {
    object(&body)?;
    let _guard = WRITES.lock().await;
    if let Some(old) = replay(&s, "project.create", &body)? {
        return Ok(Json(old));
    }
    let name = text(&body, "name", "")?.trim();
    if name.is_empty() {
        return Err(invalid("Project name is required"));
    }
    let description = text(&body, "description", "")?;
    let meta = settings(&body)?;
    let raw_scope = text(&body, "scope", "")?.trim().to_owned();
    if !matches!(raw_scope.as_str(), "" | "chat" | "src") {
        return Err(invalid("Project scope must be 'chat' or 'src'"));
    }
    let root = text(&body, "root_path", "")?.trim();
    let root = if root.is_empty() {
        None
    } else {
        Some(canonical_directory(root)?)
    };
    let resolved_scope = if !raw_scope.is_empty() {
        raw_scope
    } else if root.is_some() {
        "chat".to_owned()
    } else {
        "src".to_owned()
    };
    if resolved_scope == "src" && root.is_some() {
        return Err(invalid("SRC research projects cannot bind a local workspace directory"));
    }
    if let Some(root) = root.as_ref() {
        for ws in s.store.list(WORKSPACES).map_err(db)? {
            if ws["root_path"] == *root
                && ws["kind"] == "local"
                && s.store
                    .get(PROJECTS, ws["project_id"].as_str().unwrap_or(""))
                    .map_err(db)?
                    .is_some()
            {
                let mut err = error(
                    StatusCode::CONFLICT,
                    "conversation.project.directory_already_bound",
                    "Directory already belongs to a project",
                );
                err.1.0["error"]["detail"] = json!({"project_id":ws["project_id"],"workspace_id":ws["workspace_id"],"root_path":root});
                err.1.0["receipt"] = json!({"command_id":body["command_id"],"state":"conflict","aggregate":{"type":"project","id":ws["project_id"]},"error":err.1.0["error"].clone()});
                return Err(err);
            }
        }
    }
    let id = text(&body, "project_id", "")?.trim().to_owned();
    let id = if id.is_empty() { new_id("proj") } else { id };
    if s.store.get(PROJECTS, &id).map_err(db)?.is_some() {
        return Err(error(
            StatusCode::CONFLICT,
            "project.exists",
            "Project already exists",
        ));
    }
    let time = now();
    let project = json!({"project_id":id,"name":name,"description":description,"scope":resolved_scope,"settings":meta,"created_at":time,"updated_at":time});
    let workspace=root.map(|root|json!({"workspace_id":new_id("ws"),"project_id":id,"kind":"local","root_path":root,"settings":{"mode":""},"created_at":time}));
    let output = if let Some(ws) = workspace.as_ref() {
        json!({"workspace_id":ws["workspace_id"],"root_path":ws["root_path"]})
    } else {
        json!({})
    };
    let mut response = receipt(&body, "project", &id, output);
    let mut expected = vec![(PROJECTS,id.as_str(),None)];
    let mut records = vec![(PROJECTS,id.as_str(),&project)];
    let mut changes = vec![("project",id.as_str(),"core.project.created",json!({"project_id":id,"name":name,"description":description,"scope":resolved_scope}))];
    if let Some(ws) = workspace.as_ref() {
        let wid = ws["workspace_id"].as_str().unwrap();
        expected.push((WORKSPACES,wid,None));
        records.push((WORKSPACES,wid,ws));
        changes.push(("workspace",wid,"core.workspace.bound",json!({"workspace_id":wid,"project_id":id,"kind":"local","root_path":ws["root_path"]})));
    }
    commit_metadata(&s,"project.create",&body,&mut response,&expected,&records,&changes)?;
    Ok(Json(response))
}
pub(crate) async fn update_project(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Api {
    object(&body)?;
    let _guard = WRITES.lock().await;
    let scope = format!("project.update:{id}");
    if let Some(old) = replay(&s, &scope, &body)? {
        return Ok(Json(old));
    }
    let previous = load(&s, PROJECTS, &id)?;
    let mut project = previous.clone();
    for field in ["name", "description"] {
        if body.get(field).is_some() {
            let val = text(&body, field, "")?;
            if field == "name" && val.trim().is_empty() {
                return Err(invalid("Project name is required"));
            }
            project[field] = json!(val);
        }
    }
    if body.get("settings").is_some() {
        let additions = settings(&body)?;
        project["settings"]
            .as_object_mut()
            .ok_or_else(|| invalid("Stored project settings are invalid"))?
            .extend(additions.as_object().unwrap().clone());
    }
    project["updated_at"] = json!(now());
    let mut response = receipt(
        &body,
        "project",
        &id,
        json!({"project_id":id,"name":project["name"],"settings":project["settings"]}),
    );
    commit_metadata(&s,&scope,&body,&mut response,
        &[(PROJECTS,id.as_str(),Some(&previous))],&[(PROJECTS,id.as_str(),&project)],
        &[("project",id.as_str(),"core.project.updated",json!({"project_id":id,"name":project["name"],"description":project["description"],"settings":project["settings"]}))])?;
    Ok(Json(response))
}
pub(crate) async fn delete_project(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Api {
    let mut active = s.chat.active.lock().await;
    let _guard = WRITES.lock().await;
    let project = load(&s, PROJECTS, &id)?;
    let mut live_threads = 0i64;
    let mut live_src_tasks = 0i64;
    let mut purged_threads = Vec::new();
    let mut removed_workspaces = Vec::new();
    let committed = s
        .store
        .put_many_if_with_deletes(&[(PROJECTS, id.as_str(), Some(&project))], &[], |tx| {
            let mut workspaces = tx.prepare(
                "SELECT id FROM documents WHERE collection=?1 AND json_extract(value,'$.project_id')=?2",
            )?;
            removed_workspaces = workspaces
                .query_map(rusqlite::params![WORKSPACES, id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<_>>()?;
            let mut threads = tx.prepare(
                "SELECT id, json_extract(value,'$.thread.state.status') IS 'archived' FROM documents
                 WHERE collection=?1 AND (json_extract(value,'$.thread.project_id')=?2
                   OR json_extract(value,'$.thread.workspace_id') IN
                      (SELECT id FROM documents WHERE collection=?3 AND json_extract(value,'$.project_id')=?2))",
            )?;
            let mut all_project_threads = HashSet::new();
            for row in threads.query_map(rusqlite::params![crate::chat::COLLECTION, id, WORKSPACES], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
            })? {
                let (thread_id, archived) = row?;
                all_project_threads.insert(thread_id.clone());
                if archived {
                    purged_threads.push(thread_id);
                } else {
                    live_threads += 1;
                }
            }
            let mut src_stmt = tx.prepare("SELECT value FROM documents WHERE collection='src_tasks'")?;
            for row in src_stmt.query_map([], |row| row.get::<_, String>(0))? {
                let serialized = row?;
                if let Ok(task_doc) = serde_json::from_str::<Value>(&serialized) {
                    if task_doc["lifecycle"]["state"] == "destroyed" {
                        continue;
                    }
                    let task_pid = task_doc["project_id"]
                        .as_str()
                        .filter(|v| !v.is_empty())
                        .or_else(|| task_doc["binding"]["project_id"].as_str())
                        .unwrap_or("");
                    let bound_tid = task_doc["binding"]["thread_id"].as_str().unwrap_or("");
                    if task_pid == id || (!bound_tid.is_empty() && all_project_threads.contains(bound_tid)) {
                        live_src_tasks += 1;
                    }
                }
            }
            if live_threads > 0 || live_src_tasks > 0 {
                return Ok(None);
            }
            let mut deletes = vec![(PROJECTS.to_owned(), id.clone())];
            deletes.extend(removed_workspaces.iter().map(|w| (WORKSPACES.to_owned(), w.clone())));
            deletes.extend(purged_threads.iter().map(|t| (crate::chat::COLLECTION.to_owned(), t.clone())));
            Ok(Some(deletes))
        })
        .map_err(db)?;
    if live_threads > 0 || live_src_tasks > 0 {
        let message = if live_src_tasks > 0 && live_threads == 0 {
            "Destroy the project's archived SRC tasks before deleting it"
        } else {
            "Archive the project's threads before deleting it"
        };
        let mut err = error(StatusCode::CONFLICT, "project.in_use", message);
        err.1.0["error"]["detail"] = json!({"project_id":id,"active_threads":live_threads,"src_tasks":live_src_tasks});
        return Err(err);
    }
    if !committed {
        return Err(error(StatusCode::CONFLICT,"command.version_conflict","Project changed during deletion; read current state and retry"));
    }
    let posts = crate::chat::release_deleted(&s, &mut active, &purged_threads);
    drop(active);
    for post in posts {
        tokio::spawn(post);
    }
    Ok(Json(json!({"deleted":true,"project_id":id,"deleted_threads":purged_threads.len(),"removed_workspaces":removed_workspaces.len()})))
}
#[derive(Default, Deserialize)]
pub(crate) struct WorkspaceQuery {
    #[serde(default)]
    project_id: String,
}
pub(crate) async fn list_workspaces(State(s): State<Arc<AppState>>, Query(q): Query<WorkspaceQuery>) -> Api {
    let rows: Vec<_> = s
        .store
        .list(WORKSPACES)
        .map_err(db)?
        .into_iter()
        .filter(|v| q.project_id.is_empty() || v["project_id"] == q.project_id)
        .collect();
    Ok(Json(json!({"count":rows.len(),"workspaces":rows})))
}
pub(crate) async fn get_workspace(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Api {
    Ok(Json(load(&s, WORKSPACES, &id)?))
}
async fn git_root(path: &std::path::Path) -> Result<std::path::PathBuf, Error> {
    let out = crate::git_process::run(
        path,
        &["rev-parse", "--show-toplevel"],
        30,
        "conversation.git.not_repo",
        true,
    )
    .await?;
    crate::workspace_read_api::canonical_workspace_root(out.stdout.trim())
}
async fn common_dir(root: &std::path::Path) -> Result<std::path::PathBuf, Error> {
    let out = crate::git_process::run(
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        30,
        "conversation.git.not_repo",
        true,
    )
    .await?;
    out.stdout
        .trim()
        .parse::<std::path::PathBuf>()
        .unwrap()
        .canonicalize()
        .map_err(|_| invalid("Cannot resolve Git common directory"))
}
async fn worktree_rows(s: &AppState, root: &std::path::Path) -> Result<Vec<Value>, Error> {
    let out = crate::git_process::run(
        root,
        &["worktree", "list", "--porcelain", "-z"],
        30,
        "conversation.git.worktree_list_failed",
        true,
    )
    .await?;
    let mut rows = Vec::new();
    let mut row = Value::Null;
    for item in out.stdout.split('\0') {
        if let Some(path) = item.strip_prefix("worktree ") {
            if !row.is_null() {
                rows.push(row);
            }
            row = json!({"path":path,"branch":null,"detached":false,"bare":false,"locked":false,"prunable":false});
        } else if let Some(head) = item.strip_prefix("HEAD ") {
            row["head"] = json!(head);
        } else if let Some(branch) = item.strip_prefix("branch ") {
            row["branch"] = json!(branch.strip_prefix("refs/heads/").unwrap_or(branch));
        } else if item == "detached" {
            row["detached"] = json!(true);
        } else if item == "bare" {
            row["bare"] = json!(true);
        } else if item.starts_with("locked") {
            row["locked"] = json!(true);
        } else if item.starts_with("prunable") {
            row["prunable"] = json!(true);
        }
    }
    if !row.is_null() {
        rows.push(row);
    }
    for row in &mut rows {
        let path = std::path::PathBuf::from(row["path"].as_str().unwrap_or(""));
        let path = path.canonicalize().unwrap_or(path);
        row["path"] = json!(path);
        crate::workspace_read_api::status_occupancy(s, &path, "", row)?;
    }
    Ok(rows)
}
pub(crate) async fn list_project_worktrees(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Api {
    let root = crate::workspace_read_api::project_bound_root(&s, &id)?;
    let root = git_root(&root).await?;
    let rows = worktree_rows(&s, &root).await?;
    Ok(Json(
        json!({"project_id":id,"root_path":root,"count":rows.len(),"worktrees":rows}),
    ))
}
async fn project_parent(
    s: &AppState,
    project: &str,
    supplied: &str,
) -> Result<std::path::PathBuf, Error> {
    if project.is_empty() {
        return Err(invalid("Worktree binding requires project_id"));
    }
    let registered = crate::workspace_read_api::project_bound_root(s, project)?;
    let parent = git_root(&registered).await?;
    if !supplied.trim().is_empty() {
        let supplied = canonical_directory(supplied)?;
        let supplied = git_root(std::path::Path::new(&supplied)).await?;
        if supplied != parent {
            return Err(invalid(
                "parent_root must be the project's registered primary checkout",
            ));
        }
    }
    Ok(parent)
}
/// Clean only a checkout this request created, with repository and branch proof.
async fn cleanup_created(parent: &std::path::Path, root: &str, branch: &str) {
    let path = std::path::Path::new(root);
    if !path.join(".git").is_file() {
        return;
    }
    let Ok(found_common) = common_dir(path).await else {
        return;
    };
    let Ok(parent_common) = common_dir(parent).await else {
        return;
    };
    if found_common != parent_common {
        return;
    }
    let Ok(head) = crate::git_process::run(
        path,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        30,
        "conversation.git.error",
        false,
    )
    .await
    else {
        return;
    };
    if !head.status.success() || head.stdout.trim() != branch {
        return;
    }
    let _ = crate::git_process::run(
        parent,
        &["worktree", "remove", "--force", root],
        30,
        "conversation.git.worktree_cleanup_failed",
        false,
    )
    .await;
}
async fn cleanup_failed_save(ws: &Value) {
    if ws["kind"] == "isolated" {
        if let Some(root) = ws["root_path"].as_str() {
            let _ = std::fs::remove_dir(root);
        }
        return;
    }
    if ws["settings"]["mode"] != "new_worktree" {
        return;
    }
    let Some(parent) = ws["settings"]["worktree_of"].as_str() else {
        return;
    };
    let Some(root) = ws["root_path"].as_str() else {
        return;
    };
    let Some(branch) = ws["settings"]["branch"].as_str() else {
        return;
    };
    cleanup_created(std::path::Path::new(parent), root, branch).await;
}
async fn plan_workspace(
    s: &AppState,
    body: &Value,
    id: &str,
    previous: Option<&Value>,
) -> Result<Value, Error> {
    let project = text(body, "project_id", "")?.trim();
    if !project.is_empty() {
        let project_doc = load(s, PROJECTS, project)?;
        if project_doc["scope"].as_str() == Some("src") {
            return Err(invalid("SRC research projects cannot bind a local workspace directory"));
        }
    }
    let kind = text(body, "kind", "local")?;
    let mut meta = settings(body)?;
    let mode = text(body, "mode", meta["mode"].as_str().unwrap_or(""))?
        .trim()
        .to_owned();
    if !matches!(kind, "local" | "git" | "isolated")
        || !matches!(
            mode.as_str(),
            "" | "shared_checkout" | "new_worktree" | "existing_worktree"
        )
    {
        return Err(error(
            StatusCode::NOT_IMPLEMENTED,
            "workspace.operation_unsupported",
            "Unsupported workspace kind or mode",
        ));
    }
    let created = previous
        .and_then(|ws| ws.get("created_at"))
        .cloned()
        .unwrap_or_else(|| json!(now()));
    let mut actual_kind = kind.to_owned();
    let root;
    if matches!(mode.as_str(), "new_worktree" | "existing_worktree") {
        let supplied_parent = text(body, "parent_root", "")?;
        let parent = if mode == "existing_worktree" && project.is_empty() {
            let raw = if supplied_parent.trim().is_empty() {
                text(body, "root_path", "")?
            } else {
                supplied_parent
            };
            let directory = canonical_directory(raw)?;
            git_root(std::path::Path::new(&directory)).await?
        } else {
            project_parent(s, project, supplied_parent).await?
        };
        let common = common_dir(&parent).await?;
        actual_kind = "git".into();
        if mode == "new_worktree" {
            if let Some(prior) = previous.filter(|p| {
                p["settings"]["mode"] == "new_worktree" && p["root_path"] == body["root_path"]
            }) {
                root = canonical_directory(prior["root_path"].as_str().unwrap_or(""))?;
                if common_dir(std::path::Path::new(&root)).await? != common {
                    return Err(invalid(
                        "Workspace does not belong to the project repository",
                    ));
                }
                for key in [
                    "branch",
                    "base_ref",
                    "worktree_of",
                    "git_common_dir",
                    "created_via",
                    "pending_create",
                ] {
                    meta[key] = prior["settings"][key].clone();
                }
            } else {
                if id.is_empty()
                    || !id
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
                {
                    return Err(invalid("Invalid managed workspace_id"));
                }
                let branch = text(body, "branch", "")?.trim();
                if branch.is_empty() || branch.starts_with('-') {
                    return Err(invalid("new_worktree requires a valid branch"));
                }
                crate::git_process::run(
                    &parent,
                    &["check-ref-format", "--branch", branch],
                    30,
                    "conversation.git.invalid_branch",
                    true,
                )
                .await?;
                let base = text(body, "base_ref", "HEAD")?.trim();
                let base = if base.is_empty() { "HEAD" } else { base };
                if base.starts_with('-')
                    || base.contains("..")
                    || base
                        .chars()
                        .any(|c| c.is_whitespace() || c == '\\' || c == '\0')
                {
                    return Err(invalid("Invalid base_ref"));
                }
                let location = std::path::PathBuf::from(
                    s.store
                        .get("system", "state_dir")
                        .map_err(db)?
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .ok_or_else(|| invalid("Managed workspace directory is unavailable"))?,
                )
                .join("workspaces")
                .join("worktrees");
                std::fs::create_dir_all(&location)
                    .map_err(|_| invalid("Cannot create managed worktree directory"))?;
                let location = location
                    .canonicalize()
                    .map_err(|_| invalid("Cannot resolve managed worktree directory"))?;
                let target = location.join(id);
                if target.symlink_metadata().is_ok() {
                    let registered = worktree_rows(s, &parent).await?.into_iter().any(|row| {
                        row["path"] == target.to_string_lossy().as_ref()
                            && row["branch"] == branch
                    });
                    if !registered || git_root(&target).await? != target
                        || common_dir(&target).await? != common {
                        return Err(error(
                            StatusCode::CONFLICT,
                            "conversation.git.worktree_path_exists",
                            "Managed worktree path already exists",
                        ));
                    }
                    root = target.to_string_lossy().into_owned();
                } else {
                    root = target.to_string_lossy().into_owned();
                    if let Err(failure) = crate::git_process::run(
                        &parent,
                        &["worktree", "add", "-b", branch, &root, base],
                        120,
                        "conversation.git.worktree_add_failed",
                        true,
                    )
                    .await
                    {
                        cleanup_created(&parent, &root, branch).await;
                        return Err(failure);
                    }
                }
                meta["branch"] = json!(branch);
                meta["base_ref"] = json!(base);
                meta["pending_create"] = json!(false);
            }
        } else {
            let candidate = canonical_directory(text(body, "root_path", "")?)?;
            let candidate = git_root(std::path::Path::new(&candidate)).await?;
            if common_dir(&candidate).await? != common {
                return Err(invalid(
                    "Selected worktree does not belong to the project repository",
                ));
            }
            let rows = worktree_rows(s, &parent).await?;
            if !rows
                .iter()
                .any(|r| r["path"].as_str() == candidate.to_str())
            {
                return Err(invalid(
                    "Selected directory is not a registered Git worktree",
                ));
            }
            root = candidate.to_string_lossy().into_owned();
            let branch = crate::git_process::run(
                &candidate,
                &["symbolic-ref", "--quiet", "--short", "HEAD"],
                30,
                "conversation.git.error",
                false,
            )
            .await?;
            meta["branch"] = json!(branch.stdout.trim());
        }
        meta["worktree_of"] = if project.is_empty() && supplied_parent.trim().is_empty() {
            json!(common.parent().unwrap_or(&parent))
        } else {
            json!(parent)
        };
        meta["git_common_dir"] = json!(common);
        meta["created_via"] = json!(mode);
    } else if kind == "isolated" {
        if id.is_empty()
            || !id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
        {
            return Err(invalid("Invalid managed workspace_id"));
        }
        let location = std::path::PathBuf::from(
            s.store
                .get("system", "state_dir")
                .map_err(db)?
                .and_then(|v| v.as_str().map(str::to_owned))
                .ok_or_else(|| invalid("Managed workspace directory is unavailable"))?,
        )
        .join("workspaces");
        std::fs::create_dir_all(&location)
            .map_err(|_| invalid("Cannot create managed workspace directory"))?;
        let location = location
            .canonicalize()
            .map_err(|_| invalid("Cannot resolve managed workspace directory"))?;
        let target = location.join(id);
        if let Some(prior) = previous.filter(|p| p["kind"] == "isolated") {
            let prior_root = canonical_directory(prior["root_path"].as_str().unwrap_or(""))?;
            if std::path::Path::new(&prior_root) != target {
                return Err(invalid(
                    "Stored isolated workspace path does not match its managed directory",
                ));
            }
            root = prior_root;
        } else {
            std::fs::create_dir(&target).map_err(|err| {
                if err.kind() == std::io::ErrorKind::AlreadyExists {
                    error(
                        StatusCode::CONFLICT,
                        "workspace.path_exists",
                        "Managed workspace path already exists",
                    )
                } else {
                    invalid("Cannot create isolated workspace directory")
                }
            })?;
            root = target.to_string_lossy().into_owned();
        }
        meta.as_object_mut().unwrap().remove("pending_create");
    } else {
        root = canonical_directory(text(body, "root_path", "")?)?;
        if kind == "git" {
            if !std::path::Path::new(&root).join(".git").exists() {
                return Err(invalid("Workspace root is not a Git checkout"));
            }
            git_root(std::path::Path::new(&root)).await?;
        }
        meta.as_object_mut().unwrap().remove("pending_create");
        if mode == "shared_checkout" {
            meta["created_via"] = json!(mode);
        }
    }
    meta["mode"] = json!(mode);
    Ok(
        json!({"workspace_id":id,"project_id":if project.is_empty(){Value::Null}else{json!(project)},"kind":actual_kind,"root_path":root,"settings":meta,"created_at":created}),
    )
}
fn commit_workspace(
    s: &AppState,
    scope: &str,
    body: &Value,
    response: &Value,
    id: &str,
    workspace: Option<&Value>,
) -> Result<(), Error> {
    let mut commands = Vec::new();
    for field in ["command_id", "idempotency_key"] {
        let key = text(body, field, "")?.trim();
        if !key.is_empty() {
            commands.push((
                format!("{field}:{key}"),
                json!({"scope":scope,"request":body,"response":response}),
            ));
        }
    }
    s.store
        .workspace_commit(id, workspace, &commands)
        .map_err(db)
}
pub(crate) async fn bind_workspace(State(s): State<Arc<AppState>>, Json(body): Json<Value>) -> Api {
    object(&body)?;
    let _guard = WRITES.lock().await;
    let _git_guard = crate::git_process::WRITES.lock().await;
    if let Some(old) = replay(&s, "workspace.bind", &body)? {
        return Ok(Json(old));
    }
    let id = text(&body, "workspace_id", "")?.trim();
    let id = if id.is_empty() {
        let key = text(&body, "idempotency_key", "")?.trim();
        let key = if key.is_empty() {
            text(&body, "command_id", "")?.trim()
        } else {
            key
        };
        if key.is_empty() {
            new_id("ws")
        } else {
            use sha2::{Digest, Sha256};
            format!(
                "ws_{:x}",
                Sha256::digest(format!("workspace.bind:{key}").as_bytes())
            )
        }
    } else {
        id.to_owned()
    };
    if s.store.get(WORKSPACES, &id).map_err(db)?.is_some() {
        return Err(error(
            StatusCode::CONFLICT,
            "workspace.exists",
            "Workspace already exists",
        ));
    }
    let ws = plan_workspace(&s, &body, &id, None).await?;
    let mut response = receipt(
        &body,
        "workspace",
        &id,
        json!({"workspace_id":id,"root_path":ws["root_path"],"kind":ws["kind"],"settings":ws["settings"]}),
    );
    if let Err(failure) = commit_metadata(&s,"workspace.bind",&body,&mut response,
        &[(WORKSPACES,id.as_str(),None)],&[(WORKSPACES,id.as_str(),&ws)],
        &[("workspace",id.as_str(),"core.workspace.bound",json!({"workspace_id":id,"project_id":ws["project_id"],"kind":ws["kind"],"root_path":ws["root_path"]}))]) {
        cleanup_failed_save(&ws).await;
        return Err(failure);
    }
    Ok(Json(response))
}
pub(crate) async fn update_workspace(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Api {
    object(&body)?;
    let _guard = WRITES.lock().await;
    let _git_guard = crate::git_process::WRITES.lock().await;
    let scope = format!("workspace.update:{id}");
    if let Some(old) = replay(&s, &scope, &body)? {
        return Ok(Json(old));
    }
    let previous = load(&s, WORKSPACES, &id)?;
    if s.store
        .list("conversation_threads")
        .map_err(db)?
        .iter()
        .any(|v| v["thread"]["workspace_id"] == id)
    {
        return Err(error(
            StatusCode::CONFLICT,
            "workspace.in_use",
            "Cannot change a workspace while threads are bound to it",
        ));
    }
    let mut ws = previous.clone();
    for key in [
        "project_id",
        "kind",
        "root_path",
        "mode",
        "branch",
        "base_ref",
        "parent_root",
    ] {
        if let Some(v) = body.get(key) {
            ws[key] = v.clone();
        }
    }
    if body.get("settings").is_some() {
        let additions = settings(&body)?;
        ws["settings"]
            .as_object_mut()
            .ok_or_else(|| invalid("Stored workspace settings are invalid"))?
            .extend(additions.as_object().unwrap().clone());
    }
    if ws["project_id"].is_null() {
        ws["project_id"] = json!("");
    }
    let updated = plan_workspace(&s, &ws, &id, Some(&previous)).await?;
    if let Err(failure) = commit_workspace(&s, &scope, &body, &updated, &id, Some(&updated)) {
        if updated["root_path"] != previous["root_path"] {
            cleanup_failed_save(&updated).await;
        }
        return Err(failure);
    }
    Ok(Json(updated))
}
#[derive(Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum RemovalPhase { RemoveIntent, FilesystemRemoved, MetadataCommitted }
fn removal_phase(journal: &Value) -> Result<RemovalPhase, Error> {
    serde_json::from_value(journal["phase"].clone()).map_err(|_| error(StatusCode::CONFLICT,
        "workspace.removal_journal_invalid","Worktree removal journal has an unknown phase"))
}
fn removal_path_identity(path: &std::path::Path) -> Result<Value, Error> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_|invalid("Removal path identity is unavailable"))?;
    #[cfg(unix)] {
        use std::os::unix::fs::MetadataExt;
        Ok(json!({"device":metadata.dev(),"inode":metadata.ino(),"created_at_ns":metadata.created().ok()
            .and_then(|created|created.duration_since(std::time::UNIX_EPOCH).ok()).map(|duration|duration.as_nanos().to_string())}))
    }
    #[cfg(not(unix))] { let _ = metadata; Err(invalid("This platform cannot fence worktree removal identities")) }
}
fn changed_removal_owner() -> Error {
    error(StatusCode::CONFLICT,"workspace.removal_target_changed","The worktree or repository changed; use a new command identity to inspect and delete the current worktree")
}
async fn removal_absent(journal: &Value) -> Result<bool, Error> {
    let root = journal["root_path"].as_str().ok_or_else(|| invalid("Removal root is missing"))?;
    match std::fs::symlink_metadata(root) {
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
        Err(_) => return Err(invalid("Cannot inspect the removal path")),
    }
    let common = journal["common_dir"].as_str().ok_or_else(|| invalid("Removal repository is missing"))?;
    let common = std::path::Path::new(common);
    if common.canonicalize().map_err(|_| invalid("Removal repository is unavailable"))? != common {
        return Err(invalid("Removal repository identity changed"));
    }
    if let Some(expected) = journal.get("common_identity") {
        if *expected != removal_path_identity(common)? { return Err(changed_removal_owner()); }
    }
    let list = crate::git_process::run(common,&["--git-dir",".","worktree","list","--porcelain","-z"],
        30,"conversation.git.worktree_list_failed",true).await?;
    Ok(!list.stdout.split('\0').any(|entry| entry.strip_prefix("worktree ") == Some(root)))
}
fn finish_removal(s: &AppState, mut journal: Value) -> Result<Value, Error> {
    let id = journal["workspace_id"].as_str().ok_or_else(|| invalid("Removal workspace is missing"))?.to_owned();
    s.store.document_scope(|| -> Result<Value, Error> {
        let current = s.store.get(WORKSPACES,&id).map_err(db)?;
        if current.as_ref().is_some_and(|current| *current != journal["workspace"]) {
            return Err(error(StatusCode::CONFLICT,"workspace.removal_target_changed","Workspace metadata changed after its removal intent"));
        }
        commit_workspace(s,"workspace.delete_worktree",&journal["request"],&journal["response"],&id,None)?;
        crate::operations_api::advance_direct_version(s,"workspace",&id)
            .map_err(|message|error(StatusCode::INTERNAL_SERVER_ERROR,"storage.failed",&message))?;
        journal["phase"] = json!("metadata_committed");
        journal["committed_at"] = json!(now());
        s.store.put(REMOVALS,&id,&journal).map_err(db)?;
        Ok(journal["response"].clone())
    }).map_err(db)?
}
pub(crate) async fn recover_removals(s: &Arc<AppState>) -> Result<(), String> {
    let _guard = WRITES.lock().await;
    let _git_guard = crate::git_process::WRITES.lock().await;
    for journal in s.store.list(REMOVALS).map_err(|error|error.to_string())? {
        let recovered = async {
            let phase = removal_phase(&journal).map_err(|(_,body)|body.0.to_string())?;
            if phase == RemovalPhase::MetadataCommitted { return Ok::<_,String>(()); }
            if removal_absent(&journal).await.map_err(|(_,body)|body.0.to_string())? {
                finish_removal(s,journal.clone()).map_err(|(_,body)|body.0.to_string())?;
            } else if phase == RemovalPhase::FilesystemRemoved {
                return Err("Removed worktree was recreated before metadata recovery; inspect the removal journal".into());
            }
            Ok(())
        }.await;
        if let Err(error) = recovered {
            tracing::warn!(workspace_id=%journal["workspace_id"],%error,"Worktree removal recovery remains pending");
        }
    }
    Ok(())
}

pub(crate) async fn delete_worktree(State(s): State<Arc<AppState>>, Json(body): Json<Value>) -> Api {
    object(&body)?;
    let _guard = WRITES.lock().await;
    let _git_guard = crate::git_process::WRITES.lock().await;
    if let Some(old) = replay(&s, "workspace.delete_worktree", &body)? {
        return Ok(Json(old));
    }
    let id = text(&body, "workspace_id", "")?.trim();
    if id.is_empty() {
        return Err(invalid("workspace_id is required"));
    }
    let force = match body.get("force") {
        None => false,
        Some(Value::Bool(v)) => *v,
        _ => return Err(invalid("force must be a boolean")),
    };
    for field in ["command_id","idempotency_key"] {
        let value = text(&body,field,"")?.trim();
        if !value.is_empty() && s.store.get(REMOVAL_HISTORY,&json!([id,field,value]).to_string()).map_err(db)?.is_some() {
            return Err(error(StatusCode::CONFLICT,"command.superseded","This removal was superseded; use a new command identity to delete the current worktree"));
        }
    }
    let previous_journal = s.store.get(REMOVALS, id).map_err(db)?;
    let mut pending = previous_journal.clone();
    if let Some(journal) = previous_journal.as_ref() {
        let mut request = body.clone();
        let mut previous = journal["request"].clone();
        if body["idempotency_key"].as_str().is_some_and(|key| !key.is_empty())
            && body["idempotency_key"] == previous["idempotency_key"] {
            request.as_object_mut().unwrap().remove("command_id");
            previous.as_object_mut().unwrap().remove("command_id");
        }
        if request != previous {
            let command = text(&body,"command_id","")?.trim();
            let prior_command = journal["request"]["command_id"].as_str().filter(|id|!id.is_empty())
                .or_else(||journal["response"]["receipt"]["command_id"].as_str()).unwrap_or("");
            let idem = text(&body,"idempotency_key","")?.trim();
            let prior_idem = journal["request"]["idempotency_key"].as_str().unwrap_or("");
            let fresh = (!command.is_empty() && command != prior_command && (idem.is_empty() || idem != prior_idem))
                || (command.is_empty() && !idem.is_empty() && idem != prior_idem);
            if !fresh {
                return Err(error(StatusCode::CONFLICT,"command.conflict","A different worktree removal is pending; use a new command identity to inspect and delete the current worktree"));
            }
            pending = None;
        } else {
            if removal_phase(journal)? == RemovalPhase::MetadataCommitted {
                return Ok(Json(journal["response"].clone()));
            }
            if removal_absent(journal).await? {
                return finish_removal(&s, journal.clone()).map(Json);
            }
            if removal_phase(journal)? == RemovalPhase::FilesystemRemoved {
                return Err(changed_removal_owner());
            }
        }
    }
    let ws = load(&s, WORKSPACES, id)?;
    if pending.as_ref().is_some_and(|journal| journal["workspace"] != ws) {
        return Err(error(StatusCode::CONFLICT,"workspace.removal_target_changed","Workspace metadata changed after its removal intent"));
    }
    if !matches!(
        ws["settings"]["mode"].as_str(),
        Some("new_worktree" | "existing_worktree")
    ) && ws["kind"] != "git"
    {
        return Err(invalid("Only a registered Git worktree can be deleted"));
    }
    let root = crate::workspace_read_api::canonical_workspace_root(
        ws["root_path"].as_str().unwrap_or(""),
    )?;
    let root = git_root(&root).await?;
    if !root.join(".git").is_file() {
        return Err(invalid("Cannot remove a primary checkout"));
    }
    let project = ws["project_id"].as_str().unwrap_or("");
    let parent = if project.is_empty() {
        root.clone()
    } else {
        project_parent(&s, project, "").await?
    };
    if common_dir(&root).await? != common_dir(&parent).await? {
        return Err(invalid(
            "Workspace does not belong to the project repository",
        ));
    }
    let mut status = crate::workspace_read_api::inspect_git_status(&root).await?;
    crate::workspace_read_api::status_occupancy(&s, &root, "", &mut status)?;
    if status["occupied"] == true {
        return Err(error(
            StatusCode::CONFLICT,
            "conversation.git.worktree_occupied",
            "Worktree is occupied by active threads",
        ));
    }
    let dirty = status["dirty"] == true;
    if dirty && !force {
        return Err(error(
            StatusCode::CONFLICT,
            "conversation.git.worktree_dirty",
            "Worktree has uncommitted changes; explicit force is required",
        ));
    }
    let path = root.to_string_lossy().into_owned();
    let common = common_dir(&root).await?;
    let root_identity = removal_path_identity(&root)?;
    let common_identity = removal_path_identity(&common)?;
    if pending.as_ref().is_some_and(|journal|!journal["root_identity"].is_object() || !journal["common_identity"].is_object()) {
        return Err(error(StatusCode::CONFLICT,"workspace.removal_target_changed","This legacy removal has no owner identity; use a new command identity to inspect and delete the current worktree"));
    }
    if pending.as_ref().is_some_and(|journal|journal["root_identity"] != root_identity
        || journal["common_identity"] != common_identity) {
        return Err(changed_removal_owner());
    }
    let output = json!({"workspace_id":id,"removed":true,"root_path":root,"dirty":dirty});
    let mut journal = pending.unwrap_or_else(|| json!({"workspace_id":id,"phase":"remove_intent","request":body,
        "workspace":ws.clone(),"root_path":path,"root_identity":root_identity,"common_dir":common,"common_identity":common_identity,
        "response":receipt(&body,"workspace",id,output),"created_at":now()}));
    let mut archives = Vec::new();
    if let Some(previous) = previous_journal.as_ref().filter(|previous|previous["request"] != journal["request"]) {
        let mut archived = previous.clone();
        archived["superseded_by"] = journal["response"]["receipt"]["command_id"].clone();
        archived["superseded_at"] = json!(now());
        for field in ["command_id","idempotency_key"] {
            if let Some(value) = previous["request"][field].as_str().filter(|value|!value.is_empty()) {
                archives.push((json!([id,field,value]).to_string(),archived.clone()));
            }
        }
        if archives.is_empty() {
            archives.push((json!([id,"receipt",previous["response"]["receipt"]["command_id"]]).to_string(),archived));
        }
    }
    let mut records = vec![(REMOVALS,id,&journal)];
    records.extend(archives.iter().map(|(key,row)|(REMOVAL_HISTORY,key.as_str(),row)));
    if !s.store.put_many_if(&[(REMOVALS,id,previous_journal.as_ref()),(WORKSPACES,id,Some(&ws))],&records).map_err(db)? {
        return Err(error(StatusCode::CONFLICT,"workspace.removal_target_changed","Workspace removal changed before its intent was committed"));
    }
    let mut args = vec!["--git-dir", ".", "worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&path);
    let removed = crate::git_process::run(
        &common,
        &args,
        120,
        "conversation.git.worktree_remove_failed",
        true,
    )
    .await;
    let absent = removal_absent(&journal).await?;
    if !absent {
        removed?;
        return Err(error(StatusCode::CONFLICT,"workspace.removal_unconfirmed","Git returned success but worktree removal could not be confirmed"));
    }
    journal["phase"] = json!("filesystem_removed");
    s.store.put(REMOVALS,id,&journal).map_err(db)?;
    finish_removal(&s, journal).map(Json)
}
pub(crate) async fn delete_workspace(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Api {
    let _guard = WRITES.lock().await;
    load(&s, WORKSPACES, &id)?;
    if s.store
        .list("conversation_threads")
        .map_err(db)?
        .iter()
        .any(|v| v["thread"]["workspace_id"] == id)
    {
        return Err(error(
            StatusCode::CONFLICT,
            "workspace.in_use",
            "Cannot remove a workspace binding while threads use it",
        ));
    }
    s.store.delete(WORKSPACES, &id).map_err(db)?;
    Ok(Json(json!({"deleted":true})))
}
fn sidebar_default() -> Value {
    json!({"version":0,"pinned_ids":[],"thread_order":[],"project_order":[],"sort_mode":"updated","pinned_sort_mode":"manual","group_mode":"project","settled_at":{},"snoozed_until":{}})
}
fn flatten_prefs(record: Value) -> Value {
    let mut values = record["values"].clone();
    values["version"] = record["version"].clone();
    values
}
#[derive(Clone, Default, Deserialize)]
pub(crate) struct SidebarQuery {
    #[serde(default)]
    pub scope: Option<String>,
}

fn sidebar_settings_key(scope: Option<&str>) -> Result<&'static str, Error> {
    match scope.map(str::trim).filter(|s| !s.is_empty()) {
        None | Some("chat") => Ok("sidebar"),
        Some("src") => Ok("sidebar:src"),
        Some(_) => Err(invalid("Invalid sidebar scope")),
    }
}

pub(crate) async fn get_sidebar(
    State(s): State<Arc<AppState>>,
    Query(q): Query<SidebarQuery>,
) -> Api {
    let key = sidebar_settings_key(q.scope.as_deref())?;
    Ok(Json(
        s.store
            .get("settings", key)
            .map_err(db)?
            .map(flatten_prefs)
            .unwrap_or_else(sidebar_default),
    ))
}
pub(crate) async fn put_sidebar(
    State(s): State<Arc<AppState>>,
    Query(q): Query<SidebarQuery>,
    Json(body): Json<Value>,
) -> Api {
    object(&body)?;
    let key = sidebar_settings_key(q.scope.as_deref())?;
    let _guard = WRITES.lock().await;
    let current = s
        .store
        .get("settings", key)
        .map_err(db)?
        .unwrap_or_else(|| json!({"version":0,"values":{}}));
    let expected = match body.get("version") {
        None => current["version"].as_u64().unwrap_or(0),
        Some(v) => v
            .as_u64()
            .ok_or_else(|| invalid("version must be a nonnegative integer"))?,
    };
    let mut values = sidebar_default();
    values.as_object_mut().unwrap().remove("version");
    for field in ["pinned_ids", "thread_order", "project_order"] {
        if let Some(raw) = body.get(field) {
            let ids = raw
                .as_array()
                .ok_or_else(|| invalid("Sidebar orders must be arrays"))?;
            let mut seen = HashSet::new();
            let mut clean = Vec::new();
            for id in ids {
                let id = id
                    .as_str()
                    .filter(|v| !v.trim().is_empty())
                    .ok_or_else(|| invalid("Sidebar orders must contain nonempty strings"))?;
                if seen.insert(id) {
                    clean.push(id);
                }
            }
            values[field] = json!(clean);
        }
    }
    for field in ["sort_mode", "pinned_sort_mode"] {
        if let Some(v) = body.get(field) {
            if !matches!(v.as_str(), Some("updated" | "priority" | "manual")) {
                return Err(invalid("Invalid sidebar sort mode"));
            }
            values[field] = v.clone();
        }
    }
    if let Some(v) = body.get("group_mode") {
        if !matches!(v.as_str(), Some("project" | "list")) {
            return Err(invalid("Invalid sidebar grouping"));
        }
        values["group_mode"] = v.clone();
    }
    for field in ["settled_at", "snoozed_until"] {
        if let Some(v) = body.get(field) {
            let map = v
                .as_object()
                .ok_or_else(|| invalid("Sidebar timestamps must be objects"))?;
            if map.iter().any(|(id, time)| {
                id.trim().is_empty() || time.as_f64().is_none_or(|n| !n.is_finite() || n <= 0.0)
            }) {
                return Err(invalid("Sidebar timestamps must be positive numbers"));
            }
            values[field] = v.clone();
        }
    }
    match s
        .store
        .compare_and_put("settings", key, expected, values)
        .map_err(db)?
    {
        Ok(saved) => Ok(Json(flatten_prefs(saved))),
        Err(current) => Err((StatusCode::CONFLICT, Json(flatten_prefs(current)))),
    }
}
pub(crate) async fn list_folders(State(s): State<Arc<AppState>>) -> Api {
    let mut rows = s.store.list("folders").map_err(db)?;
    rows.sort_by_key(|v| v["order"].as_i64().unwrap_or(0));
    Ok(Json(json!({"folders":rows})))
}
pub(crate) async fn create_folder(State(s): State<Arc<AppState>>, Json(body): Json<Value>) -> Api {
    object(&body)?;
    let name = text(&body, "name", "")?.trim();
    let name = if name.is_empty() { "Folder" } else { name };
    let _guard = WRITES.lock().await;
    let folders = s.store.list("folders").map_err(db)?;
    let order = folders
        .iter()
        .filter_map(|v| v["order"].as_i64())
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| invalid("Folder order overflow"))?;
    let id = new_id("f");
    let folder = json!({"id":id,"name":name,"order":order});
    s.store.put("folders", &id, &folder).map_err(db)?;
    Ok(Json(json!({"folder":folder})))
}
pub(crate) async fn update_folder(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Api {
    object(&body)?;
    let _guard = WRITES.lock().await;
    let mut folder = load(&s, "folders", &id)?;
    if body.get("name").is_some() {
        let name = text(&body, "name", "")?.trim();
        if !name.is_empty() {
            folder["name"] = json!(name);
        }
    }
    if let Some(order) = body.get("order") {
        folder["order"] = json!(
            order
                .as_i64()
                .ok_or_else(|| invalid("order must be an integer"))?
        );
    }
    s.store.put("folders", &id, &folder).map_err(db)?;
    Ok(Json(json!({"ok":true})))
}
pub(crate) async fn delete_folder(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> Api {
    let _guard = WRITES.lock().await;
    load(&s, "folders", &id)?;
    for mut run in s.store.list("runs").map_err(db)? {
        if run["folder_id"] == id {
            let rid = run["run_id"]
                .as_str()
                .ok_or_else(|| invalid("Stored run has no run_id"))?
                .to_owned();
            run["folder_id"] = Value::Null;
            s.store.put("runs", &rid, &run).map_err(db)?;
        }
    }
    s.store.delete("folders", &id).map_err(db)?;
    Ok(Json(json!({"ok":true})))
}
async fn update_run(
    State(s): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Api {
    object(&body)?;
    let _guard = WRITES.lock().await;
    let mut run = load(&s, "runs", &id)?;
    for field in ["pinned", "archived"] {
        if let Some(v) = body.get(field) {
            if !v.is_boolean() {
                return Err(invalid("pinned and archived must be booleans"));
            }
        }
    }
    if let Some(v) = body.get("name") {
        let name = v
            .as_str()
            .ok_or_else(|| invalid("name must be a string"))?
            .trim();
        if run.get("original_name").is_none() {
            run["original_name"] = run["name"].clone();
        }
        run["custom_name"] = if name.is_empty() {
            Value::Null
        } else {
            json!(name)
        };
        run["name"] = if name.is_empty() {
            run["original_name"].clone()
        } else {
            json!(name)
        };
    }
    if let Some(v) = body.get("folder_id") {
        let folder = if v.is_null() {
            ""
        } else {
            v.as_str()
                .ok_or_else(|| invalid("folder_id must be a string or null"))?
                .trim()
        };
        if !folder.is_empty() {
            load(&s, "folders", folder)?;
        }
        run["folder_id"] = if folder.is_empty() {
            Value::Null
        } else {
            json!(folder)
        };
    }
    if let Some(v) = body.get("order") {
        run["order"] = if v.is_null() {
            Value::Null
        } else {
            json!(
                v.as_i64()
                    .ok_or_else(|| invalid("order must be an integer or null"))?
            )
        };
    }
    let timestamp = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
    if let Some(v) = body.get("operator_label") {
        let normalized = if v.is_null() {
            None
        } else {
            let raw = v
                .as_str()
                .ok_or_else(|| invalid("operator_label must be a string or null"))?
                .trim();
            match raw {
                "" | "none" | "clear" => None,
                "pending_followup" | "待跟进" => Some("pending_followup"),
                "processed" | "已处理" => Some("processed"),
                "ignored" | "已忽略" => Some("ignored"),
                _ => {
                    return Err(invalid(
                        "operator_label must be pending_followup, processed, ignored, or null",
                    ))
                }
            }
        };
        run["operator_label"] = match normalized {
            Some(label) => json!(label),
            None => Value::Null,
        };
        run["operator_label_updated_at"] = if normalized.is_some() {
            json!(timestamp)
        } else {
            Value::Null
        };
    }
    if let Some(v) = body.get("pinned") {
        run["pinned"] = v.clone();
        run["pinned_at"] = if v == true {
            json!(timestamp)
        } else {
            Value::Null
        };
    }
    if let Some(v) = body.get("archived") {
        run["archived"] = v.clone();
        run["archived_at"] = if v == true {
            json!(timestamp)
        } else {
            Value::Null
        };
    }
    if run["archived"] == true {
        run["pinned"] = json!(false);
        run["pinned_at"] = Value::Null;
    }
    run["updated"] = json!(timestamp);
    s.store.put("runs", &id, &run).map_err(db)?;
    Ok(Json(json!({"ok":true,"run":crate::runs::project_run_summary(&s, run)})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;
    fn state() -> Arc<AppState> {
        Arc::new(AppState::new(
            crate::store::Store::open(":memory:").unwrap(),
        ))
    }
    async fn request(
        s: Arc<AppState>,
        method: &str,
        path: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let response = router()
            .with_state(s)
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 2_000_000).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
    #[tokio::test]
    async fn project_directory_binding_retries_conflicts_and_settings_merge() {
        let s = state();
        let directory = std::env::temp_dir().canonicalize().unwrap();
        let body = json!({"name":" Product ","root_path":directory,"command_id":"create-product","settings":{"color":"blue"}});
        let (status, result) = request(s.clone(), "POST", "/api/projects", body.clone()).await;
        assert_eq!(status, StatusCode::OK);
        let id = result["receipt"]["aggregate"]["id"].as_str().unwrap();
        let wid = result["receipt"]["output"]["workspace_id"]
            .as_str()
            .unwrap();
        assert_eq!(
            request(s.clone(), "POST", "/api/projects", body.clone())
                .await
                .1,
            result
        );
        assert_eq!(
            request(
                s.clone(),
                "POST",
                "/api/projects",
                json!({"name":"Other","command_id":"create-product"})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        let (status, conflict) = request(
            s.clone(),
            "POST",
            "/api/projects",
            json!({"name":"Duplicate","root_path":directory,"command_id":"duplicate"}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(conflict["receipt"]["error"]["detail"]["project_id"], id);
        let (_, list) = request(s.clone(), "GET", "/api/projects", json!({})).await;
        assert_eq!(list["count"], 1);
        assert_eq!(list["projects"][0]["name"], "Product");
        assert_eq!(list["projects"][0]["workspace_id"], wid);
        assert_eq!(request(s.clone(),"PATCH",&format!("/api/projects/{id}"),json!({"settings":{"icon":"box"},"description":"Updated","command_id":"edit-product"})).await.0,StatusCode::OK);
        let (_, project) =
            request(s.clone(), "GET", &format!("/api/projects/{id}"), json!({})).await;
        assert_eq!(project["settings"], json!({"color":"blue","icon":"box"}));
        assert_eq!(project["description"], "Updated");
        assert_eq!(
            request(
                s.clone(),
                "DELETE",
                &format!("/api/projects/{id}"),
                json!({})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            request(
                s.clone(),
                "DELETE",
                &format!("/api/workspaces/{wid}"),
                json!({})
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            request(
                s.clone(),
                "DELETE",
                &format!("/api/projects/{id}"),
                json!({})
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            request(s, "GET", &format!("/api/projects/{id}"), json!({}))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }
    #[tokio::test]
    async fn workspace_validation_and_reference_guards() {
        let s = state();
        let root = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!(
            request(
                s.clone(),
                "POST",
                "/api/workspaces",
                json!({"project_id":"missing","root_path":root})
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            request(
                s.clone(),
                "POST",
                "/api/workspaces",
                json!({"mode":"new_worktree","root_path":root})
            )
            .await
            .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            request(
                s.clone(),
                "POST",
                "/api/workspaces/delete-worktree",
                json!({"workspace_id":"missing"})
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            request(
                s.clone(),
                "POST",
                "/api/workspaces",
                json!({"root_path":"/a/directory/that/does/not/exist"})
            )
            .await
            .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let (_,bound)=request(s.clone(),"POST","/api/workspaces",json!({"root_path":root,"workspace_id":"ws_test","mode":"shared_checkout","command_id":"bind"})).await;
        assert_eq!(
            bound["receipt"]["output"]["settings"]["mode"],
            "shared_checkout"
        );
        assert_eq!(
            request(
                s.clone(),
                "PATCH",
                "/api/workspaces/ws_test",
                json!({"settings":{"color":"red"}})
            )
            .await
            .0,
            StatusCode::OK
        );
        s.store
            .put(
                "conversation_threads",
                "one",
                &json!({"thread":{"thread_id":"one","workspace_id":"ws_test"}}),
            )
            .unwrap();
        assert_eq!(
            request(s.clone(), "DELETE", "/api/workspaces/ws_test", json!({}))
                .await
                .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            request(
                s.clone(),
                "PATCH",
                "/api/workspaces/ws_test",
                json!({"root_path":root})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        let (_, rows) = request(s, "GET", "/api/workspaces", json!({})).await;
        assert_eq!(rows["workspaces"][0]["settings"]["color"], "red");
        assert_eq!(rows["workspaces"][0]["settings"]["mode"], "shared_checkout");
    }
    fn fixture_git(root: &std::path::Path, args: &[&str]) {
        let output = crate::host_process::std_command("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fn fixture_repo(root: &std::path::Path) {
        std::fs::create_dir_all(root).unwrap();
        fixture_git(root, &["init", "-b", "main"]);
        fixture_git(root, &["config", "user.name", "Fixture"]);
        fixture_git(root, &["config", "user.email", "fixture@example.invalid"]);
        fixture_git(root, &["commit", "--allow-empty", "-m", "Initial"]);
    }
    #[tokio::test]
    async fn worktree_lifecycle_replays_checks_ownership_occupancy_and_removes_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fixture_repo(&root);
        let foreign = dir.path().join("foreign");
        fixture_repo(&foreign);
        let s = state();
        s.store
            .put("system", "state_dir", &json!(dir.path()))
            .unwrap();
        let (status, project) = request(
            s.clone(),
            "POST",
            "/api/projects",
            json!({"name":"Fixture","root_path":root,"command_id":"project"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let pid = project["receipt"]["aggregate"]["id"].as_str().unwrap();
        let body = json!({"project_id":pid,"mode":"new_worktree","branch":"feature","parent_root":root,"command_id":"new-tree","idempotency_key":"new-tree"});
        let (status, bound) = request(s.clone(), "POST", "/api/workspaces", body.clone()).await;
        assert_eq!(status, StatusCode::OK, "{bound}");
        assert_eq!(
            request(s.clone(), "POST", "/api/workspaces", body.clone())
                .await
                .1,
            bound
        );
        let wid = bound["receipt"]["output"]["workspace_id"].as_str().unwrap();
        let path =
            std::path::PathBuf::from(bound["receipt"]["output"]["root_path"].as_str().unwrap());
        assert_eq!(
            path.parent().unwrap(),
            dir.path()
                .join("workspaces/worktrees")
                .canonicalize()
                .unwrap()
        );
        assert_eq!(
            bound["receipt"]["output"]["settings"]["pending_create"],
            false
        );
        assert!(path.join(".git").is_file());
        assert_eq!(request(s.clone(), "POST", "/api/workspaces", json!({"project_id":pid,"mode":"existing_worktree","root_path":foreign,"parent_root":root})).await.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(request(s.clone(), "POST", "/api/workspaces", json!({"project_id":pid,"mode":"new_worktree","branch":"bad-parent","parent_root":foreign})).await.0, StatusCode::UNPROCESSABLE_ENTITY);
        let (status, existing) = request(s.clone(), "POST", "/api/workspaces", json!({"project_id":pid,"mode":"existing_worktree","root_path":path,"parent_root":root,"command_id":"existing"})).await;
        assert_eq!(status, StatusCode::OK, "{existing}");
        let existing_id = existing["receipt"]["output"]["workspace_id"]
            .as_str()
            .unwrap();
        s.store.put("conversation_threads", "occupant", &json!({"thread":{"thread_id":"occupant","workspace_id":existing_id,"state":{"status":"active"}}})).unwrap();
        let (_, listed) = request(
            s.clone(),
            "GET",
            &format!("/api/projects/{pid}/git/worktrees"),
            json!({}),
        )
        .await;
        assert_eq!(listed["count"], 2);
        assert_eq!(
            listed["root_path"].as_str(),
            root.canonicalize().unwrap().to_str()
        );
        assert!(
            listed["worktrees"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["path"].as_str() == path.to_str() && r["occupied"] == true)
        );
        let delete = json!({"workspace_id":wid,"command_id":"delete-tree","force":true});
        let (status, occupied) = request(
            s.clone(),
            "POST",
            "/api/workspaces/delete-worktree",
            delete.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            occupied["error"]["code"],
            "conversation.git.worktree_occupied"
        );
        s.store.put("conversation_threads", "occupant", &json!({"thread":{"thread_id":"occupant","workspace_id":existing_id,"state":{"status":"archived"}}})).unwrap();
        std::fs::write(path.join("keep.txt"), "uncommitted").unwrap();
        let (status, dirty) = request(
            s.clone(),
            "POST",
            "/api/workspaces/delete-worktree",
            json!({"workspace_id":wid,"command_id":"dirty-delete"}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(dirty["error"]["code"], "conversation.git.worktree_dirty");
        assert!(path.join("keep.txt").exists());
        let (status, removed) = request(
            s.clone(),
            "POST",
            "/api/workspaces/delete-worktree",
            delete.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{removed}");
        assert_eq!(removed["receipt"]["output"]["dirty"], true);
        assert!(!path.exists());
        assert!(root.join(".git").is_dir());
        assert!(foreign.join(".git").is_dir());
        assert!(s.store.get(WORKSPACES, wid).unwrap().is_none());
        assert_eq!(
            request(s.clone(), "POST", "/api/workspaces/delete-worktree", delete)
                .await
                .1,
            removed
        );
    }
    #[tokio::test]
    async fn managed_worktree_never_replaces_an_existing_directory_and_patch_keeps_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fixture_repo(&root);
        let s = state();
        s.store
            .put("system", "state_dir", &json!(dir.path()))
            .unwrap();
        let (_, project) = request(
            s.clone(),
            "POST",
            "/api/projects",
            json!({"name":"Fixture","root_path":root}),
        )
        .await;
        let pid = project["receipt"]["aggregate"]["id"].as_str().unwrap();
        let target = dir.path().join("workspaces/worktrees/ws_existing");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("keep.txt"), "user data").unwrap();
        let (status, _) = request(s.clone(), "POST", "/api/workspaces", json!({"project_id":pid,"mode":"new_worktree","workspace_id":"ws_existing","branch":"blocked"})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            std::fs::read_to_string(target.join("keep.txt")).unwrap(),
            "user data"
        );
        let (status, bound) = request(s.clone(), "POST", "/api/workspaces", json!({"project_id":pid,"mode":"new_worktree","workspace_id":"ws_patch","branch":"feature"})).await;
        assert_eq!(status, StatusCode::OK, "{bound}");
        let (status, updated) = request(
            s.clone(),
            "PATCH",
            "/api/workspaces/ws_patch",
            json!({"settings":{"color":"blue"},"command_id":"patch"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{updated}");
        assert_eq!(updated["settings"]["color"], "blue");
        assert_eq!(updated["settings"]["branch"], "feature");
        let isolated =
            json!({"kind":"isolated","workspace_id":"ws_isolated","command_id":"isolated"});
        let (status, created) =
            request(s.clone(), "POST", "/api/workspaces", isolated.clone()).await;
        assert_eq!(status, StatusCode::OK, "{created}");
        let isolated_root = dir
            .path()
            .join("workspaces/ws_isolated")
            .canonicalize()
            .unwrap();
        assert_eq!(
            created["receipt"]["output"]["root_path"].as_str(),
            isolated_root.to_str()
        );
        assert!(isolated_root.is_dir());
        assert!(!isolated_root.join(".git").exists());
        assert_eq!(
            request(s.clone(), "POST", "/api/workspaces", isolated)
                .await
                .1,
            created
        );
        assert_eq!(
            request(
                s.clone(),
                "PATCH",
                "/api/workspaces/ws_isolated",
                json!({"settings":{"color":"green"}})
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            request(
                s.clone(),
                "DELETE",
                "/api/workspaces/ws_isolated",
                json!({})
            )
            .await
            .0,
            StatusCode::OK
        );
        assert!(isolated_root.is_dir());
        std::fs::write(isolated_root.join("keep.txt"), "user data").unwrap();
        let (status, _) = request(
            s.clone(),
            "POST",
            "/api/workspaces",
            json!({"kind":"isolated","workspace_id":"ws_isolated","command_id":"replace-isolated"}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            std::fs::read_to_string(isolated_root.join("keep.txt")).unwrap(),
            "user data"
        );
        let standalone = dir.path().join("standalone");
        fixture_git(
            &root,
            &[
                "worktree",
                "add",
                "-b",
                "standalone",
                standalone.to_str().unwrap(),
            ],
        );
        let (status, standalone_bound) = request(s.clone(), "POST", "/api/workspaces", json!({"mode":"existing_worktree","root_path":standalone,"command_id":"standalone-bind"})).await;
        assert_eq!(status, StatusCode::OK, "{standalone_bound}");
        let standalone_id = standalone_bound["receipt"]["output"]["workspace_id"]
            .as_str()
            .unwrap();
        assert_eq!(request(s.clone(), "POST", "/api/workspaces", json!({"mode":"existing_worktree","root_path":standalone,"parent_root":root,"command_id":"standalone-parent"})).await.0, StatusCode::OK);
        let foreign = dir.path().join("foreign-standalone");
        fixture_repo(&foreign);
        assert_eq!(
            request(
                s.clone(),
                "POST",
                "/api/workspaces",
                json!({"mode":"existing_worktree","root_path":standalone,"parent_root":foreign})
            )
            .await
            .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let (status, plain_git) = request(
            s.clone(),
            "POST",
            "/api/workspaces",
            json!({"kind":"git","root_path":standalone,"command_id":"plain-git"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{plain_git}");
        let plain_id = plain_git["receipt"]["output"]["workspace_id"]
            .as_str()
            .unwrap();
        let (status, removed) = request(
            s.clone(),
            "POST",
            "/api/workspaces/delete-worktree",
            json!({"workspace_id":plain_id,"command_id":"remove-plain-git"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{removed}");
        assert!(!standalone.exists());
        assert!(s.store.get(WORKSPACES, plain_id).unwrap().is_none());
        assert!(s.store.get(WORKSPACES, standalone_id).unwrap().is_some());
        assert_eq!(
            request(s.clone(), "DELETE", "/api/workspaces/ws_patch", json!({}))
                .await
                .0,
            StatusCode::OK
        );
        assert!(
            std::path::Path::new(bound["receipt"]["output"]["root_path"].as_str().unwrap())
                .join(".git")
                .is_file()
        );
    }
    #[tokio::test]
    async fn sidebar_preferences_use_atomic_versions_and_validate_shapes() {
        let s = state();
        let (_, defaults) = request(s.clone(), "GET", "/api/sidebar-preferences", json!({})).await;
        assert_eq!(defaults["version"], 0);
        let (status,saved)=request(s.clone(),"PUT","/api/sidebar-preferences",json!({"version":0,"pinned_ids":["a","a","b"],"sort_mode":"manual","settled_at":{"a":1234}})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(saved["pinned_ids"], json!(["a", "b"]));
        assert_eq!(saved["version"], 1);
        let (status, current) = request(
            s.clone(),
            "PUT",
            "/api/sidebar-preferences",
            json!({"version":0,"pinned_ids":[]}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(current, saved);
        assert_eq!(
            request(
                s.clone(),
                "PUT",
                "/api/sidebar-preferences",
                json!({"version":1,"sort_mode":"wrong"})
            )
            .await
            .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            request(
                s.clone(),
                "PUT",
                "/api/sidebar-preferences",
                json!({"version":1,"snoozed_until":{"a":-1}})
            )
            .await
            .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            request(s, "GET", "/api/sidebar-preferences", json!({}))
                .await
                .1,
            saved
        );
    }
    #[tokio::test]
    async fn folder_removal_unfiles_runs_and_archiving_unpins() {
        let s = state();
        s.store
            .put(
                "runs",
                "one",
                &json!({"run_id":"one","name":"Original","pinned":false,"archived":false}),
            )
            .unwrap();
        let (_, folder) =
            request(s.clone(), "POST", "/api/folders", json!({"name":" Work "})).await;
        let fid = folder["folder"]["id"].as_str().unwrap();
        assert_eq!(folder["folder"]["name"], "Work");
        assert_eq!(
            request(
                s.clone(),
                "PATCH",
                "/api/runs/one",
                json!({"pinned":true,"name":"Renamed","folder_id":fid,"order":0})
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            request(
                s.clone(),
                "PATCH",
                "/api/runs/one",
                json!({"folder_id":"missing"})
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            request(
                s.clone(),
                "PATCH",
                "/api/runs/one",
                json!({"archived":true})
            )
            .await
            .0,
            StatusCode::OK
        );
        let run = s.store.get("runs", "one").unwrap().unwrap();
        assert_eq!(run["pinned"], false);
        assert!(run["pinned_at"].is_null());
        assert_eq!(run["archived"], true);
        assert_eq!(run["order"], 0);
        assert_eq!(
            request(
                s.clone(),
                "DELETE",
                &format!("/api/folders/{fid}"),
                json!({})
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            request(
                s.clone(),
                "PATCH",
                "/api/runs/one",
                json!({"name":"","archived":false})
            )
            .await
            .0,
            StatusCode::OK
        );
        let run = s.store.get("runs", "one").unwrap().unwrap();
        assert!(run["folder_id"].is_null());
        assert_eq!(run["name"], "Original");
        assert_eq!(run["archived"], false);
        assert_eq!(
            request(s, "PATCH", "/api/runs/missing", json!({"pinned":true}))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }
    #[tokio::test]
    async fn project_and_sidebar_metadata_survive_reopening_sqlite() {
        let file =
            std::env::temp_dir().join(format!("muteki-workspaces-{}.sqlite", Uuid::new_v4()));
        let id;
        {
            let s = Arc::new(AppState::new(crate::store::Store::open(&file).unwrap()));
            let (_,body)=request(s.clone(),"POST","/api/projects",json!({"name":"Persistent","root_path":std::env::temp_dir(),"command_id":"persist-create"})).await;
            id = body["receipt"]["aggregate"]["id"]
                .as_str()
                .unwrap()
                .to_owned();
            assert_eq!(
                request(
                    s,
                    "PUT",
                    "/api/sidebar-preferences",
                    json!({"version":0,"project_order":[id]})
                )
                .await
                .0,
                StatusCode::OK
            );
        }
        {
            let s = Arc::new(AppState::new(crate::store::Store::open(&file).unwrap()));
            let (_, rows) = request(s.clone(), "GET", "/api/projects", json!({})).await;
            assert_eq!(rows["projects"][0]["project_id"], id);
            let (_, prefs) = request(s, "GET", "/api/sidebar-preferences", json!({})).await;
            assert_eq!(prefs["project_order"], json!([id]));
            assert_eq!(prefs["version"], 1);
        }
        std::fs::remove_file(file).unwrap();
    }
}
