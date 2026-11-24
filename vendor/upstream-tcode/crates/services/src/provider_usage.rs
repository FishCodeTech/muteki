//! Provider account usage fetching and normalization.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use agent::{LaunchEnv, ProviderKind};
use chrono::DateTime;
use serde_json::Value;
use tcode_core::usage::{ProviderUsage, UsageWindow, UsageWindowKind};

fn error_usage(fetched_at: u64, message: impl Into<String>) -> ProviderUsage {
    ProviderUsage {
        fetched_at,
        error: Some(message.into()),
        ..ProviderUsage::default()
    }
}

fn unix_seconds(value: &Value) -> Option<u64> {
    value.as_u64()
}

fn rfc3339_seconds(value: &Value) -> Option<u64> {
    let timestamp = DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()?
        .timestamp();
    u64::try_from(timestamp).ok()
}

fn codex_window(value: &Value) -> Option<UsageWindow> {
    let minutes = u32::try_from(value.get("windowDurationMins")?.as_u64()?).ok()?;
    Some(UsageWindow {
        kind: UsageWindowKind::from_minutes(minutes),
        scope: None,
        used_percent: value.get("usedPercent")?.as_f64()? as f32,
        resets_at: value.get("resetsAt").and_then(unix_seconds),
    })
}

/// Normalize the account-wide `rateLimits` object returned by Codex.
pub fn parse_codex_rate_limits(value: &Value, fetched_at: u64) -> ProviderUsage {
    let Some(rate_limits) = value.get("rateLimits").filter(|value| !value.is_null()) else {
        return error_usage(fetched_at, "no rate limits reported");
    };
    let plan = rate_limits
        .get("planType")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut windows: Vec<_> = ["primary", "secondary"]
        .into_iter()
        .filter_map(|key| rate_limits.get(key).filter(|value| !value.is_null()))
        .filter_map(codex_window)
        .collect();
    windows.sort_by_key(|window| match window.kind {
        UsageWindowKind::FiveHour => 0,
        UsageWindowKind::Weekly => 1,
        UsageWindowKind::Other { .. } => 2,
    });
    ProviderUsage {
        fetched_at,
        plan,
        windows,
        error: None,
    }
}

fn claude_window(
    value: &Value,
    kind: UsageWindowKind,
    scope: Option<String>,
) -> Option<UsageWindow> {
    let used_percent = value
        .get("percent")
        .or_else(|| value.get("utilization"))?
        .as_f64()? as f32;
    Some(UsageWindow {
        kind,
        scope,
        used_percent,
        resets_at: value.get("resets_at").and_then(rfc3339_seconds),
    })
}

/// Normalize Claude Code's `get_usage` response.
pub fn parse_claude_usage(value: &Value, fetched_at: u64) -> ProviderUsage {
    let plan = value
        .get("subscription_type")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if value.get("rate_limits_available").and_then(Value::as_bool) == Some(false) {
        return ProviderUsage {
            plan,
            ..error_usage(fetched_at, "usage not available")
        };
    }
    let Some(rate_limits) = value.get("rate_limits").filter(|value| !value.is_null()) else {
        return ProviderUsage {
            plan,
            ..error_usage(fetched_at, "usage not available")
        };
    };

    let mut five_hour = Vec::new();
    let mut weekly = Vec::new();
    let mut scoped = Vec::new();
    if let Some(limits) = rate_limits.get("limits").and_then(Value::as_array) {
        for limit in limits {
            let scope = limit
                .pointer("/scope/model/display_name")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if limit.get("kind").and_then(Value::as_str) == Some("session") {
                if let Some(window) = claude_window(limit, UsageWindowKind::FiveHour, scope) {
                    five_hour.push(window);
                }
            } else if limit.get("group").and_then(Value::as_str) == Some("weekly")
                && let Some(window) = claude_window(limit, UsageWindowKind::Weekly, scope.clone())
            {
                if scope.is_some() {
                    scoped.push(window);
                } else {
                    weekly.push(window);
                }
            }
        }
    } else {
        if let Some(window) = rate_limits
            .get("five_hour")
            .filter(|value| !value.is_null())
            .and_then(|value| claude_window(value, UsageWindowKind::FiveHour, None))
        {
            five_hour.push(window);
        }
        if let Some(window) = rate_limits
            .get("seven_day")
            .filter(|value| !value.is_null())
            .and_then(|value| claude_window(value, UsageWindowKind::Weekly, None))
        {
            weekly.push(window);
        }
        for (key, label) in [("seven_day_opus", "Opus"), ("seven_day_sonnet", "Sonnet")] {
            if let Some(window) = rate_limits
                .get(key)
                .filter(|value| !value.is_null())
                .and_then(|value| {
                    claude_window(value, UsageWindowKind::Weekly, Some(label.to_owned()))
                })
            {
                scoped.push(window);
            }
        }
    }
    five_hour.extend(weekly);
    five_hour.extend(scoped);
    ProviderUsage {
        fetched_at,
        plan,
        windows: five_hour,
        error: None,
    }
}

/// Fetch and normalize usage for providers that expose account limits.
pub async fn fetch_provider_usage(
    provider: ProviderKind,
    binary: Option<PathBuf>,
    launch_env: LaunchEnv,
) -> Option<ProviderUsage> {
    let fetched_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let result = match provider {
        ProviderKind::Codex => agent::codex::read_rate_limits(binary, launch_env)
            .await
            .map(|value| parse_codex_rate_limits(&value, fetched_at)),
        ProviderKind::ClaudeCode => agent::claude::read_usage(binary, launch_env)
            .await
            .map(|value| parse_claude_usage(&value, fetched_at)),
        ProviderKind::Pi
        | ProviderKind::OpenCode
        | ProviderKind::Cursor
        | ProviderKind::Grok
        | ProviderKind::Acp => return None,
    };
    Some(result.unwrap_or_else(|error| error_usage(fetched_at, error.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CODEX_FIXTURE: &str = r#"{"rateLimits":{"limitId":"codex","limitName":null,"primary":{"usedPercent":88,"windowDurationMins":10080,"resetsAt":1788762144},"secondary":null,"credits":{"hasCredits":false,"unlimited":false,"balance":"0"},"individualLimit":null,"spendControlReached":false,"planType":"pro","rateLimitReachedType":null},"rateLimitsByLimitId":{"codex_bengalfox":{"limitId":"codex_bengalfox","limitName":"GPT-5.3-Codex-Spark","primary":{"usedPercent":0,"windowDurationMins":300,"resetsAt":1788565765},"secondary":{"usedPercent":0,"windowDurationMins":10080,"resetsAt":1789152565},"credits":null,"individualLimit":null,"spendControlReached":null,"planType":"pro","rateLimitReachedType":null},"codex":{"limitId":"codex","limitName":null,"primary":{"usedPercent":88,"windowDurationMins":10080,"resetsAt":1788762144},"secondary":null,"credits":{"hasCredits":false,"unlimited":false,"balance":"0"},"individualLimit":null,"spendControlReached":false,"planType":"pro","rateLimitReachedType":null}},"rateLimitResetCredits":{"availableCount":2,"credits":[]},"accountId":"01b26ee4-1038-4041-bf2c-557ecbec93d0","rateLimitUpsell":null}"#;
    const CLAUDE_FIXTURE: &str = r#"{"type":"control_response","response":{"subtype":"success","request_id":"u1","response":{"session":{"total_cost_usd":0,"model_usage":{}},"subscription_type":"max","rate_limits_available":true,"rate_limits":{"five_hour":{"utilization":94,"resets_at":"2026-09-04T21:20:00.451091+00:00"},"seven_day":{"utilization":49,"resets_at":"2026-09-05T15:00:00.451116+00:00"},"seven_day_oauth_apps":null,"seven_day_opus":null,"seven_day_sonnet":null,"limits":[{"kind":"session","group":"session","percent":94,"severity":"critical","resets_at":"2026-09-04T21:20:00.451091+00:00","scope":null,"is_active":true},{"kind":"weekly_all","group":"weekly","percent":49,"severity":"normal","resets_at":"2026-09-05T15:00:00.451116+00:00","scope":null,"is_active":false},{"kind":"weekly_scoped","group":"weekly","percent":79,"severity":"warning","resets_at":"2026-09-05T15:00:00.451382+00:00","scope":{"model":{"id":null,"display_name":"Fable"},"surface":null},"is_active":false}],"model_scoped":[{"display_name":"Fable","utilization":79,"resets_at":"2026-09-05T15:00:00.451382+00:00"}]},"behaviors":{}}}}"#;

    #[test]
    fn parses_codex_pro_account_limit_only() {
        let usage = parse_codex_rate_limits(&serde_json::from_str(CODEX_FIXTURE).unwrap(), 42);
        assert_eq!(usage.fetched_at, 42);
        assert_eq!(usage.plan.as_deref(), Some("pro"));
        assert_eq!(usage.error, None);
        assert_eq!(usage.windows.len(), 1);
        assert_eq!(usage.windows[0].kind, UsageWindowKind::Weekly);
        assert_eq!(usage.windows[0].scope, None);
        assert_eq!(usage.windows[0].used_percent, 88.0);
        assert_eq!(usage.windows[0].resets_at, Some(1_788_762_144));

        let usage = parse_codex_rate_limits(
            &json!({ "rateLimits": {
                "primary": {"usedPercent": 50, "windowDurationMins": 10080, "resetsAt": "invalid"},
                "secondary": {"usedPercent": 25, "windowDurationMins": 300, "resetsAt": 123}
            }}),
            44,
        );
        assert_eq!(
            usage
                .windows
                .iter()
                .map(|window| (window.kind, window.used_percent, window.resets_at))
                .collect::<Vec<_>>(),
            [
                (UsageWindowKind::FiveHour, 25.0, Some(123)),
                (UsageWindowKind::Weekly, 50.0, None),
            ]
        );
        let usage = parse_codex_rate_limits(
            &json!({ "rateLimits": {
                "primary": {"usedPercent": "malformed", "windowDurationMins": 300},
                "secondary": {"usedPercent": 10, "windowDurationMins": 60}
            }}),
            45,
        );
        assert_eq!(usage.windows.len(), 1);
        assert_eq!(
            usage.windows[0].kind,
            UsageWindowKind::Other { minutes: 60 }
        );

        let usage = parse_codex_rate_limits(&json!({ "rateLimits": null }), 45);
        assert_eq!(usage.error.as_deref(), Some("no rate limits reported"));
        assert!(usage.windows.is_empty());
    }

    #[test]
    fn parses_claude_limits_in_display_order() {
        let fixture: Value = serde_json::from_str(CLAUDE_FIXTURE).unwrap();
        let usage = parse_claude_usage(fixture.pointer("/response/response").unwrap(), 43);
        assert_eq!(usage.plan.as_deref(), Some("max"));
        assert_eq!(usage.error, None);
        assert_eq!(usage.windows.len(), 3);
        assert_eq!(usage.windows[0].kind, UsageWindowKind::FiveHour);
        assert_eq!(usage.windows[0].used_percent, 94.0);
        assert_eq!(usage.windows[0].resets_at, Some(1_788_556_800));
        assert_eq!(usage.windows[1].kind, UsageWindowKind::Weekly);
        assert_eq!(usage.windows[1].scope, None);
        assert_eq!(usage.windows[1].used_percent, 49.0);
        assert_eq!(usage.windows[2].kind, UsageWindowKind::Weekly);
        assert_eq!(usage.windows[2].scope.as_deref(), Some("Fable"));
        assert_eq!(usage.windows[2].used_percent, 79.0);
        let legacy = parse_claude_usage(
            &json!({ "rate_limits": {
                "seven_day_opus": {"utilization": 30},
                "seven_day": {"utilization": 20},
                "five_hour": {"utilization": 10},
                "seven_day_sonnet": null
            }}),
            44,
        );
        assert_eq!(
            legacy
                .windows
                .iter()
                .map(|window| (window.kind, window.scope.as_deref(), window.used_percent))
                .collect::<Vec<_>>(),
            [
                (UsageWindowKind::FiveHour, None, 10.0),
                (UsageWindowKind::Weekly, None, 20.0),
                (UsageWindowKind::Weekly, Some("Opus"), 30.0),
            ]
        );
        let usage = parse_claude_usage(
            &json!({ "subscription_type": null, "rate_limits_available": false }),
            44,
        );
        assert_eq!(usage.error.as_deref(), Some("usage not available"));
        assert!(usage.windows.is_empty());
    }

    #[test]
    #[ignore = "requires installed and signed-in Codex and Claude Code CLIs"]
    fn live_provider_usage() {
        smol::block_on(async {
            for provider in [ProviderKind::Codex, ProviderKind::ClaudeCode] {
                let usage = fetch_provider_usage(provider, None, LaunchEnv::default())
                    .await
                    .expect("supported provider");
                println!("{provider:?}: {usage:#?}");
                assert_eq!(usage.error, None);
                assert!(!usage.windows.is_empty());
            }
        });
    }
}
