//! OpenAPI source material and static declarations for a SRC Task.
use super::verify_ref;
use crate::AppState;
use sha2::{Digest,Sha256};
use crate::runs::src_api::{
    Api, Error, RECORDS, RESEARCH_OPENAPI_APPLIED, db, now, rec, records, require, stage_rec,
};
use axum::http::StatusCode;
use base64::Engine;
use serde_json::{Map, Value, json};
use serde_yaml::Value as YamlValue;
use uuid::Uuid;

const EXTRACTOR: &str = "src.openapi.v1";
const HEADS: &str = "src_research_openapi_heads";
const ARTIFACTS: &str = "src_artifacts";

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn preferred(value: Option<&Value>, fallback: Value) -> Value {
    value
        .filter(|value| truthy(value))
        .cloned()
        .unwrap_or(fallback)
}

fn yaml_field<'a>(value: &'a YamlValue, key: &str) -> Option<&'a YamlValue> {
    value.as_mapping()?.get(YamlValue::String(key.to_owned()))
}

fn yaml_keys(value: Option<&YamlValue>) -> Vec<String> {
    value
        .and_then(YamlValue::as_mapping)
        .map(|map| map.keys().map(yaml_key).collect())
        .unwrap_or_default()
}

fn yaml_key(value: &YamlValue) -> String {
    match value {
        YamlValue::String(value) => value.clone(),
        YamlValue::Number(value) => value.to_string(),
        YamlValue::Bool(value) => value.to_string(),
        YamlValue::Null => "null".to_owned(),
        value => serde_yaml::to_string(value)
            .unwrap_or_default()
            .trim()
            .to_owned(),
    }
}

fn yaml_json(value: &YamlValue) -> Value {
    match value {
        YamlValue::Null => Value::Null,
        YamlValue::Bool(value) => json!(value),
        YamlValue::Number(value) => serde_json::to_value(value).unwrap_or(Value::Null),
        YamlValue::String(value) => json!(value),
        YamlValue::Sequence(rows) => Value::Array(rows.iter().map(yaml_json).collect()),
        YamlValue::Mapping(map) => {
            let mut result = Map::new();
            for (key, value) in map {
                result.insert(yaml_key(key), yaml_json(value));
            }
            Value::Object(result)
        }
        YamlValue::Tagged(value) => yaml_json(&value.value),
    }
}

fn pointer(document: &Value, reference: &str) -> Result<Value, String> {
    if matches!(reference, "" | "#") {
        return Ok(document.clone());
    }
    let Some(path) = reference.strip_prefix("#/") else {
        return Err(format!("unsupported JSON pointer {reference}"));
    };
    let mut current = document;
    for raw in path.split('/') {
        let key = raw.replace("~1", "/").replace("~0", "~");
        current = match current {
            Value::Object(map) => map.get(&key),
            Value::Array(rows) => key.parse::<usize>().ok().and_then(|index| rows.get(index)),
            _ => None,
        }
        .ok_or_else(|| format!("JSON pointer does not exist: {reference}"))?;
    }
    Ok(current.clone())
}

fn resolve(
    document: &Value,
    value: &Value,
    location: &str,
    seen: &[String],
    unresolved: &mut Vec<Value>,
) -> Option<Value> {
    if let Some(map) = value.as_object() {
        if let Some(reference) = map.get("$ref") {
            let reference = reference
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| reference.to_string());
            let failure = if !reference.starts_with('#') {
                Some("external references are not fetched or granted authority".to_owned())
            } else if seen.iter().any(|item| item == &reference) {
                Some(format!(
                    "cyclic reference: {}",
                    seen.iter()
                        .chain(std::iter::once(&reference))
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(" -> ")
                ))
            } else {
                None
            };
            if let Some(message) = failure {
                unresolved.push(json!({"location":location,"ref":reference,
                    "code":"openapi_ref_unresolved","message":message}));
                return None;
            }
            match pointer(document, &reference) {
                Ok(target) => {
                    let mut path = seen.to_vec();
                    path.push(reference.clone());
                    return resolve(
                        document,
                        &target,
                        &format!("{location}({reference})"),
                        &path,
                        unresolved,
                    );
                }
                Err(message) => {
                    unresolved.push(json!({"location":location,"ref":reference,
                        "code":"openapi_ref_unresolved","message":message}));
                    return None;
                }
            }
        }
        let mut result = Map::new();
        for (key, item) in map {
            result.insert(
                key.clone(),
                resolve(
                    document,
                    item,
                    &format!("{location}.{key}"),
                    seen,
                    unresolved,
                )
                .unwrap_or(Value::Null),
            );
        }
        return Some(Value::Object(result));
    }
    if let Some(rows) = value.as_array() {
        return Some(Value::Array(
            rows.iter()
                .enumerate()
                .map(|(index, item)| {
                    resolve(
                        document,
                        item,
                        &format!("{location}[{index}]"),
                        seen,
                        unresolved,
                    )
                    .unwrap_or(Value::Null)
                })
                .collect(),
        ));
    }
    Some(value.clone())
}

fn declaration(content: &[u8]) -> (Value, Vec<Value>, Vec<Value>, String) {
    let mut errors = Vec::new();
    let mut unresolved = Vec::new();
    let parsed = serde_yaml::from_slice::<YamlValue>(content);
    let yaml = match parsed {
        Ok(value) => Some(value),
        Err(error) => {
            errors.push(json!({"location":"$","code":"openapi_parse_error",
                "message":error.to_string()}));
            None
        }
    };
    let document = yaml.as_ref().map(yaml_json).unwrap_or(Value::Null);
    if document.is_null() && errors.is_empty() {
        errors.push(json!({"location":"$","code":"openapi_empty",
            "message":"document is empty"}));
    } else if !document.is_null() && !document.is_object() {
        errors.push(json!({"location":"$","code":"openapi_root_invalid",
            "message":"document root must be an object"}));
    }
    let mut view = json!({"openapi":null,"servers":[],"operations":[],
        "security_schemes":{},"global_security":[]});
    if let Some(doc) = document.as_object() {
        view["openapi"] = preferred(
            doc.get("openapi"),
            doc.get("swagger").cloned().unwrap_or(Value::Null),
        );
        let swagger_server = doc.get("host").filter(|value| truthy(value)).map(|host| {
            let scheme = doc
                .get("schemes")
                .and_then(Value::as_array)
                .and_then(|schemes| schemes.first())
                .and_then(Value::as_str)
                .unwrap_or("https");
            let base = doc
                .get("basePath")
                .filter(|value| truthy(value))
                .and_then(Value::as_str)
                .unwrap_or("/");
            format!("{scheme}://{}{base}", host.as_str().unwrap_or(""))
        });
        view["servers"] = preferred(
            doc.get("servers"),
            swagger_server
                .map(|url| json!([{"url":url}]))
                .unwrap_or_else(|| json!([])),
        );
        view["global_security"] = preferred(doc.get("security"), json!([]));
        let schemes = doc
            .get("components")
            .and_then(|value| value.get("securitySchemes"))
            .filter(|value| truthy(value))
            .or_else(|| doc.get("securityDefinitions").filter(|value| truthy(value)));
        let mut resolved_schemes = Map::new();
        for (name, schema) in schemes.and_then(Value::as_object).into_iter().flatten() {
            resolved_schemes.insert(
                name.clone(),
                resolve(
                    &document,
                    schema,
                    &format!("$.securitySchemes.{name}"),
                    &[],
                    &mut unresolved,
                )
                .unwrap_or(Value::Null),
            );
        }
        view["security_schemes"] = Value::Object(resolved_schemes);
        let mut paths = preferred(doc.get("paths"), json!({}));
        if !paths.is_object() {
            errors.push(json!({"location":"$.paths","code":"openapi_paths_invalid",
                "message":"paths must be an object"}));
            paths = json!({});
        }
        let yaml_paths = yaml.as_ref().and_then(|value| yaml_field(value, "paths"));
        let mut path_keys = yaml_keys(yaml_paths);
        if path_keys.is_empty() {
            path_keys = paths
                .as_object()
                .into_iter()
                .flat_map(|map| map.keys().cloned())
                .collect();
        }
        let mut operations = Vec::new();
        for path in path_keys {
            let Some(path_item) = paths.get(&path) else {
                continue;
            };
            let Some(path_object) = path_item.as_object() else {
                errors.push(json!({"location":format!("$.paths.{path}"),
                    "code":"openapi_path_invalid","message":"path item must be an object"}));
                continue;
            };
            let shared = path_object.get("parameters").and_then(Value::as_array);
            let yaml_item = yaml_paths.and_then(|value| yaml_field(value, &path));
            let mut method_keys = yaml_keys(yaml_item);
            if method_keys.is_empty() {
                method_keys = path_object.keys().cloned().collect();
            }
            for method in method_keys {
                if !matches!(
                    method.to_ascii_lowercase().as_str(),
                    "get" | "post" | "put" | "patch" | "delete" | "options" | "head" | "trace"
                ) {
                    continue;
                }
                let location = format!("$.paths.{path}.{method}");
                let Some(operation) = path_object.get(&method).and_then(Value::as_object) else {
                    errors.push(
                        json!({"location":location,"code":"openapi_operation_invalid",
                        "message":"operation must be an object"}),
                    );
                    continue;
                };
                let mut parameters = Vec::new();
                let declared = shared.into_iter().flatten().chain(
                    operation
                        .get("parameters")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten(),
                );
                for (index, parameter) in declared.enumerate() {
                    let at = format!("{location}.parameters[{index}]");
                    let Some(resolved) = resolve(&document, parameter, &at, &[], &mut unresolved)
                    else {
                        continue;
                    };
                    let Some(parameter) = resolved.as_object() else {
                        errors.push(json!({"location":at,"code":"openapi_parameter_invalid",
                            "message":"parameter must be an object"}));
                        continue;
                    };
                    let schema = preferred(parameter.get("schema"), json!({}));
                    parameters.push(json!({"name":parameter.get("name"),"in":parameter.get("in"),
                        "required":parameter.get("required").is_some_and(truthy),
                        "schema":resolve(&document,&schema,&format!("{at}.schema"),&[],&mut unresolved),
                        "source_location":at}));
                }
                let body_parameter = parameters.iter().find(|row| row["in"] == "body");
                let request_body = if let Some(body_parameter) = body_parameter {
                    let consumes = operation
                        .get("consumes")
                        .filter(|value| truthy(value))
                        .or_else(|| doc.get("consumes").filter(|value| truthy(value)))
                        .and_then(Value::as_array);
                    let media = consumes
                        .map(|rows| {
                            rows.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_else(|| vec!["application/json".to_owned()]);
                    let mut content = Map::new();
                    for name in media {
                        content.insert(name, json!({"schema":body_parameter["schema"]}));
                    }
                    json!({"required":body_parameter["required"],"content":content,
                        "source_location":body_parameter["source_location"]})
                } else {
                    let input = preferred(operation.get("requestBody"), json!({}));
                    resolve(
                        &document,
                        &input,
                        &format!("{location}.requestBody"),
                        &[],
                        &mut unresolved,
                    )
                    .unwrap_or_else(|| json!({}))
                };
                let mut responses = Map::new();
                for (status_code, response) in operation
                    .get("responses")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                {
                    let at = format!("{location}.responses.{status_code}");
                    let Some(resolved) = resolve(&document, response, &at, &[], &mut unresolved)
                    else {
                        continue;
                    };
                    let Some(response) = resolved.as_object() else {
                        continue;
                    };
                    let mut schemas = Map::new();
                    for (response_type, response_content) in response
                        .get("content")
                        .and_then(Value::as_object)
                        .into_iter()
                        .flatten()
                    {
                        let schema = preferred(response_content.get("schema"), json!({}));
                        schemas.insert(
                            response_type.clone(),
                            resolve(
                                &document,
                                &schema,
                                &format!("{at}.content.{response_type}.schema"),
                                &[],
                                &mut unresolved,
                            )
                            .unwrap_or(Value::Null),
                        );
                    }
                    if let Some(schema) = response.get("schema").filter(|value| truthy(value)) {
                        let produces = operation
                            .get("produces")
                            .filter(|value| truthy(value))
                            .or_else(|| doc.get("produces").filter(|value| truthy(value)))
                            .and_then(Value::as_array);
                        let media = produces
                            .and_then(|rows| rows.first())
                            .and_then(Value::as_str)
                            .unwrap_or("application/json");
                        schemas.insert(
                            media.to_owned(),
                            resolve(
                                &document,
                                schema,
                                &format!("{at}.schema"),
                                &[],
                                &mut unresolved,
                            )
                            .unwrap_or(Value::Null),
                        );
                    }
                    responses.insert(
                        status_code.clone(),
                        json!({"schemas":schemas,
                        "source_location":at}),
                    );
                }
                operations.push(
                    json!({"operation_id":preferred(operation.get("operationId"),
                        json!(format!("{} {path}",method.to_uppercase()))),
                    "method":method.to_uppercase(),"path":path,
                    "parameters":parameters,"request_body":request_body,"responses":responses,
                    "security":operation.get("security")
                        .cloned().unwrap_or_else(||view["global_security"].clone()),
                    "source_location":location}),
                );
            }
        }
        view["operations"] = json!(operations);
    }
    let status = if !errors.is_empty() {
        "invalid"
    } else if !unresolved.is_empty() {
        "partial"
    } else {
        "available"
    };
    (view, unresolved, errors, status.to_owned())
}

fn same_content(s: &AppState, task_id: &str, content: &[u8]) -> Api<Option<Value>> {
    for row in records(s, task_id, "research_document")? {
        if row["payload"]["extractor_version"] != EXTRACTOR {
            continue;
        }
        if let Ok(saved) = verify_ref(s, task_id, &row["payload"]["artifact"]) {
            if saved["data"]
                .as_str()
                .and_then(|value| base64::engine::general_purpose::STANDARD.decode(value).ok())
                .is_some_and(|saved| saved == content)
            {
                return Ok(Some(row));
            }
        }
    }
    Ok(None)
}

pub(in crate::runs::src_api::domains) fn ingest(
    s: &AppState,
    task_id: &str,
    body: &Value,
    command_id: Option<&str>,
) -> Api<Value> {
    if let Some(command_id) = command_id {
        if let Some(marker) = s
            .store
            .get(RESEARCH_OPENAPI_APPLIED, command_id)
            .map_err(db)?
        {
            return Ok(marker["output"].clone());
        }
    }
    let encoded = body["content_b64"]
        .as_str()
        .ok_or_else(|| Error::invalid("content_b64 is required"))?;
    let content = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| Error::invalid("content_b64 must be base64"))?;
    let source_url = body["source_url"]
        .as_str()
        .ok_or_else(|| Error::invalid("source_url is required"))?;
    let media_type = body["media_type"].as_str().unwrap_or("application/json");
    let (declaration, unresolved_refs, errors, status) = declaration(&content);
    loop {
        if let Some(command_id) = command_id {
            if let Some(marker) = s
                .store
                .get(RESEARCH_OPENAPI_APPLIED, command_id)
                .map_err(db)?
            {
                return Ok(marker["output"].clone());
            }
        }
        let head = s.store.get(HEADS, task_id).map_err(db)?;
        if let Some(existing) = same_content(s, task_id, &content)? {
            if let Some(command_id) = command_id {
                let marker = json!({"command_id":command_id,"output":existing});
                if s.store
                    .put_many_if(
                        &[
                            (HEADS, task_id, head.as_ref()),
                            (RESEARCH_OPENAPI_APPLIED, command_id, None),
                        ],
                        &[(RESEARCH_OPENAPI_APPLIED, command_id, &marker)],
                    )
                    .map_err(db)?
                {
                    return Ok(existing);
                }
                continue;
            }
            return Ok(existing);
        }
        let source_id = format!("openapi-source-{}", Uuid::new_v4().simple());
        let record_id = format!("openapi:{source_id}:{EXTRACTOR}");
        let artifact_id = format!("sart_{}", Uuid::new_v4().simple());
        let artifact_key = format!("{task_id}:{artifact_id}");
        let artifact = json!({"artifact_id":artifact_id,"version":1,"task_id":task_id,
            "size":content.len(),"name":"openapi-source","media_type":media_type,
            "source_run_id":body["source_run_id"],"source_event_seq":null,
            "original":true,"complete":"complete"});
        let saved_artifact = json!({"artifact_id":artifact_id,"version":1,"task_id":task_id,
            "size":content.len(),"name":"openapi-source","media_type":media_type,
            "source_run_id":body["source_run_id"],"source_event_seq":null,
            "data":base64::engine::general_purpose::STANDARD.encode(&content),"created_at":now()});
        let payload = json!({"content_digest":format!("{:x}",Sha256::digest(&content)),"source_digest":format!("{:x}",Sha256::digest(&content)),"document_type":"openapi","extractor_version":EXTRACTOR,
            "source_url":source_url,"source_run_id":body["source_run_id"],
            "artifact":artifact,"observed_at":body["observed_at"],"ingested_at":now(),
            "declaration":declaration,"unresolved_refs":unresolved_refs,
            "errors":errors,"status":status,"runtime_reachability":"not_requested"});
        let (storage_id, version, mut record) = stage_rec(
            s,
            task_id,
            "research_document",
            &record_id,
            payload,
            Some(0),
        )?;
        record["record_id"] = json!(record_id);
        record["parent_id"] = json!(source_id);
        record["natural_key"] = json!(record_id);
        let document = json!({"version":version+1,"values":record});
        let mut operation_documents = Vec::new();
        for operation in declaration["operations"].as_array().into_iter().flatten() {
            let operation_id = format!(
                "{record_id}:{}:{}",
                operation["method"].as_str().unwrap_or(""),
                operation["path"].as_str().unwrap_or("")
            );
            let mut operation_payload = operation.clone();
            operation_payload["document_id"] = json!(record_id);
            operation_payload["declaration_state"] = json!("document_declared");
            let (storage_id, version, mut row) = stage_rec(
                s,
                task_id,
                "openapi_operation",
                &operation_id,
                operation_payload,
                Some(0),
            )?;
            row["record_id"] = json!(operation_id);
            row["parent_id"] = json!(record_id);
            row["natural_key"] = json!(operation_id);
            operation_documents.push((storage_id, json!({"version":version+1,"values":row})));
        }
        let next_head = json!({"version":head.as_ref()
            .and_then(|row|row["version"].as_u64()).unwrap_or(0)+1});
        let marker = command_id.map(|command_id| json!({"command_id":command_id,"output":record}));
        let mut expected = vec![
            (HEADS, task_id, head.as_ref()),
            (RECORDS, storage_id.as_str(), None),
            (ARTIFACTS, artifact_key.as_str(), None),
        ];
        let mut writes = vec![
            (HEADS, task_id, &next_head),
            (RECORDS, storage_id.as_str(), &document),
            (ARTIFACTS, artifact_key.as_str(), &saved_artifact),
        ];
        for (key, value) in &operation_documents {
            expected.push((RECORDS, key.as_str(), None));
            writes.push((RECORDS, key.as_str(), value));
        }
        if let (Some(command_id), Some(marker)) = (command_id, marker.as_ref()) {
            expected.push((RESEARCH_OPENAPI_APPLIED, command_id, None));
            writes.push((RESEARCH_OPENAPI_APPLIED, command_id, marker));
        }
        if s.store.put_many_if(&expected, &writes).map_err(db)? {
            return Ok(record);
        }
    }
}

pub(in crate::runs::src_api::domains) fn read(
    s: &AppState,
    task_id: &str,
    params: &Value,
) -> Api<Value> {
    let record_id = require(params, "record_id")?;
    let row = rec(s, task_id, "research_document", record_id)?;
    if row["payload"]["document_type"] != "openapi" {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            "openapi_type_mismatch",
            "research document is not OpenAPI",
        ));
    }
    let raw = verify_ref(s, task_id, &row["payload"]["artifact"])?;
    let mut result = json!({"record_id":row["record_id"],"kind":row["kind"],
        "revision":row["revision"],"parent_id":row["parent_id"],
        "version":row["payload"]["version"].as_u64().unwrap_or(row["revision"].as_u64().unwrap_or(0))});
    for (key, value) in row["payload"].as_object().into_iter().flatten() {
        result[key] = value.clone();
    }
    result["original"] = json!({"encoding":"base64","data":raw["data"],"size":raw["size"]});
    Ok(result)
}
