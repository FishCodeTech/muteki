//! Source-shaped comparison between declared OpenAPI operations and observed HTTP exchanges.
use super::verify_ref;
use crate::AppState;
use crate::runs::src_api::{Api, Error, now, rec, require};
use axum::http::StatusCode;
use base64::Engine;
use flate2::read::GzDecoder;
use jsonschema::paths::LocationSegment;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::io::Read;

const HEADS: &str = "src_research_openapi_comparison_heads";
const APPLIED: &str = "src_research_openapi_comparison_applied_commands";

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

fn header(headers: &Value, name: &str) -> Option<String> {
    headers.as_array().into_iter().flatten().find_map(|row| {
        let pair = row.as_array()?;
        if pair.first()?.as_str()?.eq_ignore_ascii_case(name) {
            pair.get(1)?.as_str().map(str::to_owned)
        } else {
            None
        }
    })
}

fn artifact_bytes(s: &AppState, task_id: &str, reference: &Value) -> Api<Vec<u8>> {
    let saved = verify_ref(s, task_id, reference)?;
    base64::engine::general_purpose::STANDARD
        .decode(saved["data"].as_str().unwrap_or(""))
        .map_err(|error| {
            Error::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "src.artifact_corrupt",
                error.to_string(),
            )
        })
}

fn body_bytes(raw: &[u8], request: bool) -> Vec<u8> {
    let marker = b"\r\n\r\n";
    match raw
        .windows(marker.len())
        .position(|window| window == marker)
    {
        Some(index) => raw[index + marker.len()..].to_vec(),
        None if request => Vec::new(),
        None => raw.to_vec(),
    }
}

fn form_values(raw: &[u8]) -> Result<Vec<(String, String)>, String> {
    let text = std::str::from_utf8(raw).map_err(|error| error.to_string())?;
    Ok(url::form_urlencoded::parse(text.as_bytes())
        .into_owned()
        .collect())
}

fn json_body(raw: &[u8]) -> Result<Value, String> {
    std::str::from_utf8(raw).map_err(|_| "body is not UTF-8 JSON".to_owned())?;
    serde_json::from_slice(raw).map_err(|error| {
        format!(
            "invalid JSON at line {} column {}: {error}",
            error.line(),
            error.column()
        )
    })
}

fn schema_errors(schema: &Value, instance: &Value) -> Vec<Value> {
    match jsonschema::draft202012::options().build(schema) {
        Ok(validator) => validator
            .iter_errors(instance)
            .map(|error| {
                let mut path = "$".to_owned();
                for segment in &error.instance_path {
                    match segment {
                        LocationSegment::Property(name) => {
                            path.push('.');
                            path.push_str(&name);
                        }
                        LocationSegment::Index(index) => path.push_str(&format!("[{index}]")),
                    }
                }
                json!({"path":path,"message":error.to_string()})
            })
            .collect(),
        Err(error) => vec![json!({"path":"$",
            "message":format!("response schema is invalid: {error}")})],
    }
}

fn observed_url(exchange: &Value) -> String {
    exchange["observed_url"].as_str().unwrap_or("").to_owned()
}

fn runtime_path_candidates(servers: &Value, observed: &str) -> Vec<String> {
    let actual = reqwest::Url::parse(observed).ok();
    let actual_scheme = actual.as_ref().map(reqwest::Url::scheme).unwrap_or("");
    let actual_host = actual
        .as_ref()
        .and_then(reqwest::Url::host_str)
        .unwrap_or("");
    let actual_path = actual
        .as_ref()
        .map(reqwest::Url::path)
        .unwrap_or_else(|| observed.split(['?', '#']).next().unwrap_or(""));
    let actual_path = if actual_path.is_empty() {
        "/"
    } else {
        actual_path
    };
    let rows = servers.as_array();
    let absolute_servers = rows.into_iter().flatten().any(|server| {
        reqwest::Url::parse(server["url"].as_str().unwrap_or(""))
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .is_some()
    });
    let mut candidates = Vec::new();
    for server in rows.into_iter().flatten() {
        let text = server["url"].as_str().unwrap_or("");
        let parsed = reqwest::Url::parse(text).ok();
        let server_host = parsed.as_ref().and_then(reqwest::Url::host_str);
        let server_scheme = parsed.as_ref().map(reqwest::Url::scheme).unwrap_or("");
        if server_host.is_some_and(|host| {
            !server_scheme.eq_ignore_ascii_case(actual_scheme)
                || !host.eq_ignore_ascii_case(actual_host)
        }) {
            continue;
        }
        let path = parsed
            .as_ref()
            .map(reqwest::Url::path)
            .unwrap_or_else(|| text.split(['?', '#']).next().unwrap_or(""));
        let base = path.trim_end_matches('/');
        let candidate = if base.is_empty() && (server_host.is_some() || !absolute_servers) {
            Some(actual_path.to_owned())
        } else if !base.is_empty() && actual_path.starts_with(&format!("{base}/")) {
            Some(actual_path[base.len()..].to_owned())
        } else if base == actual_path {
            Some("/".to_owned())
        } else {
            None
        };
        if let Some(candidate) = candidate {
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
    }
    if !absolute_servers && !candidates.contains(&actual_path.to_owned()) {
        candidates.push(actual_path.to_owned());
    }
    candidates
}

fn path_matches(template: &str, actual: &str) -> bool {
    let a: Vec<_> = if template.is_empty() { "/" } else { template }
        .split('/')
        .collect();
    let b: Vec<_> = if actual.is_empty() { "/" } else { actual }
        .split('/')
        .collect();
    a.len() == b.len()
        && a.iter().zip(b).all(|(expected, value)| {
            (expected.starts_with('{')
                && expected.ends_with('}')
                && expected.len() > 2
                && !value.is_empty())
                || expected == &value
        })
}

fn path_values(template: &str, actual: &str) -> Vec<Value> {
    let a: Vec<_> = template.split('/').collect();
    let b: Vec<_> = actual.split('/').collect();
    if a.len() != b.len() {
        return Vec::new();
    }
    if a.iter().zip(&b).any(|(expected, value)| {
        !(expected.starts_with('{') && expected.ends_with('}')) && expected != value
    }) {
        return Vec::new();
    }
    a.into_iter()
        .zip(b)
        .filter_map(|(expected, value)| {
            expected
                .strip_prefix('{')
                .and_then(|name| name.strip_suffix('}'))
                .map(|name| json!({"name":name,"in":"path","value":value}))
        })
        .collect()
}

pub(in crate::runs::src_api::domains) fn request_parameters(
    s: &AppState,
    task_id: &str,
    exchange: &Value,
) -> Vec<Value> {
    let mut parameters = Vec::new();
    if let Ok(url) = reqwest::Url::parse(
        exchange["observed_url"]
            .as_str()
            .filter(|value| !value.is_empty())
            .or_else(|| exchange["declared_target"].as_str())
            .unwrap_or(""),
    ) {
        parameters.extend(
            url.query_pairs()
                .map(|(name, value)| json!({"name":name,"in":"query","value":value})),
        );
    }
    if exchange["request_raw_ref"].is_object() {
        match artifact_bytes(s, task_id, &exchange["request_raw_ref"]) {
            Ok(raw) => {
                let body = body_bytes(&raw, true);
                let content_type =
                    header(&exchange["request_headers"], "content-type").unwrap_or_default();
                if content_type.contains("json") {
                    match json_body(&body) {
                        Ok(Value::Object(fields)) => {
                            parameters.extend(fields.into_iter().map(
                                |(name, value)| json!({"name":name,"in":"body","value":value}),
                            ));
                        }
                        Ok(_) => {}
                        Err(error) => parameters.push(json!({"name":null,"in":"body",
                            "error":error})),
                    }
                } else if content_type.contains("application/x-www-form-urlencoded") {
                    match form_values(&body) {
                        Ok(fields) => {
                            parameters.extend(fields.into_iter().map(
                                |(name, value)| json!({"name":name,"in":"body","value":value}),
                            ))
                        }
                        Err(error) => parameters.push(json!({"name":null,"in":"body",
                            "error":error})),
                    }
                }
            }
            Err(error) => parameters.push(json!({"name":null,"in":"body","error":error.message})),
        }
    }
    parameters
}

fn response_body(raw: &[u8], headers: &Value) -> (Vec<u8>, Vec<String>) {
    let mut body = raw.to_vec();
    let mut notes = Vec::new();
    let transfer = headers
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let pair = row.as_array()?;
            if pair
                .first()?
                .as_str()?
                .eq_ignore_ascii_case("transfer-encoding")
            {
                pair.get(1)?.as_str()
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join(",")
        .to_lowercase();
    if transfer.contains("chunked") {
        let decoded = (|| -> Result<Vec<u8>, String> {
            let mut rest = body.as_slice();
            let mut out = Vec::new();
            loop {
                let end = rest
                    .windows(2)
                    .position(|bytes| bytes == b"\r\n")
                    .ok_or("missing chunk line")?;
                let line = std::str::from_utf8(&rest[..end]).map_err(|e| e.to_string())?;
                let size = usize::from_str_radix(line.split(';').next().unwrap_or(""), 16)
                    .map_err(|e| e.to_string())?;
                rest = &rest[end + 2..];
                if size == 0 {
                    break;
                }
                if rest.len() < size + 2 || &rest[size..size + 2] != b"\r\n" {
                    return Err("chunk bytes are incomplete".to_owned());
                }
                out.extend_from_slice(&rest[..size]);
                rest = &rest[size + 2..];
            }
            Ok(out)
        })();
        match decoded {
            Ok(decoded) => {
                body = decoded;
                notes.push(
                    "chunked transfer decoding is a derived view; wire bytes are unchanged"
                        .to_owned(),
                );
            }
            Err(error) => {
                return (
                    Vec::new(),
                    vec![format!("chunked body is incomplete: {error}")],
                );
            }
        }
    }
    let encoding = headers
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let pair = row.as_array()?;
            if pair
                .first()?
                .as_str()?
                .eq_ignore_ascii_case("content-encoding")
            {
                pair.get(1)?.as_str()
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join(",")
        .to_lowercase();
    if encoding.contains("gzip") {
        let mut decoded = Vec::new();
        match GzDecoder::new(body.as_slice()).read_to_end(&mut decoded) {
            Ok(_) => {
                body = decoded;
                notes.push("gzip decoding is a derived view; wire bytes are unchanged".to_owned());
            }
            Err(error) => {
                return (
                    Vec::new(),
                    vec![format!("gzip body is incomplete: {error}")],
                );
            }
        }
    }
    (body, notes)
}

fn status_key(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

pub(in crate::runs::src_api::domains) fn compare(
    s: &AppState,
    task_id: &str,
    params: &Value,
) -> Api<Value> {
    let document_id = require(params, "document_id")?;
    let document = rec(s, task_id, "research_document", document_id)?;
    if document["payload"]["document_type"] != "openapi" {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            "openapi_type_mismatch",
            "research document is not OpenAPI",
        ));
    }
    let exchanges = params["exchanges"]
        .as_array()
        .ok_or_else(|| Error::invalid("exchanges is required"))?;
    for exchange in exchanges {
        if exchange["task_id"] != task_id {
            return Err(Error::new(
                StatusCode::FORBIDDEN,
                "task_binding_invalid",
                "exchange belongs to another Task",
            ));
        }
    }
    let declaration = &document["payload"]["declaration"];
    let operations = declaration["operations"]
        .as_array()
        .ok_or_else(|| Error::invalid("OpenAPI operations are unavailable"))?;
    let mut matches = Vec::new();
    let mut differences = Vec::new();
    for exchange in exchanges {
        let observed = observed_url(exchange);
        let mut matched = None;
        let mut matched_path = reqwest::Url::parse(&observed)
            .ok()
            .map(|url| url.path().to_owned())
            .unwrap_or_default();
        for candidate in runtime_path_candidates(&declaration["servers"], &observed) {
            if let Some(operation) = operations.iter().find(|operation| {
                operation["method"] == exchange["method"].as_str().unwrap_or("").to_uppercase()
                    && operation["path"]
                        .as_str()
                        .is_some_and(|path| path_matches(path, &candidate))
            }) {
                matched = Some(operation);
                matched_path = candidate;
                break;
            }
        }
        let Some(operation) = matched else {
            differences.push(json!({"kind":"undocumented_exchange",
                "exchange_id":exchange["exchange_id"],"method":exchange["method"],
                "url":observed,"meaning":"runtime observation only; not a vulnerability judgment"}));
            continue;
        };
        let mut local = Vec::new();
        let mut parameters = request_parameters(s, task_id, exchange);
        parameters.extend(path_values(
            operation["path"].as_str().unwrap_or(""),
            &matched_path,
        ));
        let mut documented: Vec<Value> = Vec::new();
        for parameter in operation["parameters"].as_array().into_iter().flatten() {
            if let Some(index) = documented
                .iter()
                .position(|row| row["name"] == parameter["name"] && row["in"] == parameter["in"])
            {
                documented[index] = parameter.clone();
            } else {
                documented.push(parameter.clone());
            }
        }
        let has_body_declaration = documented.iter().any(|row| row["in"] == "body");
        for parameter in &documented {
            if parameter["in"] == "body" {
                continue;
            }
            if parameter["required"] == true
                && !parameters
                    .iter()
                    .any(|row| row["name"] == parameter["name"] && row["in"] == parameter["in"])
            {
                local.push(json!({"kind":"missing_declared_parameter",
                    "name":parameter["name"],"in":parameter["in"]}));
            }
        }
        for parameter in &parameters {
            if truthy(&parameter["error"]) {
                local.push(json!({"kind":"parameter_unavailable",
                    "detail":parameter["error"]}));
            } else if parameter["in"] == "body" && has_body_declaration {
                continue;
            } else if !documented
                .iter()
                .any(|row| row["name"] == parameter["name"] && row["in"] == parameter["in"])
            {
                local.push(json!({"kind":"undocumented_parameter",
                    "name":parameter["name"],"in":parameter["in"]}));
            }
        }
        let request_spec = &operation["request_body"];
        let request_content = &request_spec["content"];
        if truthy(request_spec) || truthy(request_content) {
            match artifact_bytes(s, task_id, &exchange["request_raw_ref"]) {
                Ok(raw) => {
                    let body = body_bytes(&raw, true);
                    let request_type =
                        header(&exchange["request_headers"], "content-type").unwrap_or_default();
                    let media_type = request_type.split(';').next().unwrap_or("");
                    let schema = &request_content[media_type]["schema"];
                    if request_spec["required"] == true && body.is_empty() {
                        local.push(json!({"kind":"missing_request_body"}));
                    }
                    if truthy(schema) && !body.is_empty() {
                        if media_type.contains("json") {
                            match json_body(&body) {
                                Ok(value) => local.extend(
                                    schema_errors(schema, &value).into_iter().map(|mut error| {
                                        error["kind"] = json!("request_schema_mismatch");
                                        error
                                    }),
                                ),
                                Err(error) => local.push(json!({"kind":"request_body_unavailable",
                                    "detail":error})),
                            }
                        } else if media_type == "application/x-www-form-urlencoded" {
                            match form_values(&body) {
                                Ok(fields) => {
                                    let value = Value::Object(
                                        fields
                                            .into_iter()
                                            .map(|(key, value)| (key, json!(value)))
                                            .collect(),
                                    );
                                    local.extend(schema_errors(schema, &value).into_iter().map(
                                        |mut error| {
                                            error["kind"] = json!("request_schema_mismatch");
                                            error
                                        },
                                    ));
                                }
                                Err(error) => local.push(json!({"kind":"request_body_unavailable",
                                    "detail":error})),
                            }
                        }
                    }
                }
                Err(error) => {
                    local.push(json!({"kind":"request_body_unavailable","detail":error.message}))
                }
            }
        }
        let documented_security = truthy(&operation["security"]);
        let actual_security = truthy(&exchange["metadata"]["auth_scheme"])
            || exchange["request_headers"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|row| {
                    row[0]
                        .as_str()
                        .is_some_and(|key| key.eq_ignore_ascii_case("authorization"))
                });
        if documented_security != actual_security {
            local.push(json!({"kind":"authentication_difference",
                "documented":documented_security,"observed":actual_security}));
        }
        let response_spec = &operation["responses"][status_key(&exchange["status_code"])];
        if response_spec.is_null() {
            local.push(json!({"kind":"undocumented_response_status",
                "status_code":exchange["status_code"]}));
        } else if truthy(&exchange["response_raw_ref"]) {
            match artifact_bytes(s, task_id, &exchange["response_raw_ref"]) {
                Ok(raw) => {
                    let response = body_bytes(&raw, false);
                    let (body, notes) = response_body(&response, &exchange["response_headers"]);
                    local.extend(
                        notes
                            .into_iter()
                            .map(|detail| json!({"kind":"derived_response_view","detail":detail})),
                    );
                    let content_type = header(&exchange["response_headers"], "content-type")
                        .unwrap_or_else(|| "application/json".to_owned());
                    let media_type = content_type.split(';').next().unwrap_or("");
                    let schemas = &response_spec["schemas"];
                    let schema = schemas
                        .get(media_type)
                        .filter(|value| truthy(value))
                        .or_else(|| {
                            schemas
                                .as_object()
                                .and_then(|values| values.values().next())
                        });
                    if let Some(schema) = schema.filter(|schema| truthy(schema)) {
                        match json_body(&body) {
                            Ok(value) => local.extend(
                                schema_errors(schema, &value).into_iter().map(|mut error| {
                                    error["kind"] = json!("response_schema_mismatch");
                                    error
                                }),
                            ),
                            Err(error) => local.push(json!({"kind":"response_body_unavailable",
                                "detail":error})),
                        }
                    }
                }
                Err(error) => {
                    let code = match error.code {
                        "src.artifact_scope" => "task_binding_invalid",
                        "src.artifact_missing"
                        | "src.artifact_ref_mismatch"
                        | "src.artifact_corrupt" => "artifact_missing",
                        code => code,
                    };
                    local.push(json!({"kind":"response_unavailable",
                        "code":code,"detail":error.message}));
                }
            }
        }
        let matched = json!({"exchange_id":exchange["exchange_id"],
            "operation_id":operation["operation_id"],
            "source_location":operation["source_location"],
            "matched_path":matched_path,
            "identity_revision":exchange["identity_revision"],
            "observed_at":exchange["started_at"],"differences":local});
        for item in &local {
            let mut difference = item.clone();
            difference["exchange_id"] = exchange["exchange_id"].clone();
            differences.push(difference);
        }
        matches.push(matched);
    }
    let documented: BTreeSet<(String, String)> = operations
        .iter()
        .map(|item| {
            (
                item["method"].as_str().unwrap_or("").to_owned(),
                item["path"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect();
    let matched: BTreeSet<(String, String)> = operations
        .iter()
        .filter(|item| {
            matches
                .iter()
                .any(|row| row["operation_id"] == item["operation_id"])
        })
        .map(|item| {
            (
                item["method"].as_str().unwrap_or("").to_owned(),
                item["path"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect();
    let missing_declared_operations: Vec<Value> = documented
        .difference(&matched)
        .map(|(method, path)| json!({"method":method,"path":path}))
        .collect();
    let exchange_refs: Vec<Value> = exchanges
        .iter()
        .map(|row| {
            json!({"exchange_id":row["exchange_id"],
            "identity_revision":row["identity_revision"],
            "started_at":row["started_at"],
            "target_version":row["metadata"]["target_version"]})
        })
        .collect();
    let payload = json!({"document_id":document_id,
        "document_revision":document["revision"],
        "exchange_refs":exchange_refs,"matches":matches,"differences":differences,
        "missing_declared_operations":missing_declared_operations,
        "created_at":now(),"difference_is_not_vulnerability":true});
    let parent_id = format!("{document_id}:comparison");
    super::super::save_research_versioned(
        s,
        task_id,
        "openapi_comparison",
        &parent_id,
        super::super::research_base_revision(params)?,
        payload,
        HEADS,
        APPLIED,
        None,
        &[],
    )
}
