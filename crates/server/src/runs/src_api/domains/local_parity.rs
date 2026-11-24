//! Local SRC research, asset and workbench operations from the source domain.
use super::{
    Api, records, report, require,
};
use crate::AppState;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn versions(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let aid = require(body, "asset_id")?;
    let mut rows = records(s, id, "asset_version")?
        .into_iter()
        .filter(|v| v["payload"]["asset_id"] == aid)
        .map(|v| v["payload"].clone())
        .collect::<Vec<_>>();
    rows.sort_by_key(|v| v["version"].as_u64().unwrap_or(0));
    Ok(json!({"items":rows}))
}
pub(super) fn duplicates(s: &AppState, id: &str) -> Api<Value> {
    let mut buckets: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for row in records(s, id, "asset")? {
        let asset = &row["payload"];
        if !asset["lifecycle"].is_null() && asset["lifecycle"] != "active" {
            continue;
        }
        let Some(primary) = asset["identity"]
            .as_object()
            .into_iter()
            .flat_map(|v| v.values())
            .find_map(|v| v.as_str().map(str::trim).filter(|v| !v.is_empty()))
        else {
            continue;
        };
        let key = format!(
            "{}:{}",
            asset["type"].as_str().unwrap_or("unknown"),
            primary.to_lowercase()
        );
        if let Some(aid) = asset["asset_id"].as_str() {
            buckets.entry(key).or_default().insert(aid.to_owned());
        }
    }
    let groups = buckets
        .into_iter()
        .filter(|(_, ids)| ids.len() > 1)
        .map(|(key, ids)| {
            let count = ids.len();
            let identity = key.split_once(':').map(|(_, v)| v).unwrap_or("");
            json!({"key":key,"asset_ids":ids.into_iter().collect::<Vec<_>>(),
            "reasons":[format!("同一资产类型下标识值「{identity}」出现 {count} 条记录")]})
        })
        .collect::<Vec<_>>();
    Ok(json!({"groups":groups}))
}
pub(super) fn report_diff(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let from = require(body, "from_report_id")?;
    let to = require(body, "to_report_id")?;
    let a = report(s, id, from)?;
    let b = report(s, id, to)?;
    let mut keys = BTreeSet::new();
    if let Some(v) = a["document"].as_object() {
        keys.extend(v.keys().cloned())
    }
    if let Some(v) = b["document"].as_object() {
        keys.extend(v.keys().cloned())
    }
    let mut fields = keys
        .into_iter()
        .map(|key| {
            json!({"field":key,"changed":a["document"][&key]!=b["document"][&key],
        "before":a["document"][&key],"after":b["document"][&key]})
        })
        .collect::<Vec<_>>();
    for key in ["template", "cvss", "deduplication", "target_version"] {
        if a[key] != b[key] {
            fields.push(json!({"field":key,"changed":true,"before":a[key],"after":b[key]}));
        }
    }
    let changed = fields.iter().filter(|v| v["changed"] == true).count();
    Ok(
        json!({"from":from,"to":to,"from_version":a["version"],"to_version":b["version"],
        "fields":fields,"changed_count":changed}),
    )
}
