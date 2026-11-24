//! Provider CLI probing and process execution.

use std::path::PathBuf;

use agent::{LaunchEnv, ProviderKind};
use tcode_core::provider_status::{
    AuthStatus, ProviderAuth, ProviderProbeDiagnostic, ProviderSnapshot, ProviderStatusKind,
};

use crate::provider_auth::{parse_aggregator_auth, parse_claude_auth, parse_codex_auth};

/// The bare command name for a provider (fallback when no path resolves).
pub fn default_program(provider: ProviderKind) -> String {
    match provider {
        ProviderKind::Codex => "codex".into(),
        ProviderKind::ClaudeCode => "claude".into(),
        ProviderKind::Pi => "pi".into(),
        ProviderKind::OpenCode => "opencode".into(),
        ProviderKind::Cursor => "cursor-agent".into(),
        ProviderKind::Grok => "grok".into(),
        // ACP agents carry their own registry launch recipe.
        ProviderKind::Acp => String::new(),
    }
}

/// Return trimmed stdout on success, falling back to stderr when stdout is empty.
pub async fn run_capture(program: &str, args: &[&str]) -> Option<String> {
    run_capture_env(program, args, &[]).await
}

/// [`run_capture`] with extra environment variables applied to the child.
pub async fn run_capture_env(
    program: &str,
    args: &[&str],
    env: &[(String, String)],
) -> Option<String> {
    let mut cmd = crate::process::async_command(program);
    cmd.args(args)
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT");
    for (key, value) in env {
        cmd.env(key, value);
    }
    let output = cmd.output().await.ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !text.is_empty() {
        return Some(text);
    }
    // Some CLIs (pi among them) print `--version` output to stderr; a clean
    // exit with empty stdout is still a successful run.
    let text = String::from_utf8_lossy(&output.stderr).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Probe one provider's installation, version, and authentication state.
pub async fn probe_provider(
    provider: ProviderKind,
    binary: Option<PathBuf>,
    launch_env: LaunchEnv,
) -> ProviderSnapshot {
    let checked_at = Some(crate::store::now_secs());
    let Some(binary) = binary else {
        let diagnostic = Some(ProviderProbeDiagnostic::MissingCli);
        return ProviderSnapshot {
            checked_at,
            installed: false,
            status: Some(ProviderStatusKind::Error),
            diagnostic,
            message: None,
            ..ProviderSnapshot::default()
        };
    };
    let program = binary.to_string_lossy().into_owned();
    let env = launch_env.pairs(provider);

    let Some(raw_version) = run_capture_env(&program, &["--version"], &env).await else {
        let diagnostic = Some(ProviderProbeDiagnostic::FailedCli);
        return ProviderSnapshot {
            checked_at,
            installed: true,
            status: Some(ProviderStatusKind::Error),
            diagnostic,
            message: None,
            ..ProviderSnapshot::default()
        };
    };
    let version = crate::version_check::provider_updates::parse_version(&raw_version)
        .map(|(a, b, c)| format!("{a}.{b}.{c}"))
        .or(Some(raw_version));

    let auth = match provider {
        ProviderKind::ClaudeCode => run_capture_env(&program, &["auth", "status", "--json"], &env)
            .await
            .as_deref()
            .and_then(parse_claude_auth),
        ProviderKind::Codex => {
            let path = agent::codex::home_dir(&launch_env).map(|home| home.join("auth.json"));
            let json = path.and_then(|path| std::fs::read_to_string(path).ok());
            json.as_deref().and_then(parse_codex_auth)
        }
        ProviderKind::Pi => {
            let home = launch_env
                .home
                .or_else(|| dirs::home_dir().map(|home| home.join(".pi/agent")));
            let path = home.map(|home| home.join("auth.json"));
            let json = path.and_then(|path| std::fs::read_to_string(path).ok());
            json.as_deref().and_then(parse_aggregator_auth)
        }
        ProviderKind::OpenCode => {
            let xdg_data = env
                .iter()
                .rev()
                .find(|(key, _)| key == "XDG_DATA_HOME")
                .map(|(_, value)| PathBuf::from(value))
                .or_else(|| std::env::var_os("XDG_DATA_HOME").map(PathBuf::from))
                .or_else(|| dirs::home_dir().map(|home| home.join(".local/share")));
            let path = xdg_data.map(|home| home.join("opencode/auth.json"));
            let json = path.and_then(|path| std::fs::read_to_string(path).ok());
            json.as_deref().and_then(parse_aggregator_auth)
        }
        ProviderKind::Cursor => run_capture_env(&program, &["status", "--format", "json"], &env)
            .await
            .as_deref()
            .and_then(|json| parse_cursor_auth(json, cursor_key_configured(&env))),
        ProviderKind::Grok => run_capture_env(&program, &["models"], &env)
            .await
            .as_deref()
            .and_then(parse_grok_models_auth),
        // Authentication over ACP is surfaced by the session protocol.
        ProviderKind::Acp => None,
    };

    finalize_probe(checked_at, version, auth)
}

/// `cursor-agent status --format json` reports only the stored login: it says
/// `unauthenticated` while CURSOR_API_KEY or CURSOR_AUTH_TOKEN signs the CLI
/// in, so it counts as signed out only when neither is set.
fn parse_cursor_auth(json: &str, key_configured: bool) -> Option<ProviderAuth> {
    let status: serde_json::Value = serde_json::from_str(json).ok()?;
    if status.get("isAuthenticated") == Some(&serde_json::Value::Bool(true)) {
        return Some(ProviderAuth {
            status: AuthStatus::Authenticated,
            label: None,
            email: status
                .pointer("/userInfo/email")
                .and_then(serde_json::Value::as_str)
                .filter(|email| !email.is_empty())
                .map(str::to_string),
        });
    }
    (status.get("status").and_then(serde_json::Value::as_str) == Some("unauthenticated")
        && !key_configured)
        .then_some(ProviderAuth {
            status: AuthStatus::Unauthenticated,
            label: None,
            email: None,
        })
}

/// Whether Cursor's child sees an API key or auth token, from the profile's
/// environment or the one it inherits.
fn cursor_key_configured(env: &[(String, String)]) -> bool {
    ["CURSOR_API_KEY", "CURSOR_AUTH_TOKEN"].iter().any(|key| {
        match env.iter().rev().find(|(name, _)| name == key) {
            Some((_, value)) => !value.is_empty(),
            None => std::env::var_os(key).is_some_and(|value| !value.is_empty()),
        }
    })
}

/// The first line of `grok models`. Only the states observed from Grok 1.0.46
/// are recognized; the API-key line reports a configured key, which Grok does
/// not validate there.
fn parse_grok_models_auth(output: &str) -> Option<ProviderAuth> {
    let (status, label) = match output.lines().next()?.trim() {
        "You are not authenticated." => (AuthStatus::Unauthenticated, None),
        "You are using XAI_API_KEY." => (AuthStatus::Authenticated, Some("xAI API Key")),
        _ => return None,
    };
    Some(ProviderAuth {
        status,
        label: label.map(str::to_string),
        email: None,
    })
}

fn finalize_probe(
    checked_at: Option<u64>,
    version: Option<String>,
    auth: Option<ProviderAuth>,
) -> ProviderSnapshot {
    let (status, diagnostic, auth) = match &auth {
        Some(provider_auth) if provider_auth.status == AuthStatus::Unauthenticated => (
            ProviderStatusKind::Error,
            Some(ProviderProbeDiagnostic::Unauthenticated),
            auth,
        ),
        Some(_) => (ProviderStatusKind::Ready, None, auth),
        None => (
            ProviderStatusKind::Warning,
            Some(ProviderProbeDiagnostic::IndeterminateAuth),
            Some(ProviderAuth {
                status: AuthStatus::Unknown,
                label: None,
                email: None,
            }),
        ),
    };
    ProviderSnapshot {
        checked_at,
        installed: true,
        version,
        status: Some(status),
        auth,
        diagnostic,
        message: None,
        checking: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_binary_is_semantic_and_unlocalized() {
        let result = smol::block_on(probe_provider(
            ProviderKind::Codex,
            None,
            LaunchEnv::default(),
        ));
        assert!(!result.installed);
        assert_eq!(result.status, Some(ProviderStatusKind::Error));
        assert_eq!(result.message, None);
        assert_eq!(result.diagnostic, Some(ProviderProbeDiagnostic::MissingCli));
    }

    /// `cursor-agent status --format json` from the agent crate's stand-in,
    /// which prints the fixture it is pointed at.
    #[cfg(unix)]
    #[test]
    fn cursor_is_signed_out_only_without_an_api_key_or_auth_token() {
        let fixtures =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/cursor");
        let probe = |status: &str, api_key: &str, auth_token: &str| {
            let env = vec![
                (
                    "TCODE_STAND_IN_STATUS".to_string(),
                    fixtures.join(status).display().to_string(),
                ),
                ("CURSOR_API_KEY".to_string(), api_key.to_string()),
                ("CURSOR_AUTH_TOKEN".to_string(), auth_token.to_string()),
            ];
            smol::block_on(probe_provider(
                ProviderKind::Cursor,
                Some(fixtures.join("cursor-agent")),
                LaunchEnv { env, home: None },
            ))
        };

        let signed_out = probe("status_signed_out.json", "", "");
        assert_eq!(
            signed_out.diagnostic,
            Some(ProviderProbeDiagnostic::Unauthenticated)
        );
        for (api_key, auth_token) in [("key", ""), ("", "token")] {
            assert_eq!(
                probe("status_signed_out.json", api_key, auth_token).diagnostic,
                Some(ProviderProbeDiagnostic::IndeterminateAuth),
                "a key or token signs Cursor in without a stored login"
            );
        }

        let signed_in = probe("status_signed_in.json", "", "");
        assert_eq!(signed_in.status, Some(ProviderStatusKind::Ready));
        let auth = signed_in.auth.unwrap();
        assert_eq!(auth.status, AuthStatus::Authenticated);
        assert_eq!(auth.email.as_deref(), Some("dev@example.com"));
    }

    #[test]
    fn auth_outcomes_are_semantic_and_unlocalized() {
        let authenticated = finalize_probe(
            Some(1),
            Some("1.2.3".into()),
            Some(ProviderAuth {
                status: AuthStatus::Authenticated,
                label: Some("account".into()),
                email: None,
            }),
        );
        assert_eq!(authenticated.status, Some(ProviderStatusKind::Ready));
        assert_eq!(authenticated.message, None);
        assert_eq!(authenticated.diagnostic, None);

        let unauthenticated = finalize_probe(
            Some(1),
            Some("1.2.3".into()),
            Some(ProviderAuth {
                status: AuthStatus::Unauthenticated,
                label: None,
                email: None,
            }),
        );
        assert_eq!(unauthenticated.status, Some(ProviderStatusKind::Error));
        assert_eq!(unauthenticated.message, None);
        assert_eq!(
            unauthenticated.diagnostic,
            Some(ProviderProbeDiagnostic::Unauthenticated)
        );
        let indeterminate = finalize_probe(Some(1), Some("1.2.3".into()), None);
        assert_eq!(indeterminate.status, Some(ProviderStatusKind::Warning));
        assert_eq!(
            indeterminate.auth.as_ref().map(|auth| auth.status),
            Some(AuthStatus::Unknown)
        );
        assert_eq!(indeterminate.message, None);
        assert_eq!(
            indeterminate.diagnostic,
            Some(ProviderProbeDiagnostic::IndeterminateAuth)
        );
    }
}
