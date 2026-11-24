//! Bounded live command delivery. Durable effects are deduplicated at the node.
use super::{commands::Command, identity::Lease};
use crate::AppState;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

pub struct Job {
    pub command: Command,
    pub reply: oneshot::Sender<Result<Value, String>>,
}
struct Connection {
    id: String,
    generation: u64,
    credential_version: u64,
    lease: Lease,
    send: mpsc::Sender<Job>,
}
#[derive(Default)]
pub struct Registry {
    connections: Mutex<HashMap<String, Connection>>,
}
pub struct Guard {
    node: String,
    id: String,
    registry: Arc<Registry>,
}
impl Drop for Guard {
    fn drop(&mut self) {
        let mut map = self.registry.connections.lock().unwrap();
        if map.get(&self.node).is_some_and(|slot| slot.id == self.id) {
            map.remove(&self.node);
        }
    }
}
impl Registry {
    pub fn attach(self: &Arc<Self>, lease: &Lease) -> Result<(Guard, mpsc::Receiver<Job>), String> {
        let mut map = self.connections.lock().unwrap();
        if map.contains_key(&lease.node_id) {
            return Err("Node already connected".into());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let (send, receive) = mpsc::channel(16);
        map.insert(
            lease.node_id.clone(),
            Connection {
                id: id.clone(),
                generation: lease.generation,
                credential_version: lease.credential_version,
                lease: lease.clone(),
                send,
            },
        );
        Ok((
            Guard {
                node: lease.node_id.clone(),
                id,
                registry: self.clone(),
            },
            receive,
        ))
    }
    pub fn online(&self, node: &str) -> bool {
        self.connections.lock().unwrap().contains_key(node)
    }
    pub async fn request(&self, state: &AppState, command: Command) -> Result<Value, String> {
        self.request_stored(&state.store,&crate::native_chat::state_dir(state),command).await
    }
    pub async fn request_stored(&self, store: &crate::store::Store, root:&std::path::Path, mut command: Command) -> Result<Value, String> {
        let (sender, generation, credential_version, lease) = {
            let map = self.connections.lock().unwrap();
            let slot = map
                .get(&command.node_id)
                .ok_or("Node is disconnected; no command was sent")?;
            (
                slot.send.clone(),
                slot.generation,
                slot.credential_version,
                slot.lease.clone(),
            )
        };
        command.session_generation = generation;
        command.credential_version = credential_version;
        let now = chrono::Utc::now().timestamp();
        if command.expires_at <= now || command.expires_at > now + 120 {
            return Err("Command validity must be within 120 seconds".into());
        }
        super::identity::authorize(
            store,
            &lease,
            &command.project_id,
            command.action.new_work(),
            now,
        )?;
        if let super::commands::Operation::TransferPull { transfer_id } = &command.action {
            let source = store
                .get("runner_transfers", transfer_id)
                .map_err(|e| e.to_string())?
                .ok_or("Unknown source transfer")?;
            let scope = super::transfers::Scope {
                node_id: command.node_id.clone(),
                project_id: command.project_id.clone(),
                run_id: command.run_id.clone(),
                generation: command.generation,
            };
            if source["manifest"]["scope"] != json!(scope) || source["state"] != "verified" {
                return Err("Source transfer is not verified for this execution scope".into());
            }
            super::files::verified_source_store(store,&serde_json::from_value(source["manifest"].clone()).map_err(|e|e.to_string())?)?;
        }
        if let super::commands::Operation::CredentialInstall{grant_id,..}|super::commands::Operation::CredentialRefresh{grant_id}=&command.action {
            let vault=super::credentials::Vault::open(store.clone(),root)?;
            let grant=vault.authorize(grant_id,&lease,now)?;
            if let super::commands::Operation::CredentialInstall{revision,..}=&command.action {if *revision!=grant.revision{return Err("Credential grant revision no longer matches installation".into());}}
            if grant.scope.run_id!=command.run_id||grant.scope.project_id!=command.project_id||grant.scope.generation!=command.generation{return Err("Credential grant does not belong to this execution".into());}
        }
        super::files::prepare_grant_store(store, &command)?;
        let key = format!("{}:{}", command.node_id, command.id);
        let digest = command.digest()?;
        if command.action.mutating() {
            let cached=store.document_scope(||->Result<Option<Value>,String>{
                if let Some(row)=store.get("runner_remote_dispatches",&key).map_err(|e|e.to_string())?{
                    if row["digest"]!=digest{return Err("Remote command ID has conflicting inputs".into());}
                    if row["state"]=="completed"{return Ok(Some(row["result"].clone()));}
                }
                store.put("runner_remote_dispatches",&key,&json!({"id":command.id,"node_id":command.node_id,
                    "run_id":command.run_id,"project_id":command.project_id,"generation":command.generation,"digest":digest,"state":"pending"})).map_err(|e|e.to_string())?;
                Ok(None)
            }).map_err(|e|e.to_string())??;
            if let Some(result) = cached {
                return Ok(result);
            }
        }
        let mutating = command.action.mutating();
        let (reply, receive) = oneshot::channel();
        sender.try_send(Job { command, reply }).map_err(
            |_| "Node queue is full or disconnected; query the original receipt before retrying",
        )?;
        let result = tokio::time::timeout(Duration::from_secs(130), receive)
            .await
            .map_err(|_| "Command result timed out; execution outcome is unknown")?
            .map_err(|_| "Node disconnected before replying; execution outcome is unknown")??;
        if mutating {
            store
                .document_scope(|| -> Result<(), String> {
                    let mut row = store
                        .get("runner_remote_dispatches", &key)
                        .map_err(|e| e.to_string())?
                        .ok_or("Dispatch receipt missing")?;
                    row["state"] = json!("completed");
                    row["result"] = result.clone();
                    store
                        .put("runner_remote_dispatches", &key, &row)
                        .map_err(|e| e.to_string())
                })
                .map_err(|e| e.to_string())??;
        }
        Ok(result)
    }
}
