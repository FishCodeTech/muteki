//! Publish fully durable credential generations, then atomically publish their
//! database pointer. Recovery removes unpublished files and completes deletes.
use crate::{AppState,store::Store};
use anyhow::{Context,Result,bail};
use serde_json::{Value,json};
use sha2::{Digest,Sha256};
use std::{fs,io::Write,path::{Path,PathBuf},sync::Mutex};

static WRITES: Mutex<()> = Mutex::new(());
fn root(state_dir: &Path) -> PathBuf { state_dir.join("imported-credentials/accounts") }
fn directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; fs::set_permissions(path,fs::Permissions::from_mode(0o700))?; }
    Ok(())
}
fn write(path: &Path,bytes: &[u8]) -> Result<()> {
    directory(path.parent().context("Credential parent missing")?)?;
    let mut file = fs::OpenOptions::new().create_new(true).write(true).open(path)?;
    #[cfg(unix)] { use std::os::unix::fs::PermissionsExt; file.set_permissions(fs::Permissions::from_mode(0o600))?; }
    file.write_all(bytes)?; file.sync_all()?;
    Ok(())
}
fn lock(state_dir: &Path) -> Result<fs::File> {
    let root = root(state_dir); directory(&root)?;
    let mut options = fs::OpenOptions::new(); options.create(true).read(true).write(true);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600).custom_flags(libc::O_NOFOLLOW); }
    let file = options.open(root.join(".material.lock"))?;
    fs2::FileExt::lock_exclusive(&file)?; Ok(file)
}
pub(crate) fn base(state_dir: &Path,id: &str,row: &Value) -> Result<PathBuf> {
    if !crate::host_import::valid_account_id(id) { bail!("Invalid credential account id"); }
    let account = root(state_dir).join(id);
    if account.is_symlink() { bail!("Credential account directory is a symbolic link"); }
    if let Some(generation) = row["material_generation"].as_str() {
        if uuid::Uuid::parse_str(generation).is_err() { bail!("Invalid credential material generation"); }
        let path = account.join(".materials").join(generation);
        if path.is_symlink() || path.parent().is_some_and(Path::is_symlink) { bail!("Credential material directory is a symbolic link"); }
        return Ok(path);
    }
    Ok(account)
}
fn copy(source: &Path,target: &Path) -> Result<()> {
    if !source.exists() { return Ok(()); }
    for entry in walkdir::WalkDir::new(source).min_depth(1).follow_links(false).into_iter()
        .filter_entry(|entry|entry.file_name() != ".materials") {
        let entry = entry?;
        if entry.file_type().is_symlink() { bail!("Credential material contains a symbolic link"); }
        let path = target.join(entry.path().strip_prefix(source)?);
        if entry.file_type().is_dir() { directory(&path)?; }
        else if entry.file_type().is_file() { write(&path,&fs::read(entry.path())?)?; }
        else { bail!("Credential material requires regular files"); }
    }
    Ok(())
}
fn sync_tree(path: &Path) -> Result<()> {
    for entry in walkdir::WalkDir::new(path).contents_first(true).follow_links(false) {
        let entry = entry?; fs::File::open(entry.path())?.sync_all()?;
    }
    Ok(())
}
fn digest(path: &Path) -> Result<String> {
    let mut paths = walkdir::WalkDir::new(path).min_depth(1).follow_links(false).into_iter()
        .filter_map(|entry|match entry {Ok(entry) if entry.file_type().is_file()=>Some(Ok(entry.into_path())),Ok(_)=>None,Err(error)=>Some(Err(error))})
        .collect::<std::result::Result<Vec<_>,_>>()?;
    paths.sort();
    let mut hash = Sha256::new();
    for file in paths {
        let name = file.strip_prefix(path)?.to_string_lossy(); let bytes = fs::read(&file)?;
        hash.update((name.len() as u64).to_be_bytes()); hash.update(name.as_bytes());
        hash.update((bytes.len() as u64).to_be_bytes()); hash.update(bytes);
    }
    Ok(format!("{:x}",hash.finalize()))
}

fn recover_locked(store: &Store,state_dir: &Path) -> Result<()> {
    let transactions = root(state_dir).join(".transactions");
    if !transactions.exists() { return Ok(()); }
    for entry in fs::read_dir(&transactions)? {
        let entry = entry?; let dir = entry.path();
        if !entry.file_type()?.is_dir() || dir.is_symlink() { bail!("Invalid credential transaction directory"); }
        let journal = dir.join("transaction.json");
        if !journal.exists() { fs::remove_dir_all(&dir)?; continue; }
        let job: Value = serde_json::from_slice(&fs::read(&journal)?)?;
        let id = job["account_id"].as_str().context("Missing credential transaction identity")?;
        if !crate::host_import::valid_account_id(id) { bail!("Invalid credential transaction identity"); }
        let current = store.get("credential_accounts",id)?;
        if job["op"] == "publish" {
            let generation = job["generation"].as_str().context("Missing material generation")?;
            if uuid::Uuid::parse_str(generation).is_err() { bail!("Invalid material generation"); }
            if current.as_ref().is_none_or(|row|row["material_generation"] != generation) {
                let material = root(state_dir).join(id).join(".materials").join(generation);
                if material.exists() { fs::remove_dir_all(material)?; }
            }
        } else if job["op"] == "delete" {
            let previous = dir.join("previous"); let live = root(state_dir).join(id);
            if current.is_some() && previous.exists() {
                if live.exists() { bail!("Credential delete recovery found a conflicting live directory"); }
                fs::rename(previous,live)?; fs::File::open(root(state_dir))?.sync_all()?;
            }
            let previous_runtime = dir.join("runtime-previous");
            let runtime = state_dir.join("runtime-credentials").join(id);
            if current.is_some() && previous_runtime.exists() {
                if runtime.exists() { bail!("Credential delete recovery found a conflicting runtime directory"); }
                fs::rename(previous_runtime,runtime)?;
            }
        } else { bail!("Unknown credential material transaction"); }
        fs::remove_dir_all(dir)?;
    }
    fs::File::open(&transactions)?.sync_all()?;
    Ok(())
}
pub fn recover(store: &Store,state_dir: &Path) -> Result<()> {
    let _guard = WRITES.lock().map_err(|_|anyhow::anyhow!("Credential material lock poisoned"))?;
    let _file = lock(state_dir)?; recover_locked(store,state_dir)
}

pub(crate) fn save(state: &AppState,id: &str,mut row: Value,create_only: bool,expected: Option<&str>,carry: bool,replacement: Option<&Path>)
    -> Result<std::result::Result<Value,()>> {
    if !crate::host_import::valid_account_id(id) { bail!("Invalid credential account id"); }
    let state_dir = crate::native_chat::state_dir(state);
    let _guard = WRITES.lock().map_err(|_|anyhow::anyhow!("Credential material lock poisoned"))?;
    let _file = lock(&state_dir)?; recover_locked(&state.store,&state_dir)?;
    let previous = state.store.get("credential_accounts",id)?;
    let auth = row["codex_auth_json"].as_str().filter(|value|!value.trim().is_empty()).map(str::to_owned);
    let mut transaction = None;
    if replacement.is_some() || auth.is_some() {
        let generation = uuid::Uuid::new_v4().to_string();
        let transactions = root(&state_dir).join(".transactions"); directory(&transactions)?;
        let job = transactions.join(&generation); directory(&job)?;
        let staged = job.join("material"); directory(&staged)?;
        if let Some(source) = replacement { copy(source,&staged)?; }
        else if let Some(previous) = &previous { copy(&base(&state_dir,id,previous)?,&staged)?; }
        if let Some(auth) = auth {
            let target = staged.join("codex-home/auth.json");
            if target.exists() { fs::remove_file(&target)?; }
            write(&target,format!("{}\n",auth.trim()).as_bytes())?;
        }
        row["material_digest"] = json!(digest(&staged)?);
        if previous.as_ref().is_some_and(|old|old["material_digest"] == row["material_digest"]) {
            row["material_generation"] = previous.as_ref().unwrap()["material_generation"].clone();
            fs::remove_dir_all(&job)?;
        } else {
        sync_tree(&staged)?;
        write(&job.join("transaction.json"),&serde_json::to_vec(&json!({"op":"publish","account_id":id,"generation":generation}))?)?;
        fs::File::open(&job)?.sync_all()?; fs::File::open(&transactions)?.sync_all()?;
        let parent = root(&state_dir).join(id).join(".materials"); directory(&parent)?;
        fs::File::open(parent.parent().unwrap())?.sync_all()?;
        fs::rename(staged,parent.join(&generation))?; fs::File::open(&parent)?.sync_all()?;
        row["material_generation"] = json!(generation);
        transaction = Some(job);
        }
    } else if carry {
        if let Some(previous) = &previous {
            for field in ["material_generation","material_digest","imported_home","import_source"] {
                if let Some(value) = previous.get(field) { row[field] = value.clone(); }
            }
        }
    }
    let result = state.store.write_account(id,row,create_only,expected,carry);
    if result.as_ref().is_ok_and(|value|value.is_ok()) {
        if let Some(job) = transaction {
            if let Err(error) = fs::remove_dir_all(job) { tracing::warn!(kind=?error.kind(),"Credential publish committed; transaction cleanup will resume"); }
        }
    } else { recover_locked(&state.store,&state_dir)?; }
    Ok(result?)
}

pub(crate) fn delete(state: &AppState,id: &str,detach: bool) -> Result<std::result::Result<Option<usize>,Vec<Value>>> {
    let account_id = id.strip_prefix("endpoint:").unwrap_or(id);
    if !crate::host_import::valid_account_id(account_id) { bail!("Invalid credential account id"); }
    let state_dir = crate::native_chat::state_dir(state);
    let _guard = WRITES.lock().map_err(|_|anyhow::anyhow!("Credential material lock poisoned"))?;
    let _file = lock(&state_dir)?; recover_locked(&state.store,&state_dir)?;
    let result = state.store.delete_account_with_material(id,detach,|remove| {
        if !remove { return Ok(()); }
        let prepare = || -> Result<()> {
            let transactions = root(&state_dir).join(".transactions"); directory(&transactions)?;
            let job = transactions.join(uuid::Uuid::new_v4().to_string()); directory(&job)?;
            write(&job.join("transaction.json"),&serde_json::to_vec(&json!({"op":"delete","account_id":account_id}))?)?;
            fs::File::open(&job)?.sync_all()?; fs::File::open(&transactions)?.sync_all()?;
            let live = root(&state_dir).join(account_id);
            if live.is_symlink() { bail!("Credential account directory is a symbolic link"); }
            if live.exists() { fs::rename(live,job.join("previous"))?; }
            let runtime = state_dir.join("runtime-credentials").join(account_id);
            if runtime.is_symlink() { bail!("Credential runtime directory is a symbolic link"); }
            if runtime.exists() { fs::rename(&runtime,job.join("runtime-previous"))?; fs::File::open(runtime.parent().unwrap())?.sync_all()?; }
            fs::File::open(root(&state_dir))?.sync_all()?;
            Ok(())
        };
        prepare().map_err(|error|rusqlite::Error::ToSqlConversionFailure(error.into()))
    });
    let result=result?;
    if result.is_ok(){if let Err(error)=recover_locked(&state.store,&state_dir){tracing::warn!(account_id,%error,"Deleted credential material cleanup remains in its recovery journal");}}else{recover_locked(&state.store,&state_dir)?;}
    Ok(result)
}
