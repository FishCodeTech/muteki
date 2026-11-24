//! Settings-owned TLS endpoint for remote Runners. Saving the configuration
//! stops the previous endpoint (closing its node sessions) before binding the new one.
use super::{tls, transport};
use crate::AppState;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{net::SocketAddr, sync::Arc, time::Duration};

const COLLECTION: &str = "runner_config";
const KEY: &str = "listener";

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub enabled: bool,
    pub listen: String,
    pub public_url: String,
}
struct Running {
    stop: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}
#[derive(Default)]
pub struct Manager {
    slot: tokio::sync::Mutex<Option<Running>>,
    status: std::sync::Mutex<Value>,
    external: std::sync::Mutex<Option<SocketAddr>>,
}
pub fn load(state: &AppState) -> Result<Config, String> {
    match state.store.get(COLLECTION, KEY).map_err(|e| e.to_string())? {
        Some(value) => serde_json::from_value(value).map_err(|e| e.to_string()),
        None => Ok(Config { enabled: false, listen: "0.0.0.0:8443".into(), public_url: String::new() }),
    }
}
fn names(public_url: &str) -> Result<Vec<String>, String> {
    let url = reqwest::Url::parse(public_url).map_err(|_| "Public address must be a URL such as https://runner.example.com:8443".to_owned())?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() || url.path() != "/" {
        return Err("Public address must be an HTTPS origin without userinfo, path, query or fragment".into());
    }
    let host = match url.host().ok_or("Public address needs a host")? {
        url::Host::Domain(name) => name.to_owned(),
        url::Host::Ipv4(ip) => ip.to_string(),
        url::Host::Ipv6(ip) => ip.to_string(),
    };
    Ok(vec![host, "localhost".into(), "127.0.0.1".into()])
}
pub fn validate(config: &Config) -> Result<SocketAddr, String> {
    let address: SocketAddr = config.listen.trim().parse().map_err(|_| "Listen address must be IP:port, for example 0.0.0.0:8443".to_owned())?;
    if config.enabled {
        names(&config.public_url)?;
    }
    Ok(address)
}
impl Manager {
    /// A listener configured through command-line certificates owns the endpoint for this process.
    pub fn set_external(&self, address: SocketAddr) {
        *self.external.lock().unwrap() = Some(address);
        self.record(json!({"running":true,"address":address.to_string(),"trust":"externally_managed",
            "detail":"Use the externally managed server URL and --ca-file for private certificates when enrolling this listener"}));
    }
    pub fn status(&self) -> Value {
        let mut status = self.status.lock().unwrap().clone();
        if status.is_null() {
            status = json!({"running":false});
        }
        status["managed_by_cli"] = json!(self.external.lock().unwrap().map(|a| a.to_string()));
        status
    }
    fn record(&self, value: Value) {
        *self.status.lock().unwrap() = value;
    }
    async fn stop(&self, slot: &mut Option<Running>) {
        if let Some(running) = slot.take() {
            let _ = running.stop.send(true);
            let mut task = running.task;
            if tokio::time::timeout(Duration::from_secs(5), &mut task).await.is_err() {
                task.abort();
            }
        }
    }
    pub async fn apply(&self, state: Arc<AppState>, config: &Config) -> Result<Value, String> {
        if self.external.lock().unwrap().is_some() {
            return Err("The Runner endpoint is managed by --runner-tls-listen for this process".into());
        }
        let mut slot = self.slot.lock().await;
        self.stop(&mut slot).await;
        if !config.enabled {
            self.record(json!({"running":false}));
            return Ok(self.status());
        }
        let result = async {
            let address = validate(config)?;
            let root = crate::native_chat::state_dir(&state);
            let installation = state.store.installation_id().map_err(|e| e.to_string())?;
            let material = tls::ensure(&root, &installation, &names(&config.public_url)?)?;
            let listener = transport::Listener::bind_pem(address, &material.cert_pem, &material.key_pem)
                .await
                .map_err(|e| format!("Could not listen on {address}: {e}"))?;
            let bound = axum::serve::Listener::local_addr(&listener).map_err(|e| e.to_string())?;
            let (stop, receiver) = tokio::sync::watch::channel(false);
            let mut signal = receiver.clone();
            let router = transport::router_with(state.clone(), Some(receiver));
            let task = tokio::spawn(async move {
                let shutdown = async move {
                    let _ = signal.wait_for(|stop| *stop).await;
                };
                if let Err(error) = axum::serve(listener, router).with_graceful_shutdown(shutdown).await {
                    tracing::error!(%error, "Remote Runner listener stopped");
                }
            });
            *slot = Some(Running { stop, task });
            Ok::<Value, String>(json!({"running":true,"address":bound.to_string(),"public_url":config.public_url.trim_end_matches('/'),
                "ca_fingerprint":material.fingerprint,"certificate_names":material.names,"certificate_expires_at":material.expires_at,
                "started_at":chrono::Utc::now().timestamp()}))
        }
        .await;
        match result {
            Ok(status) => {
                tracing::info!(address = %status["address"], "Remote Runner TLS listener bound");
                self.record(status);
                Ok(self.status())
            }
            Err(error) => {
                self.record(json!({"running":false,"error":error}));
                Err(error)
            }
        }
    }
    pub async fn start_saved(&self, state: Arc<AppState>) {
        if self.external.lock().unwrap().is_some() {
            return;
        }
        match load(&state) {
            Ok(config) if config.enabled => {
                if let Err(error) = self.apply(state, &config).await {
                    tracing::error!(%error, "Saved remote Runner listener could not start");
                }
            }
            Ok(_) => {}
            Err(error) => self.record(json!({"running":false,"error":error})),
        }
    }
}
pub fn save(state: &AppState, config: &Config) -> Result<(), String> {
    state
        .store
        .put(COLLECTION, KEY, &serde_json::to_value(config).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}
