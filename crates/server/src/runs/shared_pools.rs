//! Shared pool workspace ownership. A pool mounts only its own Run directories.
use crate::{AppState, native_chat};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const TABLE: &str = "runner_shared_workspaces";
pub fn identity(state: &AppState, settings: &Value) -> Result<String, String> {
    let installation = state.store.installation_id().map_err(|e| e.to_string())?;
    Ok(format!(
        "{:x}",
        Sha256::digest(
            format!(
                "{installation}:{}",
                super::container_exec::runtime_policy(settings)
            )
            .as_bytes()
        )
    ))
}
fn state_root(state: &AppState) -> Result<PathBuf, String> {
    native_chat::state_dir(state)
        .canonicalize()
        .map_err(|e| e.to_string())
}
pub fn root(state: &AppState, pool: &str) -> Result<PathBuf, String> {
    if pool.len() != 64 || !pool.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("Invalid shared pool identity".into());
    }
    Ok(state_root(state)?
        .join("runtime/shared-pools")
        .join(pool)
        .join("runs"))
}
fn slot(state: &AppState, run: &str, pool: &str) -> Result<PathBuf, String> {
    let root = if pool == "legacy" {
        state_root(state)?.join("runtime/shared-pool/runs")
    } else {
        root(state, pool)?
    };
    Ok(root.join(super::encode_run_id(run)))
}
pub fn registered_slot(state: &AppState, run: &str) -> Result<PathBuf, String> {
    let record = state.store.get(TABLE, run).map_err(|e| e.to_string())?;
    let pool = record
        .as_ref()
        .and_then(|r| r["pool"].as_str())
        .unwrap_or("legacy");
    slot(state, run, pool)
}
/// Once a Run joins a shared pool, `sessions/<run>` is only a link to its slot. Private
/// projections refuse to traverse links and container paths are mapped from the slot, so
/// anything that writes under the Run workspace for a container must use the slot itself.
pub(crate) fn real_workspace(state: &AppState, run: &str) -> PathBuf {
    let original = super::session_dir(state, run);
    let linked = std::fs::symlink_metadata(&original).is_ok_and(|m| m.file_type().is_symlink());
    if linked && let Ok(slot) = registered_slot(state, run)
        && std::fs::read_link(&original).is_ok_and(|target| target == slot) {
        return slot;
    }
    original
}
pub fn registered_runtime(state: &AppState, run: &str) -> Result<String, String> {
    let row = state.store.get(TABLE, run).map_err(|e| e.to_string())?;
    let suffix = if let Some(pool) = row.as_ref().and_then(|r| r["pool"].as_str()).filter(|pool|*pool!="legacy") {
        root(state, pool)?;
        pool.to_owned()
    } else {
        state.store.installation_id().map_err(|e| e.to_string())?
    };
    Ok(format!(
        "{}-{suffix}",
        super::container_exec::SHARED_CONTAINER_RUN_ID
    ))
}
fn secure_directory(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| e.to_string())?;
    if !std::fs::symlink_metadata(path)
        .map_err(|e| e.to_string())?
        .is_dir()
    {
        return Err("Shared pool path is not a real directory".into());
    }
    Ok(())
}
fn replace_link(original: &Path, target: &Path) -> Result<(), String> {
    let parent = original.parent().ok_or("Workspace has no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let temporary = parent.join(format!(".shared-link-{}", uuid::Uuid::new_v4().simple()));
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, &temporary).map_err(|e| e.to_string())?;
    #[cfg(not(unix))]
    return Err("Shared pool requires directory links on this platform".into());
    if let Err(error) = std::fs::rename(&temporary, original) {
        let _ = std::fs::remove_file(temporary);
        return Err(error.to_string());
    }
    Ok(())
}
pub fn prepare_independent(state: &AppState, run: &str) -> Result<(), String> {
    let original = super::session_dir(state, run);
    let saved = state.store.get(TABLE, run).map_err(|e| e.to_string())?;
    let linked = std::fs::symlink_metadata(&original)
        .ok()
        .is_some_and(|m| m.file_type().is_symlink());
    if !linked && !saved.as_ref().is_some_and(|r| r["phase"] == "unsharing") {
        return Ok(());
    }
    if !state
        .store
        .get("runner_runs", run)
        .map_err(|e| e.to_string())?
        .is_some_and(|r| r["state"] == "stopped")
    {
        return Err("Changing to an independent workspace requires confirmed stop".into());
    }
    let slot = registered_slot(state, run)?;
    if linked && std::fs::read_link(&original).map_err(|e| e.to_string())? != slot {
        return Err("Unrecognized shared workspace link".into());
    }
    let temporary = original
        .parent()
        .ok_or("Workspace parent missing")?
        .join(format!(".unsharing-{}", super::encode_run_id(run)));
    let mut record = saved.unwrap_or_else(|| json!({"run_id":run,"pool":"legacy"}));
    record["phase"] = json!("unsharing");
    state
        .store
        .put(TABLE, run, &record)
        .map_err(|e| e.to_string())?;
    if slot.exists() {
        if temporary.exists() {
            return Err("Both shared source and unsharing destination exist".into());
        }
        if !std::fs::symlink_metadata(&slot)
            .map_err(|e| e.to_string())?
            .is_dir()
        {
            return Err("Shared source is not a real directory".into());
        }
        std::fs::rename(&slot, &temporary).map_err(|e| e.to_string())?;
    }
    if temporary.exists() {
        if !std::fs::symlink_metadata(&temporary)
            .map_err(|e| e.to_string())?
            .is_dir()
        {
            return Err("Unsharing destination is not a real directory".into());
        }
        if linked {
            std::fs::remove_file(&original).map_err(|e| e.to_string())?;
        } else if original.exists() {
            return Err("Independent workspace already exists beside migration data".into());
        }
        std::fs::rename(&temporary, &original).map_err(|e| e.to_string())?;
    } else if linked || !original.is_dir() {
        return Err("Unsharing workspace data is missing".into());
    }
    record["phase"] = json!("detached");
    state
        .store
        .put(TABLE, run, &record)
        .map_err(|e| e.to_string())?;
    Ok(())
}
pub fn prepare(state: &AppState, run: &str, settings: &Value) -> Result<(), String> {
    let pool = identity(state, settings)?;
    let target = slot(state, run, &pool)?;
    let original = super::session_dir(state, run);
    let handle = state.runs.handle(state, run);
    let _guard = handle.event_file_lock.lock().unwrap();
    (|| -> Result<(), String> {
        let saved = state.store.get(TABLE, run).map_err(|e| e.to_string())?;
        let linked = std::fs::symlink_metadata(&original)
            .ok()
            .filter(|m| m.file_type().is_symlink());
        if saved
            .as_ref()
            .is_some_and(|row| row["phase"] == "moving" && row["pool"] != pool)
        {
            return Err(
                "Finish the recorded workspace migration before selecting another pool".into(),
            );
        }
        let previous = saved
            .as_ref()
            .and_then(|r| {
                if r["phase"] == "moving" {
                    r["previous_pool"].as_str()
                } else {
                    r["pool"].as_str()
                }
            })
            .unwrap_or("legacy");
        let old = slot(state, run, previous)?;
        if linked.is_some() {
            let link = std::fs::read_link(&original).map_err(|e| e.to_string())?;
            if link == target {
                if !std::fs::symlink_metadata(&target)
                    .map_err(|e| e.to_string())?
                    .is_dir()
                {
                    return Err("Shared slot is not a real directory".into());
                }
                state
                    .store
                    .put(
                        TABLE,
                        run,
                        &json!({"run_id":run,"pool":pool,"phase":"ready"}),
                    )
                    .map_err(|e| e.to_string())?;
                return Ok(());
            }
            let prior = if let Some(moving) = saved.as_ref().filter(|r| r["phase"] == "moving") {
                slot(
                    state,
                    run,
                    moving["previous_pool"]
                        .as_str()
                        .ok_or("Migration source missing")?,
                )?
            } else {
                old.clone()
            };
            if link != prior {
                return Err("Run workspace has an unexpected shared pool target".into());
            }
            if !state
                .store
                .get("runner_runs", run)
                .map_err(|e| e.to_string())?
                .is_some_and(|r| r["state"] == "stopped")
            {
                return Err("Shared pool migration requires confirmed execution stop".into());
            }
        }
        state
            .store
            .put(
                TABLE,
                run,
                &json!({"run_id":run,"pool":pool,"previous_pool":previous,"phase":"moving"}),
            )
            .map_err(|e| e.to_string())?;
        let parent = target.parent().ok_or("Pool slot has no parent")?;
        let state_root = state_root(state)?;
        let relative = parent
            .strip_prefix(&state_root)
            .map_err(|e| e.to_string())?;
        let mut part = state_root;
        for component in relative.components() {
            part.push(component);
            secure_directory(&part)?;
        }
        let source = if linked.is_some() {
            old
        } else {
            original.clone()
        };
        if source.exists() {
            if target.exists() {
                return Err("Both source and destination workspaces exist; migration requires reconciliation".into());
            }
            if !std::fs::symlink_metadata(&source)
                .map_err(|e| e.to_string())?
                .is_dir()
            {
                return Err("Shared migration source is not a real directory".into());
            }
            std::fs::rename(&source, &target).map_err(|e| e.to_string())?;
        } else if !target.exists() {
            if linked.is_some() {
                return Err("Shared migration source data is missing".into());
            }
            secure_directory(&target)?;
        }
        secure_directory(&target)?;
        replace_link(&original, &target)?;
        state
            .store
            .put(
                TABLE,
                run,
                &json!({"run_id":run,"pool":pool,"phase":"ready"}),
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    })()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pool_identity_and_mounts_separate_policy_and_trust_domains() {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::new(
            crate::store::Store::open(directory.path().join("state.sqlite")).unwrap(),
        );
        state
            .store
            .put("system", "state_dir", &json!(directory.path()))
            .unwrap();
        let a = json!({"_runner_image_id":"sha256:a","_runner_trust_domain":"project-a"});
        let b = json!({"_runner_image_id":"sha256:a","_runner_trust_domain":"project-b"});
        assert_ne!(identity(&state, &a).unwrap(), identity(&state, &b).unwrap());
        for (run, settings) in [("a", &a), ("b", &b)] {
            let original = super::super::session_dir(&state, run);
            std::fs::create_dir_all(&original).unwrap();
            std::fs::write(original.join("evidence"), run).unwrap();
            prepare(&state, run, settings).unwrap();
        }
        let a_slot = registered_slot(&state, "a").unwrap();
        let b_slot = registered_slot(&state, "b").unwrap();
        assert_ne!(a_slot.parent(), b_slot.parent());
        assert_eq!(
            std::fs::read_to_string(a_slot.join("evidence")).unwrap(),
            "a"
        );
        assert!(prepare(&state, "a", &b).is_err());
        state
            .store
            .put("runner_runs", "a", &json!({"state":"stopped"}))
            .unwrap();
        prepare(&state, "a", &b).unwrap();
        assert!(!a_slot.exists());
        assert_eq!(
            registered_slot(&state, "a").unwrap().parent(),
            b_slot.parent()
        );
        prepare_independent(&state, "a").unwrap();
        let original = super::super::session_dir(&state, "a");
        assert!(
            !std::fs::symlink_metadata(&original)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(original.join("evidence")).unwrap(),
            "a"
        );
        assert!(!registered_slot(&state, "a").unwrap().exists());
    }
    #[test]
    fn interrupted_move_repairs_the_link_without_losing_data() {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::new(
            crate::store::Store::open(directory.path().join("state.sqlite")).unwrap(),
        );
        state
            .store
            .put("system", "state_dir", &json!(directory.path()))
            .unwrap();
        let a = json!({"_runner_trust_domain":"a"});
        let b = json!({"_runner_trust_domain":"b"});
        let original = super::super::session_dir(&state, "run");
        std::fs::create_dir_all(&original).unwrap();
        std::fs::write(original.join("evidence"), "preserved").unwrap();
        prepare(&state, "run", &a).unwrap();
        let old = registered_slot(&state, "run").unwrap();
        let next_pool = identity(&state, &b).unwrap();
        let next = slot(&state, "run", &next_pool).unwrap();
        state
            .store
            .put("runner_runs", "run", &json!({"state":"stopped"}))
            .unwrap();
        state.store.put(TABLE,"run",&json!({"run_id":"run","pool":next_pool,"previous_pool":identity(&state,&a).unwrap(),"phase":"moving"})).unwrap();
        std::fs::create_dir_all(next.parent().unwrap()).unwrap();
        std::fs::rename(old, &next).unwrap();
        prepare(&state, "run", &b).unwrap();
        assert_eq!(
            original.canonicalize().unwrap(),
            next.canonicalize().unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(original.join("evidence")).unwrap(),
            "preserved"
        );
    }
}
