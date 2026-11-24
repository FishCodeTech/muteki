//! Shared immutable file storage used by the two Worker protocols.
use super::*;
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::Path,
};

pub(super) fn save(
    root: &Path,
    cwd: &Path,
    worker: &str,
    intent: &str,
    payload: &Value,
) -> Result<Value, String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let cwd = cwd.canonicalize().map_err(|e| e.to_string())?;
    let shared = root.join("shared/live");
    let supplied = Path::new(payload["path"].as_str().unwrap_or(""));
    let container_shared = Path::new(container_exec::SHARED_CONTAINER_WORKSPACE)
        .join(root.file_name().ok_or("Run workspace identity missing")?);
    let source = if let Ok(relative) = supplied.strip_prefix(&container_shared) {
        root.join(relative)
    } else if let Ok(relative) = supplied.strip_prefix(container_exec::CONTAINER_WORKSPACE) {
        root.join(relative)
    } else if supplied.is_absolute() {
        supplied.to_path_buf()
    } else {
        let candidate = cwd.join(supplied);
        let shared_candidate = shared.join(supplied);
        if candidate.exists() || !shared_candidate.exists() {
            candidate
        } else {
            shared_candidate
        }
    };
    let source = source
        .canonicalize()
        .map_err(|_| "PoC 路径不存在或不是普通文件；目录请先打包为文件".to_owned())?;
    if !source.starts_with(&cwd) && !source.starts_with(&shared) {
        return Err("PoC 路径必须位于当前 Worker 工作目录或本 Run 的 shared/live 内".into());
    }
    if !source.is_file() {
        return Err("PoC 路径不存在或不是普通文件；目录请先打包为文件".into());
    }
    let mut input = std::fs::File::open(&source).map_err(|e| e.to_string())?;
    let staging = root.join("shared/objects");
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    let temp = staging.join(format!(".poc-{}", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|e| e.to_string())?;
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            let count = input.read(&mut buffer).map_err(|e| e.to_string())?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
            output
                .write_all(&buffer[..count])
                .map_err(|e| e.to_string())?;
        }
        output.sync_all().map_err(|e| e.to_string())?;
        let digest = format!("{:x}", hash.finalize());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let executable = source
                .metadata()
                .map_err(|e| e.to_string())?
                .permissions()
                .mode()
                & 0o111
                != 0;
            std::fs::set_permissions(
                &temp,
                std::fs::Permissions::from_mode(if executable { 0o555 } else { 0o444 }),
            )
            .map_err(|e| e.to_string())?;
        }

        let relative = format!(
            "shared/objects/{}/{}/{}",
            &digest[..2],
            &digest[2..4],
            digest
        );
        let object = root.join(&relative);
        std::fs::create_dir_all(object.parent().unwrap()).map_err(|e| e.to_string())?;
        std::fs::rename(&temp, &object).map_err(|e| e.to_string())?;
        let name = source
            .file_name()
            .ok_or("PoC filename missing")?
            .to_string_lossy()
            .into_owned();
        let links = root.join("shared/links");
        std::fs::create_dir_all(&links).map_err(|e| e.to_string())?;
        let link = links.join(&name);
        if link.symlink_metadata().is_ok() {
            std::fs::remove_file(&link).map_err(|e| e.to_string())?;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            format!("../objects/{}/{}/{}", &digest[..2], &digest[2..4], digest),
            &link,
        )
        .map_err(|e| e.to_string())?;
        let status = payload["status"]
            .as_str()
            .unwrap_or("available")
            .trim()
            .to_lowercase();
        let status = match status.as_str() {
            "wip" | "directional" | "spent" => status.as_str(),
            _ => "available",
        };
        let row = json!({"ts":now_secs(),"kind":"poc","status":status,"name":name,"sha256":digest,"path":relative,"entry_command":payload["entry_command"].as_str().unwrap_or(""),"intent_id":intent,"solver_id":worker});
        let mut index = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("shared/index.jsonl"))
            .map_err(|e| e.to_string())?;
        writeln!(index, "{row}").map_err(|e| e.to_string())?;
        Ok(
            json!({"poc_id":format!("poc-{}",&digest[..12]),"name":name,"path":relative,"artifact_id":digest,"entry_command":row["entry_command"],"status":status,"note":payload["note"].as_str().unwrap_or(""),"source_path":payload["path"]}),
        )
    })();
    if temp.exists() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

pub(super) fn verify(root: &Path, poc: &Value) -> Result<(), String> {
    let digest = poc["artifact_id"]
        .as_str()
        .ok_or("PoC content hash missing")?;
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Invalid PoC content hash".into());
    }
    let expected = format!(
        "shared/objects/{}/{}/{}",
        &digest[..2],
        &digest[2..4],
        digest
    );
    if poc["path"] != expected || poc["poc_id"] != format!("poc-{}", &digest[..12]) {
        return Err("PoC identity does not match its immutable object".into());
    }
    let mut file = std::fs::File::open(root.join(expected)).map_err(|e| e.to_string())?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let count = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    if format!("{:x}", hash.finalize()) != digest {
        return Err("PoC object content hash mismatch".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn published_content_survives_source_rewrite_and_reuses_content_identity() {
        let root = std::env::temp_dir().join(format!(
            "muteki-poc-parity-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(root.join("workers/worker-a")).unwrap();
        std::fs::create_dir_all(root.join("workers/worker-b")).unwrap();
        std::fs::create_dir_all(root.join("shared/live")).unwrap();
        std::fs::write(root.join("shared/live/probe.py"), b"print('observed')\n").unwrap();
        let payload = json!({"path":"probe.py","entry_command":"python3 shared/links/probe.py","status":"available","note":"replay"});
        let a = save(
            &root,
            &root.join("workers/worker-a"),
            "worker-a",
            "step-a",
            &payload,
        )
        .unwrap();
        let b = save(
            &root,
            &root.join("workers/worker-b"),
            "worker-b",
            "step-b",
            &payload,
        )
        .unwrap();
        assert_eq!(a["poc_id"], b["poc_id"]);
        std::fs::write(root.join("shared/live/probe.py"), b"different content").unwrap();
        verify(&root, &a).unwrap();
        assert_eq!(
            std::fs::read(root.join(a["path"].as_str().unwrap())).unwrap(),
            b"print('observed')\n"
        );
        assert!(
            save(
                &root,
                &root.join("workers/worker-a"),
                "worker-a",
                "step-a",
                &json!({"path":"missing"})
            )
            .is_err()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                root.join(a["path"].as_str().unwrap()),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap();
        }
        std::fs::write(root.join(a["path"].as_str().unwrap()), b"corrupt").unwrap();
        assert!(verify(&root, &a).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
