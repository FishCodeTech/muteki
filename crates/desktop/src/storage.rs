//! Installed desktop storage adoption. Sources are retained for rollback.
use std::{
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::fs::{OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::Stdio,
};

fn private_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|e| format!("Cannot create {}: {e}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())
}
pub fn lock(path: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|e| e.to_string())?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(format!(
            "Storage is in use: {}. Quit the other Muteki before upgrading.",
            path.display()
        ));
    }
    Ok(file)
}

pub fn installed_root() -> Result<PathBuf, String> {
    let home =
        std::env::var_os("HOME").ok_or("HOME is unavailable; set MUTEKI_STATE_DIR explicitly")?;
    let support = PathBuf::from(home).join("Library/Application Support");
    let app = support.join("Muteki");
    private_dir(&app)?;
    let _lock = lock(&app.join("storage-upgrade.lock"))?;
    let target = app.join("data");
    let previous = support.join("Muteki Rust");
    if fs::symlink_metadata(&target).is_err()
        && !app.join("environments/stable/state").is_dir()
        && previous.join("muteki.sqlite").is_file()
    {
        std::os::unix::fs::symlink(previous.canonicalize().map_err(|e| e.to_string())?, &target)
            .map_err(|e| e.to_string())?;
    }
    private_dir(&target)?;
    target.canonicalize().map_err(|e| e.to_string())
}

/// Runs before starting the service. An unfinished import is retried, never
/// mistaken for a successful first launch. No existing Rust database is merged.
pub fn prepare_legacy(state: &Path, server: &Path) -> Result<(), String> {
    let home = std::env::var_os("HOME").ok_or("HOME is unavailable")?;
    let app = PathBuf::from(home).join("Library/Application Support/Muteki");
    let source = app.join("environments/stable");
    let journal = state.join("desktop-legacy-migration.json");
    if !journal.exists() && (state.join("muteki.sqlite").exists() || !source.join("state").is_dir())
    {
        return Ok(());
    }
    let _lock = lock(&app.join("storage-upgrade.lock"))?;
    let mut record = if journal.exists() {
        serde_json::from_slice::<serde_json::Value>(&fs::read(&journal).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?
    } else {
        serde_json::json!({"version":1,"complete":false,"source":source,"backup":app.join("migration-backups").join(uuid::Uuid::new_v4().to_string())})
    };
    if record["complete"] == true {
        return Ok(());
    }
    let source = PathBuf::from(
        record["source"]
            .as_str()
            .ok_or("Invalid migration source")?,
    );
    let backup = PathBuf::from(
        record["backup"]
            .as_str()
            .ok_or("Invalid migration backup")?,
    );
    let _source_lock = lock(&source.join("backend.lock"))?;
    write_record(&journal, &record)?;
    if record["snapshot_complete"] != true {
        let backup_root = app.join("migration-backups");
        if backup.parent() != Some(backup_root.as_path())
            || backup
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| uuid::Uuid::parse_str(name).ok())
                .is_none()
        {
            return Err("Invalid migration backup path".into());
        }
        if backup.symlink_metadata().is_ok() {
            if backup.is_symlink() {
                return Err("Migration backup cannot be a link".into());
            }
            fs::remove_dir_all(&backup).map_err(|e| e.to_string())?;
        }
        private_dir(backup.parent().ok_or("Backup has no parent")?)?;
        let status = crate::shell_env::command("/usr/bin/ditto")
            .arg(&source)
            .arg(&backup)
            .status()
            .map_err(|e| e.to_string())?;
        if !status.success() {
            return Err(format!("Cannot preserve legacy data: {status}"));
        }
        let original = tree_digest(&source)?;
        let copied = tree_digest(&backup)?;
        if original != copied {
            return Err(
                "Legacy backup verification failed; source was retained. Quit Muteki and retry."
                    .into(),
            );
        }
        record["snapshot_sha256"] = serde_json::json!(original);
        record["snapshot_complete"] = serde_json::json!(true);
        write_record(&journal, &record)?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(state.join("desktop-migration.log"))
        .map_err(|e| e.to_string())?;
    let status = crate::shell_env::command(server)
        .arg("--state-dir")
        .arg(state)
        .arg("--import-from")
        .arg(&backup)
        .arg("--import-only")
        .current_dir(state)
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log)
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!(
            "Legacy migration failed ({status}); original and backup are preserved. See desktop-migration.log in {}",
            state.display()
        ));
    }
    record["complete"] = serde_json::json!(true);
    write_record(&journal, &record)
}
fn write_record(path: &Path, record: &serde_json::Value) -> Result<(), String> {
    use std::io::Write;
    let temp = path.with_extension("pending");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|e| e.to_string())?;
    file.write_all(&serde_json::to_vec_pretty(record).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    fs::rename(&temp, path).map_err(|e| e.to_string())
}

/// Next's standalone server writes route/image caches relative to server.js.
/// Run a per-package copy outside the signed, read-only application bundle.
pub fn frontend_cache(resources: &Path) -> Result<PathBuf, String> {
    let id = fs::read_to_string(resources.join("frontend-build-id"))
        .map_err(|e| format!("Missing frontend build identity: {e}"))?;
    uuid::Uuid::parse_str(&id).map_err(|e| format!("Invalid frontend build identity: {e}"))?;
    let home = std::env::var_os("HOME").ok_or("HOME is unavailable")?;
    let root = PathBuf::from(home).join("Library/Caches/Muteki/frontend");
    private_dir(&root)?;
    let _lock = lock(&root.join("prepare.lock"))?;
    let target = root.join(id.trim());
    if !target.exists() {
        let pending = root.join(format!("{}.pending", id.trim()));
        if pending.exists() {
            fs::remove_dir_all(&pending).map_err(|e| e.to_string())?;
        }
        let status = crate::shell_env::command("/usr/bin/ditto")
            .arg(resources.join("frontend"))
            .arg(&pending)
            .status()
            .map_err(|e| e.to_string())?;
        if !status.success() {
            return Err(format!("Cannot prepare frontend cache: {status}"));
        }
        fs::rename(&pending, &target).map_err(|e| e.to_string())?;
    }
    Ok(target)
}

fn tree_digest(root: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    use std::{io::Read, os::unix::ffi::OsStrExt};
    fn visit(path: &Path, hash: &mut Sha256) -> Result<(), String> {
        let mut entries = fs::read_dir(path)
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            hash.update((name.as_bytes().len() as u64).to_le_bytes());
            hash.update(name.as_bytes());
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            if kind.is_symlink() {
                hash.update(b"link");
                let target = fs::read_link(entry.path()).map_err(|e| e.to_string())?;
                hash.update((target.as_os_str().as_bytes().len() as u64).to_le_bytes());
                hash.update(target.as_os_str().as_bytes());
            } else if kind.is_dir() {
                hash.update(b"directory");
                visit(&entry.path(), hash)?;
                hash.update(b"end-directory");
            } else if kind.is_file() {
                hash.update(b"file");
                let mut file = File::open(entry.path()).map_err(|e| e.to_string())?;
                hash.update(
                    file.metadata()
                        .map_err(|e| e.to_string())?
                        .len()
                        .to_le_bytes(),
                );
                let mut buffer = [0u8; 65536];
                loop {
                    let count = file.read(&mut buffer).map_err(|e| e.to_string())?;
                    if count == 0 {
                        break;
                    }
                    hash.update(&buffer[..count]);
                }
            } else {
                return Err(format!(
                    "Unsupported special file in legacy data: {}",
                    entry.path().display()
                ));
            }
        }
        Ok(())
    }
    let mut hash = Sha256::new();
    visit(root, &mut hash)?;
    Ok(format!("{:x}", hash.finalize()))
}

/// Keep both origins stable across launches: WebKit storage, drafts and local
/// connection credentials are scoped to these origins.
pub fn service_ports(state: &Path) -> Result<(u16, u16), String> {
    let directory = state.join("desktop");
    private_dir(&directory)?;
    let path = directory.join("service-ports.json");
    if path.exists() {
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
        let port = |name: &str| {
            value[name]
                .as_u64()
                .and_then(|port| u16::try_from(port).ok())
                .filter(|port| *port > 0)
                .ok_or_else(|| format!("Invalid saved {name} service port"))
        };
        let ports = (port("backend")?, port("frontend")?);
        if value["version"] != 1 || ports.0 == ports.1 {
            return Err("Invalid saved desktop service ports".into());
        }
        return Ok(ports);
    }
    let backend = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
    let frontend = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
    let ports = (
        backend.local_addr().map_err(|e| e.to_string())?.port(),
        frontend.local_addr().map_err(|e| e.to_string())?.port(),
    );
    write_record(
        &path,
        &serde_json::json!({"version":1,"backend":ports.0,"frontend":ports.1}),
    )?;
    Ok(ports)
}
