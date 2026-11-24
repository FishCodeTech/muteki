//! Provider status presentation with core-owned semantics.
//!
//! Redaction, localized copy, and probe-message presentation remain app-owned
//! here; pure summary derivation lives in `tcode_core` and auth JSON parsing
//! lives in the services crate.

use agent::ProviderKind;

use crate::settings::provider_label;

use tcode_core::provider_status::{AuthStatus, ProviderProbeDiagnostic, ProviderStatusKind};
pub use tcode_core::provider_status::{ProviderSnapshot, StatusDot};

#[cfg(test)]
use tcode_core::provider_status::ProviderAuth;

/// The derived card summary: dot + headline + detail (+ the email that the
/// headline embeds, so the card can render it with the reveal control).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusSummary {
    pub dot: StatusDot,
    /// The headline, with `{email}` standing in for a revealable email when
    /// `email` is `Some` (the card splits on it).
    pub headline: String,
    pub detail: String,
    pub email: Option<String>,
}

/// Placeholder the headline uses where a revealable email goes.
pub const EMAIL_SLOT: &str = "{email}";

/// Localized status copy for the semantic state supplied by the core summary.
pub fn summarize(
    provider: ProviderKind,
    snapshot: Option<&ProviderSnapshot>,
    enabled: bool,
) -> StatusSummary {
    let t = |key: &str| crate::tr!(key).into_owned();
    let checking = || StatusSummary {
        dot: StatusDot::Loading,
        headline: t("providers.status.checking"),
        detail: t("providers.status.checking_detail"),
        email: None,
    };
    let Some(snapshot) = snapshot else {
        return checking();
    };
    let Some(status) = snapshot.status else {
        return checking();
    };
    let detail_or = |fallback: &str| {
        snapshot
            .message
            .as_deref()
            .map(str::trim)
            .filter(|message| !message.is_empty())
            .map(str::to_string)
            .or_else(|| {
                snapshot
                    .diagnostic
                    .map(|diagnostic| probe_diagnostic_message(provider, diagnostic))
            })
            .unwrap_or_else(|| {
                if fallback.is_empty() {
                    String::new()
                } else {
                    t(fallback)
                }
            })
    };
    if !enabled {
        return StatusSummary {
            dot: StatusDot::Amber,
            headline: t("providers.status.disabled"),
            detail: detail_or("providers.status.disabled_detail"),
            email: None,
        };
    }
    if !snapshot.installed {
        return StatusSummary {
            dot: StatusDot::Error,
            headline: t("providers.status.not_found"),
            detail: detail_or("providers.status.not_found_detail"),
            email: None,
        };
    }
    let dot = match status {
        ProviderStatusKind::Ready => StatusDot::Success,
        ProviderStatusKind::Warning => StatusDot::Warning,
        ProviderStatusKind::Error => StatusDot::Error,
    };
    let auth = snapshot.auth.as_ref();
    match auth.map(|auth| auth.status) {
        Some(AuthStatus::Authenticated) => {
            let email = auth
                .and_then(|auth| auth.email.clone())
                .filter(|email| !email.is_empty());
            let label = auth
                .and_then(|auth| auth.label.as_deref())
                .filter(|label| !label.is_empty());
            let headline = match (&email, &label) {
                (Some(_), Some(label)) => crate::tr!(
                    "providers.status.authenticated_as_with_label",
                    email = EMAIL_SLOT,
                    label = label
                )
                .into_owned(),
                (Some(_), None) => {
                    crate::tr!("providers.status.authenticated_as", email = EMAIL_SLOT).into_owned()
                }
                (None, Some(label)) => {
                    crate::tr!("providers.status.authenticated_with_label", label = label)
                        .into_owned()
                }
                (None, None) => t("providers.status.authenticated"),
            };
            StatusSummary {
                dot,
                headline,
                detail: detail_or(""),
                email,
            }
        }
        Some(AuthStatus::Unauthenticated) => StatusSummary {
            dot,
            headline: t("providers.status.not_authenticated"),
            detail: detail_or(""),
            email: None,
        },
        _ => {
            let (headline, detail) = match status {
                ProviderStatusKind::Warning => (
                    "providers.status.needs_attention",
                    "providers.status.needs_attention_detail",
                ),
                ProviderStatusKind::Error => (
                    "providers.status.unavailable",
                    "providers.status.unavailable_detail",
                ),
                ProviderStatusKind::Ready => (
                    "providers.status.available",
                    "providers.status.available_detail",
                ),
            };
            StatusSummary {
                dot,
                headline: t(headline),
                detail: detail_or(detail),
                email: None,
            }
        }
    }
}

/// Redact an email for the collapsed presentation: keep the first character of
/// the local part and the TLD, mask everything else (`t••••@•••••.com`).
pub fn redact_email(email: &str) -> String {
    let Some((local, domain)) = email.split_once('@') else {
        return "•".repeat(email.chars().count().max(1));
    };
    let head: String = local.chars().take(1).collect();
    let local_mask = "•".repeat(local.chars().count().saturating_sub(1).max(1));
    let domain_mask = match domain.rsplit_once('.') {
        Some((name, tld)) => format!("{}.{tld}", "•".repeat(name.chars().count().max(1))),
        None => "•".repeat(domain.chars().count().max(1)),
    };
    format!("{head}{local_mask}@{domain_mask}")
}

/// Translate a core-owned semantic probe diagnostic into app-localized copy.
pub fn probe_diagnostic_message(
    provider: ProviderKind,
    diagnostic: ProviderProbeDiagnostic,
) -> String {
    match diagnostic {
        ProviderProbeDiagnostic::MissingCli => match provider {
            ProviderKind::Codex => crate::tr!("providers.probe.codex_missing").into_owned(),
            ProviderKind::ClaudeCode => crate::tr!("providers.probe.claude_missing").into_owned(),
            ProviderKind::Pi => crate::tr!("providers.probe.pi_missing").into_owned(),
            ProviderKind::OpenCode => crate::tr!("providers.probe.opencode_missing").into_owned(),
            ProviderKind::Cursor => crate::tr!("providers.probe.cursor_missing").into_owned(),
            ProviderKind::Grok => crate::tr!("providers.probe.grok_missing").into_owned(),
            ProviderKind::Acp => String::new(),
        },
        ProviderProbeDiagnostic::FailedCli => crate::tr!(
            "providers.probe.failed_run",
            provider = provider_label(provider)
        )
        .into_owned(),
        ProviderProbeDiagnostic::Unauthenticated => match provider {
            ProviderKind::Codex => crate::tr!("providers.probe.codex_signed_out").into_owned(),
            ProviderKind::ClaudeCode => {
                crate::tr!("providers.probe.claude_signed_out").into_owned()
            }
            ProviderKind::Pi => crate::tr!("providers.probe.pi_signed_out").into_owned(),
            ProviderKind::OpenCode => {
                crate::tr!("providers.probe.opencode_signed_out").into_owned()
            }
            ProviderKind::Cursor => crate::tr!("providers.probe.cursor_signed_out").into_owned(),
            ProviderKind::Grok => crate::tr!("providers.probe.grok_signed_out").into_owned(),
            ProviderKind::Acp => String::new(),
        },
        ProviderProbeDiagnostic::IndeterminateAuth => crate::tr!(
            "providers.probe.indeterminate_auth",
            provider = provider_label(provider)
        )
        .into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summarize(snapshot: Option<&ProviderSnapshot>, enabled: bool) -> StatusSummary {
        super::summarize(ProviderKind::Codex, snapshot, enabled)
    }

    fn snapshot() -> ProviderSnapshot {
        ProviderSnapshot {
            installed: true,
            status: Some(ProviderStatusKind::Ready),
            ..ProviderSnapshot::default()
        }
    }

    #[test]
    fn status_summary_derivation_table() {
        let _locale_guard = crate::settings::TestLocaleGuard::acquire();
        // No snapshot yet → neutral loading dot + "checking" copy.
        let s = summarize(None, true);
        assert_eq!(s.dot, StatusDot::Loading);
        assert_eq!(s.headline, "Checking provider status");
        assert_eq!(
            s.detail,
            "Waiting for the server to report installation and authentication details."
        );

        // Disabled wins over everything else, with a server message when present.
        let s = summarize(Some(&snapshot()), false);
        assert_eq!(s.dot, StatusDot::Amber);
        assert_eq!(s.headline, "Disabled");
        assert_eq!(
            s.detail,
            "This provider is installed but disabled for new sessions in Tcode."
        );

        let missing = ProviderSnapshot {
            installed: false,
            status: Some(ProviderStatusKind::Error),
            ..ProviderSnapshot::default()
        };
        let s = summarize(Some(&missing), true);
        assert_eq!(s.dot, StatusDot::Error);
        assert_eq!(s.headline, "Not found");
        assert_eq!(s.detail, "CLI not detected on PATH.");

        let authed = ProviderSnapshot {
            auth: Some(ProviderAuth {
                status: AuthStatus::Authenticated,
                label: Some("Claude Max Subscription".into()),
                email: Some("dev@example.com".into()),
            }),
            ..snapshot()
        };
        let s = summarize(Some(&authed), true);
        assert_eq!(s.dot, StatusDot::Success);
        assert_eq!(
            s.headline,
            "Authenticated as {email} · Claude Max Subscription"
        );
        assert_eq!(s.email.as_deref(), Some("dev@example.com"));

        // Blank auth metadata behaves like absent metadata.
        let blank_auth = ProviderSnapshot {
            auth: Some(ProviderAuth {
                status: AuthStatus::Authenticated,
                label: Some(String::new()),
                email: Some(String::new()),
            }),
            ..snapshot()
        };
        let s = summarize(Some(&blank_auth), true);
        assert_eq!(s.headline, "Authenticated");
        assert_eq!(s.email, None);

        let label_only = ProviderSnapshot {
            auth: Some(ProviderAuth {
                status: AuthStatus::Authenticated,
                label: Some("OpenAI API Key".into()),
                email: None,
            }),
            ..snapshot()
        };
        assert_eq!(
            summarize(Some(&label_only), true).headline,
            "Authenticated · OpenAI API Key"
        );

        let bare = ProviderSnapshot {
            auth: Some(ProviderAuth {
                status: AuthStatus::Authenticated,
                label: None,
                email: None,
            }),
            ..snapshot()
        };
        assert_eq!(summarize(Some(&bare), true).headline, "Authenticated");

        let signed_out = ProviderSnapshot {
            auth: Some(ProviderAuth {
                status: AuthStatus::Unauthenticated,
                label: None,
                email: None,
            }),
            message: Some("Run `codex login` and try again.".into()),
            ..snapshot()
        };
        let s = summarize(Some(&signed_out), true);
        assert_eq!(s.headline, "Not authenticated");
        assert_eq!(s.detail, "Run `codex login` and try again.");

        let warn = ProviderSnapshot {
            status: Some(ProviderStatusKind::Warning),
            ..snapshot()
        };
        let s = summarize(Some(&warn), true);
        assert_eq!(s.dot, StatusDot::Warning);
        assert_eq!(s.headline, "Needs attention");
        assert_eq!(
            s.detail,
            "The provider is installed, but the server could not fully verify it."
        );

        let err = ProviderSnapshot {
            status: Some(ProviderStatusKind::Error),
            ..snapshot()
        };
        let s = summarize(Some(&err), true);
        assert_eq!(s.dot, StatusDot::Error);
        assert_eq!(s.headline, "Unavailable");
        assert_eq!(s.detail, "The provider failed its startup checks.");

        let s = summarize(Some(&snapshot()), true);
        assert_eq!(s.headline, "Available");
        assert_eq!(
            s.detail,
            "Installed and ready, but authentication could not be verified."
        );

        // An in-flight probe (the placeholder snapshot the refresh inserts)
        // reads as "checking", not as "Not found" — its `installed` flag is
        // simply not known yet.
        let in_flight = ProviderSnapshot {
            checking: true,
            message: Some("ignored".into()),
            diagnostic: Some(ProviderProbeDiagnostic::IndeterminateAuth),
            ..ProviderSnapshot::default()
        };
        let s = summarize(Some(&in_flight), false);
        assert_eq!(s.dot, StatusDot::Loading);
        assert_eq!(s.headline, "Checking provider status");
        assert_eq!(
            s.detail,
            "Waiting for the server to report installation and authentication details."
        );

        // A server message always replaces the fallback detail.
        let with_message = ProviderSnapshot {
            message: Some("custom diagnostic".into()),
            ..snapshot()
        };
        assert_eq!(
            summarize(Some(&with_message), true).detail,
            "custom diagnostic"
        );

        let disabled_message = ProviderSnapshot {
            message: Some(" custom disabled ".into()),
            ..snapshot()
        };
        assert_eq!(
            summarize(Some(&disabled_message), false).detail,
            "custom disabled"
        );
        let missing_message = ProviderSnapshot {
            status: Some(ProviderStatusKind::Error),
            message: Some(" custom missing ".into()),
            ..ProviderSnapshot::default()
        };
        assert_eq!(
            summarize(Some(&missing_message), true).detail,
            "custom missing"
        );
        let blank_message = ProviderSnapshot {
            message: Some("  ".into()),
            ..snapshot()
        };
        assert_eq!(
            summarize(Some(&blank_message), true).detail,
            "Installed and ready, but authentication could not be verified."
        );

        let diagnostic = ProviderSnapshot {
            diagnostic: Some(ProviderProbeDiagnostic::Unauthenticated),
            message: Some("  ".into()),
            ..snapshot()
        };
        assert_eq!(
            summarize(Some(&diagnostic), true).detail,
            "Codex CLI is not authenticated. Run `codex login` and try again."
        );
        let diagnostic_with_message = ProviderSnapshot {
            diagnostic: Some(ProviderProbeDiagnostic::FailedCli),
            message: Some(" precise message ".into()),
            ..snapshot()
        };
        assert_eq!(
            summarize(Some(&diagnostic_with_message), true).detail,
            "precise message"
        );
        for (locale, expected) in [
            (
                crate::LANGUAGE_ENGLISH,
                [
                    "Codex CLI (`codex`) is not installed or not on PATH.",
                    "Codex CLI is installed but failed to run.",
                    "Codex CLI is not authenticated. Run `codex login` and try again.",
                    "Could not verify Codex authentication status.",
                ],
            ),
            (
                crate::LANGUAGE_SIMPLIFIED_CHINESE,
                [
                    "未安装 Codex CLI（`codex`），或其不在 PATH 中。",
                    "Codex CLI 已安装，但运行失败。",
                    "Codex CLI 未认证。请运行 `codex login` 后重试。",
                    "无法验证 Codex 的认证状态。",
                ],
            ),
        ] {
            crate::set_locale(locale);
            for (diagnostic, expected) in [
                ProviderProbeDiagnostic::MissingCli,
                ProviderProbeDiagnostic::FailedCli,
                ProviderProbeDiagnostic::Unauthenticated,
                ProviderProbeDiagnostic::IndeterminateAuth,
            ]
            .into_iter()
            .zip(expected)
            {
                let status = ProviderSnapshot {
                    diagnostic: Some(diagnostic),
                    message: Some("  ".into()),
                    ..snapshot()
                };
                assert_eq!(summarize(Some(&status), true).detail, expected);
            }
        }
        crate::set_locale(crate::LANGUAGE_ENGLISH);
    }

    #[test]
    fn redacts_email_but_keeps_shape() {
        for (email, expected) in [
            ("developer@example.com", "d••••••••@•••••••.com"),
            ("张三@例子.com", "张•@••.com"),
            ("a@localhost", "a•@•••••••••"),
            ("@", "•@•"),
            ("invalid", "•••••••"),
            ("", "•"),
        ] {
            assert_eq!(redact_email(email), expected, "{email}");
        }
    }
}
