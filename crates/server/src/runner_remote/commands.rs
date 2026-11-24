//! Node-side command authorization and durable receipts, independent of connection lifetime.
use crate::{runs::container_exec::WorkerSpec, store::Store};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tokio::sync::Mutex;

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Ensure { settings: Value },
    Start { spec: WorkerSpec },
    Stop,
    Signal { worker_id: String, signal: String },
    Stdin { worker_id: String, data: String },
    Status,
    Health,
    WorkspaceList { prefix: String },
    WorkspaceWrite { path:String, data:String, expected_sha256:Option<String>, mode:u32 },
    Mailbox { worker_id:String },
    MailboxReply { worker_id:String, request_id:String, response:Value },
    Events { worker_id: String, after: u64 },
    Receipt { command_id: String },
    TransferPull { transfer_id: String },
    TransferPush { transfer_id: String, path: String, max_bytes: u64 },
    TransferStatus { transfer_id: String },
    CredentialInstall { grant_id: String, revision: String },
    CredentialRefresh { grant_id: String },
    CredentialStatus { grant_id: String },
}
impl Operation {
    pub fn mutating(&self) -> bool {
        !matches!(
            self,
            Self::Status | Self::Health | Self::WorkspaceList{..} | Self::Mailbox{..} | Self::Events { .. } | Self::Receipt { .. } | Self::TransferStatus { .. } | Self::CredentialStatus { .. }
        )
    }
    pub fn new_work(&self) -> bool {
        match self {
            Self::Ensure { .. }
            | Self::Start { .. }
            | Self::Stdin { .. } | Self::WorkspaceWrite{..} | Self::MailboxReply{..}
            | Self::TransferPull { .. }
            | Self::CredentialInstall { .. } | Self::CredentialRefresh { .. } => true,
            Self::Signal { signal, .. } => signal == "CONT",
            _ => false,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub id: String,
    pub node_id: String,
    pub project_id: String,
    pub run_id: String,
    pub generation: u64,
    pub session_generation: u64,
    pub credential_version: u64,
    pub policy_version: u64,
    pub expires_at: i64,
    pub action: Operation,
}
impl Command {
    pub fn digest(&self) -> Result<String, String> {
        crate::runner::canonical_digest(json!({"node":self.node_id,"project":self.project_id,"run":self.run_id,
            "generation":self.generation,"policy":self.policy_version,"action":self.action}))
    }
}
pub struct Authority {
    pub node_id: String,
    pub session_generation: u64,
    pub credential_version: u64,
    pub projects: Vec<String>,
    pub valid_until: tokio::time::Instant,
}
pub struct Processor {
    store: Store,
    socket: PathBuf,
    renew_gate: Mutex<()>,
    gates: Mutex<HashMap<String, std::sync::Weak<Mutex<()>>>>,
}
impl Processor {
    pub fn new(store: Store, socket: PathBuf) -> Self {
        Self {
            store,
            socket,
            renew_gate: Mutex::new(()),
            gates: Mutex::new(HashMap::new()),
        }
    }
    pub(super) async fn resource_snapshot(&self)->Result<Value,String>{crate::runner::request(&self.socket,"/v1/resources").await}
    pub(super) async fn gate(&self, run: &str) -> Arc<Mutex<()>> {
        let mut gates = self.gates.lock().await;
        gates.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = gates.get(run).and_then(std::sync::Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(Mutex::new(()));
        gates.insert(run.into(), Arc::downgrade(&gate));
        gate
    }
    fn authorize(&self, command: &Command, authority: &Authority, now: i64) -> Result<(), String> {
        self.authorize_inner(command, authority, now, true)
    }
    fn authorize_inner(
        &self,
        command: &Command,
        authority: &Authority,
        now: i64,
        check_window: bool,
    ) -> Result<(), String> {
        if tokio::time::Instant::now() >= authority.valid_until {
            return Err("Node execution lease expired".into());
        }
        for id in [
            &command.id,
            &command.run_id,
            &command.project_id,
            &command.node_id,
        ] {
            valid_id(id)?;
        }
        if command.node_id != authority.node_id
            || command.session_generation != authority.session_generation
            || command.credential_version != authority.credential_version
            || !authority.projects.contains(&command.project_id)
            || command.generation == 0
            || command.policy_version == 0
            || (check_window && (command.expires_at <= now || command.expires_at > now + 120))
        {
            return Err("Remote command is outside its active authority or validity window".into());
        }
        if let Some(row) = self
            .store
            .get("runner_remote_bindings", &command.run_id)
            .map_err(|e| e.to_string())?
        {
            if row["node_id"] != command.node_id || row["project_id"] != command.project_id {
                return Err("Run belongs to another node or project".into());
            }
            let generation = row["generation"]
                .as_u64()
                .ok_or("Invalid saved execution generation")?;
            if command.generation < generation
                || (command.generation == generation
                    && row["policy_version"] != command.policy_version)
            {
                return Err("Stale execution or policy version".into());
            }
            if command.generation > generation
                && (!matches!(command.action, Operation::Ensure { .. })
                    || row["state"] != "stopped")
            {
                return Err("New execution requires a confirmed prior stop".into());
            }
        } else {
            if !matches!(command.action, Operation::Ensure { .. }) {
                return Err("Remote Run has no execution binding".into());
            }
            if self
                .store
                .get("runner_runs", &command.run_id)
                .map_err(|e| e.to_string())?
                .is_some()
            {
                return Err("Remote command cannot adopt an existing local execution".into());
            }
        }
        Ok(())
    }
    fn worker(&self, run: &str, generation: u64, worker: &str) -> Result<(), String> {
        valid_id(worker)?;
        if !self
            .store
            .list("runner_commands")
            .map_err(|e| e.to_string())?
            .iter()
            .any(|r| {
                r["run_id"] == run
                    && r["generation"] == generation
                    && r["result"]["worker_id"] == worker
            })
        {
            return Err("Worker does not belong to this Run".into());
        }
        Ok(())
    }
    pub async fn execute(
        &self,
        command: &Command,
        authority: &Authority,
        now: i64,
    ) -> Result<Value, String> {
        let waiting = tokio::time::Instant::now();
        let gate = self.gate(&command.run_id).await;
        let _guard = gate.lock().await;
        self.authorize(
            command,
            authority,
            now.saturating_add(waiting.elapsed().as_secs() as i64),
        )?;
        let digest = command.digest()?;
        if command.action.mutating() {
            if let Some(receipt) = self
                .store
                .get("runner_remote_receipts", &command.id)
                .map_err(|e| e.to_string())?
            {
                if receipt["digest"] != digest {
                    return Err("Command ID conflicts with an earlier action".into());
                }
                if receipt["state"] == "completed" {
                    return Ok(receipt["result"].clone());
                }
                if matches!(command.action, Operation::Start { .. }) {
                    if let Some(row) = self
                        .store
                        .get("runner_commands", &command.id)
                        .map_err(|e| e.to_string())?
                    {
                        if row["run_id"] == command.run_id
                            && row["generation"] == command.generation
                            && row["result"].is_object()
                        {
                            return Ok(row["result"].clone());
                        }
                    }
                }
                return Err(
                    "Previous command outcome is unknown; reconcile before further execution"
                        .into(),
                );
            }
        }
        let runtime_digest = if let Operation::Ensure { settings } = &command.action {
            Some(crate::runner::canonical_digest(settings.clone())?)
        } else {
            None
        };
        if let Some(binding) = self
            .store
            .get("runner_remote_bindings", &command.run_id)
            .map_err(|e| e.to_string())?
        {
            if command.generation == binding["generation"].as_u64().unwrap_or(0) {
                if let Some(digest) = &runtime_digest {
                    if binding["runtime_digest"] != *digest {
                        return Err(
                            "Runtime settings are fixed for this execution generation".into()
                        );
                    }
                }
                if command.action.new_work() && binding["state"] != "active" {
                    return Err("Remote execution is not accepting new work".into());
                }
            }
        }
        let projections=self.store.list("runner_credential_projections").map_err(|e|e.to_string())?;
        if matches!(command.action,Operation::Ensure{..}) && projections.iter().any(|r|r["grant"]["scope"]["run_id"]==command.run_id&&r["grant"]["scope"]["generation"]!=command.generation&&r["state"]!="removed") {
            return Err("Prior credential projections must be removed before replacing this execution".into());
        }
        if matches!(command.action,Operation::Start{..}) && projections.iter().any(|r|r["grant"]["scope"]["run_id"]==command.run_id&&r["grant"]["scope"]["generation"]==command.generation&&!matches!(r["state"].as_str(),Some("active"|"removed"))) {
            return Err("Credential projection is not ready for Worker admission".into());
        }
        if matches!(command.action, Operation::Start { .. })
            && self
                .store
                .list("runner_transfer_jobs")
                .map_err(|e| e.to_string())?
                .iter()
                .any(|job| {
                    job["direction"] != "output"
                        && job["scope"]["run_id"] == command.run_id
                        && job["scope"]["generation"] == command.generation
                        && job["state"] != "committed"
                })
        {
            return Err(
                "Required workspace inputs are not committed; inspect the transfer job".into(),
            );
        }
        let (path, body) = match &command.action {
            Operation::Ensure { settings } => {
                if !settings.is_object() {
                    return Err("Runtime settings must be an object".into());
                }
                let mut settings = settings.clone();
                settings["_runner_lease_required"]=json!(true);
                settings["worker_node_id"]=json!("local");
                let credentials = crate::runner::canonical_digest(settings["credentials"].clone())?;
                settings["_runner_trust_domain"] = json!(format!(
                    "{}:{}:{credentials}",
                    command.node_id, command.project_id
                ));
                (
                    "/v1/containers/ensure".into(),
                    Some(
                        json!({"run_id":command.run_id,"generation":command.generation,"settings":settings,"owner_run_id":command.run_id}),
                    ),
                )
            }
            Operation::Start { spec } => (
                "/v1/workers/start".into(),
                Some(
                    json!({"command_id":command.id,"run_id":command.run_id,"generation":command.generation,"spec":spec}),
                ),
            ),
            Operation::Stop => (
                format!("/v1/containers/{}/teardown", command.run_id),
                Some(json!({"generation":command.generation})),
            ),
            Operation::Signal { worker_id, signal } => {
                self.worker(&command.run_id, command.generation, worker_id)?;
                if !matches!(signal.as_str(), "STOP" | "CONT" | "TERM" | "KILL") {
                    return Err("Invalid signal".into());
                }
                (
                    format!("/v1/containers/{}/control", command.run_id),
                    Some(
                        json!({"generation":command.generation,"op":"Signal","worker_id":worker_id,"signal":signal}),
                    ),
                )
            }
            Operation::Stdin { worker_id, data } => {
                self.worker(&command.run_id, command.generation, worker_id)?;
                (
                    format!("/v1/containers/{}/control", command.run_id),
                    Some(
                        json!({"generation":command.generation,"op":"WriteStdin","worker_id":worker_id,"data":data}),
                    ),
                )
            }
            Operation::Health => (format!("/v1/containers/{}/control",command.run_id),Some(json!({"generation":command.generation,"op":"Health"}))),
            Operation::WorkspaceList{..}|Operation::WorkspaceWrite{..}|Operation::Mailbox{..}|Operation::MailboxReply{..}=>(String::new(),None),
            Operation::Status => (format!("/v1/runs/{}", command.run_id), None),
            Operation::Events { worker_id, after } => {
                self.worker(&command.run_id, command.generation, worker_id)?;
                (
                    format!("/v1/workers/{worker_id}/events?after={after}"),
                    None,
                )
            }
            Operation::TransferPull { .. } | Operation::TransferPush { .. } | Operation::TransferStatus { .. } | Operation::CredentialInstall { .. } | Operation::CredentialRefresh { .. } | Operation::CredentialStatus { .. } => {
                return Err("Transfer commands require the authenticated bulk client".into());
            }
            Operation::Receipt { command_id } => {
                valid_id(command_id)?;
                let row = self
                    .store
                    .get("runner_commands", command_id)
                    .map_err(|e| e.to_string())?
                    .ok_or("Command receipt is missing")?;
                if row["run_id"] != command.run_id || row["generation"] != command.generation {
                    return Err("Receipt belongs to another execution".into());
                }
                (format!("/v1/commands/{command_id}"), None)
            }
        };
        if command.action.mutating() {
            self.store.document_scope(||->Result<(),String>{
                if matches!(command.action,Operation::Ensure{..}){
                    self.store.put("runner_remote_bindings",&command.run_id,&json!({"node_id":command.node_id,"project_id":command.project_id,
                        "run_id":command.run_id,"generation":command.generation,"policy_version":command.policy_version,"runtime_digest":runtime_digest,"credential_materials":match &command.action {Operation::Ensure{settings}=>settings["credentials"].clone(),_=>Value::Null},"state":"preparing"})).map_err(|e|e.to_string())?;
                } else if matches!(command.action,Operation::Stop) {
                    let mut binding=self.store.get("runner_remote_bindings",&command.run_id).map_err(|e|e.to_string())?.ok_or("Missing execution binding")?;
                    binding["state"]=json!("stopping");
                    self.store.put("runner_remote_bindings",&command.run_id,&binding).map_err(|e|e.to_string())?;
                }
                self.store.put("runner_remote_receipts",&command.id,&json!({"id":command.id,"digest":digest,"run_id":command.run_id,"state":"dispatching"})).map_err(|e|e.to_string())
            }).map_err(|e|e.to_string())??;
        }
        let result = if matches!(command.action,Operation::WorkspaceList{..}|Operation::WorkspaceWrite{..}|Operation::Mailbox{..}|Operation::MailboxReply{..}) {
            super::workspace::execute(self,command)?
        } else {crate::runner::exchange(&self.socket, &path, body).await?};
        if matches!(command.action,Operation::Ensure{..}) {
            self.renew_runtime(&command.run_id,command.generation,authority.valid_until).await?;
        }
        if command.action.mutating() {
            self.store.document_scope(||->Result<(),String>{
                self.store.put("runner_remote_receipts",&command.id,&json!({"id":command.id,"digest":digest,"run_id":command.run_id,"state":"completed","result":result})).map_err(|e|e.to_string())?;
                if matches!(command.action,Operation::Ensure{..}|Operation::Stop){
                    let mut binding=self.store.get("runner_remote_bindings",&command.run_id).map_err(|e|e.to_string())?.ok_or("Missing execution binding")?;
                    binding["state"]=json!(if matches!(command.action,Operation::Stop){"stopped"}else{"active"});
                    self.store.put("runner_remote_bindings",&command.run_id,&binding).map_err(|e|e.to_string())?;
                }Ok(())
            }).map_err(|e|e.to_string())??;
        }
        Ok(result)
    }
    async fn renew_runtime(&self,run:&str,generation:u64,deadline:tokio::time::Instant)->Result<(),String>{
        let remaining=deadline.saturating_duration_since(tokio::time::Instant::now()).as_millis().min(30_000) as u64;
        if remaining==0{return Err("Node execution lease expired".into());}
        let expiry=chrono::Utc::now().timestamp_millis().saturating_add(remaining as i64);
        let response=tokio::time::timeout(std::time::Duration::from_secs(3),crate::runner::exchange(&self.socket,
            &format!("/v1/containers/{run}/control"),Some(json!({"generation":generation,"op":"RenewLease","lease_millis":remaining,"lease_expires_at":expiry}))))
            .await.map_err(|_|"Runtime lease renewal timed out".to_string())??;
        if response["ok"]!=true||response["execution_lease"]!=true{return Err("Runtime does not confirm execution lease enforcement".into());}
        Ok(())
    }
    pub async fn renew_runtimes(&self,node:&str,deadline:tokio::time::Instant)->Result<(),String>{
        use futures_util::{stream,StreamExt};
        let _renewal=self.renew_gate.try_lock().map_err(|_|"Previous runtime renewal is still pending".to_string())?;
        let rows=self.store.list("runner_remote_bindings").map_err(|e|e.to_string())?;
        let results=stream::iter(rows.into_iter().filter(|row|row["node_id"]==node&&row["state"]=="active").map(|row|async move{
            let run=row["run_id"].as_str().ok_or("Missing runtime Run")?;
            self.renew_runtime(run,row["generation"].as_u64().ok_or("Missing runtime generation")?,deadline).await
        })).buffer_unordered(8).collect::<Vec<_>>().await;
        for result in results {result?;} Ok(())
    }
    pub(super) fn store(&self) -> &Store {
        &self.store
    }
    pub(super) fn file_scope(
        &self,
        command: &Command,
        authority: &Authority,
        check_window: bool,
    ) -> Result<super::transfers::Scope, String> {
        self.authorize_inner(
            command,
            authority,
            chrono::Utc::now().timestamp(),
            check_window,
        )?;
        let scope = super::transfers::Scope {
            node_id: command.node_id.clone(),
            project_id: command.project_id.clone(),
            run_id: command.run_id.clone(),
            generation: command.generation,
        };
        if matches!(command.action, Operation::TransferPull { .. } | Operation::TransferPush { .. } | Operation::CredentialInstall { .. } | Operation::CredentialRefresh { .. }) {
            let binding = self
                .store
                .get("runner_remote_bindings", &command.run_id)
                .map_err(|e| e.to_string())?
                .ok_or("Missing Run binding")?;
            let archive=matches!(command.action,Operation::TransferPush{..})&&binding["state"]=="stopped";
            if binding["state"] != "active"&&!archive {
                return Err("Run is not accepting workspace files".into());
            }
        }
        Ok(scope)
    }
    pub(super) fn file_workspace(&self, command: &Command) -> Result<PathBuf, String> {
        let contract = self
            .store
            .get("runner_runs", &command.run_id)
            .map_err(|e| e.to_string())?
            .ok_or("Missing runtime contract")?;
        let archive=matches!(command.action,Operation::TransferPush{..}|Operation::WorkspaceList{..})&&contract["state"]=="stopped";
        if contract["generation"] != command.generation || (contract["state"] != "active"&&!archive) {
            return Err("Runtime is not active for this file generation".into());
        }
        let relative = std::path::Path::new(
            contract["workspace_relative"]
                .as_str()
                .ok_or("Runtime workspace identity requires reconciliation")?,
        );
        if relative.is_absolute()
            || relative
                .components()
                .any(|p| !matches!(p, std::path::Component::Normal(_)))
        {
            return Err("Invalid recorded workspace".into());
        }
        let root = self
            .socket
            .parent()
            .and_then(|p| p.parent())
            .ok_or("Runner state root missing")?
            .canonicalize()
            .map_err(|e| e.to_string())?;
        let workspace = root
            .join(relative)
            .canonicalize()
            .map_err(|e| e.to_string())?;
        if !workspace.starts_with(&root) {
            return Err("Recorded workspace leaves node root".into());
        }
        Ok(workspace)
    }
    pub(super) async fn stop_credential_run(&self,run:&str,generation:u64)->Result<(),String>{
        let gate=self.gate(run).await;let _guard=gate.lock().await;
        let mut row=self.store.get("runner_remote_bindings",run).map_err(|e|e.to_string())?.ok_or("Credential Run binding missing")?;
        if row["generation"]!=generation{return Err("Credential belongs to another Run generation".into());}
        if row["state"]=="stopped" {return Ok(());}
        row["state"]=json!("stopping");row["reason"]=json!("credential_authority_expired");
        self.store.put("runner_remote_bindings",run,&row).map_err(|e|e.to_string())?;
        let result=crate::runner::exchange(&self.socket,&format!("/v1/containers/{run}/teardown"),Some(json!({"generation":generation}))).await?;
        row["state"]=json!("stopped");row["stop_result"]=result;
        self.store.put("runner_remote_bindings",run,&row).map_err(|e|e.to_string())?;Ok(())
    }
    /// Stop remote-owned Runs only. Failure remains stopping/unknown and is retried.
    pub async fn expire(&self, node: &str) -> Result<(), String> {
        use futures_util::{StreamExt, stream};
        let rows = self
            .store
            .document_scope(|| -> Result<Vec<Value>, String> {
                let mut pending = Vec::new();
                for mut row in self
                    .store
                    .list("runner_remote_bindings")
                    .map_err(|e| e.to_string())?
                {
                    if row["node_id"] != node || row["state"] == "stopped" {
                        continue;
                    }
                    let run = row["run_id"]
                        .as_str()
                        .ok_or("Remote binding has no Run identity")?
                        .to_owned();
                    valid_id(&run)?;
                    row["state"] = json!("stopping");
                    row["reason"] = json!("remote_lease_expired");
                    self.store
                        .put("runner_remote_bindings", &run, &row)
                        .map_err(|e| e.to_string())?;
                    pending.push(row);
                }
                Ok(pending)
            })
            .map_err(|e| e.to_string())??;
        let results=stream::iter(rows.into_iter().map(|row|async move {
            let run=row["run_id"].as_str().ok_or("Missing Run identity")?;
            let gate=self.gate(run).await;let _guard=gate.lock().await;
            let mut current=self.store.get("runner_remote_bindings",run).map_err(|e|e.to_string())?.ok_or("Binding missing")?;
            if current["generation"]!=row["generation"]||current["state"]=="stopped"{return Ok::<(),String>(());}
            current["state"]=json!("stopping");current["reason"]=json!("remote_lease_expired");
            self.store.put("runner_remote_bindings",run,&current).map_err(|e|e.to_string())?;
            match crate::runner::exchange(&self.socket,&format!("/v1/containers/{run}/teardown"),Some(json!({"generation":row["generation"]}))).await {
                Ok(result)=>{current["state"]=json!("stopped");current["stop_result"]=result;
                    self.store.put("runner_remote_bindings",run,&current).map_err(|e|e.to_string())?;},
                Err(_)=>tracing::warn!(run_id=%run,"Remote lease expired; execution stop remains unconfirmed"),
            }Ok(())
        })).buffer_unordered(8).collect::<Vec<_>>().await;
        for result in results {
            result?;
        }
        Ok(())
    }
}
pub(super) fn valid_solver_id(id:&str)->Result<(),String>{
    if id.is_empty()||id.len()>160||!id.bytes().all(|b|b.is_ascii_alphanumeric()||matches!(b,b'-'|b'_'|b'#')){return Err("Invalid solver identifier".into());}
    Ok(())
}
pub(super) fn valid_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 160
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        Err("Invalid command identifier".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn remote_receipts_enforce_scope_fencing_and_do_not_replay_unknown_actions() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("node.sqlite")).unwrap();
        let processor = Processor::new(store.clone(), dir.path().join("absent.sock"));
        let authority = Authority {
            node_id: "node".into(),
            session_generation: 2,
            credential_version: 1,
            projects: vec!["a".into()],
            valid_until: tokio::time::Instant::now() + std::time::Duration::from_secs(30),
        };
        let mut command = Command {
            id: "ensure".into(),
            node_id: "node".into(),
            project_id: "a".into(),
            run_id: "run".into(),
            generation: 1,
            session_generation: 2,
            credential_version: 1,
            policy_version: 1,
            expires_at: 110,
            action: Operation::Ensure {
                settings: json!({}),
            },
        };
        command.project_id = "other".into();
        assert!(
            processor
                .execute(&command, &authority, 100)
                .await
                .unwrap_err()
                .contains("authority")
        );
        assert!(store.list("runner_remote_receipts").unwrap().is_empty());
        command.project_id = "a".into();
        command.session_generation = 1;
        assert!(processor.execute(&command, &authority, 100).await.is_err());
        command.session_generation = 2;
        assert!(processor.execute(&command, &authority, 111).await.is_err());
        store
            .put(
                "runner_runs",
                "run",
                &json!({"generation":1,"state":"active"}),
            )
            .unwrap();
        assert!(
            processor
                .execute(&command, &authority, 100)
                .await
                .unwrap_err()
                .contains("local execution")
        );
        command.run_id = "remote-run".into();
        assert!(processor.execute(&command, &authority, 100).await.is_err());
        assert!(
            processor
                .execute(&command, &authority, 100)
                .await
                .unwrap_err()
                .contains("unknown")
        );
        let digest = command.digest().unwrap();
        store
            .put(
                "runner_remote_receipts",
                "ensure",
                &json!({"digest":digest,"state":"completed","result":{"confirmed":true}}),
            )
            .unwrap();
        assert_eq!(
            processor.execute(&command, &authority, 100).await.unwrap()["confirmed"],
            true
        );
        command.action = Operation::Ensure {
            settings: json!({"worker_container_scope":"shared"}),
        };
        assert!(
            processor
                .execute(&command, &authority, 100)
                .await
                .unwrap_err()
                .contains("conflicts")
        );
    }
}
