//! Claude Code's native plugin management, driven through `claude plugin …`
//! with one bounded child process per command.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::{
    AgentError, CommandAcceptance, DeclaredComponents, MarketplaceAction, PluginAction,
    PluginContext, PluginInstallation, PluginListing, PluginOp, PluginOpOutcome, PluginScope,
    PluginSource, PluginSourceKind, ProviderKind, ProviderPluginEntry, ProviderPluginMarketplace,
    Tri, native_message,
};

const LIST_TIMEOUT: Duration = Duration::from_secs(60);
/// Installs and marketplace adds may clone repositories.
const MUTATION_TIMEOUT: Duration = Duration::from_secs(300);
const DETAILS_CONCURRENCY: usize = 4;
const OFFICIAL_MARKETPLACE: &str = "claude-plugins-official";
const OFFICIAL_MARKETPLACE_REPO: &str = "anthropics/claude-plugins-official";

/// `CLAUDECODE` and `CLAUDE_CODE_CHILD_SESSION` make the CLI ignore
/// `--accept-command` ("ignored inside a Claude Code session"), and Tcode is
/// often started from a shell that Claude Code spawned. `CLAUDE_CODE_ENTRYPOINT`
/// is stripped for the same nesting reason as sessions.
const NESTED_SESSION_VARS: [&str; 3] = [
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_CHILD_SESSION",
];

pub(crate) async fn list(context: &PluginContext) -> Result<PluginListing, AgentError> {
    let (plugins, marketplaces) = smol::future::zip(
        run_cli(
            context,
            &args(&["plugin", "list", "--json", "--available"]),
            LIST_TIMEOUT,
        ),
        run_cli(
            context,
            &args(&["plugin", "marketplace", "list", "--json"]),
            LIST_TIMEOUT,
        ),
    )
    .await;
    let plugins = parse_listed(&succeeded(plugins?)?)?;
    let marketplaces = parse_marketplaces(&succeeded(marketplaces?)?)?;
    let in_context = InContext::new(context);
    let mut listing = listing(plugins, marketplaces, &in_context);

    let installed: Vec<String> = listing
        .entries
        .iter()
        .filter(|entry| !entry.installations.is_empty())
        .map(|entry| entry.id.clone())
        .collect();
    let mut details = HashMap::new();
    for chunk in installed.chunks(DETAILS_CONCURRENCY) {
        let tasks: Vec<_> = chunk
            .iter()
            .map(|id| {
                let context = context.clone();
                let id = id.clone();
                smol::spawn(async move {
                    let output =
                        run_cli(&context, &args(&["plugin", "details", &id]), LIST_TIMEOUT)
                            .await
                            .and_then(succeeded);
                    (id, output)
                })
            })
            .collect();
        for task in tasks {
            let (id, output) = task.await;
            details.insert(id, output);
        }
    }
    for entry in &mut listing.entries {
        match details.remove(&entry.id) {
            Some(Ok(text)) => apply_details(entry, &text),
            Some(Err(error)) => entry
                .diagnostics
                .push(("details_error".into(), native_message(&error))),
            None => {}
        }
    }
    Ok(listing)
}

pub(crate) async fn run(
    context: &PluginContext,
    op: &PluginOp,
) -> Result<PluginOpOutcome, AgentError> {
    let output = run_cli(context, &op_args(op)?, MUTATION_TIMEOUT).await?;
    match op {
        PluginOp::AddMarketplace { .. } | PluginOp::RemoveMarketplace { .. } => {
            let stdout = succeeded(output)?;
            Ok(PluginOpOutcome::Done {
                diagnostics: vec![("message".into(), stdout.trim().to_string())],
            })
        }
        _ => json_outcome(&output),
    }
}

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_string()).collect()
}

/// The argv for one mutation. `-y` is never passed: a marketplace-declared
/// command runs only with the `--accept-command` hash a person accepted.
/// Every plugin mutation names its scope, because the CLI's default
/// auto-detects one from the working directory.
fn op_args(op: &PluginOp) -> Result<Vec<String>, AgentError> {
    let args = match op {
        PluginOp::Install {
            id,
            scope,
            accept_command,
        }
        | PluginOp::Update {
            id,
            scope,
            accept_command,
        } => {
            let verb = if matches!(op, PluginOp::Install { .. }) {
                "install"
            } else {
                "update"
            };
            let mut args = plugin_args(verb, id, *scope)?;
            if let Some(sha256) = accept_command {
                args.extend(["--accept-command".into(), operand(sha256)?]);
            }
            args
        }
        PluginOp::Uninstall { id, scope } => plugin_args("uninstall", id, *scope)?,
        PluginOp::SetEnabled { id, scope, enabled } => {
            plugin_args(if *enabled { "enable" } else { "disable" }, id, *scope)?
        }
        PluginOp::AddMarketplace { source } => vec![
            "plugin".into(),
            "marketplace".into(),
            "add".into(),
            operand(source)?,
            "--scope".into(),
            "user".into(),
        ],
        PluginOp::RemoveMarketplace { name } => vec![
            "plugin".into(),
            "marketplace".into(),
            "remove".into(),
            operand(name)?,
        ],
    };
    Ok(args)
}

fn plugin_args(verb: &str, id: &str, scope: PluginScope) -> Result<Vec<String>, AgentError> {
    Ok(vec![
        "plugin".into(),
        verb.into(),
        operand(id)?,
        "--scope".into(),
        scope_arg(scope)?.into(),
        "--json".into(),
    ])
}

/// A value placed in a positional slot must not be read as an option.
fn operand(value: &str) -> Result<String, AgentError> {
    if value.is_empty() || value.starts_with('-') {
        return Err(AgentError::Protocol(format!(
            "refusing to pass {value:?} to `claude plugin`"
        )));
    }
    Ok(value.to_string())
}

fn scope_arg(scope: PluginScope) -> Result<&'static str, AgentError> {
    match scope {
        PluginScope::User => Ok("user"),
        PluginScope::Project => Ok("project"),
        PluginScope::Local => Ok("local"),
        PluginScope::Managed => Ok("managed"),
        PluginScope::Session => Err(AgentError::Protocol(
            "Claude Code session plugins have no management command".into(),
        )),
    }
}

fn scope_from_native(scope: &str) -> Option<PluginScope> {
    match scope {
        "user" => Some(PluginScope::User),
        "project" => Some(PluginScope::Project),
        "local" => Some(PluginScope::Local),
        "managed" => Some(PluginScope::Managed),
        _ => None,
    }
}

fn management_command(
    context: &PluginContext,
    args: &[String],
) -> Result<std::process::Command, AgentError> {
    let binary = crate::resolve_binary(context.binary_path.as_deref(), "claude")?;
    let mut command = crate::process::command(binary);
    command.args(args).current_dir(&context.cwd);
    for var in NESTED_SESSION_VARS {
        command.env_remove(var);
    }
    for (key, value) in context.launch_env.pairs(ProviderKind::ClaudeCode) {
        command.env(key, value);
    }
    Ok(command)
}

struct CliOutput {
    success: bool,
    status: String,
    stdout: String,
    stderr: String,
}

async fn run_cli(
    context: &PluginContext,
    args: &[String],
    timeout: Duration,
) -> Result<CliOutput, AgentError> {
    let display = format!("claude {}", args.join(" "));
    let mut command = smol::process::Command::from(management_command(context, args)?);
    // `output()` gives the child a null stdin, so a prompt can never wait on
    // input, and dropping it on timeout kills the child.
    command.kill_on_drop(true);
    let output = smol::future::or(async { Some(command.output().await) }, async {
        smol::Timer::after(timeout).await;
        None
    })
    .await
    .ok_or_else(|| AgentError::Provider(format!("`{display}` timed out")))?
    .map_err(|error| AgentError::Spawn(format!("spawning `{display}`: {error}")))?;
    Ok(CliOutput {
        success: output.status.success(),
        status: output.status.to_string(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn succeeded(output: CliOutput) -> Result<String, AgentError> {
    if output.success {
        return Ok(output.stdout);
    }
    let message = [output.stderr.trim(), output.stdout.trim()]
        .into_iter()
        .find(|text| !text.is_empty())
        .map(str::to_string)
        .unwrap_or(output.status);
    Err(AgentError::Provider(message))
}

/// Mutations print one JSON result on the last stdout line; command-source
/// installs print the command and its mode as human text before it.
fn last_json_line(stdout: &str) -> Result<(Value, String), AgentError> {
    let mut lines: Vec<&str> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let last = lines
        .pop()
        .ok_or_else(|| AgentError::Protocol("`claude plugin` printed no result".into()))?;
    let value = serde_json::from_str::<Value>(last.trim())
        .ok()
        .filter(Value::is_object)
        .ok_or_else(|| {
            AgentError::Protocol(format!(
                "`claude plugin` did not end with a JSON result: {last}"
            ))
        })?;
    Ok((value, lines.join("\n")))
}

fn json_outcome(output: &CliOutput) -> Result<PluginOpOutcome, AgentError> {
    let (result, preamble) = match last_json_line(&output.stdout) {
        Ok(parsed) => parsed,
        Err(error) if output.success => return Err(error),
        Err(_) => {
            let text = [output.stderr.trim(), output.stdout.trim()]
                .into_iter()
                .find(|text| !text.is_empty())
                .unwrap_or(&output.status);
            return Err(AgentError::Provider(text.to_string()));
        }
    };
    let text = |key: &str| result.get(key).and_then(Value::as_str).map(str::to_string);
    let message = text("message");
    if text("outcome").as_deref() == Some("ok") {
        let mut diagnostics = Vec::new();
        if result.get("command").and_then(Value::as_str) == Some("update") {
            for (key, native) in [
                ("update_outcome", "updateOutcome"),
                ("old_version", "oldVersion"),
                ("new_version", "newVersion"),
                ("message", "message"),
            ] {
                if let Some(value) = text(native) {
                    diagnostics.push((key.to_string(), value));
                }
            }
        }
        return Ok(PluginOpOutcome::Done { diagnostics });
    }
    if text("failureCode").as_deref() == Some("command_source_refused") {
        let shown = result.get("shownCommand");
        let shown_text = |key: &str| {
            shown
                .and_then(|shown| shown.get(key))
                .and_then(Value::as_str)
        };
        if let (Some(command), Some(sha256)) = (shown_text("command"), shown_text("sha256")) {
            let native_text = if preamble.trim().is_empty() {
                message.unwrap_or_default()
            } else {
                preamble
            };
            return Ok(PluginOpOutcome::AcceptCommand(CommandAcceptance {
                command: command.to_string(),
                sha256: sha256.to_string(),
                mode: shown_text("mode").map(str::to_string),
                native_text,
            }));
        }
    }
    Err(AgentError::Provider(
        message
            .or_else(|| text("failureCode"))
            .unwrap_or_else(|| result.to_string()),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListedInstallation {
    id: String,
    #[serde(default)]
    version: Option<String>,
    scope: String,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    install_path: Option<PathBuf>,
    #[serde(default)]
    project_path: Option<PathBuf>,
    #[serde(default)]
    mcp_servers: Option<serde_json::Map<String, Value>>,
    #[serde(default)]
    project_enabled: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListedAvailable {
    plugin_id: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    marketplace_name: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    source: Value,
}

#[derive(Deserialize)]
struct Listed {
    #[serde(default)]
    installed: Vec<ListedInstallation>,
    #[serde(default)]
    available: Vec<ListedAvailable>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListedMarketplace {
    name: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    install_location: Option<PathBuf>,
}

fn parse_listed(stdout: &str) -> Result<Listed, AgentError> {
    serde_json::from_str(stdout.trim()).map_err(|error| {
        AgentError::Protocol(format!("unexpected `claude plugin list` output: {error}"))
    })
}

fn parse_marketplaces(stdout: &str) -> Result<Vec<ProviderPluginMarketplace>, AgentError> {
    let listed: Vec<ListedMarketplace> = serde_json::from_str(stdout.trim()).map_err(|error| {
        AgentError::Protocol(format!(
            "unexpected `claude plugin marketplace list` output: {error}"
        ))
    })?;
    Ok(listed
        .into_iter()
        .map(|marketplace| {
            let native = marketplace.source.as_deref().unwrap_or_default();
            let kind = if marketplace.name == OFFICIAL_MARKETPLACE
                || marketplace.repo.as_deref() == Some(OFFICIAL_MARKETPLACE_REPO)
            {
                PluginSourceKind::Official
            } else {
                match native {
                    "directory" | "file" => PluginSourceKind::LocalPath,
                    "github" | "git" => PluginSourceKind::Git,
                    "url" => PluginSourceKind::ThirdParty,
                    "npm" => PluginSourceKind::Npm,
                    _ => PluginSourceKind::Unknown,
                }
            };
            ProviderPluginMarketplace {
                source: marketplace
                    .repo
                    .or(marketplace.url)
                    .or(marketplace.path)
                    .unwrap_or_else(|| native.to_string()),
                name: marketplace.name,
                kind,
                location: marketplace.install_location,
            }
        })
        .collect())
}

/// Which installations the context directory's scopes address. `list` is
/// home-wide, so a project installation of another project is reported too,
/// but `--scope project` would act on this directory's project instead.
struct InContext {
    project: Option<PathBuf>,
}

impl InContext {
    fn new(context: &PluginContext) -> Self {
        Self {
            project: context.project.then(|| canonical(&context.cwd)),
        }
    }

    fn scopes(&self) -> &'static [PluginScope] {
        if self.project.is_some() {
            &[PluginScope::User, PluginScope::Project, PluginScope::Local]
        } else {
            &[PluginScope::User]
        }
    }

    fn contains(&self, scope: PluginScope, project_path: Option<&Path>) -> bool {
        match scope {
            PluginScope::User | PluginScope::Managed => true,
            PluginScope::Project | PluginScope::Local => match (&self.project, project_path) {
                (Some(project), Some(path)) => canonical(path) == *project,
                _ => false,
            },
            PluginScope::Session => false,
        }
    }
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn listing(
    listed: Listed,
    marketplaces: Vec<ProviderPluginMarketplace>,
    in_context: &InContext,
) -> PluginListing {
    let marketplace_kind = |name: Option<&str>| {
        name.and_then(|name| marketplaces.iter().find(|m| m.name == name))
            .map(|m| m.kind)
            .unwrap_or(PluginSourceKind::Unknown)
    };
    let mut entries: Vec<ProviderPluginEntry> = Vec::new();
    // Per entry: the in-context installed scopes.
    let mut here: Vec<Vec<PluginScope>> = Vec::new();
    for installed in listed.installed {
        let Some(scope) = scope_from_native(&installed.scope) else {
            log::warn!(
                "claude plugin list: unknown scope {:?} for {}",
                installed.scope,
                installed.id
            );
            continue;
        };
        let in_this_context = in_context.contains(scope, installed.project_path.as_deref());
        let index = match entries.iter().position(|entry| entry.id == installed.id) {
            Some(index) => index,
            None => {
                let (name, marketplace) = split_id(&installed.id);
                entries.push(ProviderPluginEntry {
                    id: installed.id.clone(),
                    name,
                    version: None,
                    description: None,
                    // Installed entries carry no source; only an
                    // available listing of the same id names it.
                    source: PluginSource {
                        kind: PluginSourceKind::Unknown,
                        marketplace,
                    },
                    installations: Vec::new(),
                    enabled: installed.enabled.map(Tri::from).unwrap_or_default(),
                    declared: Some(DeclaredComponents {
                        mcp_servers: Some(Vec::new()),
                        ..DeclaredComponents::default()
                    }),
                    errors: Vec::new(),
                    actions: Vec::new(),
                    diagnostics: Vec::new(),
                });
                here.push(Vec::new());
                entries.len() - 1
            }
        };
        let entry = &mut entries[index];
        if let (Some(servers), Some(declared)) = (&installed.mcp_servers, &mut entry.declared) {
            let names = declared.mcp_servers.get_or_insert_with(Vec::new);
            for name in servers.keys() {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }
        entry.installations.push(PluginInstallation {
            scope,
            location: installed.install_path,
            version: installed.version,
            scope_enabled: (scope == PluginScope::Project && in_this_context)
                .then_some(installed.project_enabled)
                .flatten(),
        });
        if in_this_context {
            here[index].push(scope);
        }
    }
    for entry in &mut entries {
        let first = entry.installations[0].version.clone();
        if entry
            .installations
            .iter()
            .all(|installation| installation.version == first)
        {
            entry.version = first;
        }
    }
    for available in listed.available {
        let kind = source_kind(
            &available.source,
            marketplace_kind(available.marketplace_name.as_deref()),
        );
        if let Some(entry) = entries.iter_mut().find(|e| e.id == available.plugin_id) {
            entry.description = available.description;
            entry.source.kind = kind;
            continue;
        }
        entries.push(ProviderPluginEntry {
            id: available.plugin_id,
            name: available.name,
            version: available.version,
            description: available.description,
            source: PluginSource {
                marketplace: available.marketplace_name,
                kind,
            },
            installations: Vec::new(),
            enabled: Tri::Unknown,
            declared: None,
            errors: Vec::new(),
            actions: Vec::new(),
            diagnostics: Vec::new(),
        });
        here.push(Vec::new());
    }
    for (entry, here) in entries.iter_mut().zip(&here) {
        entry.actions = actions(entry.enabled, here, in_context);
    }

    let mut marketplace_actions = vec![MarketplaceAction::Add];
    for marketplace in &marketplaces {
        marketplace_actions.push(MarketplaceAction::Remove {
            marketplace: marketplace.name.clone(),
            uninstalls: entries
                .iter()
                .filter(|entry| {
                    !entry.installations.is_empty()
                        && entry.source.marketplace.as_deref() == Some(marketplace.name.as_str())
                })
                .map(|entry| entry.id.clone())
                .collect(),
        });
    }
    PluginListing {
        entries,
        marketplaces,
        marketplace_actions,
        errors: Vec::new(),
    }
}

fn split_id(id: &str) -> (String, Option<String>) {
    match id.rsplit_once('@') {
        Some((name, marketplace)) if !name.is_empty() => {
            (name.to_string(), Some(marketplace.to_string()))
        }
        _ => (id.to_string(), None),
    }
}

fn source_kind(source: &Value, marketplace: PluginSourceKind) -> PluginSourceKind {
    match source {
        Value::String(_) => match marketplace {
            PluginSourceKind::Official | PluginSourceKind::LocalPath => marketplace,
            PluginSourceKind::Unknown => PluginSourceKind::Unknown,
            _ => PluginSourceKind::ThirdParty,
        },
        Value::Object(object) => match object.get("source").and_then(Value::as_str) {
            Some("command") => PluginSourceKind::Command,
            Some("github" | "git" | "git-subdir" | "url") => PluginSourceKind::Git,
            Some("npm") => PluginSourceKind::Npm,
            Some("directory" | "file") => PluginSourceKind::LocalPath,
            _ => PluginSourceKind::Unknown,
        },
        _ => PluginSourceKind::Unknown,
    }
}

/// Native operations offered for one entry in this context. Enablement is
/// reported only as the effective value, so both directions are offered
/// when it is unknown.
fn actions(enabled: Tri, here: &[PluginScope], in_context: &InContext) -> Vec<PluginAction> {
    let mut actions = Vec::new();
    for &scope in in_context.scopes() {
        if !here.contains(&scope) {
            actions.push(PluginAction::Install { scope });
        }
    }
    for &scope in here {
        actions.push(PluginAction::Update { scope });
        if scope != PluginScope::Managed {
            actions.push(PluginAction::Uninstall { scope });
        }
    }
    if !here.is_empty() {
        for &scope in in_context.scopes() {
            if enabled != Tri::Yes {
                actions.push(PluginAction::Enable { scope });
            }
            if enabled != Tri::No {
                actions.push(PluginAction::Disable { scope });
            }
        }
    }
    actions
}

/// `plugin details` has no `--json`; its "Component inventory" section lists
/// `Label (count)  name, name  (note)` rows.
fn apply_details(entry: &mut ProviderPluginEntry, text: &str) {
    let mut complete = true;
    let mut declared = entry.declared.clone().unwrap_or_default();
    let mut in_inventory = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(description) = trimmed.strip_prefix("Description:") {
            if entry.description.is_none() {
                entry.description = Some(description.trim().to_string());
            }
            continue;
        }
        if trimmed == "Component inventory" {
            in_inventory = true;
            continue;
        }
        if !in_inventory {
            continue;
        }
        if trimmed.is_empty() {
            break;
        }
        let field = match trimmed.split(" (").next() {
            Some("Skills") => &mut declared.skills,
            Some("Agents") => &mut declared.agents,
            Some("Hooks") => &mut declared.hooks,
            Some("MCP servers") => &mut declared.mcp_servers,
            Some("LSP servers") => &mut declared.lsp_servers,
            _ => {
                complete = false;
                continue;
            }
        };
        match inventory_names(trimmed) {
            Some(names) => *field = Some(names),
            None => complete = false,
        }
    }
    if !in_inventory {
        complete = false;
    }
    entry.declared = Some(declared);
    if !complete {
        entry
            .diagnostics
            .push(("details".into(), text.trim_end().to_string()));
    }
}

fn inventory_names(row: &str) -> Option<Vec<String>> {
    let (_, rest) = row.split_once(" (")?;
    let (count, rest) = rest.split_once(')')?;
    let count: usize = count.parse().ok()?;
    let names = rest.trim().split("  (").next().unwrap_or_default();
    let names: Vec<String> = names
        .split(", ")
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect();
    (names.len() == count).then_some(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/plugins/claude/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
    }

    fn output(stdout: &str, success: bool) -> CliOutput {
        CliOutput {
            success,
            status: if success {
                "exit status: 0"
            } else {
                "exit status: 1"
            }
            .into(),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    fn context(cwd: &Path, project: bool) -> PluginContext {
        PluginContext {
            binary_path: Some(PathBuf::from("/opt/claude/bin/claude")),
            launch_env: crate::LaunchEnv {
                env: vec![("CLAUDE_CONFIG_DIR".into(), "/tmp/isolated/.claude".into())],
                home: Some(PathBuf::from("/tmp/isolated")),
            },
            cwd: cwd.to_path_buf(),
            project,
        }
    }

    /// The probe's project was `/tmp/tcode-probe/proj`, reported as its
    /// realpath; a context outside it must not act on its installations.
    #[test]
    fn listing_groups_installations_by_id_and_offers_scopes_of_the_context_only() {
        let listed = parse_listed(&fixture("plugin-list-available-after-install.json")).unwrap();
        let marketplaces = parse_marketplaces(&fixture("marketplace-list.json")).unwrap();
        let project = InContext {
            project: Some(PathBuf::from("/private/tmp/tcode-probe/proj")),
        };
        let inside = listing(listed, marketplaces.clone(), &project);
        let ids: Vec<&str> = inside.entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            ["alpha@tcode-probe", "beta@tcode-probe", "gamma@tcode-probe"]
        );

        let alpha = &inside.entries[0];
        assert_eq!(alpha.name, "alpha");
        assert_eq!(alpha.version.as_deref(), Some("1.0.0"));
        assert_eq!(alpha.enabled, Tri::Yes);
        assert_eq!(
            alpha.source,
            PluginSource {
                marketplace: Some("tcode-probe".into()),
                kind: PluginSourceKind::Unknown,
            }
        );
        assert_eq!(
            alpha.installations,
            [
                PluginInstallation {
                    scope: PluginScope::User,
                    location: Some(PathBuf::from(
                        "/tmp/tcode-probe/claude-home/.claude/plugins/cache/tcode-probe/alpha/1.0.0"
                    )),
                    version: Some("1.0.0".into()),
                    scope_enabled: None,
                },
                PluginInstallation {
                    scope: PluginScope::Project,
                    location: Some(PathBuf::from(
                        "/tmp/tcode-probe/claude-home/.claude/plugins/cache/tcode-probe/alpha/1.0.0"
                    )),
                    version: Some("1.0.0".into()),
                    scope_enabled: Some(true),
                },
            ]
        );
        assert_eq!(
            alpha.declared.as_ref().unwrap().mcp_servers.as_deref(),
            Some(&["alpha-dead".to_string()][..])
        );
        assert_eq!(alpha.declared.as_ref().unwrap().skills, None);
        use PluginScope::*;
        assert_eq!(
            alpha.actions,
            [
                PluginAction::Install { scope: Local },
                PluginAction::Update { scope: User },
                PluginAction::Uninstall { scope: User },
                PluginAction::Update { scope: Project },
                PluginAction::Uninstall { scope: Project },
                PluginAction::Disable { scope: User },
                PluginAction::Disable { scope: Project },
                PluginAction::Disable { scope: Local },
            ]
        );
        assert_eq!(
            inside.entries[1].declared.as_ref().unwrap().mcp_servers,
            Some(Vec::new())
        );

        let gamma = &inside.entries[2];
        assert!(gamma.installations.is_empty());
        assert_eq!(gamma.version, None);
        assert_eq!(gamma.description.as_deref(), Some("Command-source plugin"));
        assert_eq!(gamma.source.kind, PluginSourceKind::Command);
        assert_eq!(gamma.enabled, Tri::Unknown);
        assert_eq!(gamma.declared, None);
        assert_eq!(
            gamma.actions,
            [
                PluginAction::Install { scope: User },
                PluginAction::Install { scope: Project },
                PluginAction::Install { scope: Local },
            ]
        );
        assert_eq!(
            inside.marketplace_actions,
            [
                MarketplaceAction::Add,
                MarketplaceAction::Remove {
                    marketplace: "tcode-probe".into(),
                    uninstalls: vec!["alpha@tcode-probe".into(), "beta@tcode-probe".into()],
                },
            ]
        );

        let listed = parse_listed(&fixture("plugin-list-available-after-install.json")).unwrap();
        let outside = listing(listed, marketplaces, &InContext { project: None });
        let alpha = &outside.entries[0];
        assert_eq!(alpha.installations[1].scope_enabled, None);
        assert_eq!(
            alpha.actions,
            [
                PluginAction::Update { scope: User },
                PluginAction::Uninstall { scope: User },
                PluginAction::Disable { scope: User },
            ]
        );
        let beta = &outside.entries[1];
        assert_eq!(beta.actions, [PluginAction::Install { scope: User }]);
    }

    #[test]
    fn marketplaces_keep_their_native_source_and_location() {
        assert_eq!(
            parse_marketplaces(&fixture("real-home-marketplace-list.json")).unwrap(),
            [ProviderPluginMarketplace {
                name: "claude-plugins-official".into(),
                source: "anthropics/claude-plugins-official".into(),
                kind: PluginSourceKind::Official,
                location: Some(PathBuf::from(
                    "/Users/tryanks/.claude/plugins/marketplaces/claude-plugins-official"
                )),
            }]
        );
    }

    #[test]
    fn details_inventory_becomes_declared_components() {
        let listed = parse_listed(&fixture("plugin-list-available-after-install.json")).unwrap();
        let mut alpha = listing(listed, Vec::new(), &InContext { project: None })
            .entries
            .remove(0);
        alpha.description = None;
        apply_details(&mut alpha, &fixture("details.txt"));
        assert_eq!(
            alpha.description.as_deref(),
            Some("Skill, command, SessionStart hook and an unreachable HTTP MCP server")
        );
        let names = |names: &[&str]| Some(names.iter().map(|n| n.to_string()).collect());
        assert_eq!(
            alpha.declared,
            Some(DeclaredComponents {
                skills: names(&["greet", "hello"]),
                agents: names(&[]),
                hooks: names(&["SessionStart"]),
                mcp_servers: names(&["alpha-dead"]),
                untrusted_hooks: None,
                lsp_servers: names(&[]),
                apps: None,
            })
        );
        assert!(alpha.diagnostics.is_empty());

        apply_details(&mut alpha, "alpha 1.0.0\n  Source: alpha@tcode-probe\n");
        assert_eq!(
            alpha.diagnostics,
            [(
                "details".to_string(),
                "alpha 1.0.0\n  Source: alpha@tcode-probe".to_string()
            )]
        );
    }

    #[test]
    fn command_source_refusal_becomes_a_challenge_from_the_last_json_line() {
        let challenge = json_outcome(&output(
            &fixture("install-command-source-challenge.txt"),
            false,
        ))
        .unwrap();
        let PluginOpOutcome::AcceptCommand(acceptance) = challenge else {
            panic!("expected a challenge, got {challenge:?}");
        };
        assert_eq!(acceptance.command, "echo /tmp/tcode-probe/gamma-src");
        assert_eq!(
            acceptance.sha256,
            "b9c02c85b17261fa1fce010664bb16cbe866f14c7deeaf6cb6c37c2736bbe269"
        );
        assert_eq!(acceptance.mode.as_deref(), Some("copy"));
        assert!(
            acceptance
                .native_text
                .starts_with("\"gamma\" is installed by running a command"),
            "{}",
            acceptance.native_text
        );

        // The preamble is printed on success too; the result is still the last line.
        assert_eq!(
            json_outcome(&output(
                &fixture("install-command-source-accepted.json"),
                true
            ))
            .unwrap(),
            PluginOpOutcome::Done {
                diagnostics: Vec::new()
            }
        );
        // A hash that no longer names the shown command is shown again.
        let PluginOpOutcome::AcceptCommand(again) = json_outcome(&output(
            &fixture("install-command-source-wrong-hash.json"),
            false,
        ))
        .unwrap() else {
            panic!("a mismatched hash must surface the command again");
        };
        assert!(
            again
                .native_text
                .contains("does not name the command shown above")
        );

        // Only the last line counts: a JSON line followed by text is not a result.
        let trailing = format!("{}\nNot JSON\n", fixture("install-user.json").trim());
        assert!(matches!(
            json_outcome(&output(&trailing, true)),
            Err(AgentError::Protocol(_))
        ));
    }

    #[test]
    fn mutation_results_map_native_outcomes() {
        for name in [
            "install-user.json",
            "enable.json",
            "disable.json",
            "uninstall.json",
        ] {
            assert_eq!(
                json_outcome(&output(&fixture(name), true)).unwrap(),
                PluginOpOutcome::Done {
                    diagnostics: Vec::new()
                },
                "{name}"
            );
        }
        let PluginOpOutcome::Done { diagnostics } =
            json_outcome(&output(&fixture("update.json"), true)).unwrap()
        else {
            panic!("update must succeed");
        };
        assert_eq!(
            diagnostics,
            [
                ("update_outcome".to_string(), "updated".to_string()),
                ("old_version".to_string(), "1.0.0".to_string()),
                ("new_version".to_string(), "1.0.1".to_string()),
                (
                    "message".to_string(),
                    "Plugin \"alpha\" updated from 1.0.0 to 1.0.1 for scope project (/private/tmp/tcode-probe/proj). Restart to apply changes.".to_string()
                ),
            ]
        );
        let failed = r#"{"command":"uninstall","outcome":"failed","plugin":"beta@tcode-probe","scope":"user","message":"Plugin \"beta@tcode-probe\" not found in installed plugins","failureCode":"not_installed"}"#;
        assert_eq!(
            native_message(&json_outcome(&output(failed, false)).unwrap_err()),
            "Plugin \"beta@tcode-probe\" not found in installed plugins"
        );
    }

    #[test]
    fn management_argv_names_its_scope_never_auto_accepts_and_leaves_the_nested_session() {
        let sha = "b9c02c85b17261fa1fce010664bb16cbe866f14c7deeaf6cb6c37c2736bbe269";
        let id = "gamma@tcode-probe".to_string();
        let scope = PluginScope::Project;
        let ops = [
            PluginOp::Install {
                id: id.clone(),
                scope,
                accept_command: None,
            },
            PluginOp::Install {
                id: id.clone(),
                scope,
                accept_command: Some(sha.into()),
            },
            PluginOp::Update {
                id: id.clone(),
                scope,
                accept_command: None,
            },
            PluginOp::Update {
                id: id.clone(),
                scope,
                accept_command: Some(sha.into()),
            },
            PluginOp::Uninstall {
                id: id.clone(),
                scope,
            },
            PluginOp::SetEnabled {
                id: id.clone(),
                scope,
                enabled: true,
            },
            PluginOp::SetEnabled {
                id: id.clone(),
                scope,
                enabled: false,
            },
            PluginOp::AddMarketplace {
                source: "/tmp/tcode-probe/mkt".into(),
            },
            PluginOp::RemoveMarketplace {
                name: "tcode-probe".into(),
            },
        ];
        let cwd = std::env::temp_dir();
        for op in &ops {
            let argv = op_args(op).unwrap();
            assert!(
                !argv.iter().any(|arg| arg == "-y" || arg == "--yes"),
                "{argv:?}"
            );
            let accepted = match op {
                PluginOp::Install { accept_command, .. }
                | PluginOp::Update { accept_command, .. } => accept_command.as_deref(),
                _ => None,
            };
            let accept_at = argv.iter().position(|arg| arg == "--accept-command");
            assert_eq!(
                accept_at.map(|index| argv[index + 1].as_str()),
                accepted,
                "{argv:?}"
            );
            if !matches!(op, PluginOp::RemoveMarketplace { .. }) {
                let scope_at = argv.iter().position(|arg| arg == "--scope").unwrap();
                assert!(["user", "project"].contains(&argv[scope_at + 1].as_str()));
            }

            let command = management_command(&context(&cwd, true), &argv).unwrap();
            assert_eq!(command.get_current_dir(), Some(cwd.as_path()));
            let envs: Vec<_> = command.get_envs().collect();
            for var in NESTED_SESSION_VARS {
                assert!(
                    envs.contains(&(var.as_ref(), None)),
                    "{var} must be removed"
                );
            }
            assert!(envs.contains(&("HOME".as_ref(), Some("/tmp/isolated".as_ref()))));
        }
        assert!(
            op_args(&PluginOp::AddMarketplace {
                source: "--help".into()
            })
            .is_err()
        );
    }
}
