//! Ordinary Run adapter. Controller owns input metadata; the node owns execution and output bytes.
use super::{
    broker::Registry,
    commands::{Command, Operation},
    credentials::{Material, Vault},
    transfers::{self, Scope, Transfers},
};
use crate::{
    AppState,
    runs::container_exec::{ContainerHandle, SupervisorFrame, WorkerSpec},
    store::Store,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
const QUOTA: u64 = 4 * 1024 * 1024 * 1024;
#[derive(Clone)]
pub struct Remote {
    pub store: Store,
    registry: Arc<Registry>,
    pub root: PathBuf,
    pub workspace: PathBuf,
    pub container_workspace: String,
    pub scope: Scope,
    policy: u64,
}
fn identity(label: &str, value: Value) -> Result<String, String> {
    Ok(format!(
        "{label}-{}",
        crate::runner::canonical_digest(value)?
            .rsplit(':')
            .next()
            .unwrap()
    ))
}
pub fn material_key(engine: &str, credential: &str, model: Option<&str>) -> String {
    json!([engine, credential, model]).to_string()
}
pub fn material_id(run: &str, generation: u64, key: &str) -> Result<String, String> {
    identity("material", json!([run, generation, key]))
}
pub fn selected(settings: &Value) -> Option<&str> {
    settings["worker_node_id"]
        .as_str()
        .filter(|node| !node.is_empty() && *node != "local")
}
impl Remote {
    pub async fn request(&self, action: Operation, id: Option<String>) -> Result<Value, String> {
        let command = Command {
            id: id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            node_id: self.scope.node_id.clone(),
            project_id: self.scope.project_id.clone(),
            run_id: self.scope.run_id.clone(),
            generation: self.scope.generation,
            session_generation: 0,
            credential_version: 0,
            policy_version: self.policy,
            expires_at: chrono::Utc::now().timestamp() + 90,
            action,
        };
        self.registry
            .request_stored(&self.store, &self.root, command)
            .await
    }
    pub async fn control(&self, payload: Value) -> Result<SupervisorFrame, String> {
        let worker = || {
            payload["worker_id"]
                .as_str()
                .map(str::to_owned)
                .ok_or("Worker identity missing".to_owned())
        };
        let action = match payload["op"].as_str() {
            Some("Health") => Operation::Health,
            Some("Signal") => Operation::Signal {
                worker_id: worker()?,
                signal: payload["signal"].as_str().unwrap_or("").into(),
            },
            Some("WriteStdin") => Operation::Stdin {
                worker_id: worker()?,
                data: payload["data"].as_str().unwrap_or("").into(),
            },
            Some("TeardownRun") => {
                self.stop().await?;
                return Ok(SupervisorFrame {
                    t: "resp".into(),
                    ok: true,
                    scoped_teardown: true,
                    ..Default::default()
                });
            }
            _ => return Err("Unsupported remote supervisor operation".into()),
        };
        serde_json::from_value(self.request(action, None).await?).map_err(|e| e.to_string())
    }
    pub async fn stop(&self) -> Result<(), String> {
        let binding_key=format!("{}:{}",self.scope.run_id,self.scope.generation);
        if let Some(mut binding)=self.store.get("runner_controller_runs",&binding_key).map_err(|e|e.to_string())?{
            binding["stop_seq"]=json!(binding["stop_seq"].as_u64().unwrap_or(0)+1);
            binding["state"]=json!("stopping");binding["publication_pending"]=json!(true);
            self.store.put("runner_controller_runs",&binding_key,&binding).map_err(|e|e.to_string())?;
        }
        Vault::open(self.store.clone(), &self.root)?.revoke_scope(&self.scope)?;
        let result = self
            .request(Operation::Stop, Some(identity("stop", json!(self.scope))?))
            .await?;
        if result["exit_confirmed"] != true {
            return Err("Remote stop is not confirmed".into());
        }
        let key = format!("{}:{}", self.scope.run_id, self.scope.generation);
        if let Some(mut row) = self
            .store
            .get("runner_controller_runs", &key)
            .map_err(|e| e.to_string())?
        {
            row["state"] = json!("stopped");
            self.store
                .put("runner_controller_runs", &key, &row)
                .map_err(|e| e.to_string())?;
        }
        self.confirm_stopped()?;
        let projection=self.root.join("runner/remote-projections").join(&self.scope.run_id);
        crate::private_files::remove_tree(&self.root,&projection)?;
        self.finish_artifacts().await
    }
    async fn finish_artifacts(&self)->Result<(),String>{
        let result=tokio::time::timeout(Duration::from_secs(30),self.collect_outputs(None)).await
            .map_err(|_|"Execution stopped; artifact publication is pending".to_owned()).and_then(|result|result);
        let key=format!("{}:{}",self.scope.run_id,self.scope.generation);
        if let Some(mut row)=self.store.get("runner_controller_runs",&key).map_err(|e|e.to_string())?{
            row["publication_pending"]=json!(result.is_err());row["publication_error"]=json!(result.as_ref().err());
            self.store.put("runner_controller_runs",&key,&row).map_err(|e|e.to_string())?;
        }
        result
    }
    fn confirm_stopped(&self) -> Result<(), String> {
        self.store
            .document_scope(|| -> Result<(), String> {
                for mut row in self
                    .store
                    .list("runner_commands")
                    .map_err(|e| e.to_string())?
                {
                    if row["run_id"] == self.scope.run_id
                        && row["generation"] == self.scope.generation
                        && row["node_id"] == self.scope.node_id
                    {
                        let id = row["id"]
                            .as_str()
                            .ok_or("Remote command ID missing")?
                            .to_owned();
                        row["execution_stopped"] = json!(true);
                        if !row["exit"].is_object() {
                            row["state"] = json!("outcome_unknown");
                        }
                        self.store
                            .put("runner_commands", &id, &row)
                            .map_err(|e| e.to_string())?;
                        crate::runner_resources::release_completed(&self.store, &id)?;
                    }
                }
                Ok(())
            })
            .map_err(|e| e.to_string())?
    }
    fn local_files(&self) -> Result<Vec<(String, Vec<u8>, u32)>, String> {
        fn alias(
            root: &Path,
            target: &Path,
            prefix: &str,
            depth: usize,
            rows: &mut Vec<(String, Vec<u8>, u32)>,
        ) -> Result<(), String> {
            if depth > 16 || rows.len() > 4096 {
                return Err("Skill link expansion exceeds its bound".into());
            }
            let target = target.canonicalize().map_err(|e| e.to_string())?;
            if !target.starts_with(root) {
                return Err("Skill link leaves the Run workspace".into());
            }
            if target.is_dir() {
                for entry in std::fs::read_dir(&target).map_err(|e| e.to_string())? {
                    let entry = entry.map_err(|e| e.to_string())?;
                    let name = entry
                        .file_name()
                        .into_string()
                        .map_err(|_| "Invalid skill path")?;
                    alias(
                        root,
                        &entry.path(),
                        &format!("{prefix}/{name}"),
                        depth + 1,
                        rows,
                    )?;
                }
            } else if !prefix.ends_with("/muteki-blackboard/env.json") {
                rows.push((
                    prefix.into(),
                    crate::private_files::read(root, &target, 256 * 1024)?,
                    0o644,
                ));
            }
            Ok(())
        }
        fn visit(
            root: &Path,
            dir: &Path,
            rows: &mut Vec<(String, Vec<u8>, u32)>,
        ) -> Result<(), String> {
            for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                let path = entry.path();
                let rel = path
                    .strip_prefix(root)
                    .map_err(|e| e.to_string())?
                    .to_str()
                    .ok_or("Non-UTF8 input path")?
                    .replace('\\', "/");
                if transfers::validate_path(&rel).is_err()
                    || controller_owned(&rel)
                    || rel.starts_with(".muteki-blackboard-ingress/")
                    || rel.starts_with(".muteki-browser/home/")
                    || rel.starts_with(".muteki-agent-state/")
                    || rel == ".muteki-skills/muteki-blackboard/env.json"
                {
                    continue;
                }
                let meta = entry.file_type().map_err(|e| e.to_string())?;
                if meta.is_dir() {
                    visit(root, &path, rows)?;
                } else if meta.is_file() {
                    let bytes = crate::private_files::read(root, &path, 64 * 1024 * 1024)?;
                    #[cfg(unix)]
                    let mode = {
                        use std::os::unix::fs::PermissionsExt;
                        if path
                            .metadata()
                            .map_err(|e| e.to_string())?
                            .permissions()
                            .mode()
                            & 0o111
                            != 0
                        {
                            0o755
                        } else {
                            0o644
                        }
                    };
                    #[cfg(not(unix))]
                    let mode = 0o644;
                    rows.push((rel, bytes, mode));
                } else if meta.is_symlink() {
                    if rel == ".muteki-browser/bin/agent-browser" {
                        rows.push((
                            rel,
                            include_bytes!("../../../../docker/worker-common/browser_coord.py")
                                .to_vec(),
                            0o755,
                        ));
                        continue;
                    }
                    if let Ok(target) = path.canonicalize() {
                        if target.starts_with(root) {
                            alias(root, &target, &rel, 0, rows)?;
                        }
                    }
                }
                if rows.len() > 4096 {
                    return Err("Run input manifest exceeds 4096 files".into());
                }
                if rows.iter().map(|(_, bytes, _)| bytes.len()).sum::<usize>() > 128 * 1024 * 1024 {
                    return Err(
                        "Input staging exceeds 128 MiB; transfer large attachments explicitly"
                            .into(),
                    );
                }
            }
            Ok(())
        }
        let mut rows = Vec::new();
        visit(&self.workspace, &self.workspace, &mut rows)?;
        Ok(rows)
    }
    pub async fn sync_inputs(&self) -> Result<(), String> {
        let remote = self
            .request(
                Operation::WorkspaceList {
                    prefix: String::new(),
                },
                None,
            )
            .await?;
        let clone = self.clone();
        let rows = tokio::task::spawn_blocking(move || clone.local_files())
            .await
            .map_err(|e| e.to_string())??;
        for (path, bytes, mode) in rows {
            let key = identity(
                "input",
                json!([
                    self.scope.node_id,
                    self.scope.run_id,
                    self.scope.generation,
                    path
                ]),
            )?;
            if self
                .store
                .get("runner_controller_outputs", &key)
                .map_err(|e| e.to_string())?
                .is_some()
            {
                continue;
            }
            let hash = transfers::digest(&bytes);
            let prior = self
                .store
                .get("runner_controller_inputs", &key)
                .map_err(|e| e.to_string())?;
            if prior.as_ref().is_some_and(|row| row["sha256"] == hash) {
                continue;
            }
            let existing = remote["files"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|row| row["path"] == path);
            if existing.is_some_and(|row| row["sha256"] == hash) {
                self.store
                    .put(
                        "runner_controller_inputs",
                        &key,
                        &json!({"path":path,"sha256":hash}),
                    )
                    .map_err(|e| e.to_string())?;
                continue;
            }
            let expected = if let Some(row) = existing {
                let old = row["sha256"].as_str().ok_or("Remote file hash missing")?;
                if !prior.as_ref().is_some_and(|p| p["sha256"] == old)
                    && !(matches!(path.as_str(), "AGENTS.md" | "CLAUDE.md")
                        && row["managed_doc"] == true)
                    && !controller_published(&path)
                {
                    return Err(format!(
                        "Remote input conflict at {path}; node content was preserved"
                    ));
                }
                Some(old.to_owned())
            } else {
                None
            };
            if bytes.len() <= 256 * 1024 {
                self.request(
                    Operation::WorkspaceWrite {
                        path: path.clone(),
                        data: STANDARD.encode(&bytes),
                        expected_sha256: expected,
                        mode,
                    },
                    Some(identity("write", json!([key, hash]))?),
                )
                .await?;
            } else {
                if expected.is_some() {
                    return Err("Changed bulk input requires an explicit new path".into());
                }
                let id = identity("input-file", json!([key, hash]))?;
                let scope = self.scope.clone();
                let root = self.root.clone();
                let store = self.store.clone();
                let file_path = path.clone();
                let transfer = id.clone();
                let manifest_hash = hash.clone();
                tokio::task::spawn_blocking(move || -> Result<(), String> {
                    let files = Transfers::open(store, &root, QUOTA)?;
                    let manifest = transfers::Manifest {
                        id: transfer.clone(),
                        scope: scope.clone(),
                        path: file_path,
                        size: bytes.len() as u64,
                        sha256: manifest_hash,
                        mode,
                    };
                    let mut offset =
                        files.begin(&manifest)?["received"].as_u64().unwrap_or(0) as usize;
                    while offset < bytes.len() {
                        let end = (offset + transfers::CHUNK_BYTES).min(bytes.len());
                        let chunk = &bytes[offset..end];
                        files.chunk(
                            &transfer,
                            &scope,
                            offset as u64,
                            chunk,
                            &transfers::digest(chunk),
                        )?;
                        offset = end;
                    }
                    files.seal(&transfer, &scope)?;
                    Ok(())
                })
                .await
                .map_err(|e| e.to_string())??;
                self.request(
                    Operation::TransferPull {
                        transfer_id: id.clone(),
                    },
                    Some(identity("pull", json!(id))?),
                )
                .await?;
                self.wait_transfer(&id).await?;
            }
            self.store
                .put(
                    "runner_controller_inputs",
                    &key,
                    &json!({"path":path,"sha256":hash}),
                )
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
    async fn wait_transfer(&self, id: &str) -> Result<(), String> {
        let until = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            let row = self
                .request(
                    Operation::TransferStatus {
                        transfer_id: id.into(),
                    },
                    None,
                )
                .await?;
            if row["state"] == "committed" {
                return Ok(());
            }
            if matches!(
                row["state"].as_str(),
                Some("failed" | "cancelled" | "interrupted")
            ) {
                return Err(format!("Remote transfer did not commit: {row}"));
            }
            if tokio::time::Instant::now() > until {
                return Err("Remote transfer is pending; inspect its durable job".into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    pub async fn collect_outputs(&self, solver: Option<&str>) -> Result<(), String> {
        let archive=match self.store.get("runner_controller_runs",&format!("{}:{}",self.scope.run_id,self.scope.generation)).map_err(|e|e.to_string())?{
            Some(row) if row["state"]=="stopped" => json!(["stopped",row["stop_seq"].as_u64().unwrap_or(0)]),
            _ => json!(false),
        };
        let inventory = self
            .request(
                Operation::WorkspaceList {
                    prefix: String::new(),
                },
                None,
            )
            .await?;
        for row in inventory["files"].as_array().into_iter().flatten() {
            let path = row["path"].as_str().ok_or("Artifact path missing")?;
            if let Some(solver) = solver {
                if path.starts_with("workers/") && !path.starts_with(&format!("workers/{solver}/"))
                {
                    continue;
                }
            }
            if path.starts_with(".muteki-")
                || path.starts_with(".agents/")
                || path.starts_with(".claude/")
                || blackboard_skill_alias(path)
                || controller_owned(path)
                || controller_published(path)
            {
                continue;
            }
            let key = identity(
                "input",
                json!([
                    self.scope.node_id,
                    self.scope.run_id,
                    self.scope.generation,
                    path
                ]),
            )?;
            let mut known = ["runner_controller_inputs", "runner_controller_outputs"].into_iter();
            if known.try_fold(false, |seen, table| {
                Ok::<_, String>(seen || self.store.get(table, &key).map_err(|e| e.to_string())?
                    .is_some_and(|prior| prior["sha256"] == row["sha256"]))
            })? {
                continue;
            }
            let id = identity("output", json!([key, row["sha256"],archive]))?;
            self.request(
                Operation::TransferPush {
                    transfer_id: id.clone(),
                    path: path.into(),
                    max_bytes: 64 * 1024 * 1024,
                },
                Some(identity("push", json!(id))?),
            )
            .await?;
            self.wait_transfer(&id).await?;
            let clone = self.clone();
            let output = row.clone();
            let path = path.to_owned();
            let id2 = id.clone();
            tokio::task::spawn_blocking(move||->Result<(),String>{
                let files=Transfers::open(clone.store.clone(),&clone.root,QUOTA)?;let manifest=files.manifest(&id2)?;
                super::files::verified_source_store(&clone.store,&manifest)?;
                let mut bytes=Vec::new();while (bytes.len() as u64)<manifest.size{bytes.extend(files.read_chunk(&id2,&clone.scope,bytes.len() as u64,transfers::CHUNK_BYTES)?);}
                if transfers::digest(&bytes)!=output["sha256"]{return Err("Published output differs from inventory".into());}
                let destination=clone.workspace.join(&path);
                if destination.symlink_metadata().is_ok(){
                    let current=crate::private_files::read(&clone.workspace,&destination,64*1024*1024)?;
                    let hash=transfers::digest(&current);
                    let prior=clone.store.get("runner_controller_outputs",&key).map_err(|e|e.to_string())?
                        .or(clone.store.get("runner_controller_inputs",&key).map_err(|e|e.to_string())?);
                    if hash!=output["sha256"]&&!prior.is_some_and(|row|row["sha256"]==hash){return Err(format!("Controller output conflict at {path}; both versions preserved in the transfer store"));}
                }
                crate::private_files::write_mode(&clone.workspace,&destination,&bytes,0o644)?;
                clone.store.put("runner_controller_outputs",&key,&json!({"path":path,"sha256":output["sha256"],"transfer_id":id2})).map_err(|e|e.to_string())?;Ok(())
            }).await.map_err(|e|e.to_string())??;
        }
        Ok(())
    }
    pub async fn credentials(
        &self,
        solver: &str,
        engine: &str,
        credential: &str,
        model: Option<&str>,
        home: &Path,
        env: &mut HashMap<String, String>,
    ) -> Result<(), String> {
        let material = material_id(
            &self.scope.run_id,
            self.scope.generation,
            &material_key(engine, credential, model),
        )?;
        let key = format!("{}:{}:{solver}", self.scope.run_id, self.scope.generation);
        if let Some(prior) = self
            .store
            .get("runner_controller_credentials", &key)
            .map_err(|e| e.to_string())?
        {
            let old = env.get("HOME").cloned().unwrap_or_default();
            let new = prior["env"]["HOME"]
                .as_str()
                .ok_or("Credential home missing")?;
            for value in env.values_mut() {
                if !old.is_empty() && value.starts_with(&old) {
                    *value = value.replacen(&old, new, 1);
                }
            }
            strip_secret_env(env);
            for (name, value) in prior["env"]
                .as_object()
                .ok_or("Credential environment missing")?
            {
                env.insert(
                    name.clone(),
                    value.as_str().ok_or("Invalid environment path")?.into(),
                );
            }
            return Ok(());
        }
        let mut bundle = Material {
            kind: "model".into(),
            files: BTreeMap::new(),
            env_files: BTreeMap::new(),
            env_dirs: BTreeMap::new(),
        };
        for path in [
            ".claude.json",
            ".claude/.credentials.json",
            ".claude/settings.json",
            ".codex/auth.json",
            ".codex/config.toml",
            ".pi/agent/auth.json",
            ".pi/agent/models.json",
            ".pi/agent/settings.json",
            ".omp/agent/auth.json",
            ".omp/agent/models.json",
            ".omp/agent/models.yml",
            ".omp/agent/settings.json",
            ".kimi-code/config.toml",
            ".kimi-code/credentials.json",
            ".kimi-code/credentials/kimi-code.json",
            ".kimi-code/device_id",
            ".kimi-code/oauth/kimi-code",
            ".grok/config.json",
            ".grok/config.toml",
            ".grok/auth.json",
            ".factory/auth.json",
            ".local/share/opencode/auth.json",
            ".config/opencode/opencode.json",
            ".config/opencode/opencode.jsonc",
            ".config/opencode/selected.jsonc",
        ] {
            let file = home.join(path);
            if file.exists() {
                bundle.files.insert(
                    path.into(),
                    STANDARD.encode(crate::private_files::read(&self.root, &file, 256 * 1024)?),
                );
            }
        }
        for key in SECRET_ENV {
            if let Some(value) = env.get(*key).filter(|value| !value.is_empty()) {
                let path = format!("keys/{key}");
                bundle.files.insert(path.clone(), STANDARD.encode(value));
                bundle.env_files.insert(format!("{key}_FILE"), path);
            }
        }
        let vault = Vault::open(self.store.clone(), &self.root)?;
        let existing = vault.metadata(&material).ok();
        if existing.is_none() {
            if let Err(error) = vault.put(&material, None, &bundle) {
                if vault.metadata(&material).is_err() {
                    return Err(error);
                }
            }
        }
        let grant = vault.grant(
            &material,
            self.scope.clone(),
            86400,
            chrono::Utc::now().timestamp(),
        )?;
        let installed = self
            .request(
                Operation::CredentialInstall {
                    grant_id: grant.id.clone(),
                    revision: grant.revision.clone(),
                },
                Some(identity(
                    "credential",
                    json!([self.scope, solver, grant.id]),
                )?),
            )
            .await?;
        let old = env.get("HOME").cloned().unwrap_or_default();
        let new = installed["env"]["HOME"]
            .as_str()
            .ok_or("Remote credential home missing")?;
        for value in env.values_mut() {
            if !old.is_empty() && value.starts_with(&old) {
                *value = value.replacen(&old, new, 1);
            }
        }
        strip_secret_env(env);
        for (name, value) in installed["env"]
            .as_object()
            .ok_or("Remote credential environment missing")?
        {
            env.insert(
                name.clone(),
                value.as_str().ok_or("Invalid environment path")?.into(),
            );
        }
        self.store.put("runner_controller_credentials",&key,&json!({"run_id":self.scope.run_id,"generation":self.scope.generation,"grant_id":grant.id,"env":installed["env"]})).map_err(|e|e.to_string())?;
        Ok(())
    }
    pub async fn start(
        &self,
        spec: &WorkerSpec,
    ) -> Result<
        (
            String,
            crate::runs::supervisor_stream::Stream,
        ),
        String,
    > {
        self.sync_inputs().await?;
        let mut node_spec = spec.clone();
        node_spec.reservation_id = None;
        node_spec.owner_run_id = None;
        node_spec.owner_token = None;
        node_spec.owner_workspace = None;
        let id = identity("start", json!([self.scope, spec]))?;
        let digest = crate::runner::canonical_digest(
            json!({"run_id":self.scope.run_id,"generation":self.scope.generation,"spec":node_spec}),
        )?;
        self.store.document_scope(||->Result<(),String>{
            if self.store.get("runner_commands",&id).map_err(|e|e.to_string())?.is_none(){
                self.store.put("runner_commands",&id,&json!({"id":id,"run_id":self.scope.run_id,"generation":self.scope.generation,"solver_id":spec.tag,"digest":digest,"state":"dispatching","node_id":self.scope.node_id})).map_err(|e|e.to_string())?;
            }
            if let Some(reservation)=spec.reservation_id.as_deref(){let mut row=self.store.get("runner_dispatches",reservation).map_err(|e|e.to_string())?.ok_or("Controller reservation missing")?;
                if row["status"]!="running"||row["run_id"]!=self.scope.run_id||row["worker_id"].as_str()!=spec.tag.as_deref(){return Err("Controller reservation does not authorize remote start".into());}
                if let Some(previous)=row["runner_command_id"].as_str().filter(|prior|*prior!=id){if !self.store.get("runner_commands",previous).map_err(|e|e.to_string())?.is_some_and(|row|row["exit"].is_object()){return Err("Prior remote Worker exit is unconfirmed".into());}}
                row["runner_command_id"]=json!(id);self.store.put("runner_dispatches",reservation,&row).map_err(|e|e.to_string())?;
            }Ok(())
        }).map_err(|e|e.to_string())??;
        let value = match self
            .request(Operation::Start { spec: node_spec }, Some(id.clone()))
            .await
        {
            Ok(value) => value,
            Err(error) => {
                let receipt = self
                    .request(
                        Operation::Receipt {
                            command_id: id.clone(),
                        },
                        None,
                    )
                    .await?;
                if receipt["state"] != "started"
                    || receipt["digest"] != digest
                    || receipt["run_id"] != self.scope.run_id
                    || receipt["generation"] != self.scope.generation
                {
                    return Err(format!("Remote Start outcome unconfirmed: {error}"));
                }
                receipt["result"].clone()
            }
        };
        let worker = value["worker_id"]
            .as_str()
            .ok_or("Remote Worker identity missing")?
            .to_owned();
        let mut receipt = self
            .store
            .get("runner_commands", &id)
            .map_err(|e| e.to_string())?
            .ok_or("Controller receipt missing")?;
        receipt["state"] = json!("started");
        receipt["result"] = value;
        self.store
            .put("runner_commands", &id, &receipt)
            .map_err(|e| e.to_string())?;
        let (tx, rx) = crate::runs::supervisor_stream::channel();
        let remote = self.clone();
        let wid = worker.clone();
        let solver = spec.tag.clone();
        tokio::spawn(async move {
            let mut after = 0;
            let mut last_ok = tokio::time::Instant::now();
            loop {
                if tx.is_closed() {
                    break;
                }
                match remote
                    .request(
                        Operation::Events {
                            worker_id: wid.clone(),
                            after,
                        },
                        None,
                    )
                    .await
                {
                    Ok(page) => {
                        last_ok = tokio::time::Instant::now();
                        if let Some(solver) = &solver {
                            if let Err(error) = remote.relay_mailbox(&wid, solver).await {
                                tracing::warn!(%error,"Remote mailbox relay pending");
                            }
                        }
                        let mut ended = false;
                        for raw in page["frames"].as_array().into_iter().flatten() {
                            let Ok(frame) = serde_json::from_value::<SupervisorFrame>(raw.clone())
                            else {
                                break;
                            };
                            if frame.t == "exit" {
                                let saved =
                                    remote.store.document_scope(|| -> Result<(), String> {
                                        let mut row = remote
                                            .store
                                            .get("runner_commands", &id)
                                            .map_err(|e| e.to_string())?
                                            .ok_or("Remote receipt missing")?;
                                        row["exit"] = json!(frame);
                                        remote
                                            .store
                                            .put("runner_commands", &id, &row)
                                            .map_err(|e| e.to_string())?;
                                        crate::runner_resources::release_completed(
                                            &remote.store,
                                            &id,
                                        )
                                    });
                                if !matches!(saved, Ok(Ok(()))) {
                                    let _ = tx.send_async(SupervisorFrame {
                                        t: "transport_error".into(),
                                        error: "Remote terminal receipt persistence failed".into(),
                                        ..Default::default()
                                    }).await;
                                    return;
                                }
                                if let Err(error) = remote.collect_outputs(solver.as_deref()).await
                                {
                                    if let Ok(Some(mut row)) =
                                        remote.store.get("runner_commands", &id)
                                    {
                                        row["publication_error"] = json!(error);
                                        let _ = remote.store.put("runner_commands", &id, &row);
                                    }
                                    let _ = tx.send_async(SupervisorFrame {
                                        t: "transport_error".into(),
                                        error: format!("Remote output publication failed: {error}"),
                                        ..Default::default()
                                    }).await;
                                    return;
                                }
                                if let Ok(Some(mut row)) = remote.store.get("runner_commands", &id)
                                {
                                    row["outputs_published"] = json!(true);
                                    let _ = remote.store.put("runner_commands", &id, &row);
                                }
                                ended = true;
                            }
                            if frame.t == "transport_error" {
                                ended = true;
                            }
                            if tx.send_async(frame).await.is_err() {
                                return;
                            }
                        }
                        after = page["after"].as_u64().unwrap_or(after);
                        if ended {
                            break;
                        }
                        if page["execution_stopped"] == true {
                            if let Err(error) = remote.confirm_stopped() {
                                tracing::error!(%error,"Could not settle confirmed remote stop");
                            }
                            let _ = tx.send_async(SupervisorFrame {
                                t: "transport_error".into(),
                                error: "Remote execution stopped without Worker exit receipt"
                                    .into(),
                                ..Default::default()
                            }).await;
                            break;
                        }
                    }
                    Err(error) => {
                        if last_ok.elapsed() > Duration::from_secs(30) {
                            let _ = tx.send_async(SupervisorFrame {
                                t: "transport_error".into(),
                                error: format!("Remote stream unavailable: {error}"),
                                ..Default::default()
                            }).await;
                            break;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        });
        Ok((worker, rx))
    }
    async fn relay_mailbox(&self, worker: &str, solver: &str) -> Result<(), String> {
        super::commands::valid_solver_id(solver)?;
        let mailbox = self
            .workspace
            .join(".muteki-blackboard-ingress")
            .join(solver);
        let requests = self
            .request(
                Operation::Mailbox {
                    worker_id: worker.into(),
                },
                None,
            )
            .await?;
        for request in requests["requests"].as_array().into_iter().flatten() {
            let id = request["request_id"]
                .as_str()
                .ok_or("Mailbox request ID missing")?;
            super::commands::valid_id(id)?;
            let response = mailbox.join(format!("result-{id}.json"));
            if response.exists() {
                let bytes = crate::private_files::read(&self.workspace, &response, 256 * 1024)?;
                let response: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                self.request(
                    Operation::MailboxReply {
                        worker_id: worker.into(),
                        request_id: id.into(),
                        response,
                    },
                    Some(identity("reply", json!([self.scope, worker, id]))?),
                )
                .await?;
            } else {
                let path = mailbox.join(format!("request-{id}.json"));
                if !path.exists() {
                    crate::private_files::write(
                        &self.workspace,
                        &path,
                        &serde_json::to_vec(request).map_err(|e| e.to_string())?,
                    )?;
                }
            }
        }
        Ok(())
    }
}
/// Files the Web keeps writing in the Run workspace while Workers run (its event log and
/// planner exchanges). They are not Worker inputs, and a node copy is never authoritative.
fn controller_owned(path: &str) -> bool {
    matches!(path, "events.jsonl" | "manifest.json" | "runtime-diagnostics.json" | "graph.yaml" | "graph.jsonl")
        || path.starts_with("reason-responses/")
}
/// Graph context the Web rewrites for Workers: an input it may refresh in any generation,
/// never a Worker output.
pub(super) fn controller_published(path: &str) -> bool {
    matches!(path, "shared/live/graph.yaml" | "shared/graph.yaml")
}
/// Every engine's skill directory links the Run's own blackboard skill; the node's
/// materialized copies are not Worker output and must not be written back through the link.
fn blackboard_skill_alias(path: &str) -> bool {
    path.split_once('/').is_some_and(|(dir, rest)| {
        dir.starts_with('.')
            && (rest == "skills/muteki-blackboard" || rest.starts_with("skills/muteki-blackboard/"))
    })
}
const SECRET_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "OPENAI_API_KEY",
    "OPENCODE_API_KEY",
    "CURSOR_API_KEY",
    "CURSOR_AUTH_TOKEN",
    "FACTORY_API_KEY",
    "XAI_API_KEY",
    "KIMI_MODEL_API_KEY",
    "DEEPSEEK_API_KEY",
];
fn strip_secret_env(env: &mut HashMap<String, String>) {
    for key in SECRET_ENV {
        env.remove(*key);
        env.remove(&format!("{key}_FILE"));
    }
}
pub async fn ensure(
    state: &AppState,
    run: &str,
    workspace: &Path,
    settings: &Value,
    owner: Option<&str>,
) -> Result<ContainerHandle, String> {
    let node = selected(settings)
        .ok_or("Remote node not selected")?
        .to_owned();
    let row = match state.store.get("runs", run).map_err(|e| e.to_string())? {
        Some(row) => row,
        None => match owner.filter(|_| run.starts_with("btw-")) {
            Some(owner) => state
                .store
                .get("runs", owner)
                .map_err(|e| e.to_string())?
                .ok_or("Auxiliary owner Run not found")?,
            None => Value::Null,
        },
    };
    let generation = row["execution_generation"].as_u64().unwrap_or(1);
    let project = row["project_id"].as_str().unwrap_or("personal").to_owned();
    let scope = Scope {
        node_id: node,
        project_id: project,
        run_id: run.into(),
        generation,
    };
    let root = crate::native_chat::state_dir(state)
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let workspace = workspace.canonicalize().map_err(|e| e.to_string())?;
    if !workspace.starts_with(&root) {
        return Err("Controller workspace leaves state root".into());
    }
    let key = format!("{run}:{generation}");
    if state.store.list("runner_controller_runs").map_err(|e|e.to_string())?.iter().any(|row|row["run_id"]==run&&row["publication_pending"]==true){return Err("Prior execution artifacts are still pending; reconcile them before starting a new generation".into());}

    if let Some(prior) = state
        .store
        .get("runner_controller_runs", &key)
        .map_err(|e| e.to_string())?
    {
        if prior["node_id"] != scope.node_id || prior["project_id"] != scope.project_id {
            return Err("Active Run node/project placement is immutable".into());
        }
        if prior["state"] == "stopped" || prior["state"]=="stopping" {
            return Err("Remote generation has already stopped".into());
        }
    }
    let mut remote = Remote {
        store: state.store.clone(),
        registry: state.remote_runners.clone(),
        root,
        workspace,
        container_workspace: String::new(),
        scope,
        policy: generation,
    };
    let mut settings = settings.clone();
    let mut materials = Vec::new();
    for marker in state
        .store
        .list("run_execution_commands")
        .map_err(|e| e.to_string())?
    {
        if marker["run_id"] == run && marker["generation"] == generation {
            for spec in marker["specs"].as_array().into_iter().flatten() {
                if let (Some(engine), Some(credential)) = (
                    spec["norm_engine"].as_str(),
                    spec["effective_credential_id"].as_str(),
                ) {
                    materials.push(material_id(
                        run,
                        generation,
                        &material_key(engine, credential, spec["model"].as_str()),
                    )?);
                }
            }
        }
    }
    if let Some(key) = settings["_runner_material_key"].as_str() {
        materials.push(material_id(run, generation, key)?);
    }
    materials.sort();
    materials.dedup();
    settings["credentials"] = json!(materials);
    settings
        .as_object_mut()
        .unwrap()
        .remove("_runner_solver_id");
    settings
        .as_object_mut()
        .unwrap()
        .remove("_runner_material_key");
    let ensure_id = identity("ensure", json!([remote.scope, settings]))?;
    remote.store.put("runner_controller_runs",&key,&json!({"run_id":run,"generation":generation,"node_id":remote.scope.node_id,"project_id":remote.scope.project_id,"state":"preparing"})).map_err(|e|e.to_string())?;
    let settings_browser = settings["_runner_browser_enabled"] == true;
    let result = remote
        .request(Operation::Ensure { settings }, Some(ensure_id))
        .await?;
    remote.container_workspace = result["container_workspace"]
        .as_str()
        .ok_or("Remote workspace missing")?
        .into();
    remote.store.put("runner_controller_runs",&key,&json!({"run_id":run,"generation":generation,"node_id":remote.scope.node_id,"project_id":remote.scope.project_id,"state":"active","browser_reserved":settings_browser,"host_epoch":state.chat.epoch(),"workspace_relative":remote.workspace.strip_prefix(&remote.root).map_err(|e|e.to_string())?,"container_workspace":remote.container_workspace,"container_id":result["container_id"]})).map_err(|e|e.to_string())?;
    ContainerHandle::from_remote(result, Arc::new(remote))
}

/// An explicit stop can be completed from durable placement even after Web lost its handles.
pub async fn stop_recorded(state:&AppState,run:&str)->Result<bool,String>{
    let records=state.store.list("runner_controller_runs").map_err(|e|e.to_string())?;
    let Some(binding)=records.into_iter().filter(|row|row["run_id"]==run).max_by_key(|row|row["generation"].as_u64().unwrap_or(0))else{return Ok(false)};
    if binding["state"]=="stopped"&&binding["publication_pending"]!=true{return Ok(true);}
    let confirmed=binding["state"]=="stopped";
    let scope=Scope{node_id:binding["node_id"].as_str().ok_or("Saved node missing")?.into(),project_id:binding["project_id"].as_str().ok_or("Saved project missing")?.into(),run_id:run.into(),generation:binding["generation"].as_u64().ok_or("Saved generation missing")?};
    let root=crate::native_chat::state_dir(state);
    let relative=binding["workspace_relative"].as_str().ok_or("Saved workspace missing")?;
    transfers::validate_path(relative)?;
    let workspace=root.join(relative).canonicalize().map_err(|e|e.to_string())?;
    if !workspace.starts_with(root.canonicalize().map_err(|e|e.to_string())?){return Err("Saved workspace leaves state root".into());}
    let remote=Remote{store:state.store.clone(),registry:state.remote_runners.clone(),workspace,root,container_workspace:String::new(),policy:scope.generation,scope};
    if confirmed{remote.finish_artifacts().await?}else{remote.stop().await?}
    Ok(true)
}

pub(crate) async fn signal_recorded(state:&AppState,run:&str,generation:u64,worker:&str,signal:&str,command:&str)->Result<bool,String>{
    let binding=state.store.get("runner_controller_runs",&format!("{run}:{generation}")).map_err(|e|e.to_string())?.ok_or("Freeze recovery placement missing")?;
    if binding["state"]=="stopped"{return Ok(false);}
    let scope=Scope{node_id:binding["node_id"].as_str().ok_or("Freeze node missing")?.into(),project_id:binding["project_id"].as_str().ok_or("Freeze project missing")?.into(),run_id:run.into(),generation};
    let root=crate::native_chat::state_dir(state);
    let remote=Remote{store:state.store.clone(),registry:state.remote_runners.clone(),workspace:root.clone(),root,container_workspace:String::new(),policy:generation,scope};
    let id=identity("freeze-compensation",json!([command,run,generation,worker,signal]))?;
    let frame:SupervisorFrame=serde_json::from_value(remote.request(Operation::Signal{worker_id:worker.into(),signal:signal.into()},Some(id)).await?).map_err(|e|e.to_string())?;
    if !frame.error.is_empty(){return Err(frame.error);}
    if frame.paused!=(signal=="STOP"){return Err("Remote node has not confirmed freeze compensation".into());}
    Ok(true)
}

pub fn spawn_recovery(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            if let Err(error) = reconcile(&state).await {
                tracing::warn!(%error,"Remote outcome recovery remains pending");
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    });
}
pub async fn reconcile(state: &AppState) -> Result<(), String> {
    let root = crate::native_chat::state_dir(state);
    for binding in state.store.list("runner_controller_runs").map_err(|e|e.to_string())? {
        if let Err(error)=reconcile_binding(state,&root,binding.clone()).await {
            tracing::error!(run_id=binding["run_id"].as_str().unwrap_or(""),%error,"Remote Run recovery remains blocked");
            let key=crate::runner::canonical_digest(json!([binding["run_id"],binding["generation"],binding["node_id"]]))?;
            let receipt=json!({"run_id":binding["run_id"],"generation":binding["generation"],"node_id":binding["node_id"],
                "state":"recovery_failed","error":error,"checked_at":chrono::Utc::now().to_rfc3339()});
            if let Err(save)=state.store.put("runner_recovery_errors",&key,&receipt){tracing::error!(%save,"Could not record isolated recovery failure");}
        }
    }
    crate::runs::settle_runner_stops(state).await?;
    Ok(())
}

async fn reconcile_binding(state:&AppState,root:&PathBuf,mut binding:Value)->Result<(),String>{
        if binding["publication_pending"]!=true&&(binding["host_epoch"] == state.chat.epoch() || binding["state"] == "stopped") {
            return Ok(());
        }
        let node = binding["node_id"].as_str().ok_or("Recovery node missing")?;
        if !state.remote_runners.online(node) { return Ok(()); }
        let scope = Scope {
            node_id: node.into(),
            project_id: binding["project_id"]
                .as_str()
                .ok_or("Recovery project missing")?
                .into(),
            run_id: binding["run_id"]
                .as_str()
                .ok_or("Recovery Run missing")?
                .into(),
            generation: binding["generation"]
                .as_u64()
                .ok_or("Recovery generation missing")?,
        };
        let relative = binding["workspace_relative"].as_str().ok_or("Recovery workspace missing")?;
        transfers::validate_path(relative)?;
        let workspace = root
            .join(relative)
            .canonicalize()
            .map_err(|e| e.to_string())?;
        if !workspace.starts_with(root.canonicalize().map_err(|e| e.to_string())?) {
            return Err("Recovery workspace leaves state root".into());
        }
        let remote = Remote {
            store: state.store.clone(),
            registry: state.remote_runners.clone(),
            root: root.clone(),
            workspace,
            container_workspace: binding["container_workspace"].as_str().unwrap_or("").into(),
            policy: scope.generation,
            scope,
        };
        if binding["publication_pending"]==true{
            let result=if binding["state"]=="stopped"{remote.finish_artifacts().await}else{remote.stop().await};
            result?;
            return Ok(());
        }
        let mut pending = false;
        let mut solved = false;
        for mut row in state
            .store
            .list("runner_commands")
            .map_err(|e| e.to_string())?
        {
            if row["run_id"] != remote.scope.run_id
                || row["generation"] != remote.scope.generation
                || row["node_id"] != remote.scope.node_id
                || crate::runner_execution::facts(&row).published
            {
                continue;
            }
            let id = row["id"]
                .as_str()
                .ok_or("Recovery command missing")?
                .to_owned();
            let receipt = match remote
                .request(
                    Operation::Receipt {
                        command_id: id.clone(),
                    },
                    None,
                )
                .await
            {
                Ok(v) => v,
                Err(error) => {
                    row["recovery_error"] = json!(error);
                    state
                        .store
                        .put("runner_commands", &id, &row)
                        .map_err(|e| e.to_string())?;
                    pending = true;
                    continue;
                }
            };
            if receipt["digest"] != row["digest"]
                || receipt["run_id"] != remote.scope.run_id
                || receipt["generation"] != remote.scope.generation
            {
                return Err("Recovered receipt identity mismatch".into());
            }
            if let (Some(worker), Some(solver)) = (
                row["result"]["worker_id"].as_str().map(str::to_owned),
                row["solver_id"].as_str().map(str::to_owned),
            ) {
                let (worker, solver) = (worker.as_str(), solver.as_str());
                match remote.relay_mailbox(worker, solver).await {
                    Ok(()) => {
                        let (solved_now, finish_now) = crate::runs::drain_remote_ingress(
                            state,
                            &remote.scope.run_id,
                            solver,
                            &remote.workspace,
                        );
                        solved |= solved_now;
                        let run_solved = state
                            .store
                            .get("runs", &remote.scope.run_id)
                            .map_err(|e| e.to_string())?
                            .is_some_and(|run| run["solved"] == true);
                        if solved_now || finish_now || run_solved {
                            if let Err(error) = remote.relay_mailbox(worker, solver).await {
                                row["mailbox_error"] = json!(error);
                            }
                            row["finish_requested"] = json!(true);
                        }
                    }
                    Err(error) => row["mailbox_error"] = json!(error),
                }
                if row["finish_requested"] == true && !receipt["exit"].is_object() {
                    if let Err(error) = remote
                        .request(
                            Operation::Signal {
                                worker_id: worker.into(),
                                signal: "TERM".into(),
                            },
                            None,
                        )
                        .await
                    {
                        row["signal_error"] = json!(error);
                    }
                }
            }
            if receipt["exit"].is_object() {
                row["exit"] = receipt["exit"].clone();
                row["state"] = json!("started");
                state
                    .store
                    .put("runner_commands", &id, &row)
                    .map_err(|e| e.to_string())?;
                crate::runner_resources::release_completed(&state.store, &id)?;
                match remote.collect_outputs(row["solver_id"].as_str()).await {
                    Ok(()) => {
                        row["outputs_published"] = json!(true);
                        row["recovery_state"] = json!("exit_and_artifacts_recovered");
                    }
                    Err(error) => {
                        row["publication_error"] = json!(error);
                        pending = true;
                    }
                }
            } else if receipt["execution_stopped"] == true {
                row["execution_stopped"] = json!(true);
                row["state"] = json!("outcome_unknown");
            } else {
                row["recovery_state"] = json!("process_running_coordinator_interrupted");
                pending = true;
            }
            state
                .store
                .put("runner_commands", &id, &row)
                .map_err(|e| e.to_string())?;
            crate::runner_resources::release_completed(&state.store, &id)?;
        }
        let recovery = if pending {
            "reconciling"
        } else {
            "evidence_recovered_resume_required"
        };
        let changed = binding["recovery_state"] != recovery;
        binding["recovery_state"] = json!(recovery);
        state
            .store
            .put(
                "runner_controller_runs",
                &format!("{}:{}", remote.scope.run_id, remote.scope.generation),
                &binding,
            )
            .map_err(|e| e.to_string())?;
        if pending {
            if changed {
                crate::runs::announce_remote_reattach(state, &remote.scope.run_id, &remote.scope.node_id);
            }
            return Ok(());
        }
        if crate::runs::settle_remote_run(
            state,
            &remote.scope.run_id,
            remote.scope.generation,
            &remote.scope.node_id,
            solved,
        )? {
            if let Err(error) = remote.stop().await {
                tracing::warn!(%error, "Recovered remote Run teardown remains pending");
            }
        }
    Ok(())
}

#[cfg(test)]
mod integration {
    use super::*;
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires Docker, built server and MUTEKI_TEST_REMOTE_FIXTURE certificate directory"]
    async fn ordinary_container_handle_executes_remotely_with_mailbox_and_credentials() {
        use axum::serve::Listener as _;
        let fixture = PathBuf::from(std::env::var("MUTEKI_TEST_REMOTE_FIXTURE").unwrap());
        let root = fixture.join("controller");
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let store = Store::open(root.join("muteki.sqlite")).unwrap();
        store.put("system", "state_dir", &json!(root)).unwrap();
        let state = Arc::new(AppState::new(store));
        let listener = super::super::transport::Listener::bind(
            "127.0.0.1:0".parse().unwrap(),
            &fixture.join("cert.pem"),
            &fixture.join("key.pem"),
        )
        .await
        .unwrap();
        let address = listener.local_addr().unwrap();
        let transport = super::super::transport::router(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, transport).await.unwrap();
        });
        let token = super::super::identity::issue_enrollment(
            &state.store,
            "ordinary-flow".into(),
            vec!["personal".into()],
            120,
            chrono::Utc::now().timestamp(),
        )
        .unwrap();
        let token_file = fixture.join("enroll-token");
        std::fs::write(&token_file, token).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let binary = PathBuf::from(std::env::var("MUTEKI_TEST_SERVER_BINARY").unwrap());
        let node_root = fixture.join("node");
        let enrolled = crate::host_process::command(&binary)
            .args(["runner", "enroll", "--state-dir"])
            .arg(&node_root)
            .arg("--server")
            .arg(format!("https://localhost:{}", address.port()))
            .arg("--token-file")
            .arg(&token_file)
            .arg("--ca-file")
            .arg(fixture.join("ca.pem"))
            .env_remove("MUTEKI_RUNNER_SOCKET")
            .output()
            .await
            .unwrap();
        assert!(
            enrolled.status.success(),
            "{}",
            String::from_utf8_lossy(&enrolled.stderr)
        );
        let config: Value =
            serde_json::from_slice(&std::fs::read(node_root.join("runner/remote.json")).unwrap())
                .unwrap();
        let node = config["credential"]["node_id"].as_str().unwrap().to_owned();
        let log = std::fs::File::create(fixture.join("node.log")).unwrap();
        let mut process = crate::host_process::command(&binary)
            .args(["runner", "--remote", "--state-dir"])
            .arg(&node_root)
            .env_remove("MUTEKI_RUNNER_SOCKET")
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let until = tokio::time::Instant::now() + Duration::from_secs(20);
        while !state.remote_runners.online(&node) {
            assert!(tokio::time::Instant::now() < until, "node did not connect");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let metrics_deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while state
            .store
            .get("runner_node_resources", &node)
            .unwrap()
            .is_none()
        {
            assert!(
                tokio::time::Instant::now() < metrics_deadline,
                "node resource telemetry missing"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let metrics = crate::runner_resources::snapshot_node(&state, &node).await;
        assert!(metrics["docker"]["memory_bytes"].as_u64().unwrap() > 0);
        state
            .store
            .put(
                "runner_resource_policies",
                "local",
                &json!({"mode":"unlimited"}),
            )
            .unwrap();
        state
            .store
            .put(
                "runner_resource_policies",
                &node,
                &json!({"mode":"manual","max_workers":1,"max_runs":1}),
            )
            .unwrap();
        let local = crate::runner_resources::Reservation::enqueue(
            &state,
            "local-queue-check",
            "local-worker",
        )
        .unwrap();
        let baseline: crate::runner_resources::Recommendation =
            serde_json::from_value(metrics["recommendation"].clone()).unwrap();
        assert!(local.try_admit(&state, &baseline).unwrap());
        let remote_first = crate::runner_resources::Reservation::enqueue_on_node(
            &state,
            "queue-check",
            "first-worker",
            crate::runner_resources::Workload {
                generation: 1,
                browser: false,
            },
            &node,
        )
        .unwrap();
        assert!(
            remote_first.try_admit(&state, &baseline).unwrap(),
            "local admission must not consume a remote node slot"
        );
        let queued = crate::runner_resources::Reservation::enqueue_on_node(
            &state,
            "queue-check",
            "worker",
            crate::runner_resources::Workload {
                generation: 1,
                browser: false,
            },
            &node,
        )
        .unwrap();
        assert!(
            !queued.try_admit(&state, &baseline).unwrap(),
            "remote manual concurrency must queue the second Worker"
        );
        state
            .store
            .put(
                "runner_resource_policies",
                &node,
                &json!({"mode":"unlimited"}),
            )
            .unwrap();
        let mut recommendation: crate::runner_resources::Recommendation =
            serde_json::from_value(metrics["recommendation"].clone()).unwrap();
        recommendation.memory_budget_bytes = 0;
        recommendation.memory_admission_ceiling = Some(0);
        assert!(
            queued.try_admit(&state, &recommendation).unwrap(),
            "unlimited must ignore the automatic memory ceiling"
        );
        drop(queued);
        drop(remote_first);
        drop(local);
        state.store.put("credential_accounts","remote-fixture",&json!({"id":"remote-fixture","engine":"claude","secret":"synthetic-remote-projection","base_url":"https://example.invalid"})).unwrap();
        for scope in ["run", "shared"] {
            let run = format!("ordinary-{}", uuid::Uuid::new_v4().simple());
            let solver = "remote-solver";
            state.store.put("runs",&run,&json!({"id":run,"mode":"ctf","status":"running","execution_generation":1,"worker_runtime":{"worker_container_scope":scope},"challenge":{"title":"Remote fixture"}})).unwrap();
            let workspace = crate::runs::session_dir(&state, &run);
            std::fs::create_dir_all(&workspace).unwrap();
            let (workdir, _, ingress) = crate::runs::worker::stage_workspace(
                &workspace,
                solver,
                "claude",
                &json!({"title":"Remote fixture"}),
            )
            .unwrap();
            std::fs::write(workdir.join("input.txt"), "controller-input").unwrap();
            let (workdir2, _, _) = crate::runs::worker::stage_workspace(
                &workspace,
                "remote-solver-2",
                "claude",
                &json!({"title":"Remote fixture"}),
            )
            .unwrap();
            let settings = json!({"worker_node_id":node,"worker_container_scope":scope,"worker_network":"bridge","_runner_material_key":material_key("claude","account:remote-fixture",None)});
            let handle = crate::runs::container_exec::ensure_run_container_owned(
                &state,
                &run,
                &workspace,
                &settings,
                Some(&run),
            )
            .await
            .unwrap();
            assert!(handle.link.remote.is_some());
            assert_eq!(handle.host_workspace, workspace.canonicalize().unwrap());
            let (env, _) = crate::runs::container_exec::prepare_container_worker_env(
                &state,
                &handle,
                solver,
                "claude",
                "account:remote-fixture",
                None,
                &[],
            )
            .await
            .unwrap();
            assert!(!env.contains_key("ANTHROPIC_API_KEY"));
            assert!(env.contains_key("ANTHROPIC_API_KEY_FILE"));
            let mailbox = format!(
                "{}/.muteki-blackboard-ingress/{solver}",
                handle.container_workspace
            );
            let script = format!(
                r#"import os,json,time,pathlib
assert pathlib.Path('input.txt').read_text()=='controller-input'
assert pathlib.Path(os.environ['ANTHROPIC_API_KEY_FILE']).read_text()=='synthetic-remote-projection'
p=pathlib.Path({mailbox:?});p.mkdir(parents=True,exist_ok=True)
r={{'protocol':'muteki-blackboard-v2','request_id':'context-check','worker_id':'remote-solver','execution_generation':1,'operation':'context','payload':{{}}}}
(p/'request-context-check.json').write_text(json.dumps(r))
end=time.monotonic()+20
while not (p/'result-context-check.json').exists():
 assert time.monotonic()<end,'mailbox timed out'
 time.sleep(.05)
assert json.loads((p/'result-context-check.json').read_text())['ok']
pathlib.Path('result.txt').write_text('remote-output')
print('remote-normal-handle-ok',flush=True)
"#
            );
            let spec = WorkerSpec {
                reservation_id: None,
                owner_run_id: None,
                owner_token: None,
                owner_workspace: None,
                argv: vec!["python3".into(), "-c".into(), script],
                cwd: handle.to_container_path(&workdir).unwrap(),
                stdin: None,
                stdin_mode: None,
                env,
                timeout_sec: 40,
                tag: Some(solver.into()),
                output_limit_bytes: 1024 * 1024,
                disk_limit_bytes: 64 * 1024 * 1024,
            };
            let (wid, mut frames) = handle.link.start_worker(&spec).await.unwrap();
            let handle2 = crate::runs::container_exec::ensure_run_container_owned(
                &state,
                &run,
                &workspace,
                &settings,
                Some(&run),
            )
            .await
            .unwrap();
            let (env2, _) = crate::runs::container_exec::prepare_container_worker_env(
                &state,
                &handle2,
                "remote-solver-2",
                "claude",
                "account:remote-fixture",
                None,
                &[],
            )
            .await
            .unwrap();
            assert_ne!(spec.env["HOME"], env2["HOME"]);
            let spec2=WorkerSpec{argv:vec!["python3".into(),"-c".into(),"import pathlib,os; assert pathlib.Path(os.environ['ANTHROPIC_API_KEY_FILE']).read_text()=='synthetic-remote-projection'; pathlib.Path('second.txt').write_text('second-worker')".into()],cwd:handle2.to_container_path(&workdir2).unwrap(),env:env2,tag:Some("remote-solver-2".into()),..spec.clone()};
            let (_, mut frames2) = handle2.link.start_worker(&spec2).await.unwrap();
            let mut output = String::new();
            let mut exited = false;
            let end = tokio::time::Instant::now() + Duration::from_secs(60);
            while tokio::time::Instant::now() < end {
                crate::runs::worker::drain_blackboard_ingress(
                    &state, &run, solver, &ingress, &workspace,
                );
                if let Ok(Some(frame)) =
                    tokio::time::timeout(Duration::from_millis(50), frames.recv()).await
                {
                    if frame.t == "transport_error" {
                        panic!("{}", frame.error);
                    }
                    if frame.t == "out" {
                        output.push_str(&frame.line);
                    }
                    if frame.t == "exit" {
                        assert_eq!(frame.rc, 0, "{output}");
                        exited = true;
                        break;
                    }
                }
            }
            assert!(exited, "missing remote exit: {output}");
            assert!(output.contains("remote-normal-handle-ok"));
            assert_eq!(
                std::fs::read_to_string(workdir.join("result.txt")).unwrap(),
                "remote-output"
            );
            let mut second_exited = false;
            while let Ok(Some(frame)) =
                tokio::time::timeout(Duration::from_secs(20), frames2.recv()).await
            {
                assert_ne!(frame.t, "transport_error", "{}", frame.error);
                if frame.t == "exit" {
                    assert_eq!(frame.rc, 0);
                    second_exited = true;
                    break;
                }
            }
            assert!(second_exited);
            assert_eq!(
                std::fs::read_to_string(workdir2.join("second.txt")).unwrap(),
                "second-worker"
            );
            let key = format!("{run}:1");
            let mut binding = state
                .store
                .get("runner_controller_runs", &key)
                .unwrap()
                .unwrap();
            binding["host_epoch"] = json!("prior-controller-epoch");
            state
                .store
                .put("runner_controller_runs", &key, &binding)
                .unwrap();
            for mut command in state.store.list("runner_commands").unwrap() {
                if command["run_id"] == run {
                    let id = command["id"].as_str().unwrap().to_owned();
                    command["outputs_published"] = json!(false);
                    state.store.put("runner_commands", &id, &command).unwrap();
                }
            }
            std::fs::remove_file(workdir.join("result.txt")).unwrap();
            reconcile(&state).await.unwrap();
            assert_eq!(
                std::fs::read_to_string(workdir.join("result.txt")).unwrap(),
                "remote-output"
            );
            assert_eq!(
                state
                    .store
                    .get("runner_controller_runs", &key)
                    .unwrap()
                    .unwrap()["recovery_state"],
                "evidence_recovered_resume_required"
            );
            std::fs::write(workdir.join("result.txt"), "controller-edit").unwrap();
            assert!(
                handle
                    .link
                    .remote
                    .as_ref()
                    .unwrap()
                    .collect_outputs(Some(solver))
                    .await
                    .unwrap_err()
                    .contains("conflict")
            );
            assert_eq!(
                std::fs::read_to_string(workdir.join("result.txt")).unwrap(),
                "controller-edit"
            );
            std::fs::write(workdir.join("result.txt"),"remote-output").unwrap();
            let model_script=r#"import os,json,threading,http.server,subprocess
calls=[]
class Handler(http.server.BaseHTTPRequestHandler):
 def log_message(self,*args): pass
 def do_POST(self):
  body=json.loads(self.rfile.read(int(self.headers.get('Content-Length','0'))) or '{}')
  if 'count_tokens' in self.path:
   raw=b'{"input_tokens":10}'; self.send_response(200); self.send_header('Content-Type','application/json'); self.end_headers(); self.wfile.write(raw); return
  calls.append(self.path)
  message={'id':'msg_fixture','type':'message','role':'assistant','model':'claude-sonnet-4-6','content':[{'type':'text','text':'remote-model-fixture-ok'}],'stop_reason':'end_turn','stop_sequence':None,'usage':{'input_tokens':10,'output_tokens':5}}
  self.send_response(200)
  if body.get('stream'):
   self.send_header('Content-Type','text/event-stream'); self.end_headers()
   start=dict(message,content=[],stop_reason=None)
   events=[('message_start',{'type':'message_start','message':start}),('content_block_start',{'type':'content_block_start','index':0,'content_block':{'type':'text','text':''}}),('content_block_delta',{'type':'content_block_delta','index':0,'delta':{'type':'text_delta','text':'remote-model-fixture-ok'}}),('content_block_stop',{'type':'content_block_stop','index':0}),('message_delta',{'type':'message_delta','delta':{'stop_reason':'end_turn','stop_sequence':None},'usage':{'output_tokens':5}}),('message_stop',{'type':'message_stop'})]
   for event,value in events:self.wfile.write(('event: '+event+'\ndata: '+json.dumps(value)+'\n\n').encode());self.wfile.flush()
  else:self.send_header('Content-Type','application/json');self.end_headers();self.wfile.write(json.dumps(message).encode())
server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler);threading.Thread(target=server.serve_forever,daemon=True).start()
env=dict(os.environ,ANTHROPIC_BASE_URL='http://127.0.0.1:'+str(server.server_port),CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC='1',DISABLE_TELEMETRY='1',NO_PROXY='127.0.0.1,localhost')
result=subprocess.run(['claude','-p','--model','claude-sonnet-4-6','--output-format','stream-json','--verbose','--no-session-persistence','--dangerously-skip-permissions','--bare'],input='Reply with fixture acknowledgement',text=True,capture_output=True,env=env,timeout=35)
assert result.returncode==0,(result.returncode,result.stdout,result.stderr)
assert calls and 'remote-model-fixture-ok' in result.stdout,(calls,result.stdout,result.stderr)
print('real-cli-model-protocol-ok',flush=True)
server.shutdown()
"#;
            let model=WorkerSpec{argv:vec!["python3".into(),"-c".into(),model_script.into()],timeout_sec:45,..spec2.clone()};
            let (_,mut model_frames)=handle2.link.start_worker(&model).await.unwrap();let mut model_output=String::new();let mut model_exit=false;
            while let Ok(Some(frame))=tokio::time::timeout(Duration::from_secs(50),model_frames.recv()).await{
                if matches!(frame.t.as_str(),"out"|"err"){model_output.push_str(&frame.line);}
                assert_ne!(frame.t,"transport_error","{}",frame.error);
                if frame.t=="exit"{assert_eq!(frame.rc,0,"{model_output}");model_exit=true;break;}
            }
            assert!(model_exit&&model_output.contains("real-cli-model-protocol-ok"),"{model_output}");
            let flood = WorkerSpec {
                argv: vec![
                    "python3".into(),
                    "-c".into(),
                    "print('x'*(600*1024),flush=True)".into(),
                ],
                output_limit_bytes: 1024 * 1024,
                ..spec2.clone()
            };
            let (_, mut flood_frames) = handle2.link.start_worker(&flood).await.unwrap();
            let failure = tokio::time::timeout(Duration::from_secs(20), async {
                while let Some(frame) = flood_frames.recv().await {
                    if frame.t == "transport_error" {
                        return frame.error;
                    }
                }
                panic!("oversized log unexpectedly succeeded")
            })
            .await
            .unwrap();
            assert!(
                failure.contains("budget") || failure.contains("stopped"),
                "{failure}"
            );
            assert!(!wid.is_empty());
            crate::runs::container_exec::teardown_run_container_checked(&run)
                .await
                .unwrap();
            assert!(
                state
                    .store
                    .list("runner_commands")
                    .unwrap()
                    .iter()
                    .filter(|row| row["run_id"] == run)
                    .all(|row| row["execution_stopped"] == true)
            );
        }
        for row in state.store.list("runner_remote_dispatches").unwrap() {
            let text = row.to_string();
            assert!(!text.contains("synthetic-remote-projection"));
            assert!(!text.contains(&STANDARD.encode("synthetic-remote-projection")));
        }
        process.kill().await.unwrap();
        let _ = process.wait().await;
        server.abort();
    }
}
