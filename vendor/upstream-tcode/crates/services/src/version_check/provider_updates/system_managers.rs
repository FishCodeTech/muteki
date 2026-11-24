use std::path::{Path, PathBuf};

use serde_json::Value;

use super::InstallSource;
use super::installation::{Context, Installation, LatestQuery, Probe, UpdateCommand, env_value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Query {
    Apt(UpdateCommand),
    Apk {
        command: UpdateCommand,
        package: String,
    },
    ScoopManifest(PathBuf),
}

pub(super) async fn latest<P: Probe>(probe: &P, query: &Query) -> Option<String> {
    match query {
        Query::Apt(command) => {
            let output = probe.output(command).await?;
            let candidate = output
                .lines()
                .find_map(|line| line.trim().strip_prefix("Candidate:"))?
                .trim();
            if candidate == "(none)" || candidate.split_whitespace().count() != 1 {
                return None;
            }
            // The provider reports its upstream version, without Debian's epoch.
            Some(
                candidate
                    .split_once(':')
                    .map_or(candidate, |(_, version)| version)
                    .to_string(),
            )
        }
        Query::Apk { command, package } => {
            let world = probe.read(Path::new("/etc/apk/world"))?;
            if world.lines().any(|line| {
                let line = line.split('#').next().unwrap_or_default().trim();
                line.split(['@', '=', '<', '>', '~']).next() == Some(package.as_str())
                    && line.contains(['=', '<', '>', '~'])
            }) {
                return None;
            }
            let output = probe.output(command).await?;
            output.lines().find_map(|line| {
                let mut fields = line.split_whitespace();
                let installed = fields
                    .next()?
                    .strip_prefix(package.as_str())?
                    .strip_prefix('-')?;
                if !installed.as_bytes().first().is_some_and(u8::is_ascii_digit) {
                    return None;
                }
                if !matches!(fields.next()?, "<" | "=" | ">") {
                    return None;
                }
                let available = fields.next()?;
                available
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_digit)
                    .then(|| available.to_string())
            })
        }
        Query::ScoopManifest(path) => {
            let manifest: Value = serde_json::from_str(&probe.read(path)?).ok()?;
            manifest.get("version")?.as_str().map(str::to_string)
        }
    }
}

pub(super) async fn resolve<P: Probe>(ctx: &Context<'_, P>) -> Option<Installation> {
    if ctx.canonical.starts_with("/nix/store") {
        // A store path does not identify the profile, flake, or system configuration
        // that must be changed. Self-updating its contents would bypass all of them.
        return Some(ctx.managed(InstallSource::Nix, None));
    }
    if cfg!(target_os = "linux") {
        return linux(ctx).await;
    }
    if cfg!(windows) {
        return windows(ctx).await;
    }
    None
}

async fn linux<P: Probe>(ctx: &Context<'_, P>) -> Option<Installation> {
    for path in [&ctx.canonical, &ctx.binary] {
        let path = path.to_str()?;
        if let Some(query) = ctx.command("dpkg-query", &["--search", "--", path])
            && let Some(output) = ctx.probe.output(&query).await
            && let Some(package) = debian_owner(&output, path)
        {
            // APT's policy candidate does not account for dpkg selections. An
            // explicit install can override a hold, so require an unheld install.
            let selection = ctx.command(
                "dpkg-query",
                &[
                    "--show",
                    "--showformat",
                    "${db:Status-Want} ${db:Status-Status}\n",
                    "--",
                    package,
                ],
            );
            let update_allowed = if let Some(selection) = selection {
                ctx.probe
                    .output(&selection)
                    .await
                    .is_some_and(|output| output.trim() == "install installed")
            } else {
                false
            };
            if !update_allowed {
                return Some(owned(ctx, InstallSource::System, package, None));
            }
            let update = terminal(
                ctx,
                "apt-get",
                &["install", "--only-upgrade", "--", package],
            );
            let latest = ctx
                .command("apt-cache", &["policy", "--", package])
                .map(|mut query| {
                    query.env.push(("LC_ALL".into(), "C".into()));
                    LatestQuery::System(Query::Apt(query))
                })
                .unwrap_or(LatestQuery::None);
            return Some(ctx.installation(
                InstallSource::System,
                update,
                latest,
                vec![package.into()],
            ));
        }
        if let Some(query) = ctx.command("rpm", &["-qf", "--queryformat", "%{NAME}\n", "--", path])
            && let Some(output) = ctx.probe.output(&query).await
            && let Some(package) = single_package(&output)
        {
            let update = terminal(ctx, "dnf", &["upgrade", "--", package])
                .or_else(|| terminal(ctx, "yum", &["upgrade", "--", package]))
                .or_else(|| terminal(ctx, "zypper", &["update", "--", package]));
            return Some(owned(ctx, InstallSource::System, package, update));
        }
        if let Some(query) = ctx.command("pacman", &["-Qoq", "--", path])
            && let Some(output) = ctx.probe.output(&query).await
            && let Some(package) = single_package(&output)
        {
            // Pacman upgrades may require a full system upgrade or an AUR helper.
            return Some(owned(ctx, InstallSource::System, package, None));
        }
        if let Some(query) = ctx.command("apk", &["info", "--who-owns", "--quiet", "--", path])
            && let Some(output) = ctx.probe.output(&query).await
            && let Some(package) = single_package(&output)
            && let Some(verify) = ctx.command("apk", &["info", "--installed", "--", package])
            && let Some(installed) = ctx.probe.output(&verify).await
            && installed.trim() == package
        {
            let update = terminal(ctx, "apk", &["upgrade", "--", package]);
            let latest = ctx
                .command("apk", &["version", "--", package])
                .map(|command| {
                    LatestQuery::System(Query::Apk {
                        command,
                        package: package.to_string(),
                    })
                })
                .unwrap_or(LatestQuery::None);
            return Some(ctx.installation(
                InstallSource::System,
                update,
                latest,
                vec![package.into()],
            ));
        }
    }
    None
}

fn debian_owner<'a>(output: &'a str, path: &str) -> Option<&'a str> {
    let mut owner = None;
    for line in output.lines() {
        let Some((package, owned_path)) = line.rsplit_once(": ") else {
            continue;
        };
        if owned_path != path {
            continue;
        }
        if !valid_package(package) || owner.is_some_and(|owner| owner != package) {
            return None;
        }
        owner = Some(package);
    }
    owner
}

fn single_package(output: &str) -> Option<&str> {
    let package = output.trim();
    valid_package(package).then_some(package)
}

fn valid_package(package: &str) -> bool {
    package
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        && package
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"+._:-".contains(&byte))
}

fn terminal<P: Probe>(ctx: &Context<'_, P>, manager: &str, args: &[&str]) -> Option<UpdateCommand> {
    let manager = ctx.probe.which(manager, ctx.env)?;
    let privilege_command = ctx
        .probe
        .which("sudo", ctx.env)
        .or_else(|| ctx.probe.which("doas", ctx.env));
    let mut command = if let Some(privilege_command) = privilege_command {
        let mut command = ctx.at(privilege_command, &[]);
        command.args.push(manager.to_string_lossy().into_owned());
        command
            .args
            .extend(args.iter().map(|arg| (*arg).to_string()));
        command
    } else {
        ctx.at(manager, args)
    };
    command.requires_terminal = true;
    Some(command)
}

fn owned<P: Probe>(
    ctx: &Context<'_, P>,
    source: InstallSource,
    package: &str,
    update: Option<UpdateCommand>,
) -> Installation {
    ctx.installation(source, update, LatestQuery::None, vec![package.to_string()])
}

async fn windows<P: Probe>(ctx: &Context<'_, P>) -> Option<Installation> {
    if let Some(installation) = scoop(ctx).or_else(|| chocolatey(ctx)) {
        return Some(installation);
    }
    winget(ctx).await
}

fn scoop<P: Probe>(ctx: &Context<'_, P>) -> Option<Installation> {
    let user_root = env_value(ctx.env, "SCOOP")
        .map(PathBuf::from)
        .or_else(|| ctx.home().map(|home| home.join("scoop")));
    let global_root = env_value(ctx.env, "SCOOP_GLOBAL")
        .map(PathBuf::from)
        .or_else(|| {
            env_value(ctx.env, "ProgramData").map(|root| PathBuf::from(root).join("scoop"))
        });
    for (root, global) in [(user_root.clone(), false), (global_root, true)] {
        let Some(root) = root else { continue };
        let Some(apps) = ctx.probe.canonical(&root.join("apps")) else {
            continue;
        };
        let target = if ctx.same(ctx.binary.parent()?, &root.join("shims")) {
            let shim = ctx.probe.read(&ctx.binary.with_extension("shim"))?;
            let path = shim.lines().find_map(|line| {
                let (key, value) = line.split_once('=')?;
                (key.trim() == "path").then(|| value.trim().trim_matches('"'))
            })?;
            ctx.probe.canonical(Path::new(path))?
        } else {
            ctx.canonical.clone()
        };
        let Ok(relative) = target.strip_prefix(&apps) else {
            continue;
        };
        let mut components = relative.components();
        let package = components.next()?.as_os_str().to_str()?;
        let version = components.next()?.as_os_str();
        if !valid_package(package) {
            continue;
        }
        let installed = apps.join(package).join(version);
        let Some(manifest) = json(ctx, &installed.join("manifest.json")) else {
            continue;
        };
        let Some(info) = json(ctx, &installed.join("install.json")) else {
            continue;
        };
        if manifest.get("version").and_then(Value::as_str).is_none()
            || info.get("architecture").and_then(Value::as_str).is_none()
        {
            continue;
        }
        let mut command = if global {
            ctx.command("scoop", &["update", "--global", package])
        } else {
            ctx.command("scoop", &["update", package])
        };
        let held = info.get("hold").and_then(Value::as_bool) == Some(true);
        if held {
            command = None;
        }
        if let Some(command) = &mut command {
            command.requires_terminal = true;
        }
        let latest = (!held)
            .then(|| {
                let bucket = info.get("bucket")?.as_str()?;
                if !valid_package(bucket) {
                    return None;
                }
                let bucket = user_root.as_ref()?.join("buckets").join(bucket);
                [
                    bucket.join("bucket").join(format!("{package}.json")),
                    bucket.join(format!("{package}.json")),
                ]
                .into_iter()
                .find(|path| json(ctx, path).is_some())
                .map(|path| LatestQuery::System(Query::ScoopManifest(path)))
            })
            .flatten()
            .unwrap_or(LatestQuery::None);
        return Some(ctx.installation(InstallSource::Scoop, command, latest, vec![package.into()]));
    }
    None
}

fn chocolatey<P: Probe>(ctx: &Context<'_, P>) -> Option<Installation> {
    let root = env_value(ctx.env, "ChocolateyInstall")
        .map(PathBuf::from)
        .or_else(|| {
            env_value(ctx.env, "ProgramData").map(|root| PathBuf::from(root).join("chocolatey"))
        })?;
    let packages = ctx.probe.canonical(&root.join("lib"))?;
    let relative = ctx.canonical.strip_prefix(&packages).ok()?;
    let package = relative.components().next()?.as_os_str().to_str()?;
    if !valid_package(package) {
        return None;
    }
    let manifest = ctx
        .probe
        .read(&packages.join(package).join(format!("{package}.nuspec")))?;
    let id = manifest.split_once("<id>")?.1.split_once("</id>")?.0.trim();
    if !id.eq_ignore_ascii_case(package) {
        return None;
    }
    let mut command = ctx.command("choco", &["upgrade", package]);
    if let Some(command) = &mut command {
        command.requires_terminal = true;
    }
    Some(owned(ctx, InstallSource::Chocolatey, package, command))
}

async fn winget<P: Probe>(ctx: &Context<'_, P>) -> Option<Installation> {
    const SOURCE_SUFFIX: &str = "_Microsoft.Winget.Source_8wekyb3d8bbwe";
    for directory in ctx.canonical.ancestors().skip(1) {
        let Some(name) = directory.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(package) = name.strip_suffix(SOURCE_SUFFIX) else {
            continue;
        };
        if !valid_package(package)
            || ctx
                .probe
                .canonical(&directory.join(format!("{name}.db")))
                .is_none()
        {
            continue;
        }
        // Portable installs keep an installation database beside their files.
        // Also require the exact registered ID; a matching app name is insufficient.
        let query = ctx.command(
            "winget",
            &[
                "list",
                "--id",
                package,
                "--exact",
                "--source",
                "winget",
                "--disable-interactivity",
            ],
        )?;
        let output = ctx.probe.output(&query).await?;
        if !output.lines().any(|line| {
            line.split_whitespace()
                .any(|word| word.eq_ignore_ascii_case(package))
        }) {
            return None;
        }
        let mut command = ctx.command(
            "winget",
            &["upgrade", "--id", package, "--exact", "--source", "winget"],
        );
        if let Some(command) = &mut command {
            command.requires_terminal = true;
        }
        return Some(owned(ctx, InstallSource::Winget, package, command));
    }
    None
}

fn json<P: Probe>(ctx: &Context<'_, P>, path: &Path) -> Option<Value> {
    serde_json::from_str(&ctx.probe.read(path)?).ok()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use agent::ProviderKind;

    use super::*;

    #[derive(Default)]
    struct Fixture {
        files: BTreeMap<PathBuf, String>,
        paths: BTreeMap<PathBuf, PathBuf>,
        commands: BTreeMap<String, PathBuf>,
        outputs: BTreeMap<Vec<String>, String>,
    }

    impl Fixture {
        fn manager(&mut self, name: &str) {
            self.commands
                .insert(name.to_string(), PathBuf::from(format!("/tools/{name}")));
        }

        fn answer(&mut self, name: &str, args: &[&str], output: &str) {
            self.manager(name);
            let key = std::iter::once(format!("/tools/{name}"))
                .chain(args.iter().map(|value| (*value).to_string()))
                .collect();
            self.outputs.insert(key, output.to_string());
        }

        fn path(&mut self, path: &str) {
            self.paths.insert(path.into(), path.into());
        }

        fn context<'a>(&'a self, binary: &str, env: &'a [(String, String)]) -> Context<'a, Self> {
            Context {
                probe: self,
                provider: ProviderKind::ClaudeCode,
                binary: binary.into(),
                canonical: self
                    .canonical(Path::new(binary))
                    .unwrap_or_else(|| binary.into()),
                cwd: "/work".into(),
                env,
            }
        }
    }

    impl Probe for Fixture {
        async fn output(&self, command: &UpdateCommand) -> Option<String> {
            let key = std::iter::once(command.program.to_string_lossy().into_owned())
                .chain(command.args.iter().cloned())
                .collect::<Vec<_>>();
            self.outputs.get(&key).cloned()
        }

        fn canonical(&self, path: &Path) -> Option<PathBuf> {
            self.paths.get(path).cloned()
        }

        fn read(&self, path: &Path) -> Option<String> {
            self.files.get(path).cloned()
        }

        fn which(&self, name: &str, _env: &[(String, String)]) -> Option<PathBuf> {
            self.commands.get(name).cloned()
        }
    }

    #[test]
    fn distro_update_uses_the_exact_file_owner_and_requires_a_terminal() {
        let mut fixture = Fixture::default();
        fixture.manager("apt-get");
        fixture.manager("sudo");
        fixture.answer(
            "dpkg-query",
            &["--search", "--", "/usr/bin/claude"],
            "company-assistant:amd64: /usr/bin/claude\n",
        );
        fixture.answer(
            "dpkg-query",
            &[
                "--show",
                "--showformat",
                "${db:Status-Want} ${db:Status-Status}\n",
                "--",
                "company-assistant:amd64",
            ],
            "install installed\n",
        );
        let installation = smol::block_on(linux(&fixture.context("/usr/bin/claude", &[]))).unwrap();
        let command = installation.update.unwrap();
        assert_eq!(command.program, Path::new("/tools/sudo"));
        assert_eq!(
            command.args,
            [
                "/tools/apt-get",
                "install",
                "--only-upgrade",
                "--",
                "company-assistant:amd64"
            ]
        );
        assert!(command.requires_terminal);

        for output in [
            "company-assistant: /usr/bin/other\n",
            "company-assistant, unrelated: /usr/bin/claude\n",
            "company-assistant: /usr/bin/claude\nunrelated: /usr/bin/claude\n",
        ] {
            fixture.answer("dpkg-query", &["--search", "--", "/usr/bin/claude"], output);
            assert!(smol::block_on(linux(&fixture.context("/usr/bin/claude", &[]))).is_none());
        }
    }

    #[test]
    fn debian_hold_blocks_upgrade_and_latest_even_with_a_policy_candidate() {
        let mut fixture = Fixture::default();
        fixture.manager("apt-get");
        fixture.answer(
            "dpkg-query",
            &["--search", "--", "/usr/bin/claude"],
            "claude-code: /usr/bin/claude\n",
        );
        fixture.answer(
            "apt-cache",
            &["policy", "--", "claude-code"],
            "claude-code:\n  Installed: 2.1.0\n  Candidate: 2.2.0\n",
        );
        let selection = [
            "--show",
            "--showformat",
            "${db:Status-Want} ${db:Status-Status}\n",
            "--",
            "claude-code",
        ];
        for state in [
            None,
            Some("hold installed\n"),
            Some("unknown\n"),
            Some("install installed\nhold installed\n"),
        ] {
            if let Some(state) = state {
                fixture.answer("dpkg-query", &selection, state);
            }
            let ctx = fixture.context("/usr/bin/claude", &[]);
            let installation = smol::block_on(linux(&ctx)).unwrap();
            assert_eq!(
                installation,
                ctx.installation(
                    InstallSource::System,
                    None,
                    LatestQuery::None,
                    vec!["claude-code".into()],
                ),
                "selection: {state:?}"
            );
        }
        fixture.answer("dpkg-query", &selection, "install installed\n");
        let installation = smol::block_on(linux(&fixture.context("/usr/bin/claude", &[]))).unwrap();
        assert!(installation.update.is_some());
    }

    #[test]
    fn scoop_shim_preserves_global_scope_and_holds() {
        let mut fixture = Fixture::default();
        fixture.manager("scoop");
        for path in [
            "/global/apps",
            "/global/shims",
            "/global/apps/claude-code/2.1.0/claude.exe",
        ] {
            fixture.path(path);
        }
        fixture.files.insert(
            "/global/shims/claude.shim".into(),
            "path = \"/global/apps/claude-code/2.1.0/claude.exe\"\n".into(),
        );
        fixture.files.insert(
            "/global/apps/claude-code/2.1.0/manifest.json".into(),
            r#"{"version":"2.1.0","bin":"claude.exe"}"#.into(),
        );
        let info = PathBuf::from("/global/apps/claude-code/2.1.0/install.json");
        fixture.files.insert(
            info.clone(),
            r#"{"architecture":"64bit","bucket":"main"}"#.into(),
        );
        let env = [
            ("SCOOP".into(), "/user".into()),
            ("SCOOP_GLOBAL".into(), "/global".into()),
        ];
        let installation = scoop(&fixture.context("/global/shims/claude.exe", &env)).unwrap();
        assert_eq!(installation.source, InstallSource::Scoop);
        assert_eq!(
            installation.update.unwrap().args,
            ["update", "--global", "claude-code"]
        );
        fixture.files.insert(
            info,
            r#"{"architecture":"64bit","bucket":"main","hold":true}"#.into(),
        );
        assert!(
            scoop(&fixture.context("/global/shims/claude.exe", &env))
                .unwrap()
                .update
                .is_none()
        );
    }

    #[test]
    fn apt_latest_uses_the_policy_candidate_instead_of_the_newest_table_entry() {
        let mut fixture = Fixture::default();
        let args = ["policy", "--", "claude-code"];
        fixture.answer("apt-cache", &args,
            "claude-code:\n  Installed: 2.1.0-1\n  Candidate: 1:2.1.1-1\n  Version table:\n     2.2.0-1 100\n     1:2.1.1-1 500\n");
        let query = Query::Apt(
            fixture
                .context("/usr/bin/claude", &[])
                .command("apt-cache", &args)
                .unwrap(),
        );
        assert_eq!(
            smol::block_on(latest(&fixture, &query)).as_deref(),
            Some("2.1.1-1")
        );
        fixture.answer(
            "apt-cache",
            &args,
            "claude-code:\n  Installed: 2.1.0-1\n  Candidate: (none)\n",
        );
        assert_eq!(smol::block_on(latest(&fixture, &query)), None);
    }

    #[test]
    fn alpine_checks_installed_owner_and_respects_world_pins() {
        let mut fixture = Fixture::default();
        fixture.manager("doas");
        fixture.answer(
            "apk",
            &["info", "--who-owns", "--quiet", "--", "/usr/bin/claude"],
            "claude-code\n",
        );
        fixture.answer(
            "apk",
            &["info", "--installed", "--", "claude-code"],
            "different-package\n",
        );
        assert!(smol::block_on(linux(&fixture.context("/usr/bin/claude", &[]))).is_none());
        fixture.answer(
            "apk",
            &["info", "--installed", "--", "claude-code"],
            "claude-code\n",
        );
        let installation = smol::block_on(linux(&fixture.context("/usr/bin/claude", &[]))).unwrap();
        let command = installation.update.unwrap();
        assert!(command.requires_terminal);
        assert_eq!(command.program, Path::new("/tools/doas"));
        assert_eq!(command.args, ["/tools/apk", "upgrade", "--", "claude-code"]);

        fixture.files.insert(
            "/etc/apk/world".into(),
            "alpine-base\nclaude-code@stable\n".into(),
        );
        let args = ["version", "--", "claude-code"];
        fixture.answer("apk", &args,
            "Installed:                              Available:\nclaude-code-2.1.0-r0                     < 2.1.1-r0 @stable\n");
        let query = Query::Apk {
            command: fixture
                .context("/usr/bin/claude", &[])
                .command("apk", &args)
                .unwrap(),
            package: "claude-code".into(),
        };
        assert_eq!(
            smol::block_on(latest(&fixture, &query)).as_deref(),
            Some("2.1.1-r0")
        );
        fixture
            .files
            .insert("/etc/apk/world".into(), "claude-code=2.1.0-r0\n".into());
        assert_eq!(smol::block_on(latest(&fixture, &query)), None);
    }

    #[test]
    fn winget_portable_requires_both_installation_metadata_and_registered_id() {
        let mut fixture = Fixture::default();
        let directory = "/packages/Anthropic.ClaudeCode_Microsoft.Winget.Source_8wekyb3d8bbwe";
        let binary = format!("{directory}/claude.exe");
        let args = [
            "list",
            "--id",
            "Anthropic.ClaudeCode",
            "--exact",
            "--source",
            "winget",
            "--disable-interactivity",
        ];
        fixture.answer(
            "winget",
            &args,
            "Claude Code Anthropic.ClaudeCode 2.1.0 winget\n",
        );
        assert!(smol::block_on(winget(&fixture.context(&binary, &[]))).is_none());
        fixture.path(&format!(
            "{directory}/Anthropic.ClaudeCode_Microsoft.Winget.Source_8wekyb3d8bbwe.db"
        ));
        let installation = smol::block_on(winget(&fixture.context(&binary, &[]))).unwrap();
        assert_eq!(
            installation.update.unwrap().args,
            [
                "upgrade",
                "--id",
                "Anthropic.ClaudeCode",
                "--exact",
                "--source",
                "winget"
            ]
        );
        fixture.answer(
            "winget",
            &args,
            "Claude Code Other.ClaudeCode 2.1.0 winget\n",
        );
        assert!(smol::block_on(winget(&fixture.context(&binary, &[]))).is_none());
    }
}
