//! One rule evaluator for preview and live managed execution.
use super::*;
use std::collections::BTreeSet;
const MULTI_TENANT_CLOUDS: &[&str] = &[
    "github.io",
    "vercel.app",
    "netlify.app",
    "pages.dev",
    "myqcloud.com",
    "aliyuncs.com",
    "s3.amazonaws.com",
    "azurewebsites.net",
    "herokuapp.com",
    "workers.dev",
    "cloudfront.net",
    "fastly.net",
    "render.com",
    "fly.dev",
    "railway.app",
];

const PUBLIC_SUFFIX_ROOTS: &[&str] = &[
    "com", "cn", "net", "org", "io", "app", "dev", "ai", "gov", "edu", "mil", "co", "uk", "jp",
    "kr", "de", "fr", "ru", "au", "ca", "hk", "tw", "sg", "co.uk", "org.uk", "ac.uk", "gov.uk",
    "com.cn", "net.cn", "org.cn", "gov.cn", "edu.cn", "com.au", "com.hk", "co.jp", "ne.jp",
    "com.tw", "com.sg", "com.br",
];

/// Structural scope fuse protecting against accidental multi-tenant clouds,
/// bare TLD/public suffixes, or overly broad IPv4/IPv6 CIDR masks.
pub(in crate::runs::src_api) fn validate_scope_fuses(
    targets: &[String],
    override_reason: Option<&str>,
) -> Api<()> {
    let reason = override_reason.map(str::trim).unwrap_or("");
    let has_override = reason.chars().count() >= 6;
    for raw in targets {
        let trimmed = raw.trim();
        let host_part = if let Ok(url) = reqwest::Url::parse(trimmed) {
            url.host_str().unwrap_or("").to_ascii_lowercase()
        } else {
            trimmed.to_ascii_lowercase()
        };
        let clean = host_part.trim_start_matches('[').trim_end_matches(']');
        if clean.is_empty() {
            continue;
        }
        if let Some((ip_str, mask_str)) = clean.split_once('/') {
            if let (Ok(ip), Ok(prefix)) =
                (ip_str.parse::<std::net::IpAddr>(), mask_str.parse::<u8>())
            {
                let too_wide = match ip {
                    std::net::IpAddr::V4(_) => prefix < 24,
                    std::net::IpAddr::V6(_) => prefix < 64,
                };
                if too_wide && !has_override {
                    return Err(Error::new(
                        StatusCode::BAD_REQUEST,
                        "scope_fuse_triggered",
                        format!(
                            "Scope target '{raw}' exceeds CIDR safety limit (< /24 IPv4 or < /64 IPv6); provide scope_fuse_override_reason (>=6 chars) to confirm"
                        ),
                    ));
                }
                continue;
            }
        }
        let no_wild = clean.trim_start_matches("*.").trim_end_matches('.');
        if clean == "*" || no_wild.is_empty() || PUBLIC_SUFFIX_ROOTS.contains(&no_wild) {
            if !has_override {
                return Err(Error::new(
                    StatusCode::BAD_REQUEST,
                    "scope_fuse_triggered",
                    format!(
                        "Scope target '{raw}' matches a bare public suffix/wildcard apex; specify full tenant domain or scope_fuse_override_reason"
                    ),
                ));
            }
        }
        if MULTI_TENANT_CLOUDS.contains(&no_wild) && !has_override {
            return Err(Error::new(
                StatusCode::BAD_REQUEST,
                "scope_fuse_triggered",
                format!(
                    "Scope target '{raw}' targets shared multi-tenant cloud root '{no_wild}'; specify a specific tenant subdomain or scope_fuse_override_reason"
                ),
            ));
        }
    }
    Ok(())
}

/// Validates 7-granularity exclusions array (`host`, `port`, `path_prefix`, `method`, `route_regex`, `content_type`, `param_name`).
pub(in crate::runs::src_api) fn validate_exclusions(exclusions: &Value) -> Api<()> {
    let Some(items) = exclusions.as_array() else {
        return Err(Error::invalid("policy exclusions must be an array"));
    };
    if items.len() > 512 {
        return Err(Error::invalid("policy exclusions exceed 512 entry limit"));
    }
    for item in items {
        let obj = item
            .as_object()
            .ok_or_else(|| Error::invalid("each exclusion entry must be an object"))?;
        if obj.is_empty() {
            return Err(Error::invalid("exclusion entry cannot be empty"));
        }
        let has_known = [
            "host",
            "port",
            "path_prefix",
            "method",
            "route_regex",
            "content_type",
            "param_name",
        ]
        .iter()
        .any(|k| obj.get(*k).is_some_and(|v| !v.is_null()));
        if !has_known {
            return Err(Error::invalid(
                "exclusion must declare at least one of host, port, path_prefix, method, route_regex, content_type, param_name",
            ));
        }
        if let Some(rx) = obj.get("route_regex").and_then(Value::as_str) {
            regex::RegexBuilder::new(rx)
                .size_limit(128 * 1024)
                .build()
                .map_err(|_| Error::invalid(format!("invalid exclusion route_regex: {rx}")))?;
        }
        if let Some(port_val) = obj.get("port") {
            if let Some(p) = port_val.as_u64() {
                if p == 0 || p > 65535 {
                    return Err(Error::invalid("exclusion port must be 1..=65535"));
                }
            } else if let Some(arr) = port_val.as_array() {
                for entry in arr {
                    if entry.as_u64().is_none_or(|p| p == 0 || p > 65535) {
                        return Err(Error::invalid(
                            "exclusion port array entry must be 1..=65535",
                        ));
                    }
                }
            } else {
                return Err(Error::invalid(
                    "exclusion port must be integer or integer array",
                ));
            }
        }
    }
    Ok(())
}

fn host_matches(actual: &str, pattern: &str) -> bool {
    let actual = actual.trim().trim_end_matches('.').to_ascii_lowercase();
    let pat = pattern.trim().trim_end_matches('.').to_ascii_lowercase();
    if actual.is_empty() || pat.is_empty() {
        return false;
    }
    if let Some(suffix) = pat.strip_prefix("*.") {
        actual == suffix || actual.ends_with(&format!(".{suffix}"))
    } else {
        actual == pat
    }
}

fn exclusion_rule_matches(rule: &Value, request: &Value, target_url: &str) -> bool {
    let Some(obj) = rule.as_object() else {
        return false;
    };
    if obj.get("enabled").and_then(Value::as_bool) == Some(false) {
        return false;
    }
    let parsed = reqwest::Url::parse(target_url).ok();
    let req_host = fact(request, "host")
        .0
        .as_str()
        .map(str::to_ascii_lowercase)
        .or_else(|| {
            parsed
                .as_ref()
                .and_then(|u| u.host_str())
                .map(str::to_ascii_lowercase)
        })
        .unwrap_or_default();
    let req_port = fact(request, "port")
        .0
        .as_u64()
        .or_else(|| {
            parsed
                .as_ref()
                .and_then(|u| u.port_or_known_default())
                .map(u64::from)
        })
        .unwrap_or(0);
    let req_path = fact(request, "path")
        .0
        .as_str()
        .map(ToOwned::to_owned)
        .or_else(|| parsed.as_ref().map(|u| u.path().to_owned()))
        .unwrap_or_else(|| "/".to_owned());
    let req_method = fact(request, "method")
        .0
        .as_str()
        .unwrap_or("")
        .to_ascii_uppercase();

    if let Some(h) = obj.get("host").and_then(Value::as_str) {
        if !host_matches(&req_host, h) {
            return false;
        }
    }
    if let Some(port_val) = obj.get("port") {
        let port_ok = if let Some(p) = port_val.as_u64() {
            req_port == p
        } else if let Some(arr) = port_val.as_array() {
            arr.iter().any(|v| v.as_u64() == Some(req_port))
        } else {
            false
        };
        if !port_ok {
            return false;
        }
    }
    if let Some(prefix) = obj.get("path_prefix").and_then(Value::as_str) {
        if !req_path.starts_with(prefix) {
            return false;
        }
    }
    if let Some(method) = obj.get("method").and_then(Value::as_str) {
        if !req_method.eq_ignore_ascii_case(method.trim()) {
            return false;
        }
    }
    if let Some(rx) = obj.get("route_regex").and_then(Value::as_str) {
        let Ok(re) = regex::RegexBuilder::new(rx).size_limit(128 * 1024).build() else {
            return false;
        };
        if !re.is_match(&req_path) {
            return false;
        }
    }
    if let Some(ct) = obj.get("content_type").and_then(Value::as_str) {
        let wanted = ct.trim().to_ascii_lowercase();
        let headers = fact(request, "headers").0;
        let actual_ct = headers
            .as_array()
            .into_iter()
            .flatten()
            .find_map(|pair| {
                let name = pair.get(0)?.as_str()?;
                if name.eq_ignore_ascii_case("content-type") {
                    Some(pair.get(1)?.as_str()?.to_ascii_lowercase())
                } else {
                    None
                }
            })
            .unwrap_or_default();
        if !actual_ct.contains(&wanted) {
            return false;
        }
    }
    if let Some(param) = obj.get("param_name").and_then(Value::as_str) {
        let param = param.trim();
        let query_match = parsed
            .as_ref()
            .is_some_and(|u| u.query_pairs().any(|(k, _)| k == param));
        let arg_match = request["arguments"]["param_names"]
            .as_array()
            .is_some_and(|arr| arr.iter().any(|v| v.as_str() == Some(param)));
        if !query_match && !arg_match {
            return false;
        }
    }
    true
}

pub(super) fn validate_rule(rule: &Value) -> Api<()> {
    if !matches!(rule["effect"].as_str(), Some("allow" | "ask" | "deny")) {
        return Err(Error::invalid("rule effect must be allow, ask or deny"));
    }
    if rule["version"].as_u64().is_none_or(|version| version == 0)
        || !rule["enabled"].is_boolean()
        || !rule["reusable_approval"].is_boolean()
    {
        return Err(Error::invalid("Invalid execution rule version or flags"));
    }
    if !matches!(
        rule["resource_kind"].as_str(),
        Some("file" | "process" | "network" | "http" | "browser" | "mcp" | "control")
    ) || !matches!(
        rule["evaluation_point"].as_str(),
        Some("tool" | "filesystem" | "connection" | "request")
    ) {
        return Err(Error::invalid(
            "Invalid rule resource kind or evaluation point",
        ));
    }
    for field in ["actions", "source_refs"] {
        if rule[field]
            .as_array()
            .is_none_or(|values| values.iter().any(|value| !value.is_string()))
        {
            return Err(Error::invalid(format!(
                "rule {field} must be a string array"
            )));
        }
    }
    for field in ["selector", "conditions"] {
        let mapping = rule[field]
            .as_object()
            .ok_or_else(|| Error::invalid("Rule selector and conditions must be objects"))?;
        for condition in mapping.values() {
            if let Some(operators) = condition.as_object() {
                for (operator, value) in operators {
                    if !matches!(
                        operator.as_str(),
                        "exists"
                            | "eq"
                            | "neq"
                            | "in"
                            | "not_in"
                            | "contains"
                            | "not_contains"
                            | "prefix"
                            | "wildcard"
                            | "regex"
                            | "cidr"
                            | "target"
                            | "lt"
                            | "lte"
                            | "gt"
                            | "gte"
                    ) {
                        return Err(Error::invalid(format!(
                            "Unsupported policy operator {operator}"
                        )));
                    }
                    if operator == "regex" {
                        regex::Regex::new(
                            value
                                .as_str()
                                .ok_or_else(|| Error::invalid("Policy regex must be a string"))?,
                        )
                        .map_err(|_| Error::invalid("Policy regex is invalid"))?;
                    }
                }
            }
        }
    }
    Ok(())
}
pub(super) fn validate(request: &Value) -> Api<()> {
    if !request.is_object() {
        return Err(Error::invalid("execution request must be an object"));
    }
    for name in [
        "task_id",
        "thread_id",
        "tool_name",
        "action",
        "arguments_hash",
        "resource_kind",
    ] {
        if !request[name].is_string() {
            return Err(Error::invalid(format!(
                "execution request {name} must be a string"
            )));
        }
    }
    if !request["policy_version"].is_u64() {
        return Err(Error::invalid(
            "execution request policy_version must be a nonnegative integer",
        ));
    }
    for name in ["arguments", "resource_facts"] {
        if request.get(name).is_some_and(|value| !value.is_object()) {
            return Err(Error::invalid(format!(
                "execution request {name} must be an object"
            )));
        }
    }
    for fact in request["resource_facts"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(_, value)| value)
    {
        if !matches!(
            fact["origin"].as_str(),
            Some("tool_input" | "environment" | "connection" | "request" | "user" | "model")
        ) || fact.get("state").is_some_and(|value| {
            !matches!(value.as_str(), Some("known" | "unknown" | "not_applicable"))
        }) {
            return Err(Error::invalid("resource fact origin or state is invalid"));
        }
    }
    Ok(())
}
fn point(resource: &str) -> Option<&'static str> {
    match resource {
        "file" => Some("filesystem"),
        "network" => Some("connection"),
        "http" => Some("request"),
        "process" | "browser" | "mcp" | "control" => Some("tool"),
        _ => None,
    }
}
fn fact<'a>(request: &'a Value, name: &str) -> (Value, &'a str) {
    if let Some(fact) = request["resource_facts"].get(name) {
        return (
            fact["value"].clone(),
            fact["state"].as_str().unwrap_or("known"),
        );
    }
    let arguments = &request["arguments"];
    let url = arguments["url"]
        .as_str()
        .filter(|v| !v.is_empty())
        .or_else(|| arguments["target_url"].as_str());
    let derived = match name {
        "url" => url.map(|url| json!(url)),
        "scheme" | "host" | "hostname" | "port" | "path" | "raw_path" => url.and_then(|url| {
            reqwest::Url::parse(url).ok().map(|parsed| match name {
                "scheme" => json!(parsed.scheme()),
                "host" | "hostname" => json!(parsed.host_str()),
                "port" => json!(parsed.port_or_known_default()),
                _ => json!(domains::policy_raw_path(url)),
            })
        }),
        "arguments_ref" => request.get("arguments_hash").cloned(),
        "identity_ref"
        | "identity_revision"
        | "cwd"
        | "environment_id"
        | "environment_generation"
        | "tool_name"
        | "resource_kind"
        | "task_id"
        | "run_id"
        | "worker_id"
        | "arguments_hash" => request
            .get(name)
            .cloned()
            .filter(|value| !value.is_null() && value != ""),
        _ => arguments.get(name).cloned(),
    };
    match derived {
        Some(value) if !value.is_null() => (value, "known"),
        _ => (Value::Null, "unknown"),
    }
}
fn mapping(request: &Value, mapping: &Value) -> Api<(bool, Vec<String>)> {
    let Some(mapping) = mapping.as_object() else {
        return Ok((true, Vec::new()));
    };
    let mut unknown = Vec::new();
    for (name, condition) in mapping {
        let (actual, state) = fact(request, name);
        if state == "not_applicable" {
            return Ok((false, Vec::new()));
        }
        if state == "unknown" || actual.is_null() {
            unknown.push(name.clone());
            continue;
        }
        let ops = if condition.is_object() {
            condition.clone()
        } else {
            json!({"eq":condition})
        };
        let ops = ops.as_object().unwrap();
        if ops.is_empty() {
            return Ok((false, Vec::new()));
        }
        for (operator, expected) in ops {
            if !domains::policy_compare(name, &actual, operator, expected)? {
                return Ok((false, Vec::new()));
            }
        }
    }
    Ok((unknown.is_empty(), unknown))
}
fn target(request: &Value) -> String {
    for name in ["observed_url", "request_url", "page_url", "url"] {
        let (value, state) = fact(request, name);
        if state == "known" {
            if let Some(value) = value.as_str().filter(|value| !value.is_empty()) {
                return value.to_owned();
            }
        }
    }
    String::new()
}
/// Gateway inspection treats HTTP method names in `actions` as the transparent-proxy
/// action `request`. Keep the unified evaluator on that same contract.
fn actions_match(request: &Value, rule: &Value) -> bool {
    let Some(rows) = rule["actions"].as_array().filter(|rows| !rows.is_empty()) else {
        return true;
    };
    let action = request["action"].as_str().unwrap_or("");
    if rows.iter().any(|value| value == action) {
        return true;
    }
    if request["resource_kind"] == "http" && action == "request" {
        let (method, state) = fact(request, "method");
        if state == "known"
            && method
                .as_str()
                .is_some_and(|method| rows.iter().any(|value| value == method))
        {
            return true;
        }
    }
    false
}
pub(super) fn evaluate(policy: &Value, request: &Value) -> Api<Value> {
    let resource = request["resource_kind"].as_str().unwrap_or("");
    let mut matched = Vec::new();
    let mut observed = Vec::new();
    let mut unknown = BTreeSet::new();
    for rule in policy["rules"].as_array().into_iter().flatten() {
        let rule_point = rule["evaluation_point"].as_str().unwrap_or("tool");
        if rule["enabled"] == false
            || rule["resource_kind"] != resource
            || point(resource) != Some(rule_point)
            || point(rule["resource_kind"].as_str().unwrap_or("")) != Some(rule_point)
            || !actions_match(request, rule)
        {
            continue;
        }
        let mut missing = Vec::new();
        let mut no_match = false;
        for conditions in [&rule["selector"], &rule["conditions"]] {
            let (ok, fields) = mapping(request, conditions)?;
            if !ok && fields.is_empty() {
                no_match = true;
                break;
            }
            missing.extend(fields);
        }
        if no_match {
            continue;
        }
        let reference = format!(
            "{}:v{}",
            rule["rule_id"].as_str().unwrap_or("rule"),
            rule["version"].as_u64().unwrap_or(1)
        );
        if rule["mode"] == "observe" {
            observed.push(reference);
            continue;
        }
        if missing.is_empty() {
            matched.push((
                match rule["effect"].as_str() {
                    Some("allow") => "allow",
                    Some("ask") => "ask",
                    _ => "deny",
                },
                reference,
            ));
        } else {
            for field in missing {
                unknown.insert(format!(
                    "{}:{field}",
                    rule["rule_id"].as_str().unwrap_or("rule")
                ));
            }
        }
    }
    let refs = |effect: &str| {
        matched
            .iter()
            .filter(|(value, _)| *value == effect)
            .map(|(_, reference)| reference.clone())
            .collect::<Vec<_>>()
    };
    let asks = refs("ask");
    let denies = refs("deny");
    let allows = refs("allow");
    let target = target(request);
    let legacy_excluded = !target.is_empty()
        && policy["excluded_targets"].as_array().is_some_and(|rows| {
            rows.iter()
                .filter_map(Value::as_str)
                .any(|value| target_matches(&target, value))
        });
    let fine_excluded = policy["exclusions"].as_array().is_some_and(|rows| {
        rows.iter()
            .any(|rule| exclusion_rule_matches(rule, request, &target))
    });
    let excluded = legacy_excluded || fine_excluded;
    let authorized = if target.is_empty() {
        !matches!(resource, "http" | "network")
    } else {
        !excluded
            && policy["authorized_targets"].as_array().is_some_and(|rows| {
                rows.iter()
                    .filter_map(Value::as_str)
                    .any(|value| target_matches(&target, value))
            })
    };
    let (effect, status, reason, selected, unknown) = if !denies.is_empty() {
        ("deny", "denied", "policy_denied", denies, Vec::new())
    } else if excluded {
        ("deny", "denied", "policy_denied", Vec::new(), Vec::new())
    } else if !unknown.is_empty() {
        (
            "deny",
            "unresolved",
            "authorization_unresolved",
            matched
                .iter()
                .map(|(_, reference)| reference.clone())
                .collect(),
            unknown.into_iter().collect(),
        )
    } else if !authorized {
        (
            "deny",
            "unresolved",
            "authorization_unresolved",
            Vec::new(),
            vec!["authorized_target".to_owned()],
        )
    } else if !asks.is_empty() {
        (
            "ask",
            "waiting_approval",
            "approval_required",
            asks,
            Vec::new(),
        )
    } else if !allows.is_empty() {
        ("allow", "allowed", "allowed", allows, Vec::new())
    } else {
        match policy["defaults"]["effect"].as_str().unwrap_or("allow") {
            "ask" => (
                "ask",
                "waiting_approval",
                "approval_required",
                Vec::new(),
                Vec::new(),
            ),
            "allow" => ("allow", "allowed", "default_allow", Vec::new(), Vec::new()),
            _ => ("deny", "denied", "policy_denied", Vec::new(), Vec::new()),
        }
    };
    Ok(
        json!({"effect":effect,"status":status,"reason_code":reason,"matched_rule_refs":selected,"observed_rule_refs":observed,"unknown_fields":unknown}),
    )
}

pub(super) fn apply_grant(
    s: &AppState,
    id: &str,
    policy: &Value,
    request: &Value,
    decision: &mut Value,
) -> Api<()> {
    if decision["effect"] == "deny" || decision["status"] == "unresolved" {
        return Ok(());
    }
    let required = decision["matched_rule_refs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for row in records(s, id, "task_grant")? {
        let grant = &row["payload"];
        if grant["revoked_at"]
            .as_str()
            .is_some_and(|value| !value.is_empty())
            || grant["expires_at"].as_str().is_some_and(|value| {
                chrono::DateTime::parse_from_rfc3339(value)
                    .map_or(true, |value| value <= chrono::Utc::now())
            })
        {
            continue;
        }
        if !grant["rule_refs"]
            .as_array()
            .is_some_and(|refs| required.iter().all(|reference| refs.contains(reference)))
        {
            continue;
        }
        if grant["policy_digest"] != policy["digest"] {
            let snapshot = rec(
                s,
                id,
                "task_grant_environment",
                grant["grant_id"].as_str().unwrap_or(""),
            );
            let compatible = snapshot.is_ok_and(|row| {
                [
                    "environment_requirements",
                    "network_mode",
                    "inspection_requirements",
                ]
                .iter()
                .all(|field| row["payload"][*field] == policy[*field])
            });
            let same_rules = grant["rule_refs"].as_array().is_some_and(|refs| {
                refs.iter().all(|reference| {
                    policy["rules"].as_array().is_some_and(|rules| {
                        rules.iter().any(|rule| {
                            *reference
                                == json!(format!(
                                    "{}:v{}",
                                    rule["rule_id"].as_str().unwrap_or(""),
                                    rule["version"].as_u64().unwrap_or(1)
                                ))
                        })
                    })
                })
            });
            if !compatible || !same_rules {
                continue;
            }
        }
        if !grant["actions"]
            .as_array()
            .is_some_and(|rows| rows.contains(&request["action"]))
        {
            continue;
        }
        let mut matches = true;
        for field in [
            "resource_selector",
            "identity_constraints",
            "environment_constraints",
            "input_constraints",
        ] {
            let (ok, missing) = mapping(request, &grant[field])?;
            if !ok || !missing.is_empty() {
                matches = false;
                break;
            }
        }
        if matches {
            decision["effect"] = json!("allow");
            decision["status"] = json!("allowed");
            decision["reason_code"] = json!("task_grant");
            decision["grant_id"] = grant["grant_id"].clone();
            return Ok(());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Value {
        json!({
            "rules":[{
                "rule_id":"srule-post","version":1,"enabled":true,
                "evaluation_point":"request","resource_kind":"http","effect":"ask",
                "mode":"enforce","selector":{"method":{"eq":"POST"}},
                "conditions":{},"actions":["POST"],"reusable_approval":true
            }],
            "defaults":{"effect":"allow"},
            "authorized_targets":["http://host.docker.internal:4280/"],
            "excluded_targets":[]
        })
    }
    fn request(method: &str) -> Value {
        json!({
            "resource_kind":"http","action":"request","tool_name":"src.http.transparent_proxy",
            "arguments":{"url":"http://host.docker.internal:4280/login.php","method":method},
            "resource_facts":{
                "method":{"value":method,"origin":"request","state":"known"},
                "url":{"value":"http://host.docker.internal:4280/login.php","origin":"request","state":"known"}
            }
        })
    }

    #[test]
    fn transparent_proxy_http_method_action_requires_approval() {
        let decision = evaluate(&policy(), &request("POST")).unwrap();
        assert_eq!(decision["effect"], "ask");
        assert_eq!(decision["status"], "waiting_approval");
        assert_eq!(decision["matched_rule_refs"][0], "srule-post:v1");
        let other = evaluate(&policy(), &request("GET")).unwrap();
        assert_eq!(other["effect"], "allow");
        assert_eq!(other["reason_code"], "default_allow");
    }
}
