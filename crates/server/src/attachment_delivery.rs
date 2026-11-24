//! Native agents receive ordinary uploads as verified workspace files.
use agent::Attachment;
use base64::Engine;
use serde_json::{Value,json};
use sha2::{Digest,Sha256};
use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    io::Write,
    path::Path,
};

pub fn handoff_refs<'a>(
    messages: impl IntoIterator<Item = &'a Value>,
    inherited: impl Fn(&Value) -> bool,
) -> Vec<Value> {
    messages.into_iter()
        .filter(|message| message["attachments"].as_array().is_some_and(|items| !items.is_empty()))
        .map(|message| json!({
            "message_id": message["message_id"],
            "source_thread_id": if inherited(message) { &message["source_thread_id"] } else { &Value::Null },
            "attachments": message["attachments"],
        }))
        .collect()
}

pub(crate) fn handoff_files(
    state: &crate::AppState,
    document: &Value,
    handoff: &Value,
    current_refs: &Value,
) -> Result<Vec<(Value, Vec<u8>)>, String> {
    let mut seen: HashSet<&str> = current_refs.as_array().into_iter().flatten()
        .filter_map(Value::as_str).collect();
    let mut sources = HashMap::new();
    let mut files = Vec::new();
    for row in handoff["attachment_refs"].as_array().into_iter().flatten() {
        for reference in row["attachments"].as_array().into_iter().flatten() {
            let reference = reference.as_str().ok_or("Historical attachment reference is invalid")?;
            if !seen.insert(reference) {
                continue;
            }
            let source = if let Some(thread_id) = row["source_thread_id"].as_str() {
                match sources.entry(thread_id) {
                    Entry::Occupied(entry) => entry.into_mut(),
                    Entry::Vacant(entry) => entry.insert(
                        crate::chat::load(state, thread_id)
                            .map_err(|_| "Historical attachment source conversation is unavailable")?,
                    ),
                }
            } else {
                document
            };
            let (meta, bytes) = crate::files::read_attachment(state, source, reference)
                .map_err(|error| format!("Historical attachment {reference}: {error}"))?;
            files.push((meta, bytes));
        }
    }
    Ok(files)
}

pub fn prepare(cwd: &Path,files: Vec<(Value,Vec<u8>)>,image_input: bool) -> Result<(Vec<Attachment>,Vec<Value>),String> {
    if files.is_empty(){return Ok((Vec::new(),Vec::new()));}
    let cwd=cwd.canonicalize().map_err(|_|"Attachment workspace is unavailable")?;
    let parent=cwd.join(".muteki");let directory=parent.join("attachments");
    for path in [&parent,&directory] {
        if path.is_symlink(){return Err("Attachment staging directory is a symbolic link".into());}
        std::fs::create_dir_all(path).map_err(|_|"Cannot create attachment staging directory")?;
        #[cfg(unix)] {use std::os::unix::fs::PermissionsExt;std::fs::set_permissions(path,std::fs::Permissions::from_mode(0o700)).map_err(|_|"Cannot protect attachment staging directory")?;}
        if !path.canonicalize().map_err(|_|"Cannot resolve attachment staging directory")?.starts_with(&cwd){return Err("Attachment staging directory leaves the workspace".into());}
    }
    let mut delivery=Vec::new();
    let mut attachments=Vec::new();
    for (meta,bytes) in files {
        let reference=meta["artifact_id"].as_str().ok_or("Attachment reference missing")?;
        uuid::Uuid::parse_str(reference).map_err(|_|"Attachment reference invalid")?;
        let digest=format!("{:x}",Sha256::digest(&bytes));
        if meta["sha256"].as_str()!=Some(&digest) || meta["size"].as_u64()!=Some(bytes.len() as u64){return Err("Attachment integrity metadata does not match its bytes".into());}
        let name=meta["name"].as_str().unwrap_or("attachment");
        let safe=Path::new(name).file_name().unwrap_or_default().to_string_lossy().chars()
            .map(|character|if character.is_control() || matches!(character,'/'|'\\'|':'|'\0'){'_'}else{character}).take(100).collect::<String>();
        let path=directory.join(format!("{reference}-{}",if safe.is_empty(){"attachment"}else{&safe}));
        if path.is_symlink(){return Err("Attachment destination is a symbolic link".into());}
        let mut staged=tempfile::NamedTempFile::new_in(&directory).map_err(|_|"Cannot stage attachment")?;
        #[cfg(unix)] {use std::os::unix::fs::PermissionsExt;staged.as_file().set_permissions(std::fs::Permissions::from_mode(0o600)).map_err(|_|"Cannot protect attachment")?;}
        staged.write_all(&bytes).and_then(|_|staged.as_file().sync_all()).map_err(|_|"Cannot persist attachment")?;
        staged.persist(&path).map_err(|_|"Cannot publish attachment")?;
        let media=meta["media_type"].as_str().unwrap_or("application/octet-stream");
        let native=image_input && matches!(media,"image/png"|"image/jpeg"|"image/gif"|"image/webp");
        if native {
            let actual=crate::image_validation::mime(&bytes).map_err(|_|"Attachment image failed to decode")?;
            if actual!=media{return Err("Attachment image MIME does not match its content".into());}
        }
        attachments.push(Attachment {
            media_type: media.to_owned(),
            data_base64: if native { base64::engine::general_purpose::STANDARD.encode(&bytes) } else { String::new() },
            source_path: Some(path.to_str().ok_or("Attachment path is not UTF-8")?.to_owned()),
        });
        let entry=json!({"artifact_id":reference,"name":name,"media_type":media,"size":bytes.len(),"sha256":digest,
            "workspace_path":path,"delivery":if native{"native_image"}else{"workspace_file"}});
        let mut ledger=entry;ledger["key"]=json!(format!("attachment:{reference}"));ledger["kind"]=json!("attachment");
        ledger["trust"]=json!("user");ledger["scope"]=json!("turn");ledger["channel"]=json!(if native{"native_image"}else{"workspace_file"});
        delivery.push(ledger);
    }
    std::fs::File::open(&directory).and_then(|directory|directory.sync_all()).map_err(|_|"Cannot persist attachment directory")?;
    Ok((attachments,delivery))
}
