//! Codex's native plugin management, driven through one temporary
//! `codex app-server` per listing or mutation.
//!
//! Every discovery request names the context directory in `cwds`: repo
//! marketplaces and the plugins installed from them are found only through
//! `cwds`, and `spawn_server` gives the server no working directory of its own.

use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use smol::channel::Receiver;

use super::{
    describe_rpc_error, enrich_startup_error, initialize, send_json, settle_child_exit,
    spawn_server, stop_child,
};
use crate::process::{ChildOutput, StderrTail};
use crate::{
    AgentError, DeclaredComponents, MarketplaceAction, PluginAction, PluginContext,
    PluginInstallation, PluginListing, PluginOp, PluginOpOutcome, PluginScope, PluginSource,
    PluginSourceKind, ProviderPluginEntry, ProviderPluginMarketplace, Tri, native_message,
};

/// Reading a remote plugin takes the server about two seconds.
const LIST_TIMEOUT: Duration = Duration::from_secs(120);
/// Installs and marketplace adds may clone repositories.
const MUTATION_TIMEOUT: Duration = Duration::from_secs(300);
/// `plugin/read` requests answered concurrently by one server.
const DETAILS_CONCURRENCY: usize = 4;
/// Marketplaces Codex itself ships or syncs from OpenAI.
const OFFICIAL_MARKETPLACES: [&str; 5] = [
    "openai-curated",
    "openai-curated-remote",
    "openai-api-curated",
    "openai-bundled",
    "openai-primary-runtime",
];
/// An upstream race in Codex 0.159.3: servers started at the same moment on
/// a `CODEX_HOME` that was never initialized can exit with this error, as when
/// a session and a plugin listing start together on a new shadow home. It did
/// not recur on an initialized home, so one later start succeeds.
const SQLITE_RACE: &str = "failed to initialize sqlite state runtime";
const SQLITE_RACE_RETRY_DELAY: Duration = Duration::from_millis(500);

pub(crate) async fn list(context: &PluginContext) -> Result<PluginListing, AgentError> {
    with_server(context, LIST_TIMEOUT, async |server| {
        read_listing(server, &context.cwd).await
    })
    .await
}

pub(crate) async fn run(
    context: &PluginContext,
    op: &PluginOp,
) -> Result<PluginOpOutcome, AgentError> {
    with_server(context, MUTATION_TIMEOUT, async |server| {
        let offered = if matches!(op, PluginOp::Install { .. }) {
            let listed = server
                .request("plugin/list", json!({ "cwds": [context.cwd] }))
                .await?;
            parse::<Catalog>("plugin/list", listed)?.marketplaces
        } else {
            Vec::new()
        };
        let (method, params) = op_request(op, &offered)?;
        let result = server.request(method, params).await?;
        Ok(outcome(op, &result))
    })
    .await
}

async fn with_server<T>(
    context: &PluginContext,
    timeout: Duration,
    work: impl AsyncFnOnce(&mut Server) -> Result<T, AgentError>,
) -> Result<T, AgentError> {
    let session = async {
        let mut server = Server::start(context).await?;
        let result = work(&mut server).await;
        server.close(result)
    };
    // Dropping the session on timeout drops the server, which kills it.
    smol::future::or(async { Some(session.await) }, async {
        smol::Timer::after(timeout).await;
        None
    })
    .await
    .unwrap_or_else(|| Err(AgentError::Provider("`codex app-server` timed out".into())))
}

struct Server {
    child: Child,
    stdin: Option<BufWriter<ChildStdin>>,
    lines: Receiver<ChildOutput>,
    stderr_tail: StderrTail,
    next_id: i64,
}

impl Server {
    async fn start(context: &PluginContext) -> Result<Self, AgentError> {
        match Self::start_once(context).await {
            Err(AgentError::Protocol(message)) if message.contains(SQLITE_RACE) => {
                log::warn!("codex app-server lost the first-start sqlite race; retrying once");
                smol::Timer::after(SQLITE_RACE_RETRY_DELAY).await;
                Self::start_once(context).await
            }
            started => started,
        }
    }

    async fn start_once(context: &PluginContext) -> Result<Self, AgentError> {
        let (child, stdin, lines, stderr_tail) =
            spawn_server(context.binary_path.as_deref(), &[], &context.launch_env)?;
        let mut server = Self {
            child,
            stdin: Some(stdin),
            lines,
            stderr_tail,
            next_id: 2,
        };
        let stdin = server.stdin.as_mut().expect("the server has not stopped");
        match initialize(stdin, &server.lines).await {
            Ok(()) => Ok(server),
            Err(error) => server.close(Err(error)),
        }
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, AgentError> {
        self.requests(vec![(method, params)])
            .await?
            .pop()
            .expect("one reply per request")
    }

    /// Sends the whole batch before reading; the server answers concurrently
    /// and in any order. The outer error is the connection's, the inner ones
    /// are each request's native error.
    async fn requests(
        &mut self,
        batch: Vec<(&str, Value)>,
    ) -> Result<Vec<Result<Value, AgentError>>, AgentError> {
        let first = self.next_id;
        let stdin = self.stdin.as_mut().expect("the server has not stopped");
        for (method, params) in &batch {
            send_json(
                stdin,
                &json!({ "id": self.next_id, "method": method, "params": params }),
            )?;
            self.next_id += 1;
        }
        let mut replies: Vec<Option<Result<Value, AgentError>>> =
            batch.iter().map(|_| None).collect();
        let mut pending = replies.len();
        while pending > 0 {
            let line = match self.lines.recv().await {
                Ok(ChildOutput::Line(line)) => line,
                Ok(ChildOutput::Eof) | Err(_) => {
                    return Err(AgentError::Protocol("codex exited before answering".into()));
                }
                Ok(ChildOutput::Error(error)) => return Err(AgentError::Protocol(error)),
            };
            let value: Value = serde_json::from_str(&line).map_err(|error| {
                AgentError::Protocol(format!("invalid JSON from codex: {error}: {line}"))
            })?;
            if value.get("method").is_some() {
                continue;
            }
            let Some(slot) = value
                .get("id")
                .and_then(Value::as_i64)
                .and_then(|id| usize::try_from(id - first).ok())
                .and_then(|index| replies.get_mut(index))
                .filter(|slot| slot.is_none())
            else {
                continue;
            };
            *slot = Some(match value.get("error") {
                Some(error) => Err(AgentError::Provider(describe_rpc_error(error))),
                None => value.get("result").cloned().ok_or_else(|| {
                    AgentError::Protocol(format!("response omitted result: {line}"))
                }),
            });
            pending -= 1;
        }
        Ok(replies.into_iter().flatten().collect())
    }

    /// A broken connection is explained by the process: its exit status and
    /// stderr. Native errors pass through.
    fn close<T>(mut self, result: Result<T, AgentError>) -> Result<T, AgentError> {
        match result {
            Err(error @ (AgentError::Protocol(_) | AgentError::Io(_))) => {
                let status = settle_child_exit(&mut self.child);
                // The stderr tail joins its reader, so the child must be gone.
                self.stop();
                Err(enrich_startup_error(error, status, &mut self.stderr_tail))
            }
            other => other,
        }
    }

    fn stop(&mut self) {
        if let Some(stdin) = self.stdin.take() {
            stop_child(&mut self.child, stdin);
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

fn parse<T: serde::de::DeserializeOwned>(method: &str, value: Value) -> Result<T, AgentError> {
    serde_json::from_value(value)
        .map_err(|error| AgentError::Protocol(format!("unexpected `{method}` result: {error}")))
}

fn discovery_requests(cwd: &Path) -> Vec<(&'static str, Value)> {
    let cwds = json!({ "cwds": [cwd] });
    vec![
        ("plugin/list", cwds.clone()),
        ("plugin/installed", cwds.clone()),
        // Marketplace sources live in the user layer `marketplace/add` writes.
        ("config/read", json!({})),
        ("skills/list", cwds.clone()),
        ("hooks/list", cwds),
    ]
}

async fn read_listing(server: &mut Server, cwd: &Path) -> Result<PluginListing, AgentError> {
    let mut replies = server.requests(discovery_requests(cwd)).await?.into_iter();
    let mut next = || replies.next().expect("one reply per request");
    let listed: Catalog = parse("plugin/list", next()?)?;
    let installed: Catalog = parse("plugin/installed", next()?)?;
    let config = next()?;
    let skills = next()?;
    let hooks = next()?;
    let catalog = merge(listed, installed);
    let mut listing = listing(&catalog, &config, &skills, &hooks);

    let reads = detail_requests(&catalog);
    for chunk in reads.chunks(DETAILS_CONCURRENCY) {
        let batch = chunk
            .iter()
            .map(|(_, params)| ("plugin/read", params.clone()))
            .collect();
        let replies = server.requests(batch).await?;
        for ((id, _), reply) in chunk.iter().zip(replies) {
            let Some(entry) = listing.entries.iter_mut().find(|entry| entry.id == *id) else {
                continue;
            };
            match reply.and_then(|result| parse::<Read>("plugin/read", result)) {
                Ok(read) => apply_detail(entry, read.plugin),
                Err(error) => entry
                    .diagnostics
                    .push(("details_error".into(), native_message(&error))),
            }
        }
    }
    Ok(listing)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Catalog {
    marketplaces: Vec<Marketplace>,
    #[serde(default)]
    marketplace_load_errors: Vec<LoadError>,
}

#[derive(Deserialize)]
struct Marketplace {
    name: String,
    /// The marketplace manifest; remote catalogs have none.
    #[serde(default)]
    path: Option<PathBuf>,
    #[serde(default)]
    plugins: Vec<Summary>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoadError {
    marketplace_path: String,
    message: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Summary {
    id: String,
    name: String,
    #[serde(default)]
    version: Option<String>,
    /// The manifest version of a local plugin, whose `version` is null.
    #[serde(default)]
    local_version: Option<String>,
    #[serde(default)]
    source: Value,
    installed: bool,
    enabled: bool,
    #[serde(default)]
    install_policy: Option<String>,
    #[serde(default)]
    auth_policy: Option<String>,
    #[serde(default)]
    availability: Option<String>,
    #[serde(default)]
    disabled_reason: Option<String>,
    #[serde(default)]
    interface: Option<Interface>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Interface {
    #[serde(default)]
    short_description: Option<String>,
}

#[derive(Deserialize)]
struct Read {
    plugin: Detail,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Detail {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    skills: Vec<Named>,
    #[serde(default)]
    hooks: Vec<DeclaredHook>,
    #[serde(default)]
    mcp_servers: Vec<String>,
    #[serde(default)]
    apps: Vec<Named>,
}

#[derive(Deserialize)]
struct Named {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeclaredHook {
    event_name: String,
}

/// `plugin/installed` can report plugins `plugin/list` omits (an installed
/// plugin of a local catalog the remote catalog replaces in the listing), so
/// the catalog is their union; the installed summary wins.
fn merge(listed: Catalog, installed: Catalog) -> Catalog {
    let mut merged = listed;
    for marketplace in installed.marketplaces {
        let Some(known) = merged
            .marketplaces
            .iter_mut()
            .find(|known| known.name == marketplace.name)
        else {
            merged.marketplaces.push(marketplace);
            continue;
        };
        if known.path.is_none() {
            known.path = marketplace.path;
        }
        for plugin in marketplace.plugins {
            match known.plugins.iter_mut().find(|known| known.id == plugin.id) {
                Some(known) => *known = plugin,
                None => known.plugins.push(plugin),
            }
        }
    }
    for error in installed.marketplace_load_errors {
        if !merged.marketplace_load_errors.iter().any(|known| {
            known.marketplace_path == error.marketplace_path && known.message == error.message
        }) {
            merged.marketplace_load_errors.push(error);
        }
    }
    merged
}

fn listing(catalog: &Catalog, config: &Value, skills: &Value, hooks: &Value) -> PluginListing {
    let configured = config
        .pointer("/config/marketplaces")
        .and_then(Value::as_object);
    let configured_field = |name: &str, field: &str| {
        configured
            .and_then(|marketplaces| marketplaces.get(name))
            .and_then(|marketplace| marketplace.get(field))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let mut marketplaces = Vec::new();
    let mut entries = Vec::new();
    for marketplace in &catalog.marketplaces {
        let kind = marketplace_kind(
            &marketplace.name,
            marketplace.path.is_some(),
            configured_field(&marketplace.name, "source_type").as_deref(),
        );
        let location = marketplace.path.as_deref().and_then(marketplace_root);
        marketplaces.push(ProviderPluginMarketplace {
            name: marketplace.name.clone(),
            source: configured_field(&marketplace.name, "source")
                .or_else(|| location.as_ref().map(|root| root.display().to_string()))
                .or_else(|| {
                    marketplace
                        .path
                        .as_ref()
                        .map(|path| path.display().to_string())
                })
                .unwrap_or_else(|| marketplace.name.clone()),
            kind,
            location,
        });
        for plugin in &marketplace.plugins {
            entries.push(entry(plugin, &marketplace.name, kind, skills, hooks));
        }
    }
    // A configured marketplace that failed to load is still removable.
    for name in configured
        .into_iter()
        .flat_map(|marketplaces| marketplaces.keys())
    {
        if !marketplaces
            .iter()
            .any(|marketplace| marketplace.name == *name)
        {
            marketplaces.push(ProviderPluginMarketplace {
                name: name.clone(),
                source: configured_field(name, "source").unwrap_or_default(),
                kind: marketplace_kind(
                    name,
                    true,
                    configured_field(name, "source_type").as_deref(),
                ),
                location: None,
            });
        }
    }

    // `marketplace/remove` leaves the marketplace's installed plugins
    // installed and active but no longer listed, so it is offered only once
    // none are installed.
    let mut marketplace_actions = vec![MarketplaceAction::Add];
    for marketplace in &marketplaces {
        let removable = configured
            .is_some_and(|configured| configured.contains_key(&marketplace.name))
            && marketplace.kind != PluginSourceKind::Official
            && !entries.iter().any(|entry| {
                !entry.installations.is_empty()
                    && entry.source.marketplace.as_deref() == Some(marketplace.name.as_str())
            });
        if removable {
            marketplace_actions.push(MarketplaceAction::Remove {
                marketplace: marketplace.name.clone(),
                uninstalls: Vec::new(),
            });
        }
    }
    PluginListing {
        entries,
        marketplaces,
        marketplace_actions,
        errors: catalog
            .marketplace_load_errors
            .iter()
            .map(|error| format!("{}: {}", error.marketplace_path, error.message))
            .collect(),
    }
}

fn marketplace_kind(name: &str, local: bool, source_type: Option<&str>) -> PluginSourceKind {
    if OFFICIAL_MARKETPLACES.contains(&name) {
        return PluginSourceKind::Official;
    }
    match source_type {
        Some("git") => PluginSourceKind::Git,
        Some("local") => PluginSourceKind::LocalPath,
        Some(_) => PluginSourceKind::Unknown,
        // Unconfigured marketplaces are files found through `cwds` or HOME.
        None if local => PluginSourceKind::LocalPath,
        None => PluginSourceKind::Unknown,
    }
}

/// The directory holding `.agents/plugins/<manifest>.json`.
fn marketplace_root(manifest: &Path) -> Option<PathBuf> {
    let plugins = manifest.parent()?;
    let agents = plugins.parent()?;
    (plugins.file_name()? == "plugins" && agents.file_name()? == ".agents")
        .then(|| agents.parent().map(Path::to_path_buf))
        .flatten()
}

fn entry(
    plugin: &Summary,
    marketplace: &str,
    marketplace_kind: PluginSourceKind,
    skills: &Value,
    hooks: &Value,
) -> ProviderPluginEntry {
    let kind = match plugin.source.get("type").and_then(Value::as_str) {
        Some("git") => PluginSourceKind::Git,
        Some("npm") => PluginSourceKind::Npm,
        _ => marketplace_kind,
    };
    let installations = if plugin.installed {
        vec![PluginInstallation {
            scope: PluginScope::User,
            location: cache_root(plugin, marketplace, skills, hooks),
            version: plugin.local_version.clone().or(plugin.version.clone()),
            scope_enabled: Some(plugin.enabled),
        }]
    } else {
        Vec::new()
    };
    let mut diagnostics = Vec::new();
    for (key, value) in [
        ("install_policy", &plugin.install_policy),
        ("auth_policy", &plugin.auth_policy),
        ("availability", &plugin.availability),
        ("disabled_reason", &plugin.disabled_reason),
    ] {
        if let Some(value) = value {
            diagnostics.push((key.to_string(), value.clone()));
        }
    }
    ProviderPluginEntry {
        id: plugin.id.clone(),
        name: plugin.name.clone(),
        version: plugin.version.clone().or(plugin.local_version.clone()),
        description: plugin
            .interface
            .as_ref()
            .and_then(|interface| interface.short_description.clone()),
        source: PluginSource {
            marketplace: Some(marketplace.to_string()),
            kind,
        },
        installations,
        enabled: if plugin.installed {
            Tri::from(plugin.enabled)
        } else {
            Tri::Unknown
        },
        declared: plugin.installed.then(|| DeclaredComponents {
            // A disabled plugin's hooks are not listed at all.
            untrusted_hooks: plugin.enabled.then(|| untrusted_hooks(&plugin.id, hooks)),
            ..DeclaredComponents::default()
        }),
        errors: Vec::new(),
        actions: actions(plugin),
        diagnostics,
    }
}

fn listed<'a>(reply: &'a Value, field: &'a str) -> impl Iterator<Item = &'a Value> {
    reply
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(move |cwd| cwd.get(field).and_then(Value::as_array))
        .flatten()
}

fn of_plugin<'a>(item: &'a Value, id: &str) -> Option<&'a Value> {
    (item.get("pluginId").and_then(Value::as_str) == Some(id)).then_some(item)
}

/// Codex runs a plugin hook only once it is reviewed natively; `untrusted`
/// and `modified` hooks are skipped until then.
fn untrusted_hooks(id: &str, hooks: &Value) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for hook in listed(hooks, "hooks").filter_map(|hook| of_plugin(hook, id)) {
        let untrusted = matches!(
            hook.get("trustStatus").and_then(Value::as_str),
            Some("untrusted" | "modified")
        );
        if let Some(event) = hook.get("eventName").and_then(Value::as_str)
            && untrusted
            && !names.iter().any(|name| name == event)
        {
            names.push(event.to_string());
        }
    }
    names
}

/// The installed copy, `…/plugins/cache/<marketplace>/<plugin>/<version>`,
/// as the plugin's listed skills and hooks locate it.
fn cache_root(
    plugin: &Summary,
    marketplace: &str,
    skills: &Value,
    hooks: &Value,
) -> Option<PathBuf> {
    let paths = listed(skills, "skills")
        .filter_map(|skill| of_plugin(skill, &plugin.id)?.get("path"))
        .chain(
            listed(hooks, "hooks")
                .filter_map(|hook| of_plugin(hook, &plugin.id)?.get("sourcePath")),
        )
        .filter_map(Value::as_str);
    for path in paths {
        let components: Vec<_> = Path::new(path).components().collect();
        let Some(at) = components.windows(4).position(|window| {
            window[0].as_os_str() == "plugins"
                && window[1].as_os_str() == "cache"
                && window[2].as_os_str() == marketplace
                && window[3].as_os_str() == plugin.name.as_str()
        }) else {
            continue;
        };
        if components.len() > at + 4 {
            return Some(components[..at + 5].iter().collect());
        }
    }
    None
}

fn actions(plugin: &Summary) -> Vec<PluginAction> {
    let scope = PluginScope::User;
    if !plugin.installed {
        let installable = plugin.install_policy.as_deref() != Some("NOT_AVAILABLE")
            && plugin.availability.as_deref().unwrap_or("AVAILABLE") == "AVAILABLE";
        return if installable {
            vec![PluginAction::Install { scope }]
        } else {
            Vec::new()
        };
    }
    vec![
        PluginAction::Uninstall { scope },
        if plugin.enabled {
            PluginAction::Disable { scope }
        } else {
            PluginAction::Enable { scope }
        },
    ]
}

/// Local catalogs are read in full so components are disclosed before an
/// install; a remote catalog only for its installed plugins.
fn detail_requests(catalog: &Catalog) -> Vec<(String, Value)> {
    let mut requests = Vec::new();
    for marketplace in &catalog.marketplaces {
        for plugin in &marketplace.plugins {
            let params = match &marketplace.path {
                Some(path) => json!({ "marketplacePath": path, "pluginName": plugin.name }),
                None if plugin.installed => {
                    json!({ "remoteMarketplaceName": marketplace.name, "pluginName": plugin.name })
                }
                None => continue,
            };
            requests.push((plugin.id.clone(), params));
        }
    }
    requests
}

fn apply_detail(entry: &mut ProviderPluginEntry, detail: Detail) {
    if let Some(description) = detail.description.filter(|text| !text.trim().is_empty()) {
        entry.description = Some(description);
    }
    let mut hooks: Vec<String> = Vec::new();
    for hook in detail.hooks {
        if !hooks.contains(&hook.event_name) {
            hooks.push(hook.event_name);
        }
    }
    let declared = entry
        .declared
        .get_or_insert_with(DeclaredComponents::default);
    declared.skills = Some(detail.skills.into_iter().map(|skill| skill.name).collect());
    declared.hooks = Some(hooks);
    declared.mcp_servers = Some(detail.mcp_servers);
    declared.apps = Some(detail.apps.into_iter().map(|app| app.name).collect());
}

/// The request for one mutation. `offered` is the current `plugin/list`,
/// which names the marketplace an install reads from. Hook trust is never
/// bypassed: it stays a native review.
fn op_request(op: &PluginOp, offered: &[Marketplace]) -> Result<(&'static str, Value), AgentError> {
    let request = match op {
        PluginOp::Install { id, scope, .. } => {
            user_scope(*scope)?;
            let (marketplace, plugin) = offered
                .iter()
                .find_map(|marketplace| {
                    let plugin = marketplace.plugins.iter().find(|plugin| plugin.id == *id)?;
                    Some((marketplace, plugin))
                })
                .ok_or_else(|| AgentError::Provider(format!("no marketplace offers {id}")))?;
            let params = match &marketplace.path {
                Some(path) => json!({ "marketplacePath": path, "pluginName": plugin.name }),
                None => {
                    json!({ "remoteMarketplaceName": marketplace.name, "pluginName": plugin.name })
                }
            };
            ("plugin/install", params)
        }
        PluginOp::Uninstall { id, scope } => {
            user_scope(*scope)?;
            ("plugin/uninstall", json!({ "pluginId": id }))
        }
        PluginOp::SetEnabled { id, scope, enabled } => {
            user_scope(*scope)?;
            (
                "config/value/write",
                json!({
                    "keyPath": enabled_key_path(id)?,
                    "value": enabled,
                    "mergeStrategy": "upsert",
                }),
            )
        }
        PluginOp::Update { .. } => {
            return Err(AgentError::Provider(
                "Codex has no per-plugin update".into(),
            ));
        }
        PluginOp::AddMarketplace { source } => ("marketplace/add", json!({ "source": source })),
        PluginOp::RemoveMarketplace { name } => {
            ("marketplace/remove", json!({ "marketplaceName": name }))
        }
    };
    Ok(request)
}

/// Codex installs and enables plugins only in the user's `config.toml`.
fn user_scope(scope: PluginScope) -> Result<(), AgentError> {
    match scope {
        PluginScope::User => Ok(()),
        other => Err(AgentError::Protocol(format!(
            "Codex plugins have no {other:?} scope"
        ))),
    }
}

/// `config/value/write` splits the key path on dots outside quotes.
fn enabled_key_path(id: &str) -> Result<String, AgentError> {
    if id.contains(['"', '\\']) {
        return Err(AgentError::Protocol(format!(
            "refusing to write a config key for {id:?}"
        )));
    }
    Ok(if id.contains('.') {
        format!("plugins.\"{id}\".enabled")
    } else {
        format!("plugins.{id}.enabled")
    })
}

fn outcome(op: &PluginOp, result: &Value) -> PluginOpOutcome {
    let mut diagnostics = Vec::new();
    match op {
        PluginOp::Install { .. } => {
            let apps: Vec<String> = result
                .get("appsNeedingAuth")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|app| {
                    let name = app.get("name").and_then(Value::as_str)?;
                    Some(match app.get("installUrl").and_then(Value::as_str) {
                        Some(url) => format!("{name} ({url})"),
                        None => name.to_string(),
                    })
                })
                .collect();
            if !apps.is_empty() {
                diagnostics.push(("apps_needing_auth".into(), apps.join(", ")));
            }
        }
        PluginOp::AddMarketplace { .. }
            if result.get("alreadyAdded").and_then(Value::as_bool) == Some(true) =>
        {
            diagnostics.push(("already_added".into(), "true".into()));
        }
        _ => {}
    }
    PluginOpOutcome::Done { diagnostics }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frame with JSON-RPC `id` in a recording: the request when
    /// `request`, otherwise its response's `result`.
    fn frame(name: &str, id: i64, request: bool) -> Value {
        let path = format!(
            "{}/tests/fixtures/plugins/codex/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
        let frame = text
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|frame| frame["id"] == id && frame.get("method").is_some() == request)
            .unwrap_or_else(|| panic!("{name} has no frame {id}"));
        if request {
            frame
        } else {
            frame["result"].clone()
        }
    }

    fn result(name: &str, id: i64) -> Value {
        frame(name, id, false)
    }

    fn recorded(name: &str, id: i64) -> Catalog {
        parse("recorded", result(name, id)).unwrap()
    }

    const ALPHA: &str = "alpha@tcode-probe-mkt";

    fn names(names: &[&str]) -> Option<Vec<String>> {
        Some(names.iter().map(|name| name.to_string()).collect())
    }

    fn policies(auth: &str) -> Vec<(String, String)> {
        [
            ("install_policy", "AVAILABLE"),
            ("auth_policy", auth),
            ("availability", "AVAILABLE"),
        ]
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .to_vec()
    }

    /// Recordings: `plugin/list {cwds:[proj]}` before the install (A#4), then
    /// `plugin/installed`, `skills/list` and `hooks/list` once alpha was
    /// installed and enabled (A#10–12), `plugin/read alpha` (A#5).
    #[test]
    fn listing_unions_installed_state_with_details_and_hook_trust() {
        let catalog = merge(
            recorded("plugin-list-with-cwds.jsonl", 4),
            recorded("plugin-installed.jsonl", 10),
        );
        let config = result("config-read.jsonl", 3);
        let mut listing = listing(
            &catalog,
            &config,
            &result("skills-list.jsonl", 11),
            &result("hooks-list.jsonl", 12),
        );
        let reads = detail_requests(&catalog);
        assert_eq!(
            reads.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            [ALPHA, "beta@tcode-probe-mkt", "gamma@tcode-probe-repo"]
        );
        assert_eq!(reads[0].1, frame("plugin-read.jsonl", 5, true)["params"]);
        let read: Read = parse("plugin/read", result("plugin-read.jsonl", 5)).unwrap();
        apply_detail(&mut listing.entries[0], read.plugin);

        let user = PluginScope::User;
        assert_eq!(
            listing.entries[0],
            ProviderPluginEntry {
                id: ALPHA.into(),
                name: "alpha".into(),
                version: Some("0.1.0".into()),
                description: Some(
                    "Tcode probe plugin alpha: one skill, one MCP server, one hook.".into()
                ),
                source: PluginSource {
                    marketplace: Some("tcode-probe-mkt".into()),
                    kind: PluginSourceKind::LocalPath,
                },
                installations: vec![PluginInstallation {
                    scope: user,
                    location: Some(PathBuf::from(
                        "/private/tmp/tcode-probe/codex-home/plugins/cache/tcode-probe-mkt/alpha/0.1.0"
                    )),
                    version: Some("0.1.0".into()),
                    scope_enabled: Some(true),
                }],
                enabled: Tri::Yes,
                declared: Some(DeclaredComponents {
                    skills: names(&["alpha:alpha-hello"]),
                    agents: None,
                    hooks: names(&["sessionStart"]),
                    mcp_servers: names(&["alpha-docs"]),
                    untrusted_hooks: names(&["sessionStart"]),
                    lsp_servers: None,
                    apps: names(&[]),
                }),
                errors: Vec::new(),
                actions: vec![
                    PluginAction::Uninstall { scope: user },
                    PluginAction::Disable { scope: user },
                ],
                diagnostics: policies("ON_INSTALL"),
            }
        );
        let beta = &listing.entries[1];
        assert!(beta.installations.is_empty());
        assert_eq!(beta.version.as_deref(), Some("0.2.0"));
        assert_eq!(beta.description.as_deref(), Some("Skill-only probe plugin"));
        assert_eq!(beta.enabled, Tri::Unknown);
        assert_eq!(beta.declared, None);
        assert_eq!(beta.actions, [PluginAction::Install { scope: user }]);
        assert_eq!(beta.diagnostics, policies("ON_USE"));
        assert_eq!(
            listing.entries[2].source,
            PluginSource {
                marketplace: Some("tcode-probe-repo".into()),
                kind: PluginSourceKind::LocalPath,
            }
        );

        assert_eq!(
            listing.marketplaces,
            [
                ProviderPluginMarketplace {
                    name: "tcode-probe-mkt".into(),
                    source: "/private/tmp/tcode-probe/codex-mkt".into(),
                    kind: PluginSourceKind::LocalPath,
                    location: Some(PathBuf::from("/private/tmp/tcode-probe/codex-mkt")),
                },
                ProviderPluginMarketplace {
                    name: "tcode-probe-repo".into(),
                    source: "/tmp/tcode-probe/codex-proj".into(),
                    kind: PluginSourceKind::LocalPath,
                    location: Some(PathBuf::from("/tmp/tcode-probe/codex-proj")),
                },
            ]
        );
        // alpha is installed, and the repo marketplace is not configured.
        assert_eq!(listing.marketplace_actions, [MarketplaceAction::Add]);
        assert!(listing.errors.is_empty());

        // After `plugin/uninstall` (A#25) nothing from the marketplace is installed.
        let uninstalled = merge(
            recorded("plugin-list-with-cwds.jsonl", 4),
            recorded("plugin-installed.jsonl", 25),
        );
        let after = super::listing(
            &uninstalled,
            &config,
            &result("skills-list.jsonl", 16),
            &result("hooks-list.jsonl", 18),
        );
        assert_eq!(
            after.marketplace_actions,
            [
                MarketplaceAction::Add,
                MarketplaceAction::Remove {
                    marketplace: "tcode-probe-mkt".into(),
                    uninstalls: Vec::new(),
                },
            ]
        );
    }

    /// Recordings after `config/value/write enabled=false`: `plugin/installed`
    /// (A#14), `skills/list` (A#16) and `hooks/list` (A#18), from which the
    /// disabled plugin is absent.
    #[test]
    fn disabled_plugin_offers_enable_and_claims_no_hook_trust() {
        let catalog = merge(
            recorded("plugin-list-with-cwds.jsonl", 4),
            recorded("plugin-installed-after-disable.jsonl", 14),
        );
        let listing = listing(
            &catalog,
            &result("config-read.jsonl", 3),
            &result("skills-list.jsonl", 16),
            &result("hooks-list.jsonl", 18),
        );
        let alpha = &listing.entries[0];
        assert_eq!(alpha.enabled, Tri::No);
        assert_eq!(
            alpha.installations,
            [PluginInstallation {
                scope: PluginScope::User,
                location: None,
                version: Some("0.1.0".into()),
                scope_enabled: Some(false),
            }]
        );
        assert_eq!(alpha.declared.as_ref().unwrap().untrusted_hooks, None);
        assert_eq!(
            alpha.actions,
            [
                PluginAction::Uninstall {
                    scope: PluginScope::User
                },
                PluginAction::Enable {
                    scope: PluginScope::User
                },
            ]
        );
    }

    #[test]
    fn requests_match_the_recorded_frames_name_the_context_and_never_bypass_trust() {
        let cwd = Path::new("/tmp/tcode-probe/codex-proj");
        for (method, params) in discovery_requests(cwd) {
            if method != "config/read" {
                assert_eq!(params["cwds"], json!([cwd]), "{method}");
            }
        }

        let offered = recorded("plugin-list-with-cwds.jsonl", 4).marketplaces;
        let user = PluginScope::User;
        let cases = [
            (
                PluginOp::Install {
                    id: ALPHA.into(),
                    scope: user,
                    accept_command: Some("not a Codex concept".into()),
                },
                "plugin-install.jsonl",
                8,
            ),
            (
                PluginOp::SetEnabled {
                    id: ALPHA.into(),
                    scope: user,
                    enabled: false,
                },
                "config-value-write-enabled.jsonl",
                13,
            ),
            (
                PluginOp::Uninstall {
                    id: ALPHA.into(),
                    scope: user,
                },
                "plugin-uninstall.jsonl",
                23,
            ),
            (
                PluginOp::AddMarketplace {
                    source: "/tmp/tcode-probe/codex-mkt".into(),
                },
                "marketplace-add.jsonl",
                2,
            ),
            (
                PluginOp::RemoveMarketplace {
                    name: "tcode-probe-mkt".into(),
                },
                "marketplace-remove.jsonl",
                28,
            ),
        ];
        for (op, name, id) in cases {
            let (method, params) = op_request(&op, &offered).unwrap();
            let recorded = frame(name, id, true);
            assert_eq!(method, recorded["method"], "{op:?}");
            assert_eq!(params, recorded["params"], "{op:?}");
            let wire = params.to_string();
            assert!(
                !wire.contains("dangerously") && !wire.contains("bypass"),
                "{wire}"
            );
        }
        // A dotted id stays one key segment (written live as `[plugins."a.b@m"]`).
        assert_eq!(
            enabled_key_path("a.b@m").unwrap(),
            "plugins.\"a.b@m\".enabled"
        );
    }

    /// A `codex` that records each start and dies at startup with `$FAILURE`
    /// on stderr, as 0.159.3 does when it loses the first-start sqlite race.
    #[cfg(unix)]
    #[test]
    fn only_the_first_start_sqlite_race_is_retried_once() {
        use std::os::unix::fs::PermissionsExt;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("agent-codex-plugins-{nonce}"));
        std::fs::create_dir_all(&dir).unwrap();
        let starts = dir.join("starts");
        let bin = dir.join("codex");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\necho start >> '{}'\necho \"Error: $FAILURE\" >&2\nexit 1\n",
                starts.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let attempt = |failure: &str| {
            let context = PluginContext {
                binary_path: Some(bin.clone()),
                launch_env: crate::LaunchEnv {
                    env: vec![("FAILURE".into(), failure.into())],
                    home: None,
                },
                cwd: dir.clone(),
                project: false,
            };
            // Concurrently forked test children can hold the fresh script
            // open, so exec fails with ETXTBSY until they exit.
            for _ in 0..20 {
                let _ = std::fs::remove_file(&starts);
                let message = smol::block_on(list(&context)).unwrap_err().to_string();
                if !message.contains("Text file busy") {
                    let count = std::fs::read_to_string(&starts).unwrap().lines().count();
                    return (message, count);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            panic!("the fake codex never started");
        };

        let (message, starts_made) =
            attempt("failed to initialize sqlite state runtime under /tmp/fresh-home");
        assert_eq!(starts_made, 2);
        assert!(message.contains(SQLITE_RACE), "{message}");
        assert!(message.contains("exit status: 1"), "{message}");

        let (message, starts_made) = attempt("Cannot find module './dist/cli.js'");
        assert_eq!(starts_made, 1);
        assert!(message.contains("Cannot find module"), "{message}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
