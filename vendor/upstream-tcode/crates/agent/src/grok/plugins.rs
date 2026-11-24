//! Grok Build's native plugin management, driven through `grok plugin …`
//! with one bounded child process per command.
//!
//! Grok installs plugins for the user only, so every installation is in the
//! user scope. Enablement is left out: no `grok plugin` read reports it, and
//! `grok inspect` reports a disabled plugin as enabled.

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use crate::{
    AgentError, CommandAcceptance, MarketplaceAction, PluginAction, PluginContext,
    PluginInstallation, PluginListing, PluginOp, PluginOpOutcome, PluginScope, PluginSource,
    PluginSourceKind, ProviderKind, ProviderPluginEntry, ProviderPluginMarketplace, Tri,
    native_message,
};

const LIST_TIMEOUT: Duration = Duration::from_secs(60);
/// Installs, updates and marketplace adds may clone repositories.
const MUTATION_TIMEOUT: Duration = Duration::from_secs(300);
const DETAILS_CONCURRENCY: usize = 4;

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
    let mut listing = listing(plugins, marketplaces);

    let installed: Vec<(usize, String)> = listing
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| !entry.installations.is_empty())
        .map(|(index, entry)| (index, entry.name.clone()))
        .collect();
    for chunk in installed.chunks(DETAILS_CONCURRENCY) {
        let tasks: Vec<_> = chunk
            .iter()
            .map(|(index, name)| {
                let context = context.clone();
                let (index, name) = (*index, name.clone());
                smol::spawn(async move {
                    let details = match operand(&name) {
                        Ok(name) => run_cli(
                            &context,
                            &["plugin".into(), "details".into(), name],
                            LIST_TIMEOUT,
                        )
                        .await
                        .and_then(succeeded),
                        Err(error) => Err(error),
                    };
                    (index, details)
                })
            })
            .collect();
        for task in tasks {
            let (index, details) = task.await;
            let entry = &mut listing.entries[index];
            match details {
                Ok(text) => apply_details(entry, &text),
                Err(error) => entry
                    .diagnostics
                    .push(("details_error".into(), native_message(&error))),
            }
        }
    }
    Ok(listing)
}

pub(crate) async fn run(
    context: &PluginContext,
    op: &PluginOp,
) -> Result<PluginOpOutcome, AgentError> {
    let mut argv = op_args(op)?;
    match op {
        PluginOp::Install { accept_command, .. } => {
            let untrusted = run_cli(context, &argv, MUTATION_TIMEOUT).await?;
            match install_step(untrusted, accept_command.as_deref())? {
                InstallStep::Finished(outcome) => Ok(outcome),
                InstallStep::Trust => {
                    argv.push("--trust".into());
                    message_outcome(run_cli(context, &argv, MUTATION_TIMEOUT).await?)
                }
            }
        }
        PluginOp::Update { .. } => {
            let stdout = succeeded(run_cli(context, &argv, MUTATION_TIMEOUT).await?)?;
            Ok(update_outcome(&stdout))
        }
        _ => message_outcome(run_cli(context, &argv, MUTATION_TIMEOUT).await?),
    }
}

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_string()).collect()
}

/// The argv for one mutation, without consent: `--trust` is added only for an
/// install whose challenge a person accepted ([`install_step`]), and
/// `--confirm`, which uninstalls every plugin of a multi-plugin repository, is
/// never passed. Plugins are named as `grok plugin list` names them; installs
/// take the `name@marketplace` id.
fn op_args(op: &PluginOp) -> Result<Vec<String>, AgentError> {
    let plugin = |verb: &str, target: String, scope: PluginScope| match scope {
        PluginScope::User => Ok(vec!["plugin".into(), verb.into(), target]),
        other => Err(AgentError::Protocol(format!(
            "Grok installs plugins for the user only, not in the {other:?} scope"
        ))),
    };
    let marketplace = |verb: &str, target: &str| {
        Ok(vec![
            "plugin".into(),
            "marketplace".into(),
            verb.into(),
            operand(target)?,
        ])
    };
    match op {
        PluginOp::Install { id, scope, .. } => plugin("install", operand(id)?, *scope),
        PluginOp::Update { id, scope, .. } => plugin("update", name(id)?, *scope),
        PluginOp::Uninstall { id, scope } => plugin("uninstall", name(id)?, *scope),
        PluginOp::SetEnabled { .. } => Err(AgentError::Protocol(
            "Grok plugin enablement cannot be read back through its CLI".into(),
        )),
        PluginOp::AddMarketplace { source } => marketplace("add", source),
        PluginOp::RemoveMarketplace { name } => marketplace("remove", name),
    }
}

fn name(id: &str) -> Result<String, AgentError> {
    operand(split_id(id).0)
}

/// A value placed in a positional slot must not be read as an option.
fn operand(value: &str) -> Result<String, AgentError> {
    if value.is_empty() || value.starts_with('-') {
        return Err(AgentError::Protocol(format!(
            "refusing to pass {value:?} to `grok plugin`"
        )));
    }
    Ok(value.to_string())
}

fn split_id(id: &str) -> (&str, Option<&str>) {
    match id.rsplit_once('@') {
        Some((name, marketplace)) if !name.is_empty() => (name, Some(marketplace)),
        _ => (id, None),
    }
}

fn management_command(
    context: &PluginContext,
    args: &[String],
) -> Result<std::process::Command, AgentError> {
    let binary = crate::resolve_binary(context.binary_path.as_deref(), "grok")?;
    let mut command = crate::process::command(binary);
    command.args(args).current_dir(&context.cwd);
    for (key, value) in context.launch_env.pairs(ProviderKind::Grok) {
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
    let display = format!("grok {}", args.join(" "));
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
        Ok(output.stdout)
    } else {
        Err(failure(&output))
    }
}

/// A failed command in the CLI's own words.
fn failure(output: &CliOutput) -> AgentError {
    let message = [output.stderr.trim(), output.stdout.trim()]
        .into_iter()
        .find(|text| !text.is_empty())
        .map_or_else(|| output.status.clone(), str::to_string);
    AgentError::Provider(message)
}

fn message_outcome(output: CliOutput) -> Result<PluginOpOutcome, AgentError> {
    let stdout = succeeded(output)?;
    let message = stdout.trim();
    Ok(PluginOpOutcome::Done {
        diagnostics: if message.is_empty() {
            Vec::new()
        } else {
            vec![("message".into(), message.to_string())]
        },
    })
}

enum InstallStep {
    Finished(PluginOpOutcome),
    /// Re-run with `--trust`: a person accepted exactly the challenge Grok
    /// shows now.
    Trust,
}

/// What an install run without `--trust` leaves to do.
fn install_step(untrusted: CliOutput, accepted: Option<&str>) -> Result<InstallStep, AgentError> {
    if untrusted.success {
        return message_outcome(untrusted).map(InstallStep::Finished);
    }
    match trust_challenge(&untrusted.stderr) {
        Some(challenge) if accepted == Some(challenge.sha256.as_str()) => Ok(InstallStep::Trust),
        Some(challenge) => Ok(InstallStep::Finished(PluginOpOutcome::AcceptCommand(
            challenge,
        ))),
        None => Err(failure(&untrusted)),
    }
}

/// Without `--trust`, `install` explains what installing activates and prints
/// the command that proceeds. That command is what a person accepts, bound to
/// the whole explanation: if any of it changes, it is shown again.
fn trust_challenge(stderr: &str) -> Option<CommandAcceptance> {
    let native_text = stderr.trim();
    let (_, proceed) = native_text.split_once("re-run with --trust:")?;
    let command = proceed
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    command.ends_with(" --trust").then(|| CommandAcceptance {
        command: command.to_string(),
        sha256: Sha256::digest(native_text.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        mode: None,
        native_text: native_text.to_string(),
    })
}

/// `<repo>: updated (<old> -> <new>)`, one line per updated repository.
fn update_outcome(stdout: &str) -> PluginOpOutcome {
    let message = stdout.trim();
    let parsed = message
        .split_once(": ")
        .and_then(|(_, result)| result.split_once(" ("))
        .and_then(|(outcome, versions)| {
            let (old, new) = versions.strip_suffix(')')?.split_once(" -> ")?;
            Some([
                ("update_outcome".to_string(), outcome.to_string()),
                ("old_version".to_string(), old.to_string()),
                ("new_version".to_string(), new.to_string()),
            ])
        })
        .filter(|_| message.lines().count() == 1);
    PluginOpOutcome::Done {
        diagnostics: match parsed {
            Some(diagnostics) => diagnostics.into(),
            None if message.is_empty() => Vec::new(),
            None => vec![("message".into(), message.to_string())],
        },
    }
}

/// One row of `grok plugin list --json --available`.
#[derive(Deserialize)]
struct Listed {
    status: String,
    name: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    marketplace: Option<String>,
    /// The installed repository; one repository can hold several plugins.
    #[serde(default)]
    repo_key: Option<String>,
    #[serde(default)]
    path: Option<PathBuf>,
}

#[derive(Deserialize)]
struct ListedMarketplace {
    name: String,
    kind: String,
    #[serde(default)]
    source: ListedMarketplaceSource,
}

#[derive(Deserialize, Default)]
struct ListedMarketplaceSource {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

fn parse_listed(stdout: &str) -> Result<Vec<Listed>, AgentError> {
    serde_json::from_str(stdout.trim()).map_err(|error| {
        AgentError::Protocol(format!("unexpected `grok plugin list` output: {error}"))
    })
}

fn parse_marketplaces(stdout: &str) -> Result<Vec<ProviderPluginMarketplace>, AgentError> {
    let listed: Vec<ListedMarketplace> = serde_json::from_str(stdout.trim()).map_err(|error| {
        AgentError::Protocol(format!(
            "unexpected `grok plugin marketplace list` output: {error}"
        ))
    })?;
    Ok(listed
        .into_iter()
        .map(|marketplace| ProviderPluginMarketplace {
            kind: match marketplace.kind.as_str() {
                "local" => PluginSourceKind::LocalPath,
                "git" => PluginSourceKind::Git,
                _ => PluginSourceKind::Unknown,
            },
            source: marketplace
                .source
                .path
                .or(marketplace.source.url)
                .unwrap_or(marketplace.kind),
            name: marketplace.name,
            location: None,
        })
        .collect())
}

fn listing(listed: Vec<Listed>, marketplaces: Vec<ProviderPluginMarketplace>) -> PluginListing {
    // A plugin installed from a path or a URL takes its kind from its details
    // ([`apply_details`]).
    let source_kind = |plugin: &Listed| {
        plugin
            .marketplace
            .as_ref()
            .and_then(|name| {
                marketplaces
                    .iter()
                    .find(|marketplace| marketplace.name == *name)
            })
            .map_or(PluginSourceKind::Unknown, |marketplace| marketplace.kind)
    };
    let repositories: Vec<&str> = listed
        .iter()
        .filter(|plugin| plugin.status == "installed")
        .filter_map(|plugin| plugin.repo_key.as_deref())
        .collect();
    let mut entries = Vec::new();
    for plugin in &listed {
        let installed = match plugin.status.as_str() {
            "installed" => true,
            "available" => false,
            other => {
                log::warn!(
                    "grok plugin list: unknown status {other:?} for {}",
                    plugin.name
                );
                continue;
            }
        };
        let mut actions = Vec::new();
        let scope = PluginScope::User;
        if !installed {
            actions.push(PluginAction::Install { scope });
        } else {
            // A path install is a live link that `update` leaves as it is.
            if plugin.marketplace.is_some() {
                actions.push(PluginAction::Update { scope });
            }
            // Uninstalling one plugin of a multi-plugin repository needs
            // `--confirm` and removes them all.
            let shared = plugin.repo_key.as_deref().is_some_and(|repository| {
                repositories
                    .iter()
                    .filter(|key| **key == repository)
                    .count()
                    > 1
            });
            if !shared {
                actions.push(PluginAction::Uninstall { scope });
            }
        }
        entries.push(ProviderPluginEntry {
            id: match &plugin.marketplace {
                Some(marketplace) => format!("{}@{marketplace}", plugin.name),
                None => plugin.name.clone(),
            },
            name: plugin.name.clone(),
            version: plugin.version.clone(),
            description: plugin.description.clone(),
            source: PluginSource {
                marketplace: plugin.marketplace.clone(),
                kind: source_kind(plugin),
            },
            installations: if installed {
                vec![PluginInstallation {
                    scope,
                    location: plugin.path.clone(),
                    version: plugin.version.clone(),
                    scope_enabled: None,
                }]
            } else {
                Vec::new()
            },
            enabled: Tri::Unknown,
            declared: None,
            errors: Vec::new(),
            actions,
            diagnostics: Vec::new(),
        });
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

/// `plugin details` has no `--json`, and it counts component directories
/// rather than naming components, so its text stays a native diagnostic. Its
/// `kind:` line (`local: <path>` or `git: <url>`) is the only place Grok says
/// what a plugin installed from a path or a URL is.
fn apply_details(entry: &mut ProviderPluginEntry, text: &str) {
    let detail = |key: &str| {
        text.lines()
            .find_map(|line| line.trim().strip_prefix(key))
            .map(str::trim)
    };
    if entry.description.is_none() {
        entry.description = detail("description:").map(str::to_string);
    }
    if entry.source.marketplace.is_none() {
        entry.source.kind = match detail("kind:").and_then(|kind| kind.split_once(':')) {
            Some(("local", _)) => PluginSourceKind::LocalPath,
            Some(("git", _)) => PluginSourceKind::Git,
            _ => PluginSourceKind::Unknown,
        };
    }
    entry
        .diagnostics
        .push(("details".into(), text.trim_end().to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/plugins/grok/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
    }

    fn output(stdout: &str, stderr: &str, success: bool) -> CliOutput {
        CliOutput {
            success,
            status: if success {
                "exit status: 0"
            } else {
                "exit status: 1"
            }
            .into(),
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    /// Recorded after installing `alpha@mkt` (local marketplace), `zeta@tools`
    /// (git marketplace) and the two-plugin repository `multi` from a path.
    #[test]
    fn listing_names_plugins_as_the_cli_takes_them_and_offers_what_it_can_carry_out() {
        let listed = parse_listed(&fixture("plugin-list-available.json")).unwrap();
        let marketplaces = parse_marketplaces(&fixture("marketplace-list.json")).unwrap();
        assert_eq!(
            marketplaces,
            [
                ProviderPluginMarketplace {
                    name: "mkt".into(),
                    source: "/tmp/tcode-probe-grok/mkt".into(),
                    kind: PluginSourceKind::LocalPath,
                    location: None,
                },
                ProviderPluginMarketplace {
                    name: "tools".into(),
                    source: "file:///tmp/tcode-probe-grok/tools".into(),
                    kind: PluginSourceKind::Git,
                    location: None,
                },
            ]
        );
        let mut listing = listing(listed, marketplaces);
        let ids: Vec<&str> = listing.entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            ids,
            ["alpha@mkt", "epsilon", "delta", "zeta@tools", "beta@mkt"]
        );
        let user = PluginScope::User;

        let alpha = &listing.entries[0];
        assert_eq!(
            alpha.installations,
            [PluginInstallation {
                scope: user,
                location: Some(
                    "/tmp/tcode-probe-grok/home/.grok/installed-plugins/alpha-e84495d9".into()
                ),
                version: Some("1.0.0".into()),
                scope_enabled: None,
            }]
        );
        assert_eq!(
            alpha.source,
            PluginSource {
                marketplace: Some("mkt".into()),
                kind: PluginSourceKind::LocalPath,
            }
        );
        assert_eq!(alpha.enabled, Tri::Unknown);
        assert_eq!(
            alpha.actions,
            [
                PluginAction::Update { scope: user },
                PluginAction::Uninstall { scope: user }
            ]
        );
        assert_eq!(listing.entries[3].source.kind, PluginSourceKind::Git);

        // Each plugin of a path-installed repository is a live link that
        // `update` leaves alone, and uninstalling one needs `--confirm`. As in
        // `list`, its kind comes from its details, which Grok prints per
        // repository.
        for multi in &mut listing.entries[1..3] {
            apply_details(multi, &fixture("details-delta.txt"));
            assert_eq!(multi.source.kind, PluginSourceKind::LocalPath);
            assert!(multi.actions.is_empty(), "{multi:?}");
        }
        assert_eq!(listing.entries[2].description, None);

        let beta = &listing.entries[4];
        assert!(beta.installations.is_empty());
        assert_eq!(beta.version.as_deref(), Some("0.1.0"));
        assert_eq!(beta.description.as_deref(), Some("Skill only"));
        assert_eq!(beta.actions, [PluginAction::Install { scope: user }]);

        // Removing a marketplace uninstalls its plugins, which the host asks
        // about first.
        assert_eq!(
            listing.marketplace_actions,
            [
                MarketplaceAction::Add,
                MarketplaceAction::Remove {
                    marketplace: "mkt".into(),
                    uninstalls: vec!["alpha@mkt".into()],
                },
                MarketplaceAction::Remove {
                    marketplace: "tools".into(),
                    uninstalls: vec!["zeta@tools".into()],
                },
            ]
        );

        apply_details(&mut listing.entries[0], &fixture("details-alpha.txt"));
        let alpha = &listing.entries[0];
        assert_eq!(
            alpha.description.as_deref(),
            Some("Skill, command, SessionStart hook and an unreachable HTTP MCP server")
        );
        assert!(alpha.declared.is_none());
        assert!(matches!(alpha.diagnostics.as_slice(),
            [(key, text)] if key == "details"
                && text.contains("components: 1 skill dir(s), 1 command dir(s)")));
    }

    /// Recorded after installing the same two-plugin repository from a
    /// `file://` URL instead of its path.
    #[test]
    fn a_repository_installed_from_a_url_is_a_git_checkout() {
        let listed = parse_listed(&fixture("plugin-list-git-install.json")).unwrap();
        let mut listing = listing(listed, Vec::new());
        assert_eq!(listing.entries.len(), 2);
        for entry in &mut listing.entries {
            apply_details(entry, &fixture("details-git-install.txt"));
            assert_eq!(entry.source.kind, PluginSourceKind::Git);
        }
    }

    #[test]
    fn trust_is_passed_only_for_the_challenge_a_person_accepted() {
        let refused = || output("", &fixture("install-untrusted.stderr.txt"), false);
        let InstallStep::Finished(PluginOpOutcome::AcceptCommand(challenge)) =
            install_step(refused(), None).unwrap()
        else {
            panic!("an untrusted install must ask first");
        };
        assert_eq!(challenge.command, "grok plugin install alpha@mkt --trust");
        assert_eq!(challenge.mode, None);
        assert!(
            challenge.native_text.starts_with(
                "Installing \"alpha\" from marketplace \"mkt\" requires confirmation."
            ),
            "{}",
            challenge.native_text
        );
        assert_eq!(
            challenge.sha256,
            "73bbb8b21e0f3eccd8a8aa4b0e57361ff136482a4d41390cc332bfe3314293bc"
        );

        assert!(matches!(
            install_step(refused(), Some(&challenge.sha256)).unwrap(),
            InstallStep::Trust
        ));
        // An acceptance of anything other than what Grok shows now is asked again.
        let stale = "0".repeat(64);
        assert!(matches!(
            install_step(refused(), Some(&stale)).unwrap(),
            InstallStep::Finished(PluginOpOutcome::AcceptCommand(again)) if again == challenge
        ));
        assert_eq!(
            native_message(
                &install_step(
                    output("", &fixture("install-unknown.stderr.txt"), false),
                    Some(&challenge.sha256)
                )
                .err()
                .unwrap()
            ),
            "Error: No marketplace plugin named \"nosuch\" in \"tools\"."
        );
        let InstallStep::Finished(installed) =
            install_step(output(&fixture("install-trusted.txt"), "", true), None).unwrap()
        else {
            panic!("an install Grok did not refuse is finished");
        };
        assert_eq!(
            installed,
            PluginOpOutcome::Done {
                diagnostics: vec![(
                    "message".into(),
                    "Installed 1 plugin(s) from mkt: alpha".into()
                )],
            }
        );
    }

    #[test]
    fn management_argv_never_consents_and_targets_plugins_by_their_listed_name() {
        let user = PluginScope::User;
        let cases = [
            (
                PluginOp::Install {
                    id: "alpha@mkt".into(),
                    scope: user,
                    accept_command: Some("0".repeat(64)),
                },
                vec!["plugin", "install", "alpha@mkt"],
            ),
            (
                PluginOp::Update {
                    id: "alpha@mkt".into(),
                    scope: user,
                    accept_command: None,
                },
                vec!["plugin", "update", "alpha"],
            ),
            (
                PluginOp::Uninstall {
                    id: "alpha@mkt".into(),
                    scope: user,
                },
                vec!["plugin", "uninstall", "alpha"],
            ),
            (
                PluginOp::AddMarketplace {
                    source: "file:///tmp/tcode-probe-grok/tools".into(),
                },
                vec![
                    "plugin",
                    "marketplace",
                    "add",
                    "file:///tmp/tcode-probe-grok/tools",
                ],
            ),
            (
                PluginOp::RemoveMarketplace { name: "mkt".into() },
                vec!["plugin", "marketplace", "remove", "mkt"],
            ),
        ];
        let context = PluginContext {
            binary_path: Some(PathBuf::from("/opt/grok/bin/grok")),
            launch_env: crate::LaunchEnv {
                env: Vec::new(),
                home: Some(PathBuf::from("/tmp/isolated/.grok")),
            },
            cwd: std::env::temp_dir(),
            project: false,
        };
        for (op, expected) in cases {
            let argv = op_args(&op).unwrap();
            assert_eq!(argv, expected);
            let command = management_command(&context, &argv).unwrap();
            assert!(
                command
                    .get_envs()
                    .any(|env| env == ("GROK_HOME".as_ref(), Some("/tmp/isolated/.grok".as_ref())))
            );
        }
        assert!(
            op_args(&PluginOp::Install {
                id: "alpha@mkt".into(),
                scope: PluginScope::Project,
                accept_command: None,
            })
            .is_err()
        );
        assert!(
            op_args(&PluginOp::AddMarketplace {
                source: "--force".into()
            })
            .is_err()
        );
    }

    #[test]
    fn update_reports_the_version_change_grok_prints() {
        assert_eq!(
            update_outcome(&fixture("update.txt")),
            PluginOpOutcome::Done {
                diagnostics: vec![
                    ("update_outcome".into(), "updated".into()),
                    ("old_version".into(), "1.0.0".into()),
                    ("new_version".into(), "1.1.0".into()),
                ],
            }
        );
        assert_eq!(
            update_outcome(&fixture("update-path-plugin.txt")),
            PluginOpOutcome::Done {
                diagnostics: vec![(
                    "message".into(),
                    "multi-0b1623cc: local symlink, already live".into()
                )],
            }
        );
    }
}
