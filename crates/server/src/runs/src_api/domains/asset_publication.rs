use super::*;
use std::collections::BTreeSet;

pub(super) const APPLIED: &str = "src_asset_publication_applied_commands";
const HEADS: &str = "src_asset_publication_heads";
const ARTIFACTS: &str = "src_artifacts";

fn completed(s: &AppState, command_id: &str) -> Api<Option<Value>> {
    Ok(s.store
        .get(APPLIED, command_id)
        .map_err(db)?
        .map(|marker| marker["output"].clone()))
}

fn binding(s: &AppState, id: &str) -> Api<Value> {
    let row = task(s, id)?;
    if row["binding"].is_null() {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "task_binding_invalid",
            "SRC Task binding is unavailable",
        ));
    }
    Ok(row)
}

fn record(
    id: &str,
    kind: &str,
    record_id: &str,
    parent_id: &str,
    natural_key: &str,
    payload: Value,
    previous: Option<&Value>,
) -> Value {
    let revision = previous
        .and_then(|row| row["version"].as_u64())
        .unwrap_or(0)
        + 1;
    let stamp = now();
    let created = previous
        .and_then(|row| row["values"]["created_at"].as_str())
        .unwrap_or(&stamp)
        .to_owned();
    json!({"version":revision,"values":{"record_id":record_id,"kind":kind,"task_id":id,
        "revision":revision,"parent_id":parent_id,"natural_key":natural_key,"generation":0,
        "created_at":created,"updated_at":stamp,"payload":payload}})
}

fn selected_assets(s: &AppState, source: &str, selection: &Value) -> Api<(Vec<Value>, Vec<Value>)> {
    let chosen = selection["assets"].as_array();
    let observation_ids: BTreeSet<&str> = selection["observation_ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let selected_artifacts: BTreeSet<&str> = selection["artifact_refs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|reference| reference["artifact_id"].as_str())
        .collect();
    let observations = records(s, source, "asset_observation")?;
    let mut assets = Vec::new();
    let mut asset_ids = BTreeSet::new();
    for item in chosen.into_iter().flatten() {
        let asset_id = item
            .as_str()
            .or_else(|| item["asset_id"].as_str())
            .ok_or_else(|| Error::invalid("selection asset_id is required"))?;
        let mut entity = asset_row(s, source, asset_id)?["payload"].clone();
        if entity["lifecycle"] == "destroyed" {
            return Err(Error::new(
                StatusCode::NOT_FOUND,
                "artifact_missing",
                "Asset was destroyed; only its tombstone remains",
            ));
        }
        let fields: BTreeSet<&str> = item["fields"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        if !fields.is_empty() {
            for key in ["current_profile", "profile_sources", "conflicts"] {
                if let Some(map) = entity[key].as_object_mut() {
                    map.retain(|field, _| fields.contains(field.as_str()));
                }
            }
        }
        let mut selected = Vec::new();
        for row in &observations {
            let mut observation = row["payload"].clone();
            if observation["asset_id"] != asset_id || observation["revoked_at"].is_string() {
                continue;
            }
            if !observation_ids.is_empty()
                && !observation["observation_id"]
                    .as_str()
                    .is_some_and(|id| observation_ids.contains(id))
            {
                continue;
            }
            if !fields.is_empty() {
                for key in ["attributes", "raw_values"] {
                    if let Some(map) = observation[key].as_object_mut() {
                        map.retain(|field, _| fields.contains(field.as_str()));
                    }
                }
            }
            if let Some(refs) = observation["artifact_refs"].as_array_mut() {
                refs.retain(|reference| {
                    reference["artifact_id"]
                        .as_str()
                        .is_some_and(|id| selected_artifacts.contains(id))
                });
            }
            selected.push(observation);
        }
        asset_ids.insert(asset_id.to_owned());
        assets.push(json!({"entity":entity,"observations":selected}));
    }
    let selected_relations: BTreeSet<&str> = selection["relation_ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let relations = records(s, source, "asset_relation")?
        .into_iter()
        .filter_map(|row| {
            let relation = row["payload"].clone();
            let include = if selected_relations.is_empty() {
                relation["from_asset_id"]
                    .as_str()
                    .is_some_and(|id| asset_ids.contains(id))
                    && relation["to_asset_id"]
                        .as_str()
                        .is_some_and(|id| asset_ids.contains(id))
            } else {
                relation["relation_id"]
                    .as_str()
                    .is_some_and(|id| selected_relations.contains(id))
            };
            include.then_some(relation)
        })
        .collect();
    Ok((assets, relations))
}

fn pin_artifacts(
    s: &AppState,
    source: &str,
    target: &str,
    selection: &Value,
) -> Api<(Vec<Value>, Vec<(String, Value)>)> {
    let mut refs = Vec::new();
    let mut writes = Vec::new();
    for reference in selection["artifact_refs"].as_array().into_iter().flatten() {
        if reference["task_id"] != source {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "task_binding_invalid",
                "publication artifact belongs to another Task",
            ));
        }
        require(reference, "artifact_id")?;
        let artifact = verify_ref(s, source, reference)?;
        let data = artifact["data"]
            .as_str()
            .ok_or_else(|| Error::invalid("publication artifact data missing"))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|_| Error::invalid("publication artifact data invalid"))?;
        let artifact_id = format!("sart_{}", Uuid::new_v4().simple());
        let name = reference["name"].as_str().unwrap_or("");
        let media_type = reference["media_type"]
            .as_str()
            .unwrap_or("application/octet-stream");
        let saved = json!({"artifact_id":artifact_id,"version":1,"task_id":target,
            "size":bytes.len(),"name":name,"media_type":media_type,"data":data,"created_at":now()});
        let pinned = json!({"artifact_id":artifact_id,"version":1,"task_id":target,
            "size":bytes.len(),"name":name,"media_type":media_type,
            "source_run_id":reference["source_run_id"],"source_event_seq":reference["source_event_seq"],
            "complete":reference["complete"].as_str().unwrap_or("complete"),
            "original":reference["original"].as_bool().unwrap_or(true)});
        writes.push((format!("{target}:{artifact_id}"), saved));
        refs.push(pinned);
    }
    Ok((refs, writes))
}

fn publish(
    s: &AppState,
    source: &str,
    body: &Value,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    let target = require(body, "target_task_id")?;
    let selection = body["selection"]
        .as_object()
        .map(|_| &body["selection"])
        .ok_or_else(|| Error::invalid("selection must be an object"))?;
    let purpose = body["purpose"]
        .as_str()
        .ok_or_else(|| Error::invalid("purpose is required"))?;
    loop {
        if let Some(output) = completed(s, command_id)? {
            return Ok(output);
        }
        let source_task = binding(s, source)?;
        let target_task = binding(s, target)?;
        let (assets, relations) = selected_assets(s, source, selection)?;
        let (artifact_refs, artifact_writes) = pin_artifacts(s, source, target, selection)?;
        let head = s.store.get(HEADS, source).map_err(db)?;
        let previous_version = head
            .as_ref()
            .and_then(|row| row["version"].as_u64())
            .unwrap_or(records(s, source, "asset_publication")?.len() as u64);
        let version = previous_version + 1;
        let publication_id = format!("spub-{}", Uuid::new_v4().simple());
        let reference_id = format!("sassetref-{}", Uuid::new_v4().simple());
        let stamp = now();
        let publication = json!({"publication_id":publication_id,"version":version,
            "task_id":source,"target_task_id":target,"publisher":actor_id,"purpose":purpose,
            "published_at":stamp,"assets":assets,"relations":relations,"artifact_refs":artifact_refs,
            "selection":selection,"revoked_at":null});
        let reference = json!({"reference_id":reference_id,"task_id":target,
            "source_task_id":source,"publication_id":publication_id,"version":version,
            "created_at":stamp,"revoked_at":null});
        let publication_key = format!("asset_publication:{source}:{publication_id}");
        let reference_key = format!("asset_reference:{target}:{publication_id}");
        let publication_record = record(
            source,
            "asset_publication",
            &publication_id,
            "",
            &format!("{target}:{publication_id}"),
            publication.clone(),
            None,
        );
        let reference_record = record(
            target,
            "asset_reference",
            &reference_id,
            &publication_id,
            &publication_id,
            reference.clone(),
            None,
        );
        let next_head = json!({"version":version});
        let output = json!({"publication":publication,"reference":reference});
        let marker = json!({"command_id":command_id,"output":output});
        let mut expected = vec![
            (TASKS, source, Some(&source_task)),
            (TASKS, target, Some(&target_task)),
            (HEADS, source, head.as_ref()),
            (RECORDS, publication_key.as_str(), None),
            (RECORDS, reference_key.as_str(), None),
            (APPLIED, command_id, None),
        ];
        expected.extend(
            artifact_writes
                .iter()
                .map(|(key, _)| (ARTIFACTS, key.as_str(), None)),
        );
        let mut writes = vec![
            (HEADS, source, &next_head),
            (RECORDS, publication_key.as_str(), &publication_record),
            (RECORDS, reference_key.as_str(), &reference_record),
            (APPLIED, command_id, &marker),
        ];
        writes.extend(
            artifact_writes
                .iter()
                .map(|(key, row)| (ARTIFACTS, key.as_str(), row)),
        );
        if s.store.put_many_if(&expected, &writes).map_err(db)? {
            return Ok(output);
        }
    }
}

pub(super) fn published(s: &AppState, target: &str, publication_id: &str) -> Api<Value> {
    let reference = rec(s, target, "asset_reference", publication_id).or_else(|error| {
        if error.code == "src.record_not_found" {
            Err(Error::new(
                StatusCode::CONFLICT,
                "source_revoked",
                "Published asset reference is unavailable",
            ))
        } else {
            Err(error)
        }
    })?;
    if reference["payload"]["revoked_at"].is_string() {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "source_revoked",
            "Published asset reference is unavailable",
        ));
    }
    let source = require(&reference["payload"], "source_task_id")?;
    let publication = rec(s, source, "asset_publication", publication_id)?;
    if publication["payload"]["revoked_at"].is_string() {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "source_revoked",
            "Publication was revoked",
        ));
    }
    Ok(publication["payload"].clone())
}

fn validate_refresh_subset(current_selection: &Value, next_selection: &Value) -> Api<()> {
    let scope_err = || {
        Error::new(
            StatusCode::FORBIDDEN,
            "src.publication_scope_exceeded",
            "refreshed selection cannot expand beyond original publication scope",
        )
    };

    let mut allowed_assets: std::collections::HashMap<String, Option<BTreeSet<String>>> =
        std::collections::HashMap::new();
    for item in current_selection["assets"].as_array().into_iter().flatten() {
        let Some(aid) = item.as_str().or_else(|| item["asset_id"].as_str()) else {
            continue;
        };
        let fields: BTreeSet<String> = item["fields"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        allowed_assets.insert(
            aid.to_owned(),
            if fields.is_empty() {
                None
            } else {
                Some(fields)
            },
        );
    }
    for item in next_selection["assets"].as_array().into_iter().flatten() {
        let Some(aid) = item.as_str().or_else(|| item["asset_id"].as_str()) else {
            return Err(scope_err());
        };
        let Some(allowed_fields) = allowed_assets.get(aid) else {
            return Err(scope_err());
        };
        if let Some(allowed_set) = allowed_fields {
            let new_fields: Vec<&str> = item["fields"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            if new_fields.is_empty()
                || new_fields.iter().any(|f| !allowed_set.contains(*f))
            {
                return Err(scope_err());
            }
        }
    }

    let collect_ids = |val: &Value, keys: &[&str], id_field: &str| -> BTreeSet<String> {
        let mut set = BTreeSet::new();
        for key in keys {
            for item in val[*key].as_array().into_iter().flatten() {
                if let Some(id) = item
                    .as_str()
                    .or_else(|| item[id_field].as_str())
                    .filter(|v| !v.is_empty())
                {
                    set.insert(id.to_owned());
                }
            }
        }
        set
    };

    let old_findings = collect_ids(current_selection, &["findings", "finding_ids"], "finding_id");
    let new_findings = collect_ids(next_selection, &["findings", "finding_ids"], "finding_id");
    if !new_findings.is_subset(&old_findings) {
        return Err(scope_err());
    }

    let old_artifacts = collect_ids(
        current_selection,
        &["artifacts", "artifact_refs", "artifact_ids"],
        "artifact_id",
    );
    let new_artifacts = collect_ids(
        next_selection,
        &["artifacts", "artifact_refs", "artifact_ids"],
        "artifact_id",
    );
    if !new_artifacts.is_subset(&old_artifacts) {
        return Err(scope_err());
    }

    let old_obs = collect_ids(current_selection, &["observation_ids"], "observation_id");
    let new_obs = collect_ids(next_selection, &["observation_ids"], "observation_id");
    if !old_obs.is_empty() && (new_obs.is_empty() || !new_obs.is_subset(&old_obs)) {
        return Err(scope_err());
    }

    let old_rels = collect_ids(current_selection, &["relation_ids"], "relation_id");
    let new_rels = collect_ids(next_selection, &["relation_ids"], "relation_id");
    if !old_rels.is_empty() && (new_rels.is_empty() || !new_rels.is_subset(&old_rels)) {
        return Err(scope_err());
    }

    Ok(())
}

fn refresh(
    s: &AppState,
    target: &str,
    body: &Value,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    let publication_id = require(body, "publication_id")?;
    let current = published(s, target, publication_id)?;
    let source = require(&current, "task_id")?;
    let mut next = body.clone();
    next["target_task_id"] = json!(target);
    if next.get("purpose").is_none_or(Value::is_null) {
        next["purpose"] = current["purpose"].clone();
    }
    if next.get("selection").is_some_and(Value::is_object) {
        validate_refresh_subset(&current["selection"], &next["selection"])?;
    } else {
        next["selection"] = current["selection"].clone();
    }
    publish(s, source, &next, command_id, actor_id)
}

fn revoke(
    s: &AppState,
    source: &str,
    body: &Value,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    let publication_id = require(body, "publication_id")?;
    let key = format!("asset_publication:{source}:{publication_id}");
    let key = if s.store.get(RECORDS, &key).map_err(db)?.is_some() {
        key
    } else {
        rec_key(source, "asset_publication", publication_id)
    };
    loop {
        if let Some(output) = completed(s, command_id)? {
            return Ok(output);
        }
        let current = s.store.get(RECORDS, &key).map_err(db)?.ok_or_else(|| {
            Error::new(
                StatusCode::NOT_FOUND,
                "artifact_missing",
                "Publication is unavailable",
            )
        })?;
        if current["values"]["task_id"] != source {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "task_binding_invalid",
                "Publication belongs to another Task",
            ));
        }
        let mut payload = current["values"]["payload"].clone();
        payload["revoked_at"] = json!(now());
        payload["revoked_by"] = json!(actor_id);
        let document = record(
            source,
            "asset_publication",
            publication_id,
            current["values"]["parent_id"].as_str().unwrap_or(""),
            current["values"]["natural_key"].as_str().unwrap_or(""),
            payload.clone(),
            Some(&current),
        );
        let marker = json!({"command_id":command_id,"output":payload});
        if s.store
            .put_many_if(
                &[(RECORDS, &key, Some(&current)), (APPLIED, command_id, None)],
                &[(RECORDS, &key, &document), (APPLIED, command_id, &marker)],
            )
            .map_err(db)?
        {
            return Ok(payload);
        }
    }
}

pub(super) fn apply_command(
    s: &AppState,
    task_id: &str,
    kind: &str,
    body: &Value,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    if let Some(output) = completed(s, command_id)? {
        return Ok(output);
    }
    binding(s, task_id)?;
    match kind {
        "src.asset.publish" => publish(s, task_id, body, command_id, actor_id),
        "src.asset.reference.refresh" => refresh(s, task_id, body, command_id, actor_id),
        "src.asset.publication.revoke" => revoke(s, task_id, body, command_id, actor_id),
        _ => Err(Error::invalid("unknown asset publication command")),
    }
}
