//! Task-owned and project-scoped connector credential references. Secret bytes live
//! only in the private state directory; public Task records contain metadata and refs.
use super::{Api, Error, now, require, task};
use crate::AppState;
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use uuid::Uuid;

const METADATA: &str = "src_credential_metadata";

fn alias(value: &str) -> Api<&str> {
    if value.is_empty()
        || value.len() > 96
        || value == "."
        || value == ".."
        || !value
            .bytes()
            .all(|v| v.is_ascii_alphanumeric() || matches!(v, b'_' | b'-' | b'.'))
    {
        return Err(Error::invalid("invalid SRC credential alias"));
    }
    Ok(value)
}

fn project_slug(value: &str) -> Api<&str> {
    if value.is_empty()
        || value.len() > 128
        || value == "."
        || value == ".."
        || !value
            .bytes()
            .all(|v| v.is_ascii_alphanumeric() || matches!(v, b'_' | b'-' | b'.' | b':'))
    {
        return Err(Error::invalid("invalid SRC project identifier"));
    }
    Ok(value)
}

pub(super) fn task_project_id(s: &AppState, id: &str) -> Api<Option<String>> {
    let t = task(s, id)?;
    Ok(t["project_id"]
        .as_str()
        .or_else(|| t["binding"]["project_id"].as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned))
}

#[derive(Clone, Debug)]
enum ScopedRef<'a> {
    Task { task_id: &'a str, name: &'a str },
    Project { project_id: &'a str, name: &'a str },
}

impl<'a> ScopedRef<'a> {
    fn canonical(&self) -> String {
        match self {
            Self::Task { task_id, name } => format!("secret://src/{task_id}/{name}"),
            Self::Project { project_id, name } => format!("secret://src/project/{project_id}/{name}"),
        }
    }
    fn disk_parts(&self) -> Vec<&'a str> {
        match self {
            Self::Task { task_id, .. } => vec!["src", task_id],
            Self::Project { project_id, .. } => vec!["src", "project", project_id],
        }
    }
    fn name(&self) -> &'a str {
        match self {
            Self::Task { name, .. } | Self::Project { name, .. } => name,
        }
    }
}

fn reference(id: &str, name: &str) -> String {
    format!("secret://src/{id}/{name}")
}

fn project_reference(project_id: &str, name: &str) -> String {
    format!("secret://src/project/{project_id}/{name}")
}

pub(super) fn managed_reference(id: &str, name: &str) -> Api<String> {
    let valid = alias(name)?;
    Ok(reference(id, valid))
}

fn parse_reference_scoped<'a>(s: &AppState, id: &'a str, value: &'a str) -> Api<ScopedRef<'a>> {
    let rest = value
        .strip_prefix("secret://src/")
        .ok_or_else(|| Error::invalid("SRC credential reference must use secret://src/..."))?;
    if let Some(proj_rest) = rest.strip_prefix("project/") {
        let (project_id, name) = proj_rest
            .split_once('/')
            .ok_or_else(|| Error::invalid("invalid project credential reference"))?;
        let project_id = project_slug(project_id)?;
        let name = alias(name)?;
        let actual_project = task_project_id(s, id)?;
        if actual_project.as_deref() != Some(project_id) {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "src.credential_forbidden",
                "project credential reference belongs to another project",
            ));
        }
        return Ok(ScopedRef::Project { project_id, name });
    }
    if let Some(task_rest) = rest.strip_prefix("task/")
        && task_rest.contains('/')
    {
        let (ref_task_id, name) = task_rest
            .split_once('/')
            .ok_or_else(|| Error::invalid("invalid SRC credential reference"))?;
        if ref_task_id != id {
            return Err(Error::invalid("SRC credential reference belongs to another Task"));
        }
        let name = alias(name)?;
        return Ok(ScopedRef::Task { task_id: id, name });
    }
    let name = rest
        .strip_prefix(id)
        .and_then(|path| path.strip_prefix('/'))
        .ok_or_else(|| Error::invalid("SRC credential reference belongs to another Task"))?;
    let name = alias(name)?;
    Ok(ScopedRef::Task { task_id: id, name })
}

pub(super) fn check_reference_scope(s: &AppState, id: &str, value: &str) -> Api<()> {
    parse_reference_scoped(s, id, value)?;
    Ok(())
}

fn path_for_parts(s: &AppState, parts: &[&str], name: &str) -> Api<PathBuf> {
    alias(name)?;
    let mut root = crate::native_chat::state_dir(s);
    for part in std::iter::once("_secrets").chain(parts.iter().copied()) {
        root = root.join(part);
        fs::create_dir_all(&root).map_err(crate::runs::src_api::db)?;
        if fs::symlink_metadata(&root)
            .map_err(crate::runs::src_api::db)?
            .file_type()
            .is_symlink()
        {
            return Err(Error::invalid("SRC credential directory is a symlink"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
                .map_err(crate::runs::src_api::db)?;
        }
    }
    Ok(root.join(format!("{name}.secret")))
}

pub(super) fn put(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let value = require(body, "value")?;
    let task_project = task_project_id(s, id)?;
    let requested_scope = body["scope"].as_str().map(str::trim).unwrap_or("");
    let requested_project = body["project_id"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let explicit_ref = body["reference"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty());

    let (scoped_key, parts, name, scope_label, scoped_project_id): (
        String,
        Vec<String>,
        String,
        &str,
        Option<String>,
    ) = if let Some(raw_ref) = explicit_ref {
        match parse_reference_scoped(s, id, raw_ref)? {
            ScopedRef::Project { project_id, name } => {
                if let Some(alias_arg) = body["alias"].as_str().map(str::trim).filter(|v| !v.is_empty())
                    && alias_arg != name
                {
                    return Err(Error::invalid("alias does not match credential reference"));
                }
                (
                    project_reference(project_id, name),
                    vec!["src".into(), "project".into(), project_id.into()],
                    name.into(),
                    "project",
                    Some(project_id.into()),
                )
            }
            ScopedRef::Task { task_id, name } => {
                if let Some(alias_arg) = body["alias"].as_str().map(str::trim).filter(|v| !v.is_empty())
                    && alias_arg != name
                {
                    return Err(Error::invalid("alias does not match credential reference"));
                }
                (
                    reference(task_id, name),
                    vec!["src".into(), task_id.into()],
                    name.into(),
                    "task",
                    None,
                )
            }
        }
    } else if requested_scope == "project" || requested_project.is_some() {
        let name = alias(require(body, "alias")?)?.to_owned();
        let project_id = match (requested_project, task_project.as_deref()) {
            (Some(req), Some(actual)) if req == actual => project_slug(req)?.to_owned(),
            (Some(_), _) => {
                return Err(Error::new(
                    StatusCode::FORBIDDEN,
                    "src.credential_forbidden",
                    "cannot store credential for another project",
                ));
            }
            (None, Some(actual)) => project_slug(actual)?.to_owned(),
            (None, None) => {
                return Err(Error::invalid("Task is not bound to a project_id"));
            }
        };
        (
            project_reference(&project_id, &name),
            vec!["src".into(), "project".into(), project_id.clone()],
            name,
            "project",
            Some(project_id),
        )
    } else {
        let name = alias(require(body, "alias")?)?.to_owned();
        (
            reference(id, &name),
            vec!["src".into(), id.into()],
            name,
            "task",
            None,
        )
    };

    let part_refs: Vec<&str> = parts.iter().map(String::as_str).collect();
    let destination = path_for_parts(s, &part_refs, &name)?;
    let temporary = destination.with_extension(format!("{}.tmp", Uuid::new_v4().simple()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(crate::runs::src_api::db)?;
    if let Err(error) = file
        .write_all(value.as_bytes())
        .and_then(|_| file.sync_all())
    {
        let _ = fs::remove_file(&temporary);
        return Err(crate::runs::src_api::db(error));
    }
    fs::rename(&temporary, &destination).map_err(crate::runs::src_api::db)?;
    let old = s
        .store
        .get(METADATA, &scoped_key)
        .map_err(crate::runs::src_api::db)?;
    let stamp = now();
    let created_at = old
        .as_ref()
        .and_then(|v| v["created_at"].as_str())
        .filter(|v| !v.is_empty())
        .unwrap_or(&stamp)
        .to_owned();
    let metadata = json!({
        "reference": scoped_key,
        "scope": scope_label,
        "project_id": scoped_project_id,
        "task_id": id,
        "alias": name,
        "status": "active",
        "version": old.as_ref().and_then(|v| v["version"].as_u64()).unwrap_or(0) + 1,
        "created_at": created_at,
        "updated_at": stamp
    });
    s.store
        .put(METADATA, &scoped_key, &metadata)
        .map_err(crate::runs::src_api::db)?;
    Ok(metadata)
}

fn metadata_for_scoped(s: &AppState, _id: &str, parsed: &ScopedRef<'_>) -> Api<Value> {
    let key = parsed.canonical();
    let metadata = s
        .store
        .get(METADATA, &key)
        .map_err(crate::runs::src_api::db)?
        .ok_or_else(|| {
            Error::new(
                StatusCode::NOT_FOUND,
                "src.credential_missing",
                "SRC credential is unavailable",
            )
        })?;
    let scope_ok = match parsed {
        ScopedRef::Task { task_id, .. } => metadata["task_id"] == *task_id,
        ScopedRef::Project { project_id, .. } => {
            metadata["scope"] == "project" && metadata["project_id"] == *project_id
        }
    };
    if !scope_ok || metadata["status"] != "active" {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "src.credential_changed",
            "SRC credential reference changed",
        ));
    }
    Ok(metadata)
}

pub(super) fn version(s: &AppState, id: &str, reference: &str) -> Api<u64> {
    let parsed = parse_reference_scoped(s, id, reference)?;
    let metadata = metadata_for_scoped(s, id, &parsed)?;
    metadata["version"]
        .as_u64()
        .ok_or_else(|| Error::invalid("SRC credential version missing"))
}

pub(super) fn resolve(
    s: &AppState,
    id: &str,
    reference: &str,
    expected: Option<u64>,
) -> Api<String> {
    let parsed = parse_reference_scoped(s, id, reference)?;
    let metadata = metadata_for_scoped(s, id, &parsed)?;
    let actual = metadata["version"]
        .as_u64()
        .ok_or_else(|| Error::invalid("SRC credential version missing"))?;
    if expected.is_some_and(|value| value != actual) {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "src.credential_changed",
            "SRC credential changed before dispatch",
        ));
    }
    let parts = parsed.disk_parts();
    let destination = path_for_parts(s, &parts, parsed.name())?;
    let file = fs::symlink_metadata(&destination).map_err(crate::runs::src_api::db)?;
    if !file.file_type().is_file() || file.file_type().is_symlink() {
        return Err(Error::invalid("SRC credential is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if file.permissions().mode() & 0o077 != 0 {
            return Err(Error::invalid(
                "SRC credential file permissions are too broad",
            ));
        }
    }
    fs::read_to_string(destination).map_err(crate::runs::src_api::db)
}

pub(super) fn revoke(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let explicit_ref = body["reference"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let requested_scope = body["scope"].as_str().map(str::trim).unwrap_or("");
    let requested_project = body["project_id"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty());

    let (key, parts, name): (String, Vec<String>, String) = if let Some(raw_ref) = explicit_ref {
        match parse_reference_scoped(s, id, raw_ref)? {
            ScopedRef::Project { project_id, name } => (
                project_reference(project_id, name),
                vec!["src".into(), "project".into(), project_id.into()],
                name.into(),
            ),
            ScopedRef::Task { task_id, name } => (
                reference(task_id, name),
                vec!["src".into(), task_id.into()],
                name.into(),
            ),
        }
    } else if requested_scope == "project" || requested_project.is_some() {
        let name = alias(require(body, "alias")?)?.to_owned();
        let actual = task_project_id(s, id)?;
        let project_id = match (requested_project, actual.as_deref()) {
            (Some(req), Some(act)) if req == act => project_slug(req)?.to_owned(),
            (Some(_), _) => {
                return Err(Error::new(
                    StatusCode::FORBIDDEN,
                    "src.credential_forbidden",
                    "cannot revoke credential for another project",
                ));
            }
            (None, Some(act)) => project_slug(act)?.to_owned(),
            (None, None) => {
                return Err(Error::invalid("Task is not bound to a project_id"));
            }
        };
        (
            project_reference(&project_id, &name),
            vec!["src".into(), "project".into(), project_id],
            name,
        )
    } else {
        let name = alias(require(body, "alias")?)?.to_owned();
        (reference(id, &name), vec!["src".into(), id.into()], name)
    };

    let mut metadata = s
        .store
        .get(METADATA, &key)
        .map_err(crate::runs::src_api::db)?
        .ok_or_else(|| {
            Error::new(
                StatusCode::NOT_FOUND,
                "src.credential_missing",
                "SRC credential is unavailable",
            )
        })?;
    let part_refs: Vec<&str> = parts.iter().map(String::as_str).collect();
    let destination = path_for_parts(s, &part_refs, &name)?;
    if destination.exists() {
        fs::remove_file(destination).map_err(crate::runs::src_api::db)?;
    }
    metadata["status"] = json!("revoked");
    metadata["updated_at"] = json!(now());
    s.store
        .put(METADATA, &key, &metadata)
        .map_err(crate::runs::src_api::db)?;
    Ok(metadata)
}

pub(super) fn list(s: &AppState, id: &str) -> Api<Value> {
    let project_id = task_project_id(s, id)?;
    let rows = s
        .store
        .list(METADATA)
        .map_err(crate::runs::src_api::db)?
        .into_iter()
        .filter(|v| {
            v["task_id"] == id
                || project_id
                    .as_deref()
                    .is_some_and(|pid| v["scope"] == "project" && v["project_id"] == pid)
        })
        .collect::<Vec<_>>();
    Ok(json!(rows))
}
