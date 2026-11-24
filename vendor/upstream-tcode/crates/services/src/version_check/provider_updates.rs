//! Assessment of provider CLI updates from process-runner facts.

use agent::ProviderKind;

mod installation;
#[path = "provider_updates/system_managers.rs"]
mod system_managers;
pub use installation::{Installation, UpdateCommand, latest_version, resolve_installation};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InstallSource {
    Mise,
    Brew,
    Npm,
    Bun,
    Pnpm,
    Yarn,
    Volta,
    Asdf,
    Scoop,
    Chocolatey,
    Winget,
    System,
    Nix,
    Native,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy)]
pub struct CheckInput<'a> {
    pub installed_output: Option<&'a str>,
    pub latest_output: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assessment {
    pub current: Option<String>,
    pub latest: Option<String>,
    /// True only when both versions were parsed and latest is newer.
    pub update_available: bool,
}

/// Assess a provider update from process-runner facts without spawning a process.
/// Unparseable output is retained for display, but never announces an update.
pub fn check(input: CheckInput<'_>) -> Assessment {
    let installed_version = input.installed_output.and_then(parse_version);
    let latest_version = input.latest_output.and_then(parse_version);
    Assessment {
        current: input.installed_output.map(normalize_version),
        latest: input.latest_output.map(normalize_version),
        update_available: installed_version
            .zip(latest_version)
            .is_some_and(|(installed, latest)| latest > installed),
    }
}

/// The npm package queried by the runtime adapter for a provider's latest
/// published version.
pub fn npm_package(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::ClaudeCode => "@anthropic-ai/claude-code",
        ProviderKind::Codex => "@openai/codex",
        ProviderKind::Pi => "@earendil-works/pi-coding-agent",
        ProviderKind::OpenCode => "opencode-ai",
        ProviderKind::Grok => "@xai-official/grok",
        // Cursor's CLI is not published to npm.
        ProviderKind::Cursor | ProviderKind::Acp => "",
    }
}

/// Parse the first provider-version token from loose, human-facing CLI output.
///
/// Accepts `MAJOR.MINOR` provider output with an implicit zero patch, unlike
/// the stricter parser for app release tags.
pub(crate) fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    text.split_whitespace().find_map(parse_version_token)
}

fn parse_version_token(token: &str) -> Option<(u32, u32, u32)> {
    let token = token.trim_start_matches('v');
    let core = token.split(['-', '+', ' ']).next().unwrap_or(token);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().and_then(|part| part.parse().ok()).unwrap_or(0);
    // Reject a bare integer; two-part provider versions are intentionally
    // accepted and normalized with a zero patch.
    (core.matches('.').count() >= 1).then_some((major, minor, patch))
}

fn normalize_version(raw: &str) -> String {
    parse_version(raw).map_or_else(|| raw.to_string(), format_version)
}

fn format_version((major, minor, patch): (u32, u32, u32)) -> String {
    format!("{major}.{minor}.{patch}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(installed: Option<&'a str>, latest: Option<&'a str>) -> CheckInput<'a> {
        CheckInput {
            installed_output: installed,
            latest_output: latest,
        }
    }

    #[test]
    fn check_normalizes_versions_and_announces_only_proven_upgrades() {
        let cases = [
            ("2.1.206 (Claude Code)", "2.1.207", true, "2.1.206"),
            ("codex-cli 0.144.1", "0.145.0", true, "0.144.1"),
            ("2.1.207", "2.1.207", false, "2.1.207"),
            ("2.1.208", "2.1.207", false, "2.1.208"),
            ("v1.2.3-beta.1", "1.2.4", true, "1.2.3"),
            ("1.2", "1.2.1", true, "1.2.0"),
        ];
        for (installed, latest, available, normalized) in cases {
            let assessment = check(input(Some(installed), Some(latest)));
            assert_eq!(assessment.update_available, available, "{installed}");
            assert_eq!(assessment.current.as_deref(), Some(normalized));
        }
        for (installed, latest) in [
            (None, Some("2.0.0")),
            (Some("1.0.0"), None),
            (Some("build 5"), Some("2.0.0")),
            (Some("1.0.0"), Some("nonsense")),
        ] {
            let assessment = check(input(installed, latest));
            assert!(!assessment.update_available);
            assert_eq!(assessment.current.as_deref(), installed);
            assert_eq!(assessment.latest.as_deref(), latest);
        }
    }
}
