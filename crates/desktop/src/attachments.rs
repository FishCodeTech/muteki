//! Composer attachment drafts kept on disk while a draft is unsent. Each
//! New entries record the service identity and draft that created them.
use crate::policy::BridgeError;
use sha2::{Digest,Sha256};
use serde_json::{Value, json};
use std::{
    fs,
    io::{ErrorKind, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

pub const CACHE_LIMIT: u64 = 256 * 1024 * 1024;
static CACHE_WRITER: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub struct Scope<'a> {
    pub origin: &'a str,
    pub service_id: &'a str,
    pub identity_id: &'a str,
}

pub fn scope_key(scope: &Scope<'_>, draft_id: &str) -> Result<String, BridgeError> {
    if scope.service_id.is_empty() || scope.identity_id.is_empty() || draft_id.is_empty() {
        return Err(BridgeError::new(
            "desktop.attachment_scope_unverified",
            "登录当前服务后才能保存附件草稿。",
        ));
    }
    Ok(serde_json::to_string(&[scope.origin, scope.service_id, scope.identity_id, draft_id])
        .expect("strings serialize"))
}

fn same_scope(stored: &Value, current: &str) -> bool {
    let value = stored.as_str().unwrap_or("");
    value == current || value == format!("{:x}",Sha256::digest(current.as_bytes()))
}

fn valid_id(id: &str) -> Result<&str, BridgeError> {
    if id.len() == 36
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-')
    {
        Ok(id)
    } else {
        Err(BridgeError::new(
            "desktop.attachment_cache_id_invalid",
            "附件缓存引用无效。",
        ))
    }
}

/// Bytes of every file in the cache, including metadata and interrupted
/// writes: crash residue still occupies the user's disk.
pub fn usage(root: &Path) -> std::io::Result<u64> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut total = 0;
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            total += entry.metadata()?.len();
        }
    }
    Ok(total)
}

fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

pub struct Incoming<'a> {
    pub name: &'a str,
    pub kind: &'a str,
    pub last_modified: f64,
    pub bytes: &'a [u8],
}

pub fn save(root: &Path, key: &str, input: Incoming<'_>) -> Result<Value, BridgeError> {
    let size = input.bytes.len() as u64;
    if size > CACHE_LIMIT {
        return Err(BridgeError::new(
            "desktop.attachment_cache_limit",
            "附件超过桌面草稿缓存范围，请保留原文件后重选。",
        ));
    }
    let _writer = CACHE_WRITER.lock().unwrap_or_else(|error| error.into_inner());
    let failed = |error: std::io::Error| {
        BridgeError::new(
            "desktop.attachment_cache_write_failed",
            format!("附件草稿保存失败，原输入仍保留，请重试。{error}"),
        )
        .retryable()
    };
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)
        .map_err(failed)?;
    let id = uuid::Uuid::new_v4().to_string();
    let name = input
        .name
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or("attachment");
    let kind = if input.kind.is_empty() {
        "application/octet-stream"
    } else {
        input.kind
    };
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let row = json!({"id": id, "scope": key, "name": name, "type": kind, "lastModified": input.last_modified, "size": size, "sha256":format!("{:x}",Sha256::digest(input.bytes)), "createdAt": created});
    let serialized = row.to_string();
    if usage(root).map_err(failed)? + size + serialized.len() as u64 > CACHE_LIMIT {
        return Err(BridgeError::new(
            "desktop.attachment_cache_full",
            "桌面附件缓存已满（含元数据和未完成缓存）。请移除不再需要的草稿附件后重试。",
        ));
    }
    let file = root.join(format!("{id}.bin"));
    let metadata = root.join(format!("{id}.json"));
    let temporary = |path: &PathBuf| {
        path.with_extension(format!(
            "{}.tmp",
            path.extension().unwrap().to_string_lossy()
        ))
    };
    let written = write_new(&temporary(&file), input.bytes)
        .and_then(|()| write_new(&temporary(&metadata), serialized.as_bytes()))
        .and_then(|()| fs::rename(temporary(&file), &file))
        .and_then(|()| fs::rename(temporary(&metadata), &metadata));
    if let Err(error) = written {
        for path in [
            file.clone(),
            metadata.clone(),
            temporary(&file),
            temporary(&metadata),
        ] {
            let _ = fs::remove_file(path);
        }
        return Err(failed(error));
    }
    Ok(json!({"id": id, "name": name, "type": kind, "size": size,"sha256":row["sha256"]}))
}

pub struct Restored {
    pub bytes: Vec<u8>,
    pub name: String,
    pub kind: String,
    pub last_modified: f64,
}

pub fn restore(root: &Path, key: &str, id: &str) -> Result<Restored, BridgeError> {
    let id = valid_id(id)?;
    let unavailable = |detail: String| {
        BridgeError::new(
            "desktop.attachment_cache_unavailable",
            format!("附件草稿缓存无法读取，请重新选择原文件。{detail}"),
        )
        .retryable()
    };
    for path in [root.join(format!("{id}.json")),root.join(format!("{id}.bin"))]{let entry=fs::symlink_metadata(&path).map_err(|error|unavailable(error.to_string()))?;if !entry.is_file()||entry.file_type().is_symlink()||entry.len()>CACHE_LIMIT{return Err(unavailable("附件缓存文件无效。".into()));}}
    let metadata: Value = fs::read(root.join(format!("{id}.json")))
        .map_err(|error| unavailable(error.to_string()))
        .and_then(|raw| {
            serde_json::from_slice(&raw).map_err(|error| unavailable(error.to_string()))
        })?;
    if !same_scope(&metadata["scope"], key) {
        return Err(BridgeError::new(
            "desktop.attachment_cache_scope_changed",
            "此附件缓存属于其他工作台或草稿，请重新选择。",
        ));
    }
    let bytes =
        fs::read(root.join(format!("{id}.bin"))).map_err(|error| unavailable(error.to_string()))?;
    if metadata["id"]!=id||metadata["size"].as_u64()!=Some(bytes.len() as u64)||metadata["sha256"]!=format!("{:x}",Sha256::digest(&bytes)){
        return Err(BridgeError::new("desktop.attachment_cache_corrupt","附件缓存已损坏或缺少完整性记录，请重新选择原文件。"));
    }
    Ok(Restored {
        bytes,
        name: metadata["name"].as_str().unwrap_or("attachment").to_owned(),
        kind: metadata["type"]
            .as_str()
            .unwrap_or("application/octet-stream")
            .to_owned(),
        last_modified: metadata["lastModified"].as_f64().unwrap_or(0.0),
    })
}

pub fn remove(root: &Path, key: &str, id: &str) -> Result<(), BridgeError> {
    let id = valid_id(id)?;
    let metadata = root.join(format!("{id}.json"));
    let failed = |error: std::io::Error| {
        BridgeError::new(
            "desktop.attachment_cache_unavailable",
            format!("附件草稿缓存无法移除：{error}"),
        )
        .retryable()
    };
    let row: Value = match fs::read(&metadata) {
        Ok(raw) => serde_json::from_slice(&raw).unwrap_or(Value::Null),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(failed(error)),
    };
    if !same_scope(&row["scope"], key) {
        return Err(BridgeError::new(
            "desktop.attachment_cache_scope_changed",
            "此附件缓存属于其他工作台或草稿。",
        ));
    }
    for path in [root.join(format!("{id}.bin")), metadata] {
        match fs::remove_file(path) {
            Err(error) if error.kind() != ErrorKind::NotFound => return Err(failed(error)),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("muteki-attachments-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }

    fn scope() -> Scope<'static> {
        Scope {
            origin: "http://127.0.0.1:3101",
            service_id: "svc",
            identity_id: "operator",
        }
    }

    #[test]
    fn round_trips_within_its_draft_only() {
        let root = root("round");
        let key = scope_key(&scope(), "draft-1").unwrap();
        let other = scope_key(&scope(), "draft-2").unwrap();
        assert_ne!(key, other);
        let saved = save(
            &root,
            &key,
            Incoming {
                name: "dir/a.txt",
                kind: "",
                last_modified: 7.0,
                bytes: b"hello",
            },
        )
        .unwrap();
        assert_eq!(saved["name"], "a.txt");
        assert_eq!(saved["type"], "application/octet-stream");
        assert!(saved.get("sha512").is_none());
        let id = saved["id"].as_str().unwrap();
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(root.join(format!("{id}.bin")))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let restored = restore(&root, &key, id).unwrap();
        assert_eq!(
            (
                restored.bytes.as_slice(),
                restored.name.as_str(),
                restored.last_modified
            ),
            (&b"hello"[..], "a.txt", 7.0)
        );
        assert_eq!(
            restore(&root, &other, id).err().unwrap().code,
            "desktop.attachment_cache_scope_changed"
        );
        assert_eq!(
            remove(&root, &other, id).unwrap_err().code,
            "desktop.attachment_cache_scope_changed"
        );
        fs::write(root.join(format!("{id}.bin")), b"jello").unwrap();
        assert_eq!(restore(&root, &key, id).err().unwrap().code,"desktop.attachment_cache_corrupt");
        remove(&root, &key, id).unwrap();
        remove(&root, &key, id).unwrap();
        assert_eq!(
            restore(&root, &key, id).err().unwrap().code,
            "desktop.attachment_cache_unavailable"
        );
        assert_eq!(usage(&root).unwrap(), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_bad_ids_unscoped_drafts_and_a_full_cache() {
        let root = root("limits");
        assert_eq!(
            restore(&root, "k", "../../etc/passwd").err().unwrap().code,
            "desktop.attachment_cache_id_invalid"
        );
        assert_eq!(
            remove(&root, "k", "ABCDEF00-0000-0000-0000-000000000000")
                .unwrap_err()
                .code,
            "desktop.attachment_cache_id_invalid"
        );
        assert_eq!(
            scope_key(
                &Scope {
                    origin: "o",
                    service_id: "",
                    identity_id: "i"
                },
                "d"
            )
            .unwrap_err()
            .code,
            "desktop.attachment_scope_unverified"
        );
        fs::create_dir_all(&root).unwrap();
        let residue = fs::File::create(root.join("residue.bin.tmp")).unwrap();
        residue.set_len(CACHE_LIMIT - 4).unwrap();
        let key = scope_key(&scope(), "d").unwrap();
        assert_eq!(
            save(
                &root,
                &key,
                Incoming {
                    name: "a",
                    kind: "text/plain",
                    last_modified: 0.0,
                    bytes: b"12345"
                }
            )
            .unwrap_err()
            .code,
            "desktop.attachment_cache_full"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_cache_without_matching_scope_or_digest_is_rejected() {
        let root = root("legacy");
        let key = scope_key(&scope(), "draft-legacy").unwrap();
        let saved = save(
            &root,
            &key,
            Incoming {
                name: "old.txt",
                kind: "text/plain",
                last_modified: 0.0,
                bytes: b"old",
            },
        )
        .unwrap();
        let id = saved["id"].as_str().unwrap();
        let metadata = root.join(format!("{id}.json"));
        let mut row: Value = serde_json::from_slice(&fs::read(&metadata).unwrap()).unwrap();
        row["scope"] = json!("a".repeat(64));
        row["sha256"] = json!("legacy-file-digest");
        fs::write(&metadata, row.to_string()).unwrap();
        assert_eq!(restore(&root, &key, id).err().unwrap().code,"desktop.attachment_cache_scope_changed");
        assert!(root.join(format!("{id}.bin")).exists());
        assert_eq!(remove(&root, &key, id).unwrap_err().code,"desktop.attachment_cache_scope_changed");
        fs::remove_dir_all(root).unwrap();
    }
}
