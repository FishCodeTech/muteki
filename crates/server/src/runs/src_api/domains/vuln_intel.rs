//! Deterministic SemVer vulnerability intelligence matching (zero substring/keyword guessing).

use crate::AppState;
use crate::runs::src_api::{Api, Error, digest, now, rec, records, require, save_rec, task};
use semver::{Version, VersionReq};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn parse_version_strict(raw: &str) -> Option<Version> {
    let cleaned = raw
        .trim()
        .trim_start_matches(['v', 'V', '=', '^', '~'])
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim();
    if cleaned.is_empty() {
        return None;
    }
    if let Ok(v) = Version::parse(cleaned) {
        return Some(v);
    }
    let parts = cleaned.split('.').collect::<Vec<_>>();
    if parts.len() == 2 && parts.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit())) {
        return Version::parse(&format!("{}.{}.0", parts[0], parts[1])).ok();
    }
    if parts.len() == 1 && parts[0].bytes().all(|b| b.is_ascii_digit()) {
        return Version::parse(&format!("{}.0.0", parts[0])).ok();
    }
    None
}

fn canonical_pkg(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

pub(super) fn advisory_import(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let _ = task(s, id)?;
    let items = body["advisories"]
        .as_array()
        .ok_or_else(|| Error::invalid("advisories array is required"))?;
    if items.len() > 2000 {
        return Err(Error::invalid("advisories batch exceeds 2000 limit"));
    }
    let mut imported = Vec::new();
    for item in items {
        let advisory_id = require(item, "advisory_id")?.trim();
        if advisory_id.is_empty() {
            return Err(Error::invalid("advisory_id cannot be empty"));
        }
        let ecosystem = item["ecosystem"]
            .as_str()
            .unwrap_or("npm")
            .trim()
            .to_ascii_lowercase();
        let package_name = canonical_pkg(require(item, "package_name")?);
        let ranges = item["affected_ranges"]
            .as_array()
            .filter(|a| !a.is_empty())
            .ok_or_else(|| Error::invalid(format!("advisory {advisory_id}: affected_ranges non-empty array required")))?;
        let mut validated_ranges = Vec::with_capacity(ranges.len());
        for r in ranges {
            let expr = r
                .as_str()
                .ok_or_else(|| Error::invalid("affected_ranges entry must be a SemVer requirement string"))?
                .trim();
            let _ = VersionReq::parse(expr).map_err(|err| {
                Error::invalid(format!("advisory {advisory_id}: invalid SemVer requirement '{expr}': {err}"))
            })?;
            validated_ranges.push(expr.to_owned());
        }
        let severity = match item["severity"]
            .as_str()
            .unwrap_or("medium")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "critical" => "critical",
            "high" => "high",
            "medium" => "medium",
            "low" => "low",
            "info" => "info",
            other => {
                return Err(Error::invalid(format!(
                    "advisory {advisory_id}: unsupported severity '{other}'"
                )))
            }
        };
        let title = item["title"]
            .as_str()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or(advisory_id);
        let cve_ids = item["cve_ids"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .collect::<Vec<_>>();
        let record_id = advisory_id.to_owned();
        let existing_rev = rec(s, id, "intel_advisory", &record_id)
            .ok()
            .and_then(|r| r["revision"].as_u64());
        let doc = json!({
            "advisory_id": advisory_id,
            "ecosystem": ecosystem,
            "package_name": package_name,
            "affected_ranges": validated_ranges,
            "severity": severity,
            "title": title,
            "cve_ids": cve_ids,
            "fix_version": item["fix_version"].as_str().unwrap_or(""),
            "reference_url": item["reference_url"].as_str().unwrap_or(""),
            "description": item["description"].as_str().unwrap_or(""),
            "imported_at": now(),
        });
        save_rec(s, id, "intel_advisory", &record_id, doc, existing_rev)?;
        imported.push(advisory_id.to_owned());
    }
    Ok(json!({
        "imported_count": imported.len(),
        "advisory_ids": imported,
    }))
}

pub(super) fn match_run(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let _ = task(s, id)?;
    let emit_suggestions = body["emit_suggestions"].as_bool().unwrap_or(true);
    let advisories = records(s, id, "intel_advisory")?;
    let advisory_count = advisories.len();
    let mut adv_by_pkg: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for row in advisories {
        let pkg = canonical_pkg(row["payload"]["package_name"].as_str().unwrap_or(""));
        if !pkg.is_empty() {
            adv_by_pkg.entry(pkg).or_default().push(row["payload"].clone());
        }
    }

    let mut observations = Vec::new();
    for row in records(s, id, "pkg_version_observation")? {
        observations.push(row["payload"].clone());
    }
    if let Some(extra) = body["observations"].as_array() {
        for item in extra {
            if let (Some(pkg), Some(ver)) = (item["package_name"].as_str(), item["version"].as_str()) {
                observations.push(json!({
                    "ecosystem": item["ecosystem"].as_str().unwrap_or("npm"),
                    "package_name": canonical_pkg(pkg),
                    "version": ver.trim(),
                    "source_ref": item["source_ref"].as_str().unwrap_or("manual"),
                }));
            }
        }
    }

    let observation_count = observations.len();
    let mut valid_semver_count = 0usize;
    let mut matches = Vec::new();
    let mut suggestion_ids = Vec::new();
    let mut seen_pairs = BTreeSet::new();

    for obs in observations {
        let pkg = canonical_pkg(obs["package_name"].as_str().unwrap_or(""));
        let eco = obs["ecosystem"].as_str().unwrap_or("npm").to_ascii_lowercase();
        let raw_ver = obs["version"].as_str().unwrap_or("");
        let Some(parsed_ver) = parse_version_strict(raw_ver) else {
            continue;
        };
        valid_semver_count += 1;
        let Some(pkg_advs) = adv_by_pkg.get(&pkg) else {
            continue;
        };
        for adv in pkg_advs {
            let adv_eco = adv["ecosystem"].as_str().unwrap_or("npm").to_ascii_lowercase();
            if adv_eco != eco && !adv_eco.is_empty() && !eco.is_empty() {
                continue;
            }
            let matched_range = adv["affected_ranges"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .find(|expr| VersionReq::parse(expr).is_ok_and(|req| req.matches(&parsed_ver)));
            let Some(range) = matched_range else {
                continue;
            };
            let adv_id = adv["advisory_id"].as_str().unwrap_or("");
            let match_key = format!("intel_{}", &digest(&json!([adv_id, eco, pkg, parsed_ver.to_string()]))[..24]);
            if !seen_pairs.insert(match_key.clone()) {
                continue;
            }
            let match_doc = json!({
                "match_id": match_key,
                "advisory_id": adv_id,
                "ecosystem": eco,
                "package_name": pkg,
                "observed_version_raw": raw_ver,
                "observed_semver": parsed_ver.to_string(),
                "matched_range": range,
                "severity": adv["severity"],
                "title": adv["title"],
                "cve_ids": adv["cve_ids"],
                "fix_version": adv["fix_version"],
                "reference_url": adv["reference_url"],
                "source_ref": obs["source_ref"],
                "asset_id": obs["asset_id"],
                "matched_at": now(),
            });
            let rev = rec(s, id, "intel_match", &match_key)
                .ok()
                .and_then(|r| r["revision"].as_u64());
            save_rec(s, id, "intel_match", &match_key, match_doc.clone(), rev)?;
            matches.push(match_doc);

            if emit_suggestions {
                let sug_id = format!("ssug_intel_{}", &digest(&json!([id, match_key]))[..18]);
                let sug_rev = rec(s, id, "workbench_suggestion", &sug_id)
                    .ok()
                    .and_then(|r| r["revision"].as_u64());
                let summary = format!(
                    "{} — 观测版本 `{parsed_ver}` 严格满足受影响 SemVer 区间 `{range}`{}",
                    adv["title"].as_str().unwrap_or(adv_id),
                    adv["fix_version"]
                        .as_str()
                        .filter(|v| !v.is_empty())
                        .map(|f| format!("，建议升级或核对 >= {f}"))
                        .unwrap_or_default()
                );
                let stamped = now();
                let sug = json!({
                    "suggestion_id": sug_id,
                    "task_id": id,
                    "kind": "intel_match",
                    "status": "pending",
                    "decision": "pending",
                    "decided_by": null,
                    "decided_at": null,
                    "applied_ref": null,
                    "proposed_by": "system:intel",
                    "proposed_at": stamped,
                    "title": format!("组件漏洞情报命中: {pkg}@{parsed_ver} ({adv_id})"),
                    "summary": summary,
                    "rationale": summary,
                    "proposal": {
                        "match_id": match_key,
                        "advisory_id": adv_id,
                        "package_name": pkg,
                        "observed_semver": parsed_ver.to_string(),
                        "matched_range": range,
                    },
                    "severity": adv["severity"],
                    "match_id": match_key,
                    "advisory_id": adv_id,
                    "package_name": pkg,
                    "observed_semver": parsed_ver.to_string(),
                    "matched_range": range,
                    "cve_ids": adv["cve_ids"],
                    "source_ref": obs["source_ref"],
                    "updated_at": stamped,
                });
                save_rec(s, id, "workbench_suggestion", &sug_id, sug, sug_rev)?;
                suggestion_ids.push(sug_id);
            }
        }
    }

    let matched_count = matches.len();
    let status_label = intel_status_label(
        observation_count,
        advisory_count,
        valid_semver_count,
        matched_count,
    );
    let last_run = now();
    let status_snapshot = json!({
        "observation_count": observation_count,
        "valid_semver_count": valid_semver_count,
        "advisory_count": advisory_count,
        "matched_count": matched_count,
        "status": status_label,
        "last_run": last_run,
    });
    let status_rev = rec(s, id, "intel_status", "latest")
        .ok()
        .and_then(|r| r["revision"].as_u64());
    save_rec(s, id, "intel_status", "latest", status_snapshot, status_rev)?;

    Ok(json!({
        "match_count": matched_count,
        "matched_count": matched_count,
        "observation_count": observation_count,
        "valid_semver_count": valid_semver_count,
        "advisory_count": advisory_count,
        "status": status_label,
        "last_run": last_run,
        "matches": matches,
        "suggestion_ids": suggestion_ids,
    }))
}

fn intel_status_label(
    observation_count: usize,
    advisory_count: usize,
    valid_semver_count: usize,
    matched_count: usize,
) -> &'static str {
    if observation_count == 0 {
        "no_observations"
    } else if advisory_count == 0 {
        "no_advisories"
    } else if valid_semver_count == 0 {
        "no_valid_semver"
    } else if matched_count == 0 {
        "matched_zero"
    } else {
        "matched"
    }
}

pub(super) fn status(s: &AppState, id: &str) -> Api<Value> {
    let _ = task(s, id)?;
    let advisories = records(s, id, "intel_advisory")?;
    let matches = records(s, id, "intel_match")?;
    let observations = records(s, id, "pkg_version_observation")?;
    let saved = rec(s, id, "intel_status", "latest")
        .ok()
        .map(|r| r["payload"].clone());

    let observation_count = saved
        .as_ref()
        .and_then(|v| v["observation_count"].as_u64())
        .map(|v| v as usize)
        .unwrap_or(observations.len());
    let valid_semver_count = saved
        .as_ref()
        .and_then(|v| v["valid_semver_count"].as_u64())
        .map(|v| v as usize)
        .unwrap_or_else(|| {
            observations
                .iter()
                .filter(|r| {
                    r["payload"]["version"]
                        .as_str()
                        .and_then(parse_version_strict)
                        .is_some()
                })
                .count()
        });
    let advisory_count = advisories.len();
    let matched_count = matches.len();
    let status_label = intel_status_label(
        observation_count,
        advisory_count,
        valid_semver_count,
        matched_count,
    );
    let last_run = saved
        .as_ref()
        .and_then(|v| v.get("last_run"))
        .cloned()
        .unwrap_or(Value::Null);

    Ok(json!({
        "observation_count": observation_count,
        "valid_semver_count": valid_semver_count,
        "advisory_count": advisory_count,
        "matched_count": matched_count,
        "status": status_label,
        "last_run": last_run,
    }))
}

pub(super) fn list(s: &AppState, id: &str) -> Api<Value> {
    let _ = task(s, id)?;
    let advisories = records(s, id, "intel_advisory")?
        .into_iter()
        .map(|r| r["payload"].clone())
        .collect::<Vec<_>>();
    let matches = records(s, id, "intel_match")?
        .into_iter()
        .map(|r| r["payload"].clone())
        .collect::<Vec<_>>();
    let observations = records(s, id, "pkg_version_observation")?
        .into_iter()
        .map(|r| r["payload"].clone())
        .collect::<Vec<_>>();
    Ok(json!({
        "advisories": advisories,
        "matches": matches,
        "observations": observations,
    }))
}
