//! Ingestion keeps source bytes and the outcome of every source row.
use super::*;
use base64::Engine;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::net::IpAddr;

fn first<'a>(value: &'a Value, names: &[&str]) -> Option<&'a Value> {
    names
        .iter()
        .find_map(|key| value.get(*key).filter(|v| !v.is_null() && *v != ""))
}

fn item_error(index: usize, error: Error) -> Value {
    json!({"index":index,"ok":false,"code":error.code,"message":error.message,
        "detail":error.detail,"recovery":"correct this item and retry only failed items"})
}

fn update_outcomes(batch: &mut Value, mut items: Vec<Value>) {
    items.sort_by_key(|item| item["index"].as_u64().unwrap_or(0));
    let mut counts = json!({"processed":items.len(),"created":0,"merged":0,
        "updated":0,"skipped":0,"failed":0,"truncated":0});
    let mut created = Vec::new();
    let mut failures = Vec::new();
    for item in &items {
        let action = if item["ok"] == true {
            item["action"].as_str().unwrap_or("")
        } else {
            "failed"
        };
        if let Some(count) = counts.get_mut(action) {
            *count = json!(count.as_u64().unwrap_or(0) + 1);
        }
        if action == "failed" {
            failures.push(item.clone());
        }
        if action == "created" && item["asset_id"].is_string() {
            created.push(item["asset_id"].clone());
        }
    }
    batch["status"] = json!(if items.is_empty() || failures.len() == items.len() {
        "failed"
    } else if failures.is_empty() {
        "completed"
    } else {
        "partial"
    });
    batch["total"] = json!(items.len());
    batch["imported"] = json!(items.len() - failures.len());
    batch["failed"] = json!(failures.len());
    batch["created_asset_ids"] = json!(created);
    batch["failed_items"] = json!(failures);
    batch["counts"] = counts;
    batch["items"] = json!(items);
}

fn commit_item(
    s: &AppState,
    task_id: &str,
    batch: &Value,
    index: usize,
    row: &Value,
) -> Api<Value> {
    let batch_id = require(batch, "batch_id")?;
    let raw = &row["raw"];
    let raw_http = row["attributes"]["raw_http"] == true;
    let captured = raw_http
        || row["attributes"]["har_derived"] == true
        || row["attributes"]["burp_xml"] == true;
    let artifact = if raw_http || row["attributes"]["burp_xml"] == true {
        batch["original_artifact"].clone()
    } else {
        put_artifact(
            s,
            task_id,
            &serde_json::to_vec(raw).map_err(db)?,
            &format!("{batch_id}-{index}.json"),
            "application/json",
        )?
    };
    if matches!(
        row["asset_type"].as_str(),
        Some("finding" | "vulnerability")
    ) {
        return Ok(json!({"index":index,"ok":true,"action":"skipped",
            "reason":"external finding is evidence, not a local Finding","artifact":artifact}));
    }
    let source_event = if captured {
        json!(format!("{batch_id}:{index}"))
    } else {
        first(raw, &["uuid", "event_id", "id"])
            .cloned()
            .map(|v| {
                if v.is_string() {
                    v
                } else {
                    json!(v.to_string())
                }
            })
            .unwrap_or_else(|| json!(format!("{batch_id}:{index}")))
    };
    let observed_at = if captured {
        first(raw, &["startedDateTime"])
            .cloned()
            .unwrap_or_else(|| batch["source_metadata"]["captured_at"].clone())
    } else {
        first(raw, &["observed_at", "timestamp", "time", "seen_at"])
            .cloned()
            .unwrap_or_else(|| batch["observed_at"].clone())
    };
    let data = json!({"asset_type":row["asset_type"],"identity":row["identity"],
        "source_kind":if captured {json!("import")} else {batch["source_kind"].clone()},
        "source_id":batch["source_id"],"source_event_id":source_event,
        "attributes":row["attributes"],"raw_values":raw,"manual":false,
        "artifact_refs":[artifact],"observed_at":observed_at,
        "source_run_id":raw["run_id"],"source_event_seq":first(raw,&["seq","event_seq"]),
        "parser_version":"src-ingestion.v1","success":raw["success"].as_bool(),
        "completeness":if captured || batch["scope"]["has_next_page"] == true {"partial"}else{"complete"}});
    let result = observe_asset(s, task_id, &data, None)?;
    let exchange_id = record_import_exchange(s, task_id, batch_id, index, row)?;
    let mut output = json!({"index":index,"ok":true,"action":result["action"],
        "asset_id":result["asset"]["asset_id"],"observation_id":result["observation"]["observation_id"],
        "source_event_id":source_event,"exchange_id":exchange_id});
    if captured {
        output["live_replay"] = json!(false);
    } else {
        let known: BTreeSet<&str> = row["identity"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, _)| k.as_str())
            .chain(
                row["attributes"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(k, _)| k.as_str()),
            )
            .chain([
                "identity",
                "attributes",
                "type",
                "asset_type",
                "observed_at",
                "timestamp",
                "time",
                "seen_at",
                "success",
                "run_id",
                "seq",
                "event_seq",
                "uuid",
                "event_id",
                "id",
                "parent_uuid",
                "parent_id",
                "parent",
            ])
            .collect();
        let mut unknown: Vec<&str> = raw
            .as_object()
            .into_iter()
            .flatten()
            .map(|(key, _)| key.as_str())
            .filter(|key| !known.contains(key))
            .collect();
        unknown.sort_unstable();
        output["unknown_fields"] = json!(unknown);
        output["parent_source_event_id"] = first(raw, &["parent_uuid", "parent_id", "parent"])
            .cloned()
            .unwrap_or(Value::Null);
    }
    if raw_http {
        let url = require(&row["identity"], "url")?;
        let parsed =
            reqwest::Url::parse(url).map_err(|_| Error::invalid("invalid imported HTTP URL"))?;
        let mut parameters = Vec::new();
        for (position, (name, value)) in parsed.query_pairs().enumerate() {
            let parameter = observe_asset(
                s,
                task_id,
                &json!({"asset_type":"parameter",
                "identity":{"url":url,"method":row["identity"]["method"],"location":"query","name":name},
                "source_kind":"import","source_id":batch["source_id"],
                "attributes":{"example":value,"original_name":name,"occurrence":position},
                "raw_values":{"name":name,"value":value,"location":"query","occurrence":position},
                "observed_at":observed_at,"artifact_refs":[artifact],"parser_version":"src-ingestion.v1",
                "source_event_id":format!("{batch_id}:{index}:parameter:query:{position}:{name}")}),
                None,
            )?;
            parameters.push(parameter["asset"]["asset_id"].clone());
        }
        output["parameter_asset_ids"] = json!(parameters);
    }
    Ok(output)
}

fn commit_items(
    s: &AppState,
    task_id: &str,
    batch: &Value,
    parsed: import_formats::Parsed,
    only: Option<&BTreeSet<u64>>,
) -> Vec<Value> {
    let mut items: Vec<Value> = parsed
        .failures
        .into_iter()
        .filter(|v| only.is_none_or(|indexes| indexes.contains(&v["index"].as_u64().unwrap_or(0))))
        .map(|mut item| {
            item["ok"] = json!(false);
            item["recovery"] = json!("fix this row and retry failed items");
            item
        })
        .collect();
    for (index, row) in parsed.rows {
        if only.is_some_and(|indexes| !indexes.contains(&(index as u64))) {
            continue;
        }
        items.push(
            s.store.document_scope(||commit_item(s, task_id, batch, index, &row)).map_err(db).and_then(|result|result)
                .unwrap_or_else(|error| item_error(index, error)),
        );
    }
    items
}

pub(super) fn ingest(s: &AppState, task_id: &str, body: &Value) -> Api<Value> {
    let source_kind = body["source_kind"]
        .as_str()
        .or_else(|| body["connector_id"].as_str())
        .unwrap_or("json")
        .to_lowercase();
    let encoded = body["content_b64"]
        .as_str()
        .ok_or_else(|| Error::invalid("content_b64 is required"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| Error::invalid("invalid base64 import content"))?;
    let batch_id = format!("sbatch-{}", Uuid::new_v4().simple());
    let filename = body["source_metadata"]["filename"]
        .as_str()
        .or_else(|| body["filename"].as_str())
        .unwrap_or("import.json");
    let parsed = import_formats::parse_import(
        &bytes,
        &source_kind,
        filename,
        &body["source_metadata"],
        &body["field_mapping"],body["media_type"].as_str().unwrap_or(""),body["encoding"].as_str().unwrap_or(""),
    );
    let media_type = body["media_type"]
        .as_str()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            parsed
                .as_ref()
                .map(|p| p.media_type)
                .unwrap_or("application/octet-stream")
        });
    let artifact = put_artifact(s, task_id, &bytes, filename, media_type)?;
    let mut batch = json!({"batch_id":batch_id,"task_id":task_id,"source_kind":source_kind,
        "source_id":body["source_id"].as_str().filter(|v|!v.is_empty()).unwrap_or(&batch_id),
        "connector_id":body["connector_id"],"media_type":media_type,"encoding":body["encoding"].as_str().unwrap_or(""),"source_metadata":body["source_metadata"].as_object().cloned().unwrap_or_default(),
        "field_mapping":body["field_mapping"].as_object().cloned().unwrap_or_default(),"mapping_version":"src-ingestion.v1",
        "idempotency_key":body["idempotency_key"],"observed_at":body["observed_at"],
        "original_artifact":artifact,"source_artifact":artifact,"created_at":now(),"status":"processing",
        "cost":body.get("cost").filter(|v|v.is_object()).cloned().unwrap_or_else(||json!({"state":"not_applicable"})),
        "scope":{"query":body["source_metadata"]["query"],"page":body["source_metadata"]["page"],
            "cursor":body["source_metadata"]["cursor"],"upstream_limit":body["source_metadata"]["upstream_limit"],
            "has_next_page":body["source_metadata"]["has_next_page"],
            "completeness":body["source_metadata"]["completeness"].as_str().unwrap_or("unknown")}});
    save_rec(
        s,
        task_id,
        "import_batch",
        &batch_id,
        batch.clone(),
        Some(0),
    )?;
    match parsed {
        Ok(parsed) => {
            let items = commit_items(s, task_id, &batch, parsed, None);
            update_outcomes(&mut batch, items);
            batch["completed_at"] = json!(now());
        }
        Err(error) => {
            update_outcomes(&mut batch, Vec::new());
            batch["error"] =
                json!({"code":error.code,"message":error.message,"detail":error.detail});
        }
    }
    save_rec(s, task_id, "import_batch", &batch_id, batch.clone(), None)?;
    Ok(batch)
}

pub(super) fn retry(s: &AppState, task_id: &str, body: &Value) -> Api<Value> {
    let batch_id = require(body, "batch_id")?;
    let record = rec(s, task_id, "import_batch", batch_id)
        ?;
    let mut batch = record["payload"].clone();
    if batch["reverted_at"].is_string() {
        return Err(Error::conflict("cannot retry reverted batch"));
    }
    if batch["cost"]["state"] == "unknown" {
        batch["status"] = json!("external_unknown");
        batch["recovery"] = json!("reconcile the upstream receipt before any retry");
    } else {
        let mut previous: BTreeMap<u64, Value> = batch["items"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(batch["failed_items"].as_array().into_iter().flatten())
            .map(|item| (item["index"].as_u64().unwrap_or(0), item.clone()))
            .collect();
        let failed: BTreeSet<u64> = previous
            .iter()
            .filter(|(_, item)| item["ok"] != true)
            .map(|(i, _)| *i)
            .collect();
        let artifact = if batch["original_artifact"].is_object() {
            &batch["original_artifact"]
        } else {
            &batch["source_artifact"]
        };
        let verified = verify_ref(s, task_id, artifact)?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(require(&verified, "data")?)
            .map_err(|_| Error::invalid("stored import content is not valid base64"))?;
        let parsed = import_formats::parse_import(
            &bytes,
            batch["source_kind"].as_str().unwrap_or("json"),
            batch["source_metadata"]["filename"]
                .as_str()
                .or_else(|| artifact["name"].as_str())
                .unwrap_or("import.json"),
            &batch["source_metadata"],
            &batch["field_mapping"],batch["media_type"].as_str().unwrap_or(""),batch["encoding"].as_str().unwrap_or(""),
        )?;
        for item in commit_items(s, task_id, &batch, parsed, Some(&failed)) {
            previous.insert(item["index"].as_u64().unwrap_or(0), item);
        }
        update_outcomes(&mut batch, previous.into_values().collect());
        batch["last_retry_at"] = json!(now());
    }
    save_rec(
        s,
        task_id,
        record["kind"].as_str().unwrap_or("import_batch"),
        record["natural_key"]
            .as_str()
            .filter(|v| !v.is_empty())
            .unwrap_or(batch_id),
        batch.clone(),
        record["revision"].as_u64(),
    )?;
    Ok(batch)
}

fn candidate_from_connector_item(
    connector_id: &str,
    item: &Value,
    field_mapping: &Value,
) -> Option<(String, Value, Value)> {
    let mapped = if item["mapped"].is_object() {
        &item["mapped"]
    } else {
        item
    };
    let explicit_type = item["asset_type"]
        .as_str()
        .or_else(|| mapped["asset_type"].as_str())
        .or_else(|| mapped["type"].as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty());

    if let Some(at) = explicit_type
        && item["identity"].is_object()
    {
        let attrs = item["attributes"]
            .as_object()
            .cloned()
            .map(Value::Object)
            .unwrap_or_else(|| json!({}));
        return Some((at.to_owned(), item["identity"].clone(), attrs));
    }
    if let Some(at) = explicit_type
        && mapped["identity"].is_object()
    {
        let attrs = mapped["attributes"]
            .as_object()
            .cloned()
            .map(Value::Object)
            .unwrap_or_else(|| json!({}));
        return Some((at.to_owned(), mapped["identity"].clone(), attrs));
    }
    if mapped["type"] == "repository" || mapped["type"] == "code_repository" {
        let platform = mapped["provider"]
            .as_str()
            .or_else(|| mapped["platform"].as_str())
            .unwrap_or(connector_id);
        let stable_id = mapped["full_name"]
            .as_str()
            .or_else(|| mapped["repository_id"].as_str())
            .unwrap_or("");
        if !stable_id.is_empty() {
            return Some((
                "code_repository".to_owned(),
                json!({"platform": platform, "stable_id": stable_id}),
                mapped.clone(),
            ));
        }
    }
    if mapped["type"] == "cloud_resource" || mapped["type"] == "cloud" {
        if let (Some(provider), Some(resource_id)) = (
            mapped["provider"].as_str().filter(|v| !v.is_empty()),
            mapped["resource_id"].as_str().filter(|v| !v.is_empty()),
        ) {
            return Some((
                "cloud_resource".to_owned(),
                json!({
                    "provider": provider,
                    "resource_id": resource_id,
                    "tenant": mapped["tenant"].as_str().unwrap_or(""),
                    "region": mapped["region"].as_str().unwrap_or("")
                }),
                mapped.clone(),
            ));
        }
    }
    if let Some(url_str) = mapped["url"]
        .as_str()
        .or_else(|| {
            mapped["host"]
                .as_str()
                .filter(|h| h.starts_with("http://") || h.starts_with("https://"))
        })
        .or_else(|| {
            mapped["target_ref"]
                .as_str()
                .filter(|h| h.starts_with("http://") || h.starts_with("https://"))
        })
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        return Some((
            "web_origin".to_owned(),
            json!({"url": url_str}),
            mapped.clone(),
        ));
    }

    let raw_ip = mapped["ip"]
        .as_str()
        .or_else(|| mapped["ip_str"].as_str())
        .or_else(|| mapped["address"].as_str())
        .map(str::trim)
        .filter(|v| v.parse::<IpAddr>().is_ok());
    let raw_domain = mapped["domain"]
        .as_str()
        .or_else(|| mapped["site"].as_str())
        .or_else(|| {
            mapped["domains"]
                .as_array()
                .and_then(|arr| arr.first())
                .and_then(Value::as_str)
        })
        .or_else(|| {
            mapped["host"]
                .as_str()
                .map(|h| h.split(':').next().unwrap_or(h))
                .filter(|h| !h.is_empty() && h.parse::<IpAddr>().is_err() && !h.contains('/'))
        })
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let raw_port = mapped["port"]
        .as_u64()
        .or_else(|| {
            mapped["port"]
                .as_str()
                .and_then(|v| v.trim().parse::<u64>().ok())
        })
        .or_else(|| mapped["portinfo"]["port"].as_u64())
        .or_else(|| {
            mapped["host"]
                .as_str()
                .and_then(|h| h.rsplit_once(':'))
                .and_then(|(_, p)| p.parse::<u64>().ok())
        })
        .filter(|p| (1..=65535).contains(p));

    if let (Some(port), Some(host)) = (raw_port, raw_domain.or(raw_ip)) {
        let protocol = mapped["protocol"]
            .as_str()
            .or_else(|| mapped["service"].as_str())
            .or_else(|| mapped["portinfo"]["service"].as_str())
            .unwrap_or("tcp");
        return Some((
            "port_service".to_owned(),
            json!({
                "host": host,
                "port": port,
                "protocol": protocol,
                "transport": mapped["transport"].as_str().unwrap_or("tcp")
            }),
            mapped.clone(),
        ));
    }
    if let Some(domain) = raw_domain {
        return Some((
            "domain".to_owned(),
            json!({"name": domain}),
            mapped.clone(),
        ));
    }
    if let Some(ip) = raw_ip {
        return Some((
            "ip".to_owned(),
            json!({"address": ip}),
            mapped.clone(),
        ));
    }
    if let Some(target_ref) = mapped["target_ref"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        if target_ref.parse::<IpAddr>().is_ok() {
            return Some((
                "ip".to_owned(),
                json!({"address": target_ref}),
                mapped.clone(),
            ));
        }
        if !target_ref.contains(['/', ':', ' ']) && target_ref.contains('.') {
            return Some((
                "domain".to_owned(),
                json!({"name": target_ref}),
                mapped.clone(),
            ));
        }
    }
    let normalized = import_formats::normalize(
        if item["raw"].is_object() {
            &item["raw"]
        } else {
            mapped
        },
        field_mapping,
    );
    let norm_type = normalized["asset_type"].as_str().unwrap_or("").trim();
    if !norm_type.is_empty() && normalized["identity"].is_object() {
        return Some((
            norm_type.to_owned(),
            normalized["identity"].clone(),
            normalized["attributes"].clone(),
        ));
    }
    None
}

pub(super) fn ingest_connector_batch_observations(
    s: &AppState,
    task_id: &str,
    batch: &Value,
    command_id: Option<&str>,
) -> Api<Value> {
    let _ = command_id;
    let batch_id = batch["batch_id"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::invalid("connector batch_id is required"))?;
    let connector_id = batch["connector_id"].as_str().unwrap_or("connector");
    let items = batch["items"].as_array().cloned().unwrap_or_default();
    if batch["external_state"] == "failed" || items.is_empty() {
        return Ok(json!({
            "batch_id": batch_id,
            "connector_id": connector_id,
            "counts": {
                "created_assets": 0,
                "merged_assets": 0,
                "observations": 0,
                "skipped": 0
            },
            "asset_ids": [],
            "observation_ids": []
        }));
    }

    let valid_artifacts: Vec<Value> = batch["source_refs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| verify_ref(s, task_id, r).is_ok())
        .cloned()
        .collect();
    let field_mapping = &batch["field_mapping"];
    let mut created_assets: usize = 0;
    let mut merged_assets: usize = 0;
    let mut observations: usize = 0;
    let mut skipped: usize = 0;
    let mut asset_ids: Vec<String> = Vec::new();
    let mut observation_ids: Vec<String> = Vec::new();

    for (index, item) in items.iter().enumerate() {
        let Some((asset_type, identity, attrs)) =
            candidate_from_connector_item(connector_id, item, field_mapping)
        else {
            skipped += 1;
            continue;
        };
        if matches!(asset_type.as_str(), "finding" | "vulnerability") {
            skipped += 1;
            continue;
        }
        let source_event_id = item["input_key"]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(|k| format!("{batch_id}:{index}:{k}"))
            .unwrap_or_else(|| format!("{batch_id}:{index}"));
        let obs_body = json!({
            "asset_type": asset_type,
            "identity": identity,
            "source_kind": connector_id,
            "source_id": batch_id,
            "source_event_id": source_event_id,
            "attributes": attrs,
            "raw_values": if item["raw"].is_object() { item["raw"].clone() } else { item.clone() },
            "manual": false,
            "artifact_refs": valid_artifacts,
            "observed_at": batch["created_at"],
            "parser_version": "src-connector-ingestion.v1",
            "completeness": if batch["acquisition"]["truncated"] == true { "partial" } else { "complete" },
            "active_operation_authorized": false,
            "observation_role": "candidate",
            "source_batch_id": batch_id
        });
        match observe_asset(s, task_id, &obs_body, None) {
            Ok(res) => {
                if let Some(aid) = res["asset"]["asset_id"].as_str()
                    && !asset_ids.iter().any(|v| v == aid)
                {
                    asset_ids.push(aid.to_owned());
                }
                if let Some(oid) = res["observation"]["observation_id"].as_str() {
                    observation_ids.push(oid.to_owned());
                }
                match res["action"].as_str() {
                    Some("created") => {
                        created_assets += 1;
                        observations += 1;
                    }
                    Some("merged") => {
                        merged_assets += 1;
                        observations += 1;
                    }
                    Some("deduplicated") => {}
                    _ => {
                        observations += 1;
                    }
                }
            }
            Err(_) => {
                skipped += 1;
            }
        }
    }

    Ok(json!({
        "batch_id": batch_id,
        "connector_id": connector_id,
        "counts": {
            "created_assets": created_assets,
            "merged_assets": merged_assets,
            "observations": observations,
            "skipped": skipped
        },
        "asset_ids": asset_ids,
        "observation_ids": observation_ids
    }))
}

pub(super) fn preview(s: &AppState, task_id: &str, body: &Value) -> Api<Value> {
    let bytes = if let Some(b64) = body["content_b64"]
        .as_str()
        .or_else(|| body["data_b64"].as_str())
        .filter(|v| !v.is_empty())
    {
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|_| Error::invalid("invalid base64 preview content"))?
    } else if let Some(raw) = body["content"]
        .as_str()
        .or_else(|| body["data"].as_str())
        .filter(|v| !v.is_empty())
    {
        raw.as_bytes().to_vec()
    } else {
        return Err(Error::invalid(
            "content, data, content_b64, or data_b64 is required",
        ));
    };

    let filename = body["source_metadata"]["filename"]
        .as_str()
        .or_else(|| body["filename"].as_str())
        .unwrap_or("preview.json");
    let raw_kind = body["source_kind"]
        .as_str()
        .or_else(|| body["connector_id"].as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("");
    let inferred_kind = if raw_kind.is_empty() || raw_kind.eq_ignore_ascii_case("auto") {
        let lower_name = filename.to_ascii_lowercase();
        let trimmed = std::str::from_utf8(&bytes).unwrap_or("").trim_start();
        if lower_name.ends_with(".har") {
            "har".to_owned()
        } else if lower_name.ends_with(".xml") || lower_name.ends_with(".burp") || trimmed.starts_with('<') {
            "burp_xml".to_owned()
        } else if lower_name.ends_with(".csv") {
            "csv".to_owned()
        } else if lower_name.ends_with(".jsonl") || lower_name.ends_with(".ndjson") {
            "jsonl".to_owned()
        } else if lower_name.ends_with(".http") || lower_name.ends_with(".request") {
            "raw_http".to_owned()
        } else if serde_json::from_slice::<Value>(&bytes).is_ok() {
            "json".to_owned()
        } else if trimmed.lines().count() > 1
            && trimmed
                .lines()
                .filter(|l| !l.trim().is_empty())
                .any(|l| serde_json::from_str::<Value>(l).is_ok())
        {
            "jsonl".to_owned()
        } else {
            "json".to_owned()
        }
    } else {
        raw_kind.to_ascii_lowercase()
    };

    let parsed = import_formats::parse_import(
        &bytes,
        &inferred_kind,
        filename,
        &body["source_metadata"],
        &body["field_mapping"],
        body["media_type"].as_str().unwrap_or(""),
        body["encoding"].as_str().unwrap_or(""),
    )?;

    let format_name = if inferred_kind == "burp_xml" || parsed.media_type == "application/xml" {
        "burp_xml"
    } else if parsed.media_type == "message/http" {
        "raw_http"
    } else if parsed.media_type == "text/csv" {
        "csv"
    } else if parsed.media_type == "application/x-ndjson" {
        "jsonl"
    } else if inferred_kind == "har" {
        "har"
    } else if inferred_kind.is_empty() {
        "json"
    } else {
        inferred_kind.as_str()
    };

    let existing_assets = records(s, task_id, "asset")?;
    let mut existing_by_key = BTreeMap::<String, String>::new();
    for row in &existing_assets {
        let p = &row["payload"];
        if let (Some(nk), Some(aid)) = (p["natural_key"].as_str(), p["asset_id"].as_str()) {
            existing_by_key.insert(nk.to_owned(), aid.to_owned());
        }
    }
    let mut seen_new_keys = HashSet::<String>::new();
    let mut new_entities = Vec::new();
    let mut added_observations = Vec::new();
    let mut unrecognized_rows: Vec<Value> = parsed
        .failures
        .iter()
        .map(|fail| {
            json!({
                "index": fail["index"],
                "code": fail["code"],
                "message": fail["message"],
                "raw": fail["item"]
            })
        })
        .collect();

    let total_rows = parsed.rows.len() + parsed.failures.len();
    for (index, row) in parsed.rows {
        let asset_type = row["asset_type"].as_str().unwrap_or("");
        if asset_type.is_empty() || matches!(asset_type, "finding" | "vulnerability") {
            unrecognized_rows.push(json!({
                "index": index,
                "code": if asset_type.is_empty() { "unrecognized_asset_type" } else { "skipped_external_finding" },
                "message": if asset_type.is_empty() { "无法识别资产类型" } else { "外部漏洞仅作为证据，不直接生成资产" },
                "raw": row["raw"]
            }));
            continue;
        }
        match super::preview_asset_identity(
            asset_type,
            &row["identity"],
            Some((inferred_kind.as_str(), "preview")),
        ) {
            Ok((norm_identity, key, display_label)) => {
                if let Some(existing_aid) = existing_by_key.get(&key) {
                    added_observations.push(json!({
                        "index": index,
                        "asset_type": asset_type,
                        "identity": norm_identity,
                        "identity_key": key,
                        "display_label": display_label,
                        "existing_asset_id": existing_aid,
                        "attributes": row["attributes"]
                    }));
                } else if seen_new_keys.contains(&key) {
                    added_observations.push(json!({
                        "index": index,
                        "asset_type": asset_type,
                        "identity": norm_identity,
                        "identity_key": key,
                        "display_label": display_label,
                        "existing_asset_id": null,
                        "deduplicated_in_batch": true,
                        "attributes": row["attributes"]
                    }));
                } else {
                    seen_new_keys.insert(key.clone());
                    new_entities.push(json!({
                        "index": index,
                        "asset_type": asset_type,
                        "identity": norm_identity,
                        "identity_key": key,
                        "display_label": display_label,
                        "attributes": row["attributes"]
                    }));
                }
            }
            Err(error) => {
                unrecognized_rows.push(json!({
                    "index": index,
                    "code": error.code,
                    "message": error.message,
                    "raw": row["raw"]
                }));
            }
        }
    }

    let counts = json!({
        "total": total_rows,
        "new_entities": new_entities.len(),
        "added_observations": added_observations.len(),
        "unrecognized": unrecognized_rows.len()
    });
    Ok(json!({
        "format": format_name,
        "total_rows": total_rows,
        "new_entities": new_entities,
        "added_observations": added_observations,
        "unrecognized_rows": unrecognized_rows,
        "counts": counts
    }))
}
