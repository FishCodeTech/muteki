//! Minimal SRC build contexts travel inside both desktop and web service binaries.
use crate::AppState;
use sha2::{Digest,Sha256};
use std::{path::{Path,PathBuf},fs::File,sync::{Arc,OnceLock},collections::HashMap};
const EGRESS:&[(&str,&[u8])]=&[
    ("Dockerfile",include_bytes!("../../../docker/src-egress/Dockerfile")),
    ("gateway.py",include_bytes!("../../../docker/src-egress/gateway.py")),
    ("http_proxy.py",include_bytes!("../../../docker/src-egress/http_proxy.py")),
    ("proxy_process.py",include_bytes!("../../../docker/src-egress/proxy_process.py")),
];
const BROWSER:&[(&str,&[u8])]=&[("Dockerfile",include_bytes!("../../../docker/src-browser/Dockerfile"))];
fn files(kind:&str)->Result<&'static[(&'static str,&'static[u8])],String>{match kind{"src-egress"=>Ok(EGRESS),"src-browser"=>Ok(BROWSER),_=>Err("unknown SRC resource context".into())}}
pub(crate) fn digest(kind:&str)->String{let mut hash=Sha256::new();for(name,bytes)in files(kind).expect("known embedded SRC resource"){hash.update(name.as_bytes());hash.update([0]);hash.update((bytes.len() as u64).to_be_bytes());hash.update(bytes);}format!("{:x}",hash.finalize())}
pub(crate) fn image_digest(tag:&str)->String{digest(if tag.contains("egress"){"src-egress"}else{"src-browser"})}
fn directory(path:&Path)->Result<(),String>{
    for ancestor in path.ancestors(){if let Ok(metadata)=std::fs::symlink_metadata(ancestor){if metadata.file_type().is_symlink(){return Err(format!("SRC resource parent is a link: {}",ancestor.display()));}}}
    if let Ok(metadata)=std::fs::symlink_metadata(path){if metadata.file_type().is_symlink()||!metadata.is_dir(){return Err(format!("SRC resource path is not a private directory: {}",path.display()));}}
    std::fs::create_dir_all(path).map_err(|error|error.to_string())?;
    #[cfg(unix)]{use std::os::unix::fs::PermissionsExt;std::fs::set_permissions(path,std::fs::Permissions::from_mode(0o700)).map_err(|error|error.to_string())?;}Ok(())
}
fn state_root(s:&AppState)->Result<PathBuf,String>{s.store.get("system","state_dir").map_err(|error|error.to_string())?.and_then(|value|value.as_str().map(PathBuf::from)).ok_or("managed state directory unavailable".into())}
pub(crate) fn context(s:&AppState,kind:&str)->Result<PathBuf,String>{
    let root=state_root(s)?.join("src-resources");directory(&root)?;let group=root.join(kind);directory(&group)?;
    let mut options=std::fs::OpenOptions::new();options.create(true).write(true);#[cfg(unix)]{use std::os::unix::fs::OpenOptionsExt;options.mode(0o600).custom_flags(libc::O_NOFOLLOW);}
    let lock=options.open(group.join(".extract.lock")).map_err(|error|error.to_string())?;fs2::FileExt::lock_exclusive(&lock).map_err(|error|error.to_string())?;
    let path=group.join(digest(kind));directory(&path)?;
    for(name,bytes)in files(kind)?{let output=path.join(name);if let Ok(metadata)=std::fs::symlink_metadata(&output){if metadata.file_type().is_symlink()||!metadata.is_file(){return Err("SRC resource file was replaced by a non-file".into());}if std::fs::read(&output).map_err(|error|error.to_string())?==*bytes{continue;}}
        use std::io::Write;let mut file=tempfile::NamedTempFile::new_in(&path).map_err(|error|error.to_string())?;file.write_all(bytes).map_err(|error|error.to_string())?;#[cfg(unix)]{use std::os::unix::fs::PermissionsExt;file.as_file().set_permissions(std::fs::Permissions::from_mode(0o644)).map_err(|error|error.to_string())?;}file.as_file().sync_all().map_err(|error|error.to_string())?;file.persist(&output).map_err(|error|error.to_string())?;
    }File::open(&path).and_then(|file|file.sync_all()).map_err(|error|error.to_string())?;Ok(path)
}
pub(crate) struct ActivationGuard{_gate:tokio::sync::OwnedMutexGuard<()>,_file:File}
pub(crate) async fn activation(s:&AppState,task:&str)->Result<ActivationGuard,String>{
    static GATES:OnceLock<std::sync::Mutex<HashMap<String,std::sync::Weak<tokio::sync::Mutex<()>>>>>=OnceLock::new();
    let root=state_root(s)?.join("src-activation");directory(&root)?;let identity=format!("{}:{task}",root.display());
    let gate={let mut gates=GATES.get_or_init(||std::sync::Mutex::new(HashMap::new())).lock().unwrap_or_else(|error|error.into_inner());gates.retain(|_,value|value.strong_count()>0);if let Some(gate)=gates.get(&identity).and_then(std::sync::Weak::upgrade){gate}else{let gate=Arc::new(tokio::sync::Mutex::new(()));gates.insert(identity,Arc::downgrade(&gate));gate}};
    let gate=gate.lock_owned().await;let filename=format!("{:x}.lock",Sha256::digest(task.as_bytes()));let mut options=std::fs::OpenOptions::new();options.create(true).write(true);#[cfg(unix)]{use std::os::unix::fs::OpenOptionsExt;options.mode(0o600).custom_flags(libc::O_NOFOLLOW);}
    let file=options.open(root.join(filename)).map_err(|error|error.to_string())?;let deadline=tokio::time::Instant::now()+std::time::Duration::from_secs(30);
    loop{match fs2::FileExt::try_lock_exclusive(&file){Ok(())=>break,Err(error)if error.kind()==std::io::ErrorKind::WouldBlock&&tokio::time::Instant::now()<deadline=>tokio::time::sleep(std::time::Duration::from_millis(100)).await,Err(error)=>return Err(format!("SRC gateway activation is busy: {error}"))}}
    Ok(ActivationGuard{_gate:gate,_file:file})
}
