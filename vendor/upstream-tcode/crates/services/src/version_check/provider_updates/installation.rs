use std::future::Future;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent::ProviderKind;
use serde_json::Value;

use super::{InstallSource, npm_package};

/// A plan is bound to the binary and manager evidence observed when it was made.
/// Re-resolve it before executing: a PATH or configuration change can replace its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installation {
    pub source: InstallSource,
    pub update: Option<UpdateCommand>,
    binary: PathBuf,
    canonical: PathBuf,
    identity: Vec<String>,
    latest: LatestQuery,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCommand {
    pub(super) program: PathBuf,
    pub(super) args: Vec<String>,
    pub(super) cwd: PathBuf,
    pub(super) env: Vec<(String, String)>,
    pub requires_terminal: bool,
}

impl UpdateCommand {
    /// Render only the command and working directory, never provider secrets.
    pub fn display(&self) -> String {
        self.display_for_shell(cfg!(windows))
    }

    /// A compact label; copying uses `display` with the complete install context.
    pub fn summary(&self) -> String {
        std::iter::once(
            self.program
                .file_name()
                .unwrap_or(self.program.as_os_str())
                .to_string_lossy()
                .into_owned(),
        )
        .chain(self.args.iter().cloned())
        .map(|part| quote(&part, cfg!(windows)))
        .collect::<Vec<_>>()
        .join(" ")
    }

    fn display_for_shell(&self, windows: bool) -> String {
        let command = std::iter::once(self.program.to_string_lossy().into_owned())
            .chain(self.args.iter().cloned())
            .map(|part| quote(&part, windows))
            .collect::<Vec<_>>()
            .join(" ");
        let bindings = self
            .env
            .iter()
            .filter(|(key, _)| DISPLAY_ENV.contains(&key.as_str()))
            .map(|(key, value)| {
                if windows {
                    format!("$env:{key} = {}; ", quote(value, true))
                } else {
                    format!("{key}={} ", quote(value, false))
                }
            })
            .collect::<String>();
        if windows {
            format!(
                "Set-Location -LiteralPath {}; {bindings}& {command}",
                quote(&self.cwd.to_string_lossy(), true)
            )
        } else {
            format!(
                "cd {} && {bindings}{command}",
                quote(&self.cwd.to_string_lossy(), false)
            )
        }
    }

    pub async fn run(&self) -> bool {
        if self.requires_terminal {
            return false;
        }
        run_process(self, Duration::from_secs(300)).await.is_some()
    }
}

fn quote(value: &str, windows: bool) -> String {
    if windows {
        format!("'{}'", value.replace('\'', "''"))
    } else if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-/.:@=+".contains(c))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

// These values select installation roots or configuration scopes. Provider
// credentials and arbitrary environment values must never reach UI text.
const DISPLAY_ENV: &[&str] = &[
    "HOME",
    "USERPROFILE",
    "PATH",
    "MISE_DATA_DIR",
    "MISE_CONFIG_DIR",
    "MISE_CONFIG_FILE",
    "MISE_GLOBAL_CONFIG_FILE",
    "MISE_SYSTEM_CONFIG_FILE",
    "MISE_ENV",
    "MISE_LOCKED",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "VOLTA_HOME",
    "ASDF_DATA_DIR",
    "PNPM_HOME",
    "BUN_INSTALL",
    "BUN_INSTALL_GLOBAL_DIR",
    "BUN_INSTALL_BIN",
    "SCOOP",
    "SCOOP_GLOBAL",
    "ChocolateyInstall",
    "NPM_CONFIG_PREFIX",
    "npm_config_prefix",
    "CLAUDE_CONFIG_DIR",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LatestQuery {
    None,
    Registry(String),
    Package {
        command: UpdateCommand,
        yarn: bool,
    },
    Claude {
        channel: String,
        maximum: Option<(u32, u32, u32)>,
    },
    Mise {
        command: UpdateCommand,
        tool: String,
        current: String,
    },
    Brew {
        command: UpdateCommand,
        target: String,
        cask: bool,
    },
    System(super::system_managers::Query),
}

pub(super) trait Probe: Sync {
    fn output(&self, command: &UpdateCommand) -> impl Future<Output = Option<String>> + Send;
    fn canonical(&self, path: &Path) -> Option<PathBuf>;
    fn read(&self, path: &Path) -> Option<String>;
    fn which(&self, name: &str, env: &[(String, String)]) -> Option<PathBuf>;
}

struct SystemProbe;

impl Probe for SystemProbe {
    async fn output(&self, command: &UpdateCommand) -> Option<String> {
        run_process(command, Duration::from_secs(12)).await
    }

    fn canonical(&self, path: &Path) -> Option<PathBuf> {
        path.canonicalize().ok()
    }

    fn read(&self, path: &Path) -> Option<String> {
        let metadata = std::fs::metadata(path).ok()?;
        (metadata.len() <= 1024 * 1024)
            .then(|| std::fs::read_to_string(path).ok())
            .flatten()
    }

    fn which(&self, name: &str, env: &[(String, String)]) -> Option<PathBuf> {
        let path = env_value(env, "PATH")?;
        let mut names = Vec::new();
        if cfg!(windows) && Path::new(name).extension().is_none() {
            names.extend(
                env_value(env, "PATHEXT")
                    .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into())
                    .split(';')
                    .map(|extension| format!("{name}{extension}")),
            );
        }
        names.push(name.to_string());
        std::env::split_paths(&path)
            .filter(|path| path.is_absolute())
            .find_map(|dir| {
                names.iter().map(|name| dir.join(name)).find(|path| {
                    let Ok(metadata) = std::fs::metadata(path) else {
                        return false;
                    };
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt as _;
                        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                    }
                    #[cfg(not(unix))]
                    {
                        metadata.is_file()
                    }
                })
            })
    }
}

async fn run_process(command: &UpdateCommand, timeout: Duration) -> Option<String> {
    use smol::io::AsyncReadExt as _;

    let mut process = crate::process::async_command(&command.program);
    process
        .args(&command.args)
        .envs(command.env.iter().cloned())
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .current_dir(&command.cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = process.spawn().ok()?;
    let stdout = child.stdout.take()?;
    smol::future::race(
        async {
            let mut output = Vec::new();
            stdout
                .take(4 * 1024 * 1024 + 1)
                .read_to_end(&mut output)
                .await
                .ok()?;
            if output.len() > 4 * 1024 * 1024 {
                return None;
            }
            child
                .status()
                .await
                .ok()?
                .success()
                .then(|| String::from_utf8(output).ok())
                .flatten()
        },
        async {
            smol::Timer::after(timeout).await;
            None
        },
    )
    .await
}

pub(super) fn env_value(env: &[(String, String)], key: &str) -> Option<String> {
    env.iter()
        .rev()
        .find(|(name, _)| {
            if cfg!(windows) {
                name.eq_ignore_ascii_case(key)
            } else {
                name == key
            }
        })
        .map(|(_, value)| value.clone())
        .or_else(|| std::env::var(key).ok())
}

pub(super) struct Context<'a, P> {
    pub probe: &'a P,
    pub provider: ProviderKind,
    pub binary: PathBuf,
    pub canonical: PathBuf,
    pub cwd: PathBuf,
    pub env: &'a [(String, String)],
}

impl<P: Probe> Context<'_, P> {
    pub fn command(&self, name: &str, args: &[&str]) -> Option<UpdateCommand> {
        Some(self.at(self.probe.which(name, self.env)?, args))
    }

    pub fn at(&self, program: PathBuf, args: &[&str]) -> UpdateCommand {
        UpdateCommand {
            program,
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            cwd: self.cwd.clone(),
            env: self.env.to_vec(),
            requires_terminal: false,
        }
    }

    pub fn managed(&self, source: InstallSource, update: Option<UpdateCommand>) -> Installation {
        self.installation(source, update, LatestQuery::None, Vec::new())
    }

    pub fn installation(
        &self,
        source: InstallSource,
        update: Option<UpdateCommand>,
        latest: LatestQuery,
        identity: Vec<String>,
    ) -> Installation {
        Installation {
            source,
            update,
            binary: self.binary.clone(),
            canonical: self.canonical.clone(),
            latest,
            identity,
        }
    }

    pub fn home(&self) -> Option<PathBuf> {
        env_value(self.env, if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
    }

    pub fn same(&self, first: &Path, second: &Path) -> bool {
        self.probe
            .canonical(first)
            .zip(self.probe.canonical(second))
            .is_some_and(|(first, second)| first == second)
    }
}

pub async fn resolve_installation(
    provider: ProviderKind,
    binary: &Path,
    env: &[(String, String)],
) -> Installation {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let mut env = env.to_vec();
    for key in DISPLAY_ENV {
        if !env.iter().any(|(name, _)| name == key)
            && let Ok(value) = std::env::var(key)
        {
            env.push(((*key).to_string(), value));
        }
    }
    resolve(&SystemProbe, provider, binary, &env, cwd).await
}

async fn resolve<P: Probe>(
    probe: &P,
    provider: ProviderKind,
    binary: &Path,
    env: &[(String, String)],
    cwd: PathBuf,
) -> Installation {
    let context = Context {
        probe,
        provider,
        binary: binary.to_path_buf(),
        canonical: probe
            .canonical(binary)
            .unwrap_or_else(|| binary.to_path_buf()),
        cwd,
        env,
    };
    if provider == ProviderKind::Acp || probe.canonical(binary).is_none() {
        return context.managed(InstallSource::Unknown, None);
    }
    if let Some(installation) = mise(&context).await {
        return installation;
    }
    if let Some(installation) = version_managers(&context).await {
        return installation;
    }
    if let Some(installation) = super::system_managers::resolve(&context).await {
        return installation;
    }
    if let Some(installation) = brew(&context).await {
        return installation;
    }
    if let Some(installation) = javascript(&context).await {
        return installation;
    }
    native(&context).unwrap_or_else(|| context.managed(InstallSource::Unknown, None))
}

/// Run on the host's blocking worker: release registries use synchronous HTTP.
pub fn latest_version(installation: &Installation) -> Option<String> {
    smol::block_on(latest(&SystemProbe, &installation.latest))
}

async fn latest<P: Probe>(probe: &P, query: &LatestQuery) -> Option<String> {
    match query {
        LatestQuery::None => None,
        LatestQuery::Registry(package) => {
            let url = format!("https://registry.npmjs.org/{package}/latest");
            let response = ureq::get(&url)
                .timeout(Duration::from_secs(10))
                .call()
                .ok()?;
            let mut bytes = Vec::new();
            response
                .into_reader()
                .take(1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.len() > 1024 * 1024 {
                return None;
            }
            let json: Value = serde_json::from_slice(&bytes).ok()?;
            json.get("version")?.as_str().map(str::to_string)
        }
        LatestQuery::Package { command, yarn } => {
            let output = probe.output(command).await?;
            if *yarn {
                output
                    .lines()
                    .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    .find_map(|value| {
                        value
                            .get("data")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
            } else {
                serde_json::from_str::<Value>(&output)
                    .ok()?
                    .as_str()
                    .map(str::to_string)
            }
        }
        LatestQuery::Claude { channel, maximum } => {
            let url = format!("https://downloads.claude.ai/claude-code-releases/{channel}");
            let response = ureq::get(&url)
                .timeout(Duration::from_secs(10))
                .call()
                .ok()?;
            let mut version = String::new();
            response
                .into_reader()
                .take(256)
                .read_to_string(&mut version)
                .ok()?;
            let parsed = super::parse_version(&version)?;
            if maximum.is_some_and(|maximum| parsed > maximum) {
                return None;
            }
            Some(version.trim().to_string())
        }
        LatestQuery::Mise {
            command,
            tool,
            current,
        } => {
            let json: Value = serde_json::from_str(&probe.output(command).await?).ok()?;
            let entries = json.as_object()?;
            match entries.get(tool) {
                Some(entry) => entry.get("latest")?.as_str().map(str::to_string),
                None if entries.is_empty() => Some(current.clone()),
                None => None,
            }
        }
        LatestQuery::Brew {
            command,
            target,
            cask,
        } => {
            let json: Value = serde_json::from_str(&probe.output(command).await?).ok()?;
            let records = json
                .get(if *cask { "casks" } else { "formulae" })?
                .as_array()?;
            let record = records
                .iter()
                .find(|record| brew_name(record, *cask) == Some(target.as_str()))?;
            if record.get("pinned").and_then(Value::as_bool) == Some(true) {
                return None;
            }
            if *cask {
                record.get("version")?.as_str().map(str::to_string)
            } else {
                record
                    .get("versions")?
                    .get("stable")?
                    .as_str()
                    .map(str::to_string)
            }
        }
        LatestQuery::System(query) => super::system_managers::latest(probe, query).await,
    }
}

fn binary_name(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::ClaudeCode => "claude",
        ProviderKind::Codex => "codex",
        ProviderKind::Pi => "pi",
        ProviderKind::OpenCode => "opencode",
        ProviderKind::Cursor => "cursor-agent",
        ProviderKind::Grok => "grok",
        ProviderKind::Acp => "",
    }
}

fn within<P: Probe>(context: &Context<'_, P>, path: &Path, root: &Path) -> bool {
    path.starts_with(root)
        || context
            .probe
            .canonical(root)
            .is_some_and(|root| path.starts_with(root))
}

async fn mise<P: Probe>(context: &Context<'_, P>) -> Option<Installation> {
    let data = env_value(context.env, "MISE_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            env_value(context.env, "XDG_DATA_HOME").map(|path| PathBuf::from(path).join("mise"))
        })
        .or_else(|| context.home().map(|home| home.join(".local/share/mise")));
    let shims = env_value(context.env, "MISE_SHIMS_DIR")
        .map(PathBuf::from)
        .or_else(|| data.as_ref().map(|data| data.join("shims")));
    let is_shim = shims
        .as_ref()
        .is_some_and(|root| context.binary.starts_with(root));
    let in_data = data
        .as_ref()
        .is_some_and(|root| within(context, &context.canonical, &root.join("installs")));
    let fallback = || (is_shim || in_data).then(|| context.managed(InstallSource::Mise, None));
    let Some(list) = context.command("mise", &["ls", "--json"]) else {
        return fallback();
    };
    let Some(json) = context
        .probe
        .output(&list)
        .await
        .and_then(|json| serde_json::from_str::<Value>(&json).ok())
    else {
        return fallback();
    };
    let Some(tools) = json.as_object() else {
        return fallback();
    };
    let which = context.at(
        list.program.clone(),
        &["which", binary_name(context.provider)],
    );
    let selected = context
        .probe
        .output(&which)
        .await
        .and_then(|path| context.probe.canonical(Path::new(path.trim())));
    let Some(selected) = selected else {
        return fallback();
    };
    if !is_shim && selected != context.canonical {
        return fallback();
    }
    for (tool, records) in tools {
        for record in records.as_array().into_iter().flatten() {
            let Some(root) = record.get("install_path").and_then(Value::as_str) else {
                continue;
            };
            if !within(context, &selected, Path::new(root)) {
                continue;
            }
            let metadata = context.at(list.program.clone(), &["tool", tool, "--json"]);
            let backend = context
                .probe
                .output(&metadata)
                .await
                .and_then(|output| serde_json::from_str::<Value>(&output).ok())
                .and_then(|value| {
                    value
                        .get("backend")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
            let Some(backend) = backend else {
                return Some(context.managed(InstallSource::Mise, None));
            };
            if runtime_backend(&backend) {
                return if is_shim {
                    Some(context.managed(InstallSource::Mise, None))
                } else {
                    None
                };
            }
            if !provider_backend(context.provider, &backend) {
                return Some(context.managed(InstallSource::Mise, None));
            }
            if record.get("active").and_then(Value::as_bool) != Some(true)
                || record.get("installed").and_then(Value::as_bool) != Some(true)
            {
                return Some(context.managed(InstallSource::Mise, None));
            }
            let source = record
                .get("source")
                .and_then(|value| value.get("path"))
                .and_then(Value::as_str);
            let requested = record.get("requested_version").and_then(Value::as_str);
            let current = record.get("version").and_then(Value::as_str);
            let (Some(source), Some(requested), Some(current)) = (source, requested, current)
            else {
                return Some(context.managed(InstallSource::Mise, None));
            };
            // External path/ref aliases are not a release channel that can be upgraded safely.
            if requested.starts_with("path:") || requested.starts_with("ref:") {
                return Some(context.managed(InstallSource::Mise, None));
            }
            let update = context.at(list.program.clone(), &["upgrade", "--yes", "--", tool]);
            let query = context.at(list.program.clone(), &["outdated", "--json", "--", tool]);
            return Some(context.installation(
                InstallSource::Mise,
                Some(update),
                LatestQuery::Mise {
                    command: query,
                    tool: tool.clone(),
                    current: current.to_string(),
                },
                vec![
                    tool.clone(),
                    backend,
                    source.to_string(),
                    requested.to_string(),
                    selected.to_string_lossy().into_owned(),
                ],
            ));
        }
    }
    fallback()
}

fn runtime_backend(backend: &str) -> bool {
    matches!(
        backend,
        "core:node"
            | "core:bun"
            | "core:deno"
            | "asdf:nodejs"
            | "vfox:nodejs"
            | "aqua:oven-sh/bun"
            | "github:oven-sh/bun"
            | "npm:pnpm"
            | "npm:yarn"
    )
}

fn provider_backend(provider: ProviderKind, backend: &str) -> bool {
    if let Some(package) = backend.strip_prefix("npm:") {
        return recognized_package(provider, package);
    }
    let Some((kind, repository)) = backend.split_once(':') else {
        return false;
    };
    if !matches!(kind, "aqua" | "github" | "ubi") {
        return false;
    }
    matches!(
        (provider, repository),
        (ProviderKind::ClaudeCode, "anthropics/claude-code")
            | (ProviderKind::Codex, "openai/codex")
            | (ProviderKind::Pi, "earendil-works/pi" | "badlogic/pi-mono")
            | (
                ProviderKind::OpenCode,
                "anomalyco/opencode" | "sst/opencode"
            )
    )
}

async fn version_managers<P: Probe>(context: &Context<'_, P>) -> Option<Installation> {
    let home = context.home()?;
    let volta = env_value(context.env, "VOLTA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".volta"));
    if context.binary.starts_with(&volta) || within(context, &context.canonical, &volta) {
        let Some(which) = context.command("volta", &["which", binary_name(context.provider)])
        else {
            return Some(context.managed(InstallSource::Volta, None));
        };
        let actual = context
            .probe
            .output(&which)
            .await
            .and_then(|path| context.probe.canonical(Path::new(path.trim())));
        let shim = context.binary.parent() == Some(volta.join("bin").as_path());
        let Some(actual) = actual.filter(|actual| shim || *actual == context.canonical) else {
            return Some(context.managed(InstallSource::Volta, None));
        };
        if within(context, &actual, &volta.join("tools/image/packages"))
            && let Some(package) = package_for_binary(context, &actual)
        {
            let target = format!("{}@latest", package.name);
            let npm = context.at(which.program.clone(), &["which", "npm"]);
            let latest = if let Some(npm) = context
                .probe
                .output(&npm)
                .await
                .and_then(|path| context.probe.canonical(Path::new(path.trim())))
            {
                LatestQuery::Package {
                    command: context.at(npm, &["view", &target, "version", "--json"]),
                    yarn: false,
                }
            } else {
                LatestQuery::None
            };
            let update = context.at(which.program, &["install", &target]);
            return Some(context.installation(
                InstallSource::Volta,
                Some(update),
                latest,
                vec![package.name, actual.to_string_lossy().into_owned()],
            ));
        }
        return Some(context.managed(InstallSource::Volta, None));
    }
    let asdf = env_value(context.env, "ASDF_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".asdf"));
    if context.binary.starts_with(asdf.join("shims"))
        || within(context, &context.canonical, &asdf.join("installs"))
    {
        // asdf install does not activate a version; updating .tool-versions is a
        // separate policy decision, so never substitute a nested npm update.
        return Some(context.managed(InstallSource::Asdf, None));
    }
    None
}

fn brew_name(record: &Value, cask: bool) -> Option<&str> {
    record
        .get(if cask { "full_token" } else { "full_name" })
        .or_else(|| record.get(if cask { "token" } else { "name" }))?
        .as_str()
}

async fn brew<P: Probe>(context: &Context<'_, P>) -> Option<Installation> {
    let info = context.command("brew", &["info", "--json=v2", "--installed"])?;
    let json: Value = serde_json::from_str(&context.probe.output(&info).await?).ok()?;
    let prefix_command = context.at(info.program.clone(), &["--prefix"]);
    let prefix = PathBuf::from(context.probe.output(&prefix_command).await?.trim());
    for cask in [false, true] {
        for record in json
            .get(if cask { "casks" } else { "formulae" })
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(target) = brew_name(record, cask) else {
                continue;
            };
            let owned_name = record
                .get(if cask { "token" } else { "name" })
                .and_then(Value::as_str)?;
            if !provider_brew_package(context.provider, owned_name) {
                continue;
            }
            let owns = if cask {
                let Some(token) = record.get("token").and_then(Value::as_str) else {
                    continue;
                };
                within(
                    context,
                    &context.canonical,
                    &prefix.join("Caskroom").join(token),
                ) && record
                    .get("installed")
                    .is_some_and(|installed| !installed.is_null())
            } else {
                let Some(name) = record.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let root = prefix.join("Cellar").join(name);
                record
                    .get("installed")
                    .and_then(Value::as_array)
                    .is_some_and(|versions| {
                        versions.iter().any(|version| {
                            version
                                .get("version")
                                .and_then(Value::as_str)
                                .is_some_and(|version| {
                                    within(context, &context.canonical, &root.join(version))
                                })
                        })
                    })
            };
            if !owns {
                continue;
            }
            let kind = if cask { "--cask" } else { "--formula" };
            let query = context.at(info.program.clone(), &["info", "--json=v2", kind, target]);
            let update = if record.get("pinned").and_then(Value::as_bool) == Some(true) {
                None
            } else {
                Some(context.at(info.program.clone(), &["upgrade", kind, "--", target]))
            };
            return Some(context.installation(
                InstallSource::Brew,
                update,
                LatestQuery::Brew {
                    command: query,
                    target: target.to_string(),
                    cask,
                },
                vec![target.to_string(), prefix.to_string_lossy().into_owned()],
            ));
        }
    }
    None
}

fn provider_brew_package(provider: ProviderKind, name: &str) -> bool {
    match provider {
        ProviderKind::ClaudeCode => matches!(name, "claude-code" | "claude-code@latest"),
        ProviderKind::Codex => matches!(name, "codex" | "codex@alpha"),
        ProviderKind::Pi => name == "pi-coding-agent",
        ProviderKind::OpenCode => matches!(name, "opencode" | "opencode-v2"),
        ProviderKind::Cursor | ProviderKind::Grok | ProviderKind::Acp => false,
    }
}

struct Package {
    name: String,
    root: PathBuf,
}

fn recognized_package(provider: ProviderKind, name: &str) -> bool {
    name == npm_package(provider)
        || (provider == ProviderKind::OpenCode && name == "@opencode/cli")
        || (provider == ProviderKind::Pi && name == "@mariozechner/pi-coding-agent")
}

/// A node_modules component alone proves nothing. The owning manifest must
/// name this provider and its declared bin must resolve to the selected file.
fn package_for_binary<P: Probe>(context: &Context<'_, P>, binary: &Path) -> Option<Package> {
    for root in binary.ancestors().skip(1) {
        let Some(json) = context.probe.read(&root.join("package.json")) else {
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<Value>(&json) else {
            continue;
        };
        let Some(name) = manifest.get("name").and_then(Value::as_str) else {
            continue;
        };
        if !recognized_package(context.provider, name) {
            continue;
        }
        let Some(bin) = manifest.get("bin") else {
            continue;
        };
        let path = bin.as_str().or_else(|| {
            bin.get(binary_name(context.provider))
                .and_then(Value::as_str)
        });
        if path.is_some_and(|path| context.same(&root.join(path), binary)) {
            return Some(Package {
                name: name.to_string(),
                root: root.to_path_buf(),
            });
        }
    }
    None
}

async fn javascript<P: Probe>(context: &Context<'_, P>) -> Option<Installation> {
    let Some(package) = package_for_binary(context, &context.canonical) else {
        return javascript_windows(context).await;
    };
    // Each candidate must report the exact global package root. The existence
    // of another copy of the package in a manager's store is not ownership.
    for (manager, source, query_args) in [
        ("pnpm", InstallSource::Pnpm, vec!["root", "--global"]),
        ("yarn", InstallSource::Yarn, vec!["global", "dir"]),
        ("npm", InstallSource::Npm, vec!["root", "--global"]),
    ] {
        let Some(query) = context.command(manager, &query_args) else {
            continue;
        };
        let Some(root) = context.probe.output(&query).await else {
            continue;
        };
        let mut root = PathBuf::from(root.trim());
        if manager == "yarn" {
            root.push("node_modules");
        }
        if !context.same(&root.join(&package.name), &package.root) {
            continue;
        }
        if let Some(installation) = javascript_plan(
            context,
            source,
            query.program,
            &root,
            &package,
            cfg!(windows),
        )
        .await
        {
            return Some(installation);
        }
    }
    // Bun's global package and bin directories can be independently configured.
    // Verify its reported bin symlink and bind both locations explicitly.
    if let Some(query) = context.command("bun", &["pm", "bin", "--global"])
        && let Some(bin) = context.probe.output(&query).await
    {
        let bin = PathBuf::from(bin.trim());
        if context.same(&bin.join(binary_name(context.provider)), &context.canonical)
            && let Some(global_dir) = bun_global_directory(context, &package)
        {
            let target = format!("{}@latest", package.name);
            let mut update = context.at(query.program, &["add", "--global", "--", &target]);
            update.env.retain(|(name, _)| {
                !matches!(name.as_str(), "BUN_INSTALL_GLOBAL_DIR" | "BUN_INSTALL_BIN")
            });
            update.env.push((
                "BUN_INSTALL_GLOBAL_DIR".into(),
                global_dir.to_string_lossy().into_owned(),
            ));
            update
                .env
                .push(("BUN_INSTALL_BIN".into(), bin.to_string_lossy().into_owned()));
            let mut latest = update.clone();
            latest.args = vec!["info".into(), target, "version".into(), "--json".into()];
            return Some(context.installation(
                InstallSource::Bun,
                Some(update),
                LatestQuery::Package {
                    command: latest,
                    yarn: false,
                },
                vec![package.name, package.root.to_string_lossy().into_owned()],
            ));
        }
    }
    None
}

fn bun_global_directory<'a, P: Probe>(
    context: &Context<'_, P>,
    package: &'a Package,
) -> Option<&'a Path> {
    package
        .root
        .ancestors()
        .filter(|root| root.file_name().is_some_and(|name| name == "node_modules"))
        .filter_map(Path::parent)
        .find(|directory| {
            // Isolated linking puts another node_modules inside the .bun store;
            // only the global project's manifest and direct link own the install.
            context
                .probe
                .read(&directory.join("package.json"))
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .is_some_and(|json| {
                    json.get("dependencies")
                        .is_some_and(|dependencies| dependencies.get(&package.name).is_some())
                })
                && context.same(
                    &directory.join("node_modules").join(&package.name),
                    &package.root,
                )
        })
}

async fn javascript_plan<P: Probe>(
    context: &Context<'_, P>,
    source: InstallSource,
    program: PathBuf,
    root: &Path,
    package: &Package,
    windows: bool,
) -> Option<Installation> {
    let target = format!("{}@latest", package.name);
    let update = match source {
        InstallSource::Npm => {
            let prefix_query = context.at(program.clone(), &["prefix", "--global"]);
            let prefix = context.probe.output(&prefix_query).await?;
            let prefix = prefix.trim();
            let expected_root = if windows {
                PathBuf::from(prefix).join("node_modules")
            } else {
                PathBuf::from(prefix).join("lib/node_modules")
            };
            if !context.same(&expected_root, root) {
                return None;
            }
            context.at(
                program,
                &["install", "--global", "--prefix", prefix, "--", &target],
            )
        }
        InstallSource::Pnpm => {
            let bin_query = context.at(program.clone(), &["bin", "--global"]);
            let bin = context.probe.output(&bin_query).await?;
            let bin = PathBuf::from(bin.trim());
            // pnpm's global root has a layout version below global-dir.
            let global_dir = root.parent()?.parent()?;
            context.at(
                program,
                &[
                    "add",
                    "--global",
                    "--global-dir",
                    &global_dir.to_string_lossy(),
                    "--global-bin-dir",
                    &bin.to_string_lossy(),
                    "--",
                    &target,
                ],
            )
        }
        InstallSource::Yarn => {
            let dir = root.parent()?;
            let prefix_query = context.at(program.clone(), &["global", "bin"]);
            let bin = context.probe.output(&prefix_query).await?;
            let prefix = Path::new(bin.trim()).parent()?;
            context.at(
                program,
                &[
                    "global",
                    "add",
                    "--global-folder",
                    &dir.to_string_lossy(),
                    "--prefix",
                    &prefix.to_string_lossy(),
                    "--",
                    &target,
                ],
            )
        }
        _ => unreachable!(),
    };
    let mut query = update.clone();
    query.args = vec![
        if source == InstallSource::Yarn {
            "info"
        } else {
            "view"
        }
        .into(),
        target,
        "version".into(),
        "--json".into(),
    ];
    if source == InstallSource::Npm
        && let Some(prefix) = update
            .args
            .iter()
            .position(|arg| arg == "--prefix")
            .and_then(|index| update.args.get(index + 1))
    {
        query
            .args
            .extend(["--global".into(), "--prefix".into(), prefix.clone()]);
    }
    Some(context.installation(
        source,
        Some(update),
        LatestQuery::Package {
            command: query,
            yarn: source == InstallSource::Yarn,
        },
        vec![
            package.name.clone(),
            package.root.to_string_lossy().into_owned(),
        ],
    ))
}

async fn javascript_windows<P: Probe>(context: &Context<'_, P>) -> Option<Installation> {
    if !context
        .canonical
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("cmd"))
    {
        return None;
    }
    let wrapper = context.probe.read(&context.canonical)?;
    let candidates: &[&str] = match context.provider {
        ProviderKind::ClaudeCode => &["@anthropic-ai/claude-code"],
        ProviderKind::Codex => &["@openai/codex"],
        ProviderKind::Pi => &[
            "@earendil-works/pi-coding-agent",
            "@mariozechner/pi-coding-agent",
        ],
        ProviderKind::OpenCode => &["opencode-ai", "@opencode/cli"],
        ProviderKind::Grok => &["@xai-official/grok"],
        ProviderKind::Cursor | ProviderKind::Acp => return None,
    };
    for (manager, source, args, bin_args) in [
        (
            "pnpm",
            InstallSource::Pnpm,
            vec!["root", "--global"],
            vec!["bin", "--global"],
        ),
        (
            "yarn",
            InstallSource::Yarn,
            vec!["global", "dir"],
            vec!["global", "bin"],
        ),
        (
            "npm",
            InstallSource::Npm,
            vec!["root", "--global"],
            vec!["prefix", "--global"],
        ),
    ] {
        let Some(query) = context.command(manager, &args) else {
            continue;
        };
        let Some(root) = context.probe.output(&query).await else {
            continue;
        };
        let mut root = PathBuf::from(root.trim());
        if source == InstallSource::Yarn {
            root.push("node_modules");
        }
        let bin_query = context.at(query.program.clone(), &bin_args);
        let Some(bin) = context.probe.output(&bin_query).await else {
            continue;
        };
        let bin = PathBuf::from(bin.trim());
        if !context.same(context.canonical.parent()?, &bin) {
            continue;
        }
        for name in candidates {
            let package_root = root.join(name);
            let Some(json) = context.probe.read(&package_root.join("package.json")) else {
                continue;
            };
            let Ok(manifest) = serde_json::from_str::<Value>(&json) else {
                continue;
            };
            if manifest.get("name").and_then(Value::as_str) != Some(name) {
                continue;
            }
            let Some(declared) = manifest.get("bin").and_then(|bin| {
                bin.as_str().or_else(|| {
                    bin.get(binary_name(context.provider))
                        .and_then(Value::as_str)
                })
            }) else {
                continue;
            };
            let target = package_root.join(declared);
            if context.probe.canonical(&target).is_none() {
                continue;
            }
            let Some(relative) = windows_relative(&bin, &target) else {
                continue;
            };
            if !known_cmd_shim(&wrapper, &relative) {
                continue;
            }
            let package = Package {
                name: (*name).to_string(),
                root: package_root,
            };
            if let Some(installation) = javascript_plan(
                context,
                source,
                query.program.clone(),
                &root,
                &package,
                true,
            )
            .await
            {
                return Some(installation);
            }
        }
    }
    None
}

fn windows_relative(from: &Path, to: &Path) -> Option<String> {
    let from: Vec<_> = from.components().collect();
    let to: Vec<_> = to.components().collect();
    let shared = from
        .iter()
        .zip(&to)
        .take_while(|(from, to)| from == to)
        .count();
    if shared == 0 {
        return None;
    }
    let relative = std::iter::repeat_n("..".to_string(), from.len() - shared)
        .chain(
            to[shared..]
                .iter()
                .map(|part| part.as_os_str().to_string_lossy().into_owned()),
        )
        .collect::<Vec<_>>()
        .join("\\");
    (!relative.contains(['%', '"', '\r', '\n'])).then_some(relative)
}

/// Recognize npm/cmd-shim and pnpm/@zkochan/cmd-shim launchers. Checking a
/// substring would authorize arbitrary wrappers that happen to mention a bin.
fn known_cmd_shim(wrapper: &str, relative: &str) -> bool {
    let lines: Vec<_> = wrapper
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let npm_header = [
        "@ECHO off",
        "GOTO start",
        ":find_dp0",
        "SET dp0=%~dp0",
        "EXIT /b",
        ":start",
        "SETLOCAL",
        "CALL :find_dp0",
    ];
    let npm_target = format!("\"%dp0%\\{relative}\"");
    if let Some(body) = lines.strip_prefix(&npm_header) {
        if body.len() == 1 && invocation(body[0], &npm_target) {
            return true;
        }
        let setup = [
            "IF EXIST \"%dp0%\\node.exe\" (",
            "SET \"_prog=%dp0%\\node.exe\"",
            ") ELSE (",
            "SET \"_prog=node\"",
        ];
        if let Some(body) = body.strip_prefix(&setup) {
            let body = body
                .strip_prefix(&["SET PATHEXT=%PATHEXT:;.JS;=;%"])
                .unwrap_or(body);
            if let [")", launch] = body {
                for prefix in [
                    "endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"",
                    "endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & set PATHEXT=%PATHEXT:;.JS;=;% & \"%_prog%\"",
                ] {
                    if let Some(launch) = launch.strip_prefix(prefix) {
                        return invocation(launch.trim(), &npm_target);
                    }
                }
            }
        }
        return false;
    }
    // Yarn Classic's older cmd-shim puts SETLOCAL inside the fallback branch.
    let legacy = if let Some(paths) = lines
        .first()
        .and_then(|line| line.strip_prefix("@SET NODE_PATH="))
    {
        if paths
            .replace("%~dp0", "")
            .contains(['%', '"', '&', '|', '<', '>', '^'])
        {
            return false;
        }
        &lines[1..]
    } else {
        &lines
    };
    let target = format!("\"%~dp0\\{relative}\"");
    if let [launch] = legacy {
        return invocation(launch.trim_start_matches('@'), &target);
    }
    if legacy.len() == 7
        && legacy[0] == "@IF EXIST \"%~dp0\\node.exe\" ("
        && legacy[2] == ") ELSE ("
        && legacy[3] == "@SETLOCAL"
        && legacy[4] == "@SET PATHEXT=%PATHEXT:;.JS;=;%"
        && legacy[6] == ")"
    {
        return legacy[1]
            .strip_prefix("\"%~dp0\\node.exe\"")
            .is_some_and(|line| invocation(line.trim(), &target))
            && legacy[5]
                .strip_prefix("node ")
                .is_some_and(|line| invocation(line.trim(), &target));
    }
    let Some(body) = lines.strip_prefix(&["@SETLOCAL"]) else {
        return false;
    };
    let body = if body.first() == Some(&"@IF NOT DEFINED NODE_PATH (") {
        if body.len() < 5 {
            return false;
        }
        let Some(paths) = body[1]
            .strip_prefix("@SET \"NODE_PATH=")
            .and_then(|line| line.strip_suffix('"'))
        else {
            return false;
        };
        if paths
            .replace("%~dp0", "")
            .contains(['%', '"', '&', '|', '<', '>', '^'])
            || body[2] != ") ELSE ("
            || body[3] != format!("@SET \"NODE_PATH={paths};%NODE_PATH%\"")
            || body[4] != ")"
        {
            return false;
        }
        &body[5..]
    } else {
        body
    };
    if let [launch] = body {
        return invocation(launch.trim_start_matches('@'), &target);
    }
    if body.len() != 6
        || body[0] != "@IF EXIST \"%~dp0\\node.exe\" ("
        || body[2] != ") ELSE ("
        || body[3] != "@SET PATHEXT=%PATHEXT:;.JS;=;%"
        || body[5] != ")"
    {
        return false;
    }
    body[1]
        .strip_prefix("\"%~dp0\\node.exe\"")
        .is_some_and(|line| invocation(line.trim(), &target))
        && body[4]
            .strip_prefix("node ")
            .is_some_and(|line| invocation(line.trim(), &target))
}

fn invocation(line: &str, target: &str) -> bool {
    line.strip_prefix(target)
        .is_some_and(|rest| rest.trim() == "%*")
}

fn native<P: Probe>(context: &Context<'_, P>) -> Option<Installation> {
    let home = context.home()?;
    match context.provider {
        ProviderKind::ClaudeCode
            if within(
                context,
                &context.canonical,
                &home.join(".local/share/claude/versions"),
            ) =>
        {
            if env_value(context.env, "DISABLE_UPDATES")
                .is_some_and(|value| value != "0" && !value.is_empty())
            {
                return Some(context.managed(InstallSource::Native, None));
            }
            let (latest, identity) =
                claude_channel(context, &home).unwrap_or((LatestQuery::None, Vec::new()));
            Some(context.installation(
                InstallSource::Native,
                Some(context.at(context.binary.clone(), &["update"])),
                latest,
                identity,
            ))
        }
        ProviderKind::OpenCode if context.canonical == home.join(".opencode/bin/opencode") => {
            Some(context.installation(
                InstallSource::Native,
                Some(context.at(context.binary.clone(), &["upgrade", "--method", "curl"])),
                LatestQuery::Registry("opencode-ai".into()),
                Vec::new(),
            ))
        }
        _ => None,
    }
}

fn claude_channel<P: Probe>(
    context: &Context<'_, P>,
    home: &Path,
) -> Option<(LatestQuery, Vec<String>)> {
    let config = env_value(context.env, "CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"));
    let managed = if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.json")
    } else if cfg!(windows) {
        PathBuf::from(env_value(context.env, "ProgramFiles")?)
            .join("ClaudeCode/managed-settings.json")
    } else {
        PathBuf::from("/etc/claude-code/managed-settings.json")
    };
    let mut channel = "latest".to_string();
    let mut maximum = None;
    let mut identity = Vec::new();
    for path in [
        config.join("settings.json"),
        context.cwd.join(".claude/settings.json"),
        context.cwd.join(".claude/settings.local.json"),
        managed,
    ] {
        let Some(raw) = context.probe.read(&path) else {
            continue;
        };
        let json: Value = serde_json::from_str(&raw).ok()?;
        if let Some(value) = json.get("autoUpdatesChannel") {
            channel = value.as_str()?.to_string();
            if !matches!(channel.as_str(), "stable" | "latest") {
                return None;
            }
        }
        if let Some(value) = json.get("requiredMaximumVersion") {
            maximum = Some(super::parse_version(value.as_str()?)?);
        }
        identity.push(path.to_string_lossy().into_owned());
        identity.push(format!("{channel}:{maximum:?}"));
    }
    Some((LatestQuery::Claude { channel, maximum }, identity))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Fake {
        paths: HashMap<PathBuf, PathBuf>,
        files: HashMap<PathBuf, String>,
        programs: HashMap<String, PathBuf>,
        outputs: HashMap<Vec<String>, String>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl Fake {
        fn path(&mut self, path: &str) {
            for path in Path::new(path).ancestors() {
                self.paths.insert(path.into(), path.into());
            }
        }

        fn link(&mut self, from: &str, to: &str) {
            self.path(from);
            self.path(to);
            self.paths.insert(from.into(), to.into());
        }

        fn file(&mut self, path: &str, contents: &str) {
            self.path(path);
            self.files.insert(path.into(), contents.into());
        }

        fn answer(&mut self, program: &str, args: &[&str], output: &str) {
            let path = PathBuf::from(format!("/tools/{program}"));
            self.programs.insert(program.into(), path.clone());
            let mut key = vec![path.to_string_lossy().into_owned()];
            key.extend(args.iter().map(|arg| (*arg).to_string()));
            self.outputs.insert(key, output.into());
        }

        fn package(&mut self, root: &str, name: &str, bin: &str) -> String {
            self.file(
                &format!("{root}/package.json"),
                &serde_json::json!({"name":name,"bin":{"codex":bin}}).to_string(),
            );
            let binary = format!("{root}/{bin}");
            self.path(&binary);
            binary
        }
    }

    impl Probe for Fake {
        async fn output(&self, command: &UpdateCommand) -> Option<String> {
            let mut key = vec![command.program.to_string_lossy().into_owned()];
            key.extend(command.args.iter().cloned());
            self.calls.lock().unwrap().push(key.clone());
            self.outputs.get(&key).cloned()
        }

        fn canonical(&self, path: &Path) -> Option<PathBuf> {
            self.paths.get(path).cloned()
        }
        fn read(&self, path: &Path) -> Option<String> {
            self.files.get(path).cloned()
        }
        fn which(&self, name: &str, _: &[(String, String)]) -> Option<PathBuf> {
            self.programs.get(name).cloned()
        }
    }

    fn resolve_fake(
        fake: &Fake,
        provider: ProviderKind,
        binary: &str,
        extra: &[(&str, &str)],
    ) -> Installation {
        let mut env = vec![
            ("HOME".into(), "/home/test".into()),
            ("USERPROFILE".into(), "/home/test".into()),
            (
                "MISE_DATA_DIR".into(),
                "/home/test/.local/share/mise".into(),
            ),
            ("VOLTA_HOME".into(), "/home/test/.volta".into()),
            ("ASDF_DATA_DIR".into(), "/home/test/.asdf".into()),
        ];
        env.extend(
            extra
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string())),
        );
        smol::block_on(resolve(
            fake,
            provider,
            Path::new(binary),
            &env,
            "/work".into(),
        ))
    }

    #[test]
    fn mise_owns_nested_npm_packages_and_obeys_its_configured_range() {
        let mut fake = Fake::default();
        let root = "/custom/mise/installs/npm-openai-codex/0.154.0";
        let binary = fake.package(
            &format!("{root}/node_modules/@openai/codex"),
            "@openai/codex",
            "bin/codex.js",
        );
        fake.answer(
            "mise",
            &["ls", "--json"],
            &serde_json::json!({
                "npm:@openai/codex":[{"version":"0.154.0","requested_version":"0.154.0",
                    "install_path":root,"installed":true,"active":true,
                    "source":{"path":"/home/test/.config/mise/config.toml"}}]
            })
            .to_string(),
        );
        fake.answer("mise", &["which", "codex"], &binary);
        fake.answer(
            "mise",
            &["tool", "npm:@openai/codex", "--json"],
            r#"{"backend":"npm:@openai/codex"}"#,
        );
        fake.answer(
            "mise",
            &["outdated", "--json", "--", "npm:@openai/codex"],
            "{}",
        );
        let installation = resolve_fake(
            &fake,
            ProviderKind::Codex,
            &binary,
            &[("MISE_DATA_DIR", "/custom/mise")],
        );
        assert_eq!(installation.source, InstallSource::Mise);
        assert_eq!(
            installation.update.as_ref().unwrap().args,
            ["upgrade", "--yes", "--", "npm:@openai/codex"]
        );
        assert_eq!(
            smol::block_on(latest(&fake, &installation.latest)).as_deref(),
            Some("0.154.0")
        );
        assert!(
            !fake
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|call| call[0].ends_with("/npm"))
        );
        fake.answer(
            "mise",
            &["outdated", "--json", "--", "npm:@openai/codex"],
            r#"{"npm:@openai/codex":{"current":"0.154.0","requested":"0.154","latest":"0.154.2"}}"#,
        );
        assert_eq!(
            smol::block_on(latest(&fake, &installation.latest)).as_deref(),
            Some("0.154.2")
        );
    }

    #[test]
    fn mise_shims_are_resolved_before_canonicalization_and_fail_closed() {
        let mut fake = Fake::default();
        let binary = "/custom/mise/shims/claude";
        let actual = "/custom/mise/installs/claude/2.1.274/claude";
        fake.link(binary, "/tools/mise");
        fake.path(actual);
        fake.answer(
            "mise",
            &["ls", "--json"],
            r#"{"claude":[{
            "version":"2.1.274","requested_version":"latest","installed":true,"active":true,
            "install_path":"/custom/mise/installs/claude/2.1.274",
            "source":{"path":"/home/test/.config/mise/config.toml"}}]}"#,
        );
        fake.answer("mise", &["which", "claude"], actual);
        fake.answer(
            "mise",
            &["tool", "claude", "--json"],
            r#"{"backend":"aqua:anthropics/claude-code"}"#,
        );
        let installation = resolve_fake(
            &fake,
            ProviderKind::ClaudeCode,
            binary,
            &[("MISE_DATA_DIR", "/custom/mise")],
        );
        assert_eq!(installation.source, InstallSource::Mise);
        assert_eq!(installation.update.unwrap().args.last().unwrap(), "claude");
        fake.answer("mise", &["which", "claude"], "/elsewhere/claude");
        let failed = resolve_fake(
            &fake,
            ProviderKind::ClaudeCode,
            binary,
            &[("MISE_DATA_DIR", "/custom/mise")],
        );
        assert_eq!(failed.source, InstallSource::Mise);
        assert!(failed.update.is_none());
    }

    #[test]
    fn mise_runtime_alias_does_not_own_its_global_npm_packages() {
        let mut fake = Fake::default();
        let root = "/custom/mise/installs/my-node/22";
        let package_root = if cfg!(windows) {
            format!("{root}/node_modules")
        } else {
            format!("{root}/lib/node_modules")
        };
        let binary = fake.package(
            &format!("{package_root}/@openai/codex"),
            "@openai/codex",
            "bin/codex.js",
        );
        fake.answer("mise", &["ls", "--json"], &serde_json::json!({"my-node":[{
            "install_path":root,"active":true,"installed":true,"version":"22","requested_version":"22",
            "source":{"path":"/home/test/.config/mise/config.toml"}}]}).to_string());
        fake.answer("mise", &["which", "codex"], &binary);
        fake.answer(
            "mise",
            &["tool", "my-node", "--json"],
            r#"{"backend":"core:node"}"#,
        );
        fake.answer("npm", &["root", "--global"], &package_root);
        fake.answer("npm", &["prefix", "--global"], root);
        let installation = resolve_fake(
            &fake,
            ProviderKind::Codex,
            &binary,
            &[("MISE_DATA_DIR", "/custom/mise")],
        );
        assert_eq!(installation.source, InstallSource::Npm);
        assert_eq!(
            installation.update.unwrap().args,
            [
                "install",
                "--global",
                "--prefix",
                root,
                "--",
                "@openai/codex@latest"
            ]
        );
    }

    #[test]
    fn unknown_wrappers_and_runtime_directory_names_never_authorize_updates() {
        let mut fake = Fake::default();
        for path in [
            "/home/test/.local/bin/claude",
            "/home/test/.nvm/versions/node/22/bin/codex",
            "/home/test/.bun/bin/codex",
            "/random/node_modules/codex",
            "/opt/homebrew/bin/codex",
        ] {
            fake.path(path);
            let installation = resolve_fake(&fake, ProviderKind::Codex, path, &[]);
            assert_eq!(installation.source, InstallSource::Unknown, "{path}");
            assert!(installation.update.is_none(), "{path}");
        }
    }

    #[test]
    fn npm_update_is_bound_to_the_verified_global_prefix() {
        let mut fake = Fake::default();
        let root = if cfg!(windows) {
            "/node-v22/node_modules"
        } else {
            "/node-v22/lib/node_modules"
        };
        let binary = fake.package(
            &format!("{root}/@openai/codex"),
            "@openai/codex",
            "bin/codex.js",
        );
        fake.answer("npm", &["root", "--global"], root);
        fake.answer("npm", &["prefix", "--global"], "/node-v22\n");
        let installation = resolve_fake(&fake, ProviderKind::Codex, &binary, &[]);
        assert_eq!(installation.source, InstallSource::Npm);
        fake.answer(
            "npm",
            &[
                "view",
                "@openai/codex@latest",
                "version",
                "--json",
                "--global",
                "--prefix",
                "/node-v22",
            ],
            "\"0.155.0\"",
        );
        assert_eq!(
            smol::block_on(latest(&fake, &installation.latest)).as_deref(),
            Some("0.155.0")
        );
        assert_eq!(
            installation.update.unwrap().args,
            [
                "install",
                "--global",
                "--prefix",
                "/node-v22",
                "--",
                "@openai/codex@latest"
            ]
        );
        fake.answer(
            "npm",
            &["root", "--global"],
            "/different-node/lib/node_modules",
        );
        assert!(
            resolve_fake(&fake, ProviderKind::Codex, &binary, &[])
                .update
                .is_none()
        );
    }

    #[test]
    fn bun_binds_independently_configured_package_and_binary_directories() {
        for package_root in [
            "/custom/global/node_modules/@openai/codex",
            "/custom/global/node_modules/.bun/@openai+codex@0.154.0/node_modules/@openai/codex",
        ] {
            let mut fake = Fake::default();
            fake.file(
                "/custom/global/package.json",
                r#"{"dependencies":{"@openai/codex":"^0.154.0"}}"#,
            );
            let binary = fake.package(package_root, "@openai/codex", "bin/codex.js");
            fake.link("/custom/global/node_modules/@openai/codex", package_root);
            fake.link("/custom/launchers/codex", &binary);
            fake.answer("bun", &["pm", "bin", "--global"], "/custom/launchers");
            let installation = resolve_fake(&fake, ProviderKind::Codex, &binary, &[]);
            assert_eq!(installation.source, InstallSource::Bun);
            let update = installation.update.unwrap();
            assert_eq!(
                update.args,
                ["add", "--global", "--", "@openai/codex@latest"]
            );
            assert!(
                update
                    .env
                    .contains(&("BUN_INSTALL_GLOBAL_DIR".into(), "/custom/global".into()))
            );
            assert!(
                update
                    .env
                    .contains(&("BUN_INSTALL_BIN".into(), "/custom/launchers".into()))
            );
        }
    }

    #[test]
    fn windows_global_shims_require_both_manager_metadata_and_the_declared_bin() {
        // Recorded templates from npm/cmd-shim, pnpm/cmd-shim and Yarn Classic's
        // @zkochan/cmd-shim. The final target is package.json's declared bin.
        let cases = [
            (
                "npm",
                InstallSource::Npm,
                "/windows/npm/node_modules",
                "/windows/npm",
                "@ECHO off\r\nGOTO start\r\n:find_dp0\r\nSET dp0=%~dp0\r\nEXIT /b\r\n:start\r\nSETLOCAL\r\nCALL :find_dp0\r\n\r\nIF EXIST \"%dp0%\\node.exe\" (\r\n  SET \"_prog=%dp0%\\node.exe\"\r\n) ELSE (\r\n  SET \"_prog=node\"\r\n  SET PATHEXT=%PATHEXT:;.JS;=;%\r\n)\r\n\r\nendLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & \"%_prog%\"  \"%dp0%\\node_modules\\@openai\\codex\\bin\\codex.js\" %*\r\n",
            ),
            (
                "pnpm",
                InstallSource::Pnpm,
                "/windows/pnpm/global/5/node_modules",
                "/windows/pnpm",
                "@SETLOCAL\r\n@IF NOT DEFINED NODE_PATH (\r\n  @SET \"NODE_PATH=%~dp0\\global\\5\\node_modules\"\r\n) ELSE (\r\n  @SET \"NODE_PATH=%~dp0\\global\\5\\node_modules;%NODE_PATH%\"\r\n)\r\n@IF EXIST \"%~dp0\\node.exe\" (\r\n  \"%~dp0\\node.exe\"  \"%~dp0\\global\\5\\node_modules\\@openai\\codex\\bin\\codex.js\" %*\r\n) ELSE (\r\n  @SET PATHEXT=%PATHEXT:;.JS;=;%\r\n  node  \"%~dp0\\global\\5\\node_modules\\@openai\\codex\\bin\\codex.js\" %*\r\n)\r\n",
            ),
            (
                "yarn",
                InstallSource::Yarn,
                "/windows/yarn/global/node_modules",
                "/windows/yarn/bin",
                "@IF EXIST \"%~dp0\\node.exe\" (\r\n  \"%~dp0\\node.exe\"  \"%~dp0\\..\\global\\node_modules\\@openai\\codex\\bin\\codex.js\" %*\r\n) ELSE (\r\n  @SETLOCAL\r\n  @SET PATHEXT=%PATHEXT:;.JS;=;%\r\n  node  \"%~dp0\\..\\global\\node_modules\\@openai\\codex\\bin\\codex.js\" %*\r\n)",
            ),
        ];
        for (manager, source, root, bin, wrapper) in cases {
            let mut fake = Fake::default();
            let launcher = format!("{bin}/codex.cmd");
            fake.file(&launcher, wrapper);
            fake.package(
                &format!("{root}/@openai/codex"),
                "@openai/codex",
                "bin/codex.js",
            );
            let root_args = if manager == "yarn" {
                ["global", "dir"]
            } else {
                ["root", "--global"]
            };
            let root_output = if manager == "yarn" {
                "/windows/yarn/global"
            } else {
                root
            };
            let bin_args = match manager {
                "npm" => ["prefix", "--global"],
                "yarn" => ["global", "bin"],
                _ => ["bin", "--global"],
            };
            fake.answer(manager, &root_args, root_output);
            fake.answer(manager, &bin_args, bin);
            let installation = resolve_fake(&fake, ProviderKind::Codex, &launcher, &[]);
            assert_eq!(installation.source, source, "{manager}");
            assert!(
                installation
                    .update
                    .unwrap()
                    .args
                    .contains(&"@openai/codex@latest".to_string())
            );
            fake.file(&launcher, &format!("{wrapper}\r\necho custom-command\r\n"));
            assert!(
                resolve_fake(&fake, ProviderKind::Codex, &launcher, &[])
                    .update
                    .is_none(),
                "{manager} custom wrapper"
            );
            fake.file(&launcher, wrapper);
            fake.file(
                &format!("{root}/@openai/codex/package.json"),
                r#"{"name":"@openai/codex","bin":{"codex":"other.js"}}"#,
            );
            assert!(
                resolve_fake(&fake, ProviderKind::Codex, &launcher, &[])
                    .update
                    .is_none(),
                "{manager} mismatched manifest"
            );
        }
    }

    #[test]
    fn claude_native_updates_use_the_configured_channel_and_exact_executable() {
        let mut fake = Fake::default();
        let binary = "/home/test/.local/share/claude/versions/2.1.274";
        fake.path(binary);
        fake.file(
            "/custom/claude/settings.json",
            r#"{"autoUpdatesChannel":"stable"}"#,
        );
        let installation = resolve_fake(
            &fake,
            ProviderKind::ClaudeCode,
            binary,
            &[
                ("CLAUDE_CONFIG_DIR", "/custom/claude"),
                ("ProgramFiles", "/program-files"),
            ],
        );
        assert_eq!(installation.source, InstallSource::Native);
        assert_eq!(installation.update.unwrap().program, PathBuf::from(binary));
        assert_eq!(
            installation.latest,
            LatestQuery::Claude {
                channel: "stable".into(),
                maximum: None
            }
        );
        let disabled = resolve_fake(
            &fake,
            ProviderKind::ClaudeCode,
            binary,
            &[("DISABLE_UPDATES", "1")],
        );
        assert!(disabled.update.is_none());
    }

    #[test]
    fn manifest_name_without_matching_bin_is_not_package_ownership() {
        let mut fake = Fake::default();
        fake.package(
            "/node/lib/node_modules/@openai/codex",
            "@openai/codex",
            "bin/codex.js",
        );
        let impostor = "/node/lib/node_modules/@openai/codex/custom-wrapper";
        fake.path(impostor);
        fake.answer("npm", &["root", "--global"], "/node/lib/node_modules");
        fake.answer("npm", &["prefix", "--global"], "/node");
        assert!(
            resolve_fake(&fake, ProviderKind::Codex, impostor, &[])
                .update
                .is_none()
        );
    }

    #[test]
    fn volta_project_selection_does_not_authorize_updating_the_global_default() {
        let mut fake = Fake::default();
        let actual = fake.package(
            "/work/node_modules/@openai/codex",
            "@openai/codex",
            "bin/codex.js",
        );
        let shim = "/home/test/.volta/bin/codex";
        fake.link(shim, "/tools/volta-shim");
        fake.answer("volta", &["which", "codex"], &actual);
        let installation = resolve_fake(&fake, ProviderKind::Codex, shim, &[]);
        assert_eq!(installation.source, InstallSource::Volta);
        assert!(installation.update.is_none());
    }

    #[test]
    fn brew_preserves_cask_channel_and_formula_tap_and_pins() {
        let mut fake = Fake::default();
        let binary = "/custom/brew/Caskroom/claude-code@latest/2.1.274/claude";
        fake.path(binary);
        fake.answer("brew", &["--prefix"], "/custom/brew");
        fake.answer(
            "brew",
            &["info", "--json=v2", "--installed"],
            r#"{
            "formulae":[],"casks":[{"token":"claude-code@latest","full_token":"claude-code@latest",
            "installed":"2.1.274","version":"2.1.276","pinned":false}]}"#,
        );
        let installation = resolve_fake(&fake, ProviderKind::ClaudeCode, binary, &[]);
        assert_eq!(installation.source, InstallSource::Brew);
        assert_eq!(
            installation.update.unwrap().args,
            ["upgrade", "--cask", "--", "claude-code@latest"]
        );
        let binary = "/custom/brew/Cellar/opencode/1.0.0/bin/opencode";
        fake.path(binary);
        fake.answer("brew", &["info", "--json=v2", "--installed"], r#"{
            "formulae":[{"name":"opencode","full_name":"anomalyco/tap/opencode",
            "installed":[{"version":"1.0.0"}],"versions":{"stable":"1.1.0"},"pinned":false}],"casks":[]}"#);
        let installation = resolve_fake(&fake, ProviderKind::OpenCode, binary, &[]);
        assert_eq!(
            installation.update.unwrap().args,
            ["upgrade", "--formula", "--", "anomalyco/tap/opencode"]
        );
        fake.answer(
            "brew",
            &["info", "--json=v2", "--installed"],
            r#"{
            "formulae":[{"name":"opencode","full_name":"anomalyco/tap/opencode",
            "installed":[{"version":"1.0.0"}],"pinned":true}],"casks":[]}"#,
        );
        assert!(
            resolve_fake(&fake, ProviderKind::OpenCode, binary, &[])
                .update
                .is_none()
        );
    }

    #[test]
    fn display_quotes_paths_and_preserves_ownership_context_without_secrets() {
        let command = UpdateCommand {
            program: "/tools/owner's mise".into(),
            args: vec!["upgrade".into(), "npm:@openai/codex".into()],
            cwd: "/work/a b".into(),
            env: vec![
                ("MISE_DATA_DIR".into(), "/data/a b".into()),
                ("ANTHROPIC_API_KEY".into(), "private-secret".into()),
            ],
            requires_terminal: false,
        };
        assert_eq!(
            command.display_for_shell(false),
            "cd '/work/a b' && MISE_DATA_DIR='/data/a b' '/tools/owner'\\''s mise' upgrade npm:@openai/codex"
        );
        assert_eq!(
            command.display_for_shell(true),
            "Set-Location -LiteralPath '/work/a b'; $env:MISE_DATA_DIR = '/data/a b'; & '/tools/owner''s mise' 'upgrade' 'npm:@openai/codex'"
        );
    }
}
