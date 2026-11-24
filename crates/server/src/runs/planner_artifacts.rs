//! Project source Web Run ArtifactStore files into the matching Rust Run.
//! The history importer snapshots sessions under `legacy/sessions`; Pi's
//! historical peek reads its own Run's `arts` directory.

use crate::{
    AppState, native_chat,
    runs::{encode_run_id, session_dir},
};
use serde_json::Value;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

fn artifact_ids(snapshot: &Value, graph_events: &[Value]) -> Vec<String> {
    let steps: HashSet<&str> = ["open_steps", "done_steps"]
        .into_iter()
        .flat_map(|section| snapshot[section].as_array().into_iter().flatten())
        .filter_map(|step| step["id"].as_str())
        .collect();
    let mut ids = HashSet::new();
    for event in graph_events.iter().filter(|event| {
        event["kind"] == "observation_added"
            && event["payload"]["intent_id"]
                .as_str()
                .is_some_and(|intent| steps.contains(intent))
    }) {
        let payload = &event["payload"];
        for reference in payload["evidence_provenance"]["artifact_refs"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(id) = reference["artifact_id"].as_str() {
                ids.insert(id.to_owned());
            }
        }
        if let Some(id) = payload["artifact_id"].as_str() {
            ids.insert(id.to_owned());
        }
    }
    let mut ids: Vec<String> = ids
        .into_iter()
        .filter(|id| id.len() == 12 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .collect();
    ids.sort();
    ids
}

fn source_file(legacy_arts: &Path, artifact_id: &str) -> Result<Option<PathBuf>, String> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir(legacy_arts).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name
            .strip_prefix(artifact_id)
            .is_some_and(|suffix| suffix.starts_with('.'))
            && entry
                .file_type()
                .map_err(|error| error.to_string())?
                .is_file()
        {
            candidates.push(entry.path());
        }
    }
    candidates.sort();
    Ok(candidates.into_iter().next())
}

pub(super) fn materialize_for_peek(
    state: &AppState,
    run_id: &str,
    snapshot: &Value,
    graph_events: &[Value],
) -> Result<usize, String> {
    let imported = state
        .store
        .get("runs", run_id)
        .map_err(|error| error.to_string())?
        .is_some_and(|run| {
            run["historical"] == true || run["history_event_file"].as_str().is_some()
        });
    if !imported {
        return Ok(0);
    }
    let ids = artifact_ids(snapshot, graph_events);
    if ids.is_empty() {
        return Ok(0);
    }
    let legacy_arts = native_chat::state_dir(state)
        .join("legacy/sessions")
        .join(encode_run_id(run_id))
        .join("workspace/arts");
    if !fs::symlink_metadata(&legacy_arts).is_ok_and(|meta| meta.file_type().is_dir()) {
        return Ok(0);
    }
    let local_arts = session_dir(state, run_id).join("arts");
    fs::create_dir_all(&local_arts).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&local_arts, fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
    }
    let mut copied = 0;
    for id in ids {
        let Some(source) = source_file(&legacy_arts, &id)? else {
            continue;
        };
        let name = source
            .file_name()
            .ok_or_else(|| "legacy artifact has no filename".to_owned())?;
        let destination = local_arts.join(name);
        if destination.exists() {
            continue;
        }
        fs::copy(&source, &destination).map_err(|error| error.to_string())?;
        copied += 1;
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{runs::graph::SharedGraph, store::Store};
    use serde_json::json;

    #[test]
    fn imported_run_maps_only_its_referenced_legacy_artifact_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("muteki.sqlite")).unwrap();
        store
            .put("system", "state_dir", &json!(temp.path()))
            .unwrap();
        let state = AppState::new(store);
        let run_id = "legacy-pentest-one";
        state
            .store
            .put(
                "runs",
                run_id,
                &json!({"historical":true,
            "history_event_file":"state/legacy-pentest-one.jsonl"}),
            )
            .unwrap();
        let source = temp
            .path()
            .join("legacy/sessions")
            .join(run_id)
            .join("workspace/arts");
        fs::create_dir_all(&source).unwrap();
        let bytes = b"HTTP/1.1 302 Found\r\nLocation: /login.php\r\n";
        fs::write(source.join("abcdef123456.txt"), bytes).unwrap();
        fs::write(
            source.join("fedcba654321.txt"),
            b"another Run-independent artifact",
        )
        .unwrap();
        let graph = SharedGraph::open(&state, run_id).unwrap();
        let events = vec![json!({"kind":"observation_added","payload":{
            "intent_id":"step_1","evidence_provenance":{"artifact_refs":[
                {"artifact_id":"abcdef123456","command":"curl /login.php"}]}}})];
        let snapshot = json!({"done_steps":[{"id":"step_1"}],"open_steps":[]});
        assert_eq!(
            materialize_for_peek(&state, run_id, &snapshot, &events).unwrap(),
            1
        );
        let local = session_dir(&state, run_id).join("arts");
        assert_eq!(fs::read(local.join("abcdef123456.txt")).unwrap(), bytes);
        assert!(!local.join("fedcba654321.txt").exists());
        assert_eq!(
            materialize_for_peek(&state, run_id, &snapshot, &events).unwrap(),
            0
        );
        drop(graph);
    }
}
