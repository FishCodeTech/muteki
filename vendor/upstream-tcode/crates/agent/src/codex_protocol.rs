use super::*;
#[derive(Clone,Default)]
pub(super) struct Protocol {pub methods:HashSet<String>,pub plan:bool,pub fork_at_turn:bool,pub additional_context:bool,pub steer_context:bool}
pub(super) async fn discover(opts:&SessionOptions)->Protocol {
    let opts=opts.clone();
    crate::process::unblock(move||{
        let root=std::env::temp_dir().join(format!("muteki-codex-schema-{}",uuid::Uuid::new_v4()));
        struct Cleanup(PathBuf);impl Drop for Cleanup{fn drop(&mut self){let _=std::fs::remove_dir_all(&self.0);}}
        let _cleanup=Cleanup(root.clone());
        if std::fs::create_dir(&root).is_err(){return Protocol::default();}
        #[cfg(unix)]{use std::os::unix::fs::PermissionsExt;let _=std::fs::set_permissions(&root,std::fs::Permissions::from_mode(0o700));}
        let Ok(binary)=crate::resolve_binary(opts.binary_path.as_deref(),"codex")else{return Protocol::default()};
        let experimental=opts.launch_env.env.iter().rev().find(|(key,_)|key=="MUTEKI_CODEX_EXPERIMENTAL_API").is_some_and(|(_,value)|matches!(value.as_str(),"1"|"true"));
        let mut cmd=crate::process::command(binary);cmd.args(["app-server","generate-json-schema","--out"]).arg(&root);
        if experimental { cmd.arg("--experimental"); }
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        for(key,value)in opts.launch_env.pairs(ProviderKind::Codex){cmd.env(key,value);}
        let Ok(mut child)=cmd.spawn()else{return Protocol::default()};
        let deadline=std::time::Instant::now()+std::time::Duration::from_secs(30);
        loop{match child.try_wait(){Ok(Some(status))if status.success()=>break,Ok(Some(_))|Err(_)=>return Protocol::default(),_=>{if std::time::Instant::now()>deadline{let _=child.kill();let _=child.wait();return Protocol::default();}std::thread::sleep(std::time::Duration::from_millis(20));}}}
        let read=|name:&str|std::fs::read(root.join(name)).ok().and_then(|bytes|serde_json::from_slice::<Value>(&bytes).ok()).unwrap_or(Value::Null);
        let doc=read("ClientRequest.json");let mut methods=HashSet::new();
        for variant in doc["oneOf"].as_array().or_else(||doc["anyOf"].as_array()).into_iter().flatten(){let method=&variant["properties"]["method"];if let Some(value)=method["const"].as_str(){methods.insert(value.into());}for value in method["enum"].as_array().into_iter().flatten().filter_map(Value::as_str){methods.insert(value.into());}}
        let turn=match read("TurnStartParams.json"){Value::Null=>read("v2/TurnStartParams.json"),v=>v};
        let steer=match read("TurnSteerParams.json"){Value::Null=>read("v2/TurnSteerParams.json"),v=>v};
        let fork=match read("ThreadForkParams.json"){Value::Null=>read("v2/ThreadForkParams.json"),v=>v};
        let fork_at_turn=methods.contains("thread/fork")&&fork["properties"].get("lastTurnId").is_some();
        Protocol{methods,fork_at_turn,steer_context:experimental&&steer["properties"].get("additionalContext").is_some(),additional_context:experimental&&turn["properties"].get("additionalContext").is_some(),plan:experimental&&(turn["properties"].get("collaborationMode").is_some()||turn["definitions"].get("CollaborationMode").is_some()||turn["$defs"].get("CollaborationMode").is_some())}
    }).await
}
