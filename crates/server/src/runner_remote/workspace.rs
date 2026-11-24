//! Scoped workspace RPC for small control files; ordinary bulk files use transfers.
use super::{
    commands::{Command, Operation, Processor},
    transfers,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::path::Path;
const MAX_FILE: usize = 256 * 1024;
pub(super) fn list(root: &Path, prefix: &str) -> Result<Value, String> {
    if !prefix.is_empty() {
        transfers::validate_path(prefix)?;
    }
    let mut pending = vec![prefix.to_owned()];
    let mut rows = Vec::new();
    while let Some(relative) = pending.pop() {
        let path = root.join(&relative);
        let metadata = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            for entry in std::fs::read_dir(&path).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| "Non-UTF8 workspace path")?;
                let child = if relative.is_empty() {
                    name
                } else {
                    format!("{relative}/{name}")
                };
                if transfers::validate_path(&child).is_ok()
                    && !child.starts_with(".muteki-browser/home")
                    && !child.starts_with(".muteki-blackboard-ingress")
                {
                    pending.push(child);
                }
                if rows.len() + pending.len() > 4096 {
                    return Err("Workspace inventory exceeds 4096 entries".into());
                }
            }
        } else if metadata.is_file() {
            let data = crate::private_files::read(root, &path, 64 * 1024 * 1024)?;
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o111 != 0 {
                    0o755
                } else {
                    0o644
                }
            };
            #[cfg(not(unix))]
            let mode = 0o644;
            rows.push(json!({"path":relative,"size":data.len(),"sha256":transfers::digest(&data),"mode":mode,
                "managed_doc":data.starts_with(b"<!-- muteki-workspace-doc:")}));
        }
    }
    Ok(json!({"files":rows}))
}
pub(super) fn execute(processor: &Processor, command: &Command) -> Result<Value, String> {
    let root = processor.file_workspace(command)?;
    match &command.action {
        Operation::WorkspaceList { prefix } => list(&root, prefix),
        Operation::WorkspaceWrite {
            path,
            data,
            expected_sha256,
            mode,
        } => {
            transfers::validate_path(path)?;
            if data.len() > MAX_FILE * 4 / 3 + 4 || !matches!(*mode, 0o644 | 0o755) {
                return Err("Control file exceeds supported size/mode".into());
            }
            let bytes = STANDARD
                .decode(data)
                .map_err(|_| "Invalid control-file encoding")?;
            if bytes.len() > MAX_FILE {
                return Err("Control file exceeds 256 KiB; use bulk transfer".into());
            }
            let destination = root.join(path);
            let hash = transfers::digest(&bytes);
            if destination.symlink_metadata().is_ok() {
                let prior = crate::private_files::read(&root, &destination, MAX_FILE)?;
                let old = transfers::digest(&prior);
                if old == hash {
                    return Ok(json!({"sha256":hash,"path":path}));
                }
                if expected_sha256.as_deref() != Some(&old) {
                    return Err(
                        "Workspace file changed; refusing to overwrite node-owned content".into(),
                    );
                }
                if !super::controller::controller_published(path) && processor
                    .store()
                    .list("runner_commands")
                    .map_err(|e| e.to_string())?
                    .iter()
                    .any(|row| {
                        row["run_id"] == command.run_id
                            && row["generation"] == command.generation
                            && row["result"]["worker_id"].is_string()
                            && !row["exit"].is_object()
                            && row["execution_stopped"] != true
                    })
                {
                    return Err("Cannot replace an existing workspace input while Workers are active; use a new path".into());
                }
            } else if expected_sha256.is_some() {
                return Err("Expected workspace version is missing".into());
            }
            crate::private_files::write_mode(&root, &destination, &bytes, *mode)?;
            Ok(json!({"sha256":hash,"path":path}))
        }
        Operation::Mailbox { worker_id } | Operation::MailboxReply { worker_id, .. } => {
            let row = processor
                .store()
                .list("runner_commands")
                .map_err(|e| e.to_string())?
                .into_iter()
                .find(|row| {
                    row["run_id"] == command.run_id
                        && row["generation"] == command.generation
                        && row["result"]["worker_id"] == worker_id.as_str()
                })
                .ok_or("Worker mailbox ownership is unavailable")?;
            let solver = row["solver_id"]
                .as_str()
                .ok_or("Worker has no solver identity")?;
            super::commands::valid_solver_id(solver)?;
            let mailbox = root.join(".muteki-blackboard-ingress").join(solver);
            if let Operation::MailboxReply {
                request_id,
                response,
                ..
            } = &command.action
            {
                super::commands::valid_id(request_id)?;
                let bytes = serde_json::to_vec(response).map_err(|e| e.to_string())?;
                if bytes.len() > MAX_FILE {
                    return Err("Mailbox response exceeds 256 KiB".into());
                }
                let path = mailbox.join(format!("result-{request_id}.json"));
                crate::private_files::write_mode(&root, &path, &bytes, 0o644)?;
                return Ok(json!({"request_id":request_id,"written":true}));
            }
            if !mailbox.exists() {
                return Ok(json!({"requests":[]}));
            }
            if mailbox
                .symlink_metadata()
                .map_err(|e| e.to_string())?
                .file_type()
                .is_symlink()
            {
                return Err("Mailbox cannot be a symlink".into());
            }
            let mut requests = Vec::new();
            let mut size = 0;
            for entry in std::fs::read_dir(&mailbox).map_err(|e| e.to_string())? {
                let path = entry.map_err(|e| e.to_string())?.path();
                let name = path.file_name().and_then(|v| v.to_str()).unwrap_or("");
                if !name.starts_with("request-")||!name.ends_with(".json"){continue;}
                let data = crate::private_files::read(&root, &path, MAX_FILE)?;
                size += data.len();
                if size > MAX_FILE {break;}
                let value: Value = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
                let id=value["request_id"].as_str().ok_or("Mailbox request ID missing")?;
                let operation=value["operation"].as_str().ok_or("Mailbox operation missing")?;
                super::commands::valid_id(id)?;super::commands::valid_id(operation)?;
                if (name!=format!("request-{id}.json")&&name!=format!("request-{operation}-{id}.json"))
                    ||value["worker_id"]!=solver||value["execution_generation"]!=command.generation{
                    return Err("Mailbox request identity mismatch".into());
                }
                if mailbox.join(format!("result-{id}.json")).exists(){continue;}
                requests.push(value);
                if requests.len() >= 16 {
                    break;
                }
            }
            Ok(json!({"requests":requests}))
        }
        _ => Err("Not a workspace action".into()),
    }
}
