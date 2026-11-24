//! Resumable, scoped file staging. Async callers must run disk work on blocking workers.
mod secure;
mod snapshot;
#[cfg(test)]
mod tests;
use crate::store::Store;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

pub const CHUNK_BYTES: usize = 1024 * 1024;
const TABLE: &str = "runner_transfers";
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub node_id: String,
    pub project_id: String,
    pub run_id: String,
    pub generation: u64,
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub id: String,
    pub scope: Scope,
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub mode: u32,
}
impl Manifest {
    fn validate(&self) -> Result<(), String> {
        for id in [
            &self.id,
            &self.scope.node_id,
            &self.scope.project_id,
            &self.scope.run_id,
        ] {
            if id.is_empty()
                || id.len() > 160
                || !id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
            {
                return Err("Invalid transfer scope or identity".into());
            }
        }
        if self.scope.generation == 0
            || !digest_valid(&self.sha256)
            || !matches!(self.mode, 0o600 | 0o644 | 0o700 | 0o755)
        {
            return Err("Invalid transfer generation, digest or file mode".into());
        }
        secure::parts(&self.path)?;
        Ok(())
    }
}
fn digest_valid(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || matches!(c, b'a'..=b'f'))
}
pub fn validate_path(path: &str)->Result<(),String>{secure::parts(path).map(|_|())}
pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn file_digest(file: &mut File) -> Result<String, String> {
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let mut hash = Sha256::new();
    let mut block = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut block).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        hash.update(&block[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
pub struct Transfers {
    store: Store,
    root: PathBuf,
    quota: u64,
}
impl Transfers {
    pub fn manifest(&self, id: &str) -> Result<Manifest, String> {
        let row = self
            .store
            .get(TABLE, id)
            .map_err(|e| e.to_string())?
            .ok_or("Unknown transfer")?;
        serde_json::from_value(row["manifest"].clone()).map_err(|e| e.to_string())
    }
    pub fn open(store: Store, state_root: &Path, quota: u64) -> Result<Self, String> {
        let mut root = state_root.canonicalize().map_err(|e| e.to_string())?;
        for part in ["runner", "transfers"] {
            root.push(part);
            std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
            if !std::fs::symlink_metadata(&root)
                .map_err(|e| e.to_string())?
                .is_dir()
            {
                return Err("Transfer staging must be a private real directory".into());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(Self { store, root, quota })
    }
    fn path(&self, id: &str, extension: &str) -> PathBuf {
        self.root.join(format!("{id}.{extension}"))
    }
    fn lock(&self, id: &str) -> Result<File, String> {
        if id.is_empty()
            || id.len() > 160
            || !id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
        {
            return Err("Invalid transfer identity".into());
        }
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options
            .open(self.path(id, "lock"))
            .map_err(|e| e.to_string())?;
        file.lock_exclusive().map_err(|e| e.to_string())?;
        Ok(file)
    }
    fn record(&self, id: &str, scope: &Scope) -> Result<Value, String> {
        let row = self
            .store
            .get(TABLE, id)
            .map_err(|e| e.to_string())?
            .ok_or("Unknown transfer")?;
        let manifest: Manifest =
            serde_json::from_value(row["manifest"].clone()).map_err(|e| e.to_string())?;
        if &manifest.scope != scope {
            return Err("Transfer belongs to another execution scope".into());
        }
        Ok(row)
    }
    fn staging(&self, id: &str) -> Result<File, String> {
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options
            .open(self.path(id, "part"))
            .map_err(|e| e.to_string())?;
        if !file.metadata().map_err(|e| e.to_string())?.is_file() {
            return Err("Transfer staging is not a regular file".into());
        }
        Ok(file)
    }
    pub fn begin(&self, manifest: &Manifest) -> Result<Value, String> {
        manifest.validate()?;
        self.store.document_scope(||->Result<Value,String>{
            if let Some(row)=self.store.get(TABLE,&manifest.id).map_err(|e|e.to_string())?{
                let saved:Manifest=serde_json::from_value(row["manifest"].clone()).map_err(|e|e.to_string())?;
                if saved!=*manifest{return Err("Transfer ID was already used with another manifest".into());}return Ok(row);
            }
            let rows=self.store.list(TABLE).map_err(|e|e.to_string())?;
            let active:Vec<_>=rows.iter().filter(|r|r["staging_retained"]==true).collect();
            if active.len()>=4096{return Err("Too many unfinished transfers".into());}
            let used=active.into_iter().try_fold(0u64,|total,row|total.checked_add(row["manifest"]["size"].as_u64().unwrap_or(u64::MAX))).ok_or("Transfer quota accounting overflow")?;
            if manifest.size>self.quota.saturating_sub(used){return Err("Transfer staging quota exceeded; finish or cancel another transfer".into());}
            let row=json!({"manifest":manifest,"received":0,"state":"receiving","staging_retained":true});
            self.store.put(TABLE,&manifest.id,&row).map_err(|e|e.to_string())?;Ok(row)
        }).map_err(|e|e.to_string())??;
        let _lock = self.lock(&manifest.id)?;
        let row = self.record(&manifest.id, &manifest.scope)?;
        if row["state"] == "receiving" && row["received"] == 0 {
            let mut options = OpenOptions::new();
            options.create_new(true).read(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
            }
            match options.open(self.path(&manifest.id, "part")) {
                Ok(file) => {
                    file.sync_all().map_err(|e| e.to_string())?;
                    #[cfg(unix)]
                    File::open(&self.root)
                        .and_then(|root| root.sync_all())
                        .map_err(|e| e.to_string())?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    self.staging(&manifest.id)?;
                }
                Err(error) => return Err(error.to_string()),
            }
        }
        Ok(row)
    }
    pub fn status(&self, id: &str, scope: &Scope) -> Result<Value, String> {
        self.record(id, scope)?;
        let _lock = self.lock(id)?;
        self.record(id, scope)
    }
    pub fn seal(&self, id: &str, scope: &Scope) -> Result<Value, String> {
        self.record(id, scope)?;
        let _lock = self.lock(id)?;
        let mut row = self.record(id, scope)?;
        if !matches!(row["state"].as_str(), Some("receiving" | "verified")) {
            return Err("Transfer cannot be sealed in its current state".into());
        }
        let manifest = self.manifest(id)?;
        if row["received"] != manifest.size {
            return Err("Transfer is incomplete".into());
        }
        let mut file = self.staging(id)?;
        if file.metadata().map_err(|e| e.to_string())?.len() != manifest.size
            || file_digest(&mut file)? != manifest.sha256
        {
            return Err("Staged file does not match its manifest".into());
        }
        row["state"] = json!("verified");
        self.store.put(TABLE, id, &row).map_err(|e| e.to_string())?;
        Ok(row)
    }
    pub fn read_chunk(
        &self,
        id: &str,
        scope: &Scope,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>, String> {
        if length == 0 || length > CHUNK_BYTES {
            return Err("Invalid chunk length".into());
        }
        self.record(id, scope)?;
        let _lock = self.lock(id)?;
        let row = self.record(id, scope)?;
        if row["state"] != "verified" {
            return Err("Transfer is not verified and available for download".into());
        }
        let size = row["manifest"]["size"]
            .as_u64()
            .ok_or("Invalid transfer size")?;
        if offset > size {
            return Err("Chunk offset exceeds file size".into());
        }
        let mut file = self.staging(id)?;
        if file.metadata().map_err(|e| e.to_string())?.len() != size {
            return Err("Verified source size changed".into());
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| e.to_string())?;
        let mut bytes = vec![0; (size - offset).min(length as u64) as usize];
        file.read_exact(&mut bytes).map_err(|e| e.to_string())?;
        Ok(bytes)
    }
    pub fn chunk(
        &self,
        id: &str,
        scope: &Scope,
        offset: u64,
        bytes: &[u8],
        sha256: &str,
    ) -> Result<Value, String> {
        if bytes.is_empty() || bytes.len() > CHUNK_BYTES || digest(bytes) != sha256 {
            return Err("Chunk is empty, oversized or has an invalid digest".into());
        }
        self.record(id, scope)?;
        let _lock = self.lock(id)?;
        let mut row = self.record(id, scope)?;
        if row["state"] != "receiving" {
            return Err("Transfer is not receiving data".into());
        }
        let received = row["received"].as_u64().ok_or("Invalid transfer offset")?;
        let size = row["manifest"]["size"]
            .as_u64()
            .ok_or("Invalid manifest size")?;
        let end = offset
            .checked_add(bytes.len() as u64)
            .filter(|end| *end <= size)
            .ok_or("Chunk exceeds declared size")?;
        let mut file = self.staging(id)?;
        let length = file.metadata().map_err(|e| e.to_string())?.len();
        if length < received {
            return Err("Previously acknowledged transfer data is missing".into());
        }
        if length > received {
            file.set_len(received).map_err(|e| e.to_string())?;
        }
        if offset < received {
            if end > received {
                return Err("Chunk overlaps the acknowledged boundary".into());
            }
            file.seek(SeekFrom::Start(offset))
                .map_err(|e| e.to_string())?;
            let mut old = vec![0; bytes.len()];
            file.read_exact(&mut old).map_err(|e| e.to_string())?;
            if old != bytes {
                return Err("Repeated chunk differs from acknowledged data".into());
            }
            return Ok(row);
        }
        if offset != received {
            return Err("Chunk offset does not match the acknowledged cursor".into());
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| e.to_string())?;
        file.write_all(bytes).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        row["received"] = json!(end);
        self.store.put(TABLE, id, &row).map_err(|e| e.to_string())?;
        Ok(row)
    }
    pub fn commit(&self, id: &str, scope: &Scope, workspace: &Path) -> Result<Value, String> {
        self.commit_guarded(id, scope, workspace, || Ok(()))
    }
    pub fn commit_guarded<F, G>(
        &self,
        id: &str,
        scope: &Scope,
        workspace: &Path,
        authorize: F,
    ) -> Result<Value, String>
    where
        F: Fn() -> Result<G, String>,
    {
        self.record(id, scope)?;
        let _lock = self.lock(id)?;
        let mut row = self.record(id, scope)?;
        let manifest: Manifest =
            serde_json::from_value(row["manifest"].clone()).map_err(|e| e.to_string())?;
        if row["state"] == "cancelled" {
            return Err("Transfer was cancelled".into());
        }
        if row["state"] == "committed" {
            let _publication = authorize()?;
            return self.cleanup(id, row);
        }
        if row["received"] != manifest.size {
            return Err("Transfer is incomplete".into());
        }
        let mut file = self.staging(id)?;
        if file.metadata().map_err(|e| e.to_string())?.len() != manifest.size
            || file_digest(&mut file)? != manifest.sha256
        {
            return Err("Staged file does not match the manifest".into());
        }
        row["state"] = json!("verified");
        self.store.put(TABLE, id, &row).map_err(|e| e.to_string())?;
        secure::install_guarded(workspace, &manifest, &mut file, authorize)?;
        row["state"] = json!("committed");
        self.store.put(TABLE, id, &row).map_err(|e| e.to_string())?;
        drop(file);
        self.cleanup(id, row)
    }
    fn cleanup(&self, id: &str, mut row: Value) -> Result<Value, String> {
        if row["staging_retained"] == true {
            match std::fs::remove_file(self.path(id, "part")) {
                Ok(()) => row["staging_retained"] = json!(false),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    row["staging_retained"] = json!(false)
                }
                Err(_) => return Ok(row),
            }
            self.store.put(TABLE, id, &row).map_err(|e| e.to_string())?;
        }
        Ok(row)
    }
    pub fn cancel(&self, id: &str, scope: &Scope) -> Result<Value, String> {
        self.record(id, scope)?;
        let _lock = self.lock(id)?;
        let mut row = self.record(id, scope)?;
        if row["state"] == "committed" {
            return Err("Committed files cannot be deleted by cancelling a transfer".into());
        }
        if let Err(error) = std::fs::remove_file(self.path(id, "part"))
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(error.to_string());
        }
        row["state"] = json!("cancelled");
        row["staging_retained"] = json!(false);
        self.store.put(TABLE, id, &row).map_err(|e| e.to_string())?;
        Ok(row)
    }
}
