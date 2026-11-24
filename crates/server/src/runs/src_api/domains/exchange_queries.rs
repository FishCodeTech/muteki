//! Task-scoped HTTP Exchange reads, comparisons and exports.
use super::*;
use sha2::{Digest, Sha256};
use std::io::Write;
mod diff;

fn bytes(s: &AppState, task_id: &str, reference: &Value) -> Api<Vec<u8>> {
    let saved = verify_ref(s, task_id, reference)?;
    base64::engine::general_purpose::STANDARD
        .decode(saved["data"].as_str().unwrap_or(""))
        .map_err(|_| {
            Error::new(
                StatusCode::CONFLICT,
                "src.artifact_corrupt",
                "HTTP artifact is unreadable",
            )
        })
}

pub(super) fn split_message(raw: &[u8]) -> Api<(&[u8], &[u8], &'static str)> {
    if let Some(index) = raw.windows(4).position(|part| part == b"\r\n\r\n") {
        return Ok((&raw[..index], &raw[index + 4..], "\r\n\r\n"));
    }
    if let Some(index) = raw.windows(2).position(|part| part == b"\n\n") {
        return Ok((&raw[..index], &raw[index + 2..], "\n\n"));
    }
    Err(Error::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "capture_partial",
        "HTTP headers are incomplete",
    ))
}

fn byte_field(raw: &[u8]) -> Value {
    json!({"base64":base64::engine::general_purpose::STANDARD.encode(raw),
        "size":raw.len(),"sha256":format!("{:x}",Sha256::digest(raw))})
}

pub(super) fn parsed_request(raw: &[u8], scheme: &str) -> Api<Value> {
    let (head, body, separator) = split_message(raw)?;
    let (method, url, headers) = parse_raw_request(raw, scheme)?;
    let start = head.iter().map(|byte|char::from(*byte)).collect::<String>()
        .lines()
        .next()
        .unwrap_or("")
        .trim_end_matches('\r')
        .to_owned();
    let parts: Vec<_> = start.split(' ').collect();
    let parsed = parse_url(&url)?;
    let query_parameters: Vec<_> = parsed
        .query_pairs()
        .map(|(name, value)| json!([name, value]))
        .collect();
    Ok(
        json!({"method":method,"target":parts.get(1).copied().unwrap_or(""),
        "version":parts.get(2).copied().unwrap_or(""),"headers":headers,
        "body":byte_field(body),"url":url,"host":parsed.host_str(),
        "port":parsed.port_or_known_default(),"scheme":parsed.scheme(),"path":super::raw_endpoint_path(&url),
        "query":parsed.query().unwrap_or(""),"query_parameters":query_parameters,
        "header_separator":separator}),
    )
}

pub(super) fn parsed_response(raw: &[u8], method: &str) -> Api<Value> {
    let mut remaining = raw;
    let mut interim = Vec::new();
    let mut consumed = 0;
    loop {
        let (head, body, separator) = split_message(remaining)?;
        let text = head.iter().map(|byte|char::from(*byte)).collect::<String>();
        let mut lines = text.lines();
        let start = lines.next().unwrap_or("").trim_end_matches('\r');
        let mut parts = start.splitn(3, ' ');
        let version = parts.next().unwrap_or("");
        let status_text=parts.next().unwrap_or("");
        if !matches!(version,"HTTP/1.0"|"HTTP/1.1")||status_text.len()!=3||!status_text.bytes().all(|value|value.is_ascii_digit()){return Err(Error::new(StatusCode::UNPROCESSABLE_ENTITY,"capture_unavailable","invalid HTTP response status line"));}
        let status = status_text.parse::<u16>().map_err(|_| {
            Error::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "capture_unavailable",
                "invalid HTTP response status line",
            )
        })?;
        let reason = parts.next().unwrap_or("");
        let mut headers=Vec::new();for line in lines{let line=line.trim_end_matches('\r');if line.starts_with([' ','\t']){return Err(Error::invalid("folded HTTP response header"));}let(name,value)=line.split_once(':').ok_or_else(||Error::invalid("malformed HTTP response header"))?;if name.is_empty()||name.chars().any(|value|(value as u32)<=32||value=='\u{7f}'){return Err(Error::invalid("invalid HTTP response header name"));}headers.push(json!([name,value.trim_start_matches([' ','\t'])]));}
        consumed += head.len() + separator.len();
        if (100..200).contains(&status) && status != 101 && !body.is_empty() {
            interim.push(
                json!({"version":version,"status_code":status,"reason":reason,"headers":headers}),
            );
            remaining = body;
            continue;
        }
        return Ok(
            json!({"version":version,"status_code":status,"reason":reason,"headers":headers,
            "body":byte_field(body),"body_offset":consumed,
            "method":method.to_uppercase(),"header_separator":separator,"interim_responses":interim}),
        );
    }
}

fn raw_parts(raw: &[u8]) -> Api<(String, Vec<(String, String)>, Vec<u8>)> {
    let (head, body, _) = split_message(raw)?;
    let text = head.iter().map(|byte|char::from(*byte)).collect::<String>();
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("").trim_end_matches('\r').to_owned();
    let headers = lines
        .filter_map(|line| line.trim_end_matches('\r').split_once(':'))
        .map(|(name, value)| (name.to_owned(), value.trim_start().to_owned()))
        .collect();
    Ok((first, headers, body.to_vec()))
}

fn matches(s: &AppState, task_id: &str, exchange: &Value, filters: &Value) -> Api<bool> {
    let value = |name: &str| filters[name].as_str().filter(|part| !part.is_empty());
    let status = if let Some(value) = filters["status_code"].as_i64() {
        Some(value)
    } else if let Some(value) = filters["status_code"].as_str() {
        Some(
            value
                .parse::<i64>()
                .map_err(|_| Error::invalid("invalid status_code filter"))?,
        )
    } else {
        None
    };
    if value("method").is_some_and(|part| {
        !exchange["method"]
            .as_str()
            .is_some_and(|method| method.eq_ignore_ascii_case(part))
    }) || value("url").is_some_and(|part| exchange["observed_url"] != part)
        || value("context_id").is_some_and(|part| exchange["context_id"] != part)
        || value("source_kind").is_some_and(|part| exchange["source_kind"] != part)
        || value("capture_state").is_some_and(|part| exchange["capture_state"] != part)
        || value("execution_state").is_some_and(|part| exchange["execution_state"] != part)
        || value("tag").is_some_and(|part| {
            !exchange["tags"]
                .as_array()
                .is_some_and(|tags| tags.iter().any(|tag| tag == part))
        })
        || value("media_type").is_some_and(|part| exchange["metadata"]["media_type"] != part)
        || filters["source_run_id"].is_null() == false
            && exchange["source_run_id"] != filters["source_run_id"]
        || status.is_some_and(|status| exchange["status_code"].as_i64() != Some(status))
    {
        return Ok(false);
    }
    let url = parse_url(exchange["observed_url"].as_str().unwrap_or(""))?;
    if value("host").is_some_and(|host| {
        !url.host_str()
            .is_some_and(|actual| actual.eq_ignore_ascii_case(host))
    }) || value("path").is_some_and(|path| url.path() != path)
    {
        return Ok(false);
    }
    for (name, ordering) in [("started_after", true), ("started_before", false)] {
        if let Some(bound) = value(name) {
            let actual = exchange["started_at"]
                .as_str()
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok());
            let bound = chrono::DateTime::parse_from_rfc3339(bound)
                .map_err(|_| Error::invalid(format!("{name} must be a date-time")))?;
            if actual
                .is_some_and(|actual| (ordering && actual < bound) || (!ordering && actual > bound))
            {
                return Ok(false);
            }
        }
    }
    let parameter = value("parameter").or_else(|| value("parameter_name"));
    let body_text = value("body_text");
    if parameter.is_none() && body_text.is_none() {
        return Ok(true);
    }
    let raw = bytes(s, task_id, &exchange["request_raw_ref"])?;
    let (_, headers, body) = raw_parts(&raw)?;
    if let Some(parameter) = parameter {
        let location = value("parameter_location").unwrap_or("any");
        let query = (location == "any" || location == "query")
            && url.query_pairs().any(|(name, _)| name == parameter);
        let form = (location == "any" || location == "body")
            && reqwest::Url::parse(&format!(
                "http://localhost/?{}",
                String::from_utf8_lossy(&body)
            ))
            .ok()
            .is_some_and(|form| form.query_pairs().any(|(name, _)| name == parameter));
        let header = (location == "any" || location == "header")
            && headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case(parameter));
        if !(query || form || header) {
            return Ok(false);
        }
    }
    if let Some(term) = body_text {
        let media = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| {
                value
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase()
            })
            .unwrap_or_default();
        let textual = !body.contains(&0)
            && (media.starts_with("text/")
                || matches!(
                    media.as_str(),
                    "application/json"
                        | "application/xml"
                        | "application/javascript"
                        | "application/x-www-form-urlencoded"
                )
                || media.ends_with("+json")
                || media.ends_with("+xml")
                || media.ends_with("+text"));
        if !textual || !String::from_utf8_lossy(&body).contains(term) {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) fn query_exchanges(s: &AppState, task_id: &str, params: &Value) -> Api<Value> {
    let _ = super::sync_inspection(s, task_id);
    let filters = if params["filters"].is_null() {
        json!({})
    } else {
        params["filters"].clone()
    };
    if !filters.is_object() {
        return Err(Error::invalid("filters must be an object"));
    }
    let mut rows = Vec::new();
    for (rowid, row) in records_with_rowid(s, task_id, "http_exchange")? {
        if matches(s, task_id, &row["payload"], &filters)? {
            rows.push((rowid, row));
        }
    }
    let total = rows.len();
    let cursor = match params.get("cursor") {
        None | Some(Value::Null) => "",
        Some(Value::String(value)) => value.as_str(),
        _ => return Err(Error::new(StatusCode::BAD_REQUEST, "query.cursor_invalid", "malformed event cursor")),
    };
    let scope = format!("{:x}", Sha256::digest(filters.to_string().as_bytes()));
    let after = crate::runs::src_api::cursors::after(s, cursor, "src_exchange_query", task_id, &scope)?;
    let limit = params["limit"]
        .as_u64()
        .or_else(|| {
            params["limit"]
                .as_str()
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(50)
        .clamp(1, 200) as usize;
    let mut matching: Vec<_> = rows.into_iter().filter(|(rowid, _)| *rowid > after)
        .take(limit + 1).collect();
    let has_more = matching.len() > limit;
    matching.truncate(limit);
    let next = if has_more {
        matching.last().map(|(rowid, _)| crate::runs::src_api::cursors::signed(
            s, "src_exchange_query", task_id, *rowid, &scope)).transpose()?.unwrap_or_default()
    } else {
        String::new()
    };
    let page: Vec<_> = matching.into_iter().map(|(_, row)| row).collect();
    let watermarks = crate::runs::src_api::source_watermarks(s, task_id)?;
    let complete = !watermarks.iter().any(|row| row["state"] == "failed");
    Ok(json!({"items":page,"total":total,"returned":page.len(),"next_cursor":next,
        "filters":filters,"semantic_version":"HttpExchange.query.v1",
        "watermarks":watermarks,"complete":complete}))
}

pub(super) fn read_draft(s: &AppState, task_id: &str, params: &Value) -> Api<Value> {
    let draft_id = require(params, "draft_id")?;
    let row = rec(s, task_id, "request_draft", draft_id)?;
    let draft = &row["payload"];
    if params["revision"]
        .as_u64()
        .is_some_and(|revision| draft["revision"] != revision)
    {
        return Err(Error::conflict("request draft revision is stale"));
    }
    let raw = bytes(s, task_id, &draft["raw_request_ref"])?;
    let scheme = draft["target_url"]
        .as_str()
        .and_then(|url| parse_url(url).ok())
        .map(|url| url.scheme().to_owned())
        .unwrap_or_else(|| "http".to_owned());
    let structured = parsed_request(&raw, &scheme)?;
    if params["include_bytes"] == false {
        Ok(
            json!({"draft":draft,"record_revision":row["revision"],"raw_request":null,
            "raw_request_base64":base64::engine::general_purpose::STANDARD.encode(raw),"structured":structured}),
        )
    } else {
        Ok(
            json!({"draft":draft,"record_revision":row["revision"],"raw_request":byte_field(&raw),
            "raw_request_base64":null,"structured":structured}),
        )
    }
}

fn unified_diff(left: Option<&[u8]>, right: Option<&[u8]>, side: &str) -> String {
    let (Some(left), Some(right)) = (left, right) else {
        return if left.is_none() {
            "baseline unavailable"
        } else {
            "experiment unavailable"
        }
        .to_owned();
    };
    diff::unified(left,right,side)
}

fn side_diff(left: Option<&[u8]>, right: Option<&[u8]>, side: &str) -> Value {
    json!({"baseline_sha256":left.map(|bytes| format!("{:x}",Sha256::digest(bytes))),
        "experiment_sha256":right.map(|bytes| format!("{:x}",Sha256::digest(bytes))),
        "baseline_size":left.map(<[u8]>::len),"experiment_size":right.map(<[u8]>::len),
        "unified_diff":unified_diff(left,right,side),"identical":left==right})
}

fn explicit_port(raw: &str) -> Option<u16> {
    let authority = raw.split_once("://")?.1
        .split(['/', '?', '#']).next()?;
    let port = if authority.starts_with('[') {
        authority.split_once(']')?.1.strip_prefix(':')?
    } else {
        authority.rsplit_once(':')?.1
    };
    port.parse().ok()
}

fn remove_pointer(value: &mut Value, pointer: &str) {
    let parts: Vec<_> = pointer
        .split('/')
        .filter(|part| !part.is_empty())
        .map(|part| part.replace("~1", "/").replace("~0", "~"))
        .collect();
    let Some((last, parents)) = parts.split_last() else {
        return;
    };
    let mut parent = value;
    for part in parents {
        parent = match parent {
            Value::Object(map) => match map.get_mut(part) {
                Some(next) => next,
                None => return,
            },
            Value::Array(items) => match part
                .parse::<usize>()
                .ok()
                .and_then(|index| items.get_mut(index))
            {
                Some(next) => next,
                None => return,
            },
            _ => return,
        };
    }
    match parent {
        Value::Object(map) => {
            map.remove(last);
        }
        Value::Array(items) => {
            if let Ok(index) = last.parse::<usize>() {
                if index < items.len() {
                    items.remove(index);
                }
            }
        }
        _ => {}
    }
}

fn normalized(raw: Option<&[u8]>, rules: &Value, request: bool) -> Option<Vec<u8>> {
    let raw = raw?;
    let mut parsed = match if request { parsed_request(raw, "http") }
        else { parsed_response(raw, "") } {
        Ok(value) => value,
        Err(_) => return Some(python_json_default(&byte_field(raw)).into_bytes()),
    };
    let ignored: Vec<_> = rules["headers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_ascii_lowercase)
        .collect();
    if let Some(headers) = parsed["headers"].as_array_mut() {
        headers.retain(|header| !header[0].as_str()
            .is_some_and(|name| ignored.contains(&name.to_ascii_lowercase())));
    }
    if rules["json_pointers"].as_array().is_some_and(|items| !items.is_empty()) {
        if let Some(body) = parsed["body"]["base64"].as_str()
            .and_then(|encoded| base64::engine::general_purpose::STANDARD.decode(encoded).ok()) {
            if let Ok(mut json_body) = serde_json::from_slice::<Value>(&body) {
                for pointer in rules["json_pointers"]
                    .as_array().into_iter().flatten().filter_map(Value::as_str) {
                    remove_pointer(&mut json_body, pointer);
                }
                parsed["body"] = json_body;
            }
        }
    }
    Some(python_json_default(&parsed).into_bytes())
}

pub(super) fn compare_exchanges(s: &AppState, task_id: &str, params: &Value) -> Api<Value> {
    let baseline_id = require(params, "baseline_id")?;
    let experiment_id = require(params, "experiment_id")?;
    let baseline = super::find_exchange_record(s, task_id, baseline_id)?["payload"].clone();
    let experiment = super::find_exchange_record(s, task_id, experiment_id)?["payload"].clone();
    let before_request = bytes(s, task_id, &baseline["request_raw_ref"])?;
    let after_request = bytes(s, task_id, &experiment["request_raw_ref"])?;
    let before_response = if baseline["response_raw_ref"].is_object() {
        Some(bytes(s, task_id, &baseline["response_raw_ref"])?)
    } else {
        None
    };
    let after_response = if experiment["response_raw_ref"].is_object() {
        Some(bytes(s, task_id, &experiment["response_raw_ref"])?)
    } else {
        None
    };
    let rules = if params["ignore_rules"].is_object() {
        params["ignore_rules"].clone()
    } else {
        json!({})
    };
    let request_diff = side_diff(Some(&before_request), Some(&after_request), "request");
    let response_diff = side_diff(
        before_response.as_deref(),
        after_response.as_deref(),
        "response",
    );
    let normalized_before_request = normalized(Some(&before_request), &rules, true);
    let normalized_after_request = normalized(Some(&after_request), &rules, true);
    let normalized_before_response = normalized(before_response.as_deref(), &rules, false);
    let normalized_after_response = normalized(after_response.as_deref(), &rules, false);
    let mut dimensions = Vec::new();
    let mut unknown = Vec::new();
    let before_ctx = if params["baseline_context"].is_object() {
        params["baseline_context"].clone()
    } else {
        json!({})
    };
    let after_ctx = if params["experiment_context"].is_object() {
        params["experiment_context"].clone()
    } else {
        json!({})
    };
    let before_identity = before_ctx
        .get("identity")
        .unwrap_or(&baseline["context_id"]);
    let after_identity = after_ctx
        .get("identity")
        .unwrap_or(&experiment["context_id"]);
    if before_identity != after_identity {
        dimensions.push("identity");
    }
    for name in [
        "tenant",
        "object_version",
        "target_version",
        "preconditions",
    ] {
        if before_ctx[name].is_null() && after_ctx[name].is_null() {
            unknown.push(name);
        } else if before_ctx[name] != after_ctx[name] {
            dimensions.push(name);
        }
    }
    if baseline["method"] != experiment["method"] {
        dimensions.push("method");
    }
    let before_url = parse_url(baseline["observed_url"].as_str().unwrap_or(""))?;
    let after_url = parse_url(experiment["observed_url"].as_str().unwrap_or(""))?;
    if before_url.host_str() != after_url.host_str()
        || explicit_port(baseline["observed_url"].as_str().unwrap_or(""))
            != explicit_port(experiment["observed_url"].as_str().unwrap_or(""))
    {
        dimensions.push("host");
    }
    if before_url.path() != after_url.path() {
        dimensions.push("path");
    }
    if before_url.query() != after_url.query() {
        dimensions.push("query");
    }
    if baseline["metadata"]["request_body_sha256"]
        != experiment["metadata"]["request_body_sha256"] {
        dimensions.push("body");
    }
    let warnings = if dimensions.len() > 1 {
        vec!["身份、租户、对象、目标版本或前置条件同时变化；这不是单变量对照"]
    } else {
        Vec::new()
    };
    let structural_json = match (before_response.as_deref(), after_response.as_deref()) {
        (Some(lb), Some(rb)) => {
            let extract_json = |raw: &[u8], method: &str| -> Option<Value> {
                let parsed = parsed_response(raw, method).ok()?;
                let (body, limits) = decoded_body(&parsed, method).ok()?;
                if !limits.is_empty() {
                    return None;
                }
                serde_json::from_slice(&body).ok()
            };
            match (
                extract_json(lb, baseline["method"].as_str().unwrap_or("GET")),
                extract_json(rb, experiment["method"].as_str().unwrap_or("GET")),
            ) {
                (Some(lj), Some(rj)) => diff::json_structural_diff(&lj, &rj),
                _ => Value::Null,
            }
        }
        _ => Value::Null,
    };
    let comparison_id = format!("exchange-comparison-{}", Uuid::new_v4().simple());
    let mut result = json!({"baseline_id":baseline_id,"experiment_id":experiment_id,
        "raw":{"request":request_diff,"response":response_diff},
        "structural_json": structural_json,
        "normalized":{"request":side_diff(normalized_before_request.as_deref(),normalized_after_request.as_deref(),"normalized"),
            "response":side_diff(normalized_before_response.as_deref(),normalized_after_response.as_deref(),"normalized")},
        "ignore_rules":rules,"changed_dimensions":dimensions,"unknown_dimensions":unknown,
        "single_variable":dimensions.len()<=1,"warnings":warnings,
        "context":{"baseline":before_ctx,"experiment":after_ctx},"conclusion":"diff_only"});
    let row = save_rec(
        s,
        task_id,
        "exchange_comparison",
        &comparison_id,
        result.clone(),
        Some(0),
    )?;
    result["comparison_id"] = json!(comparison_id);
    result["record_revision"] = row["revision"].clone();
    Ok(result)
}

fn har_entry(exchange: &Value, request_raw: &[u8], response_raw: Option<&[u8]>) -> Api<Value> {
    let scheme = parse_url(exchange["observed_url"].as_str().unwrap_or(""))?
        .scheme()
        .to_owned();
    let request = parsed_request(request_raw, &scheme)?;
    let (_, _, request_body) = raw_parts(request_raw)?;
    let response = if exchange["capture_state"] == "complete" {
        response_raw
            .map(|raw| parsed_response(raw, exchange["method"].as_str().unwrap_or("")))
            .transpose()?
    } else {
        None
    };
    let response_headers = response
        .as_ref()
        .map(|value| value["headers"].as_array().cloned().unwrap_or_default())
        .unwrap_or_else(||exchange["response_headers"].as_array().cloned().unwrap_or_default());
    let mime = |headers: &[Value]| {
        headers
            .iter()
            .find_map(|header| {
                let row = header.as_array()?;
                if !row.first()?.as_str()?.eq_ignore_ascii_case("content-type") {
                    return None;
                }
                Some(
                    row.get(1)?
                        .as_str()?
                        .split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_owned(),
                )
            })
            .unwrap_or_default()
    };
    let response_body = response
        .as_ref()
        .map(|value| value["body"].clone())
        .unwrap_or(Value::Null);
    let request_headers = request["headers"].as_array().cloned().unwrap_or_default();
    let request_header_objects: Vec<_> = request_headers
        .iter()
        .filter_map(|row| Some(json!({"name":row.get(0)?,"value":row.get(1)?})))
        .collect();
    let response_header_objects: Vec<_> = response_headers
        .iter()
        .filter_map(|row| Some(json!({"name":row.get(0)?,"value":row.get(1)?})))
        .collect();
    let query_parameters: Vec<_> = request["query_parameters"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| Some(json!({"name":row.get(0)?,"value":row.get(1)?})))
        .collect();
    let redirect = response_headers
        .iter()
        .find_map(|row| {
            let items = row.as_array()?;
            if !items.first()?.as_str()?.eq_ignore_ascii_case("location") {
                return None;
            }
            Some(items.get(1)?.as_str().unwrap_or("").to_owned())
        })
        .unwrap_or_default();
    let time = exchange["started_at"]
        .as_str()
        .and_then(|start| chrono::DateTime::parse_from_rfc3339(start).ok())
        .zip(
            exchange["finished_at"]
                .as_str()
                .and_then(|end| chrono::DateTime::parse_from_rfc3339(end).ok()),
        )
        .map(|(start, end)| (end - start).num_milliseconds())
        .unwrap_or(0);
    Ok(json!({"startedDateTime":exchange["started_at"],"time":time,
        "request":{"method":exchange["method"],"url":exchange["observed_url"],"httpVersion":request["version"],
            "cookies":[],"headers":request_header_objects,"queryString":query_parameters,
            "postData":{"mimeType":mime(&request_headers),"text":base64::engine::general_purpose::STANDARD.encode(&request_body),"encoding":"base64"},
            "headersSize":-1,"bodySize":request_body.len()},
        "response":{"status":exchange["status_code"].as_u64().unwrap_or(0),
            "statusText":response.as_ref().map(|value|value["reason"].clone()).unwrap_or_else(||json!("")),
            "httpVersion":response.as_ref().map(|value|value["version"].clone()).unwrap_or_else(||json!("")),
            "cookies":[],"headers":response_header_objects,
            "content":{"size":response_body["size"].as_u64().unwrap_or(0),"mimeType":mime(&response_headers),
                "text":response_body["base64"],"encoding":"base64"},
            "redirectURL":redirect,"headersSize":-1,"bodySize":if response.is_some(){response_body["size"].as_i64().unwrap_or(0)}else{-1}},
        "cache":{},"timings":{"send":0,"wait":0,"receive":0},
        "_muteki":{"exchange_id":exchange["exchange_id"],"capture_state":exchange["capture_state"],
            "request_raw_ref":exchange["request_raw_ref"],"response_raw_ref":exchange["response_raw_ref"],
            "raw_representation":exchange["metadata"]["raw_representation"],
            "note":"HAR is an exchange representation, not proof of original wire bytes"}}))
}

pub(super) fn export_exchanges(s: &AppState, task_id: &str, params: &Value) -> Api<Value> {
    let filters = if params["filters"].is_object() {
        params["filters"].clone()
    } else {
        json!({})
    };
    let ids: Vec<String> = if let Some(selected) = params["exchange_ids"].as_array() {
        let mut ids = Vec::new();
        for value in selected {
            let id = value
                .as_str()
                .ok_or_else(|| Error::invalid("exchange_ids must contain IDs"))?;
            if !ids.iter().any(|seen| seen == id) {
                ids.push(id.to_owned());
            }
        }
        ids
    } else {
        let mut selected = Vec::new();
        for row in records(s, task_id, "http_exchange")? {
            if matches(s, task_id, &row["payload"], &filters)? {
                selected.push(
                    row["payload"]["exchange_id"]
                        .as_str()
                        .unwrap_or("")
                        .to_owned(),
                );
            }
        }
        selected
    };
    let mut omissions = Vec::new();
    let mut collected = Vec::new();
    for exchange_id in &ids {
        let item = (|| -> Api<(Value, Vec<u8>, Option<Vec<u8>>)> {
            let row = rec(s, task_id, "http_exchange", exchange_id)?;
            let exchange = row["payload"].clone();
            let request = bytes(s, task_id, &exchange["request_raw_ref"])?;
            let response = if exchange["response_raw_ref"].is_object() {
                Some(bytes(s, task_id, &exchange["response_raw_ref"])?)
            } else {
                None
            };
            Ok((exchange, request, response))
        })();
        match item {
            Ok(item) => collected.push(item),
            Err(error) => omissions
                .push(json!({"exchange_id":exchange_id,"code":error.code,"message":error.message})),
        }
    }
    let partial = !omissions.is_empty();
    if partial && params["allow_partial"] != true {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "artifact_missing",
            format!("exchange export is incomplete: {omissions:?}"),
        ));
    }
    let format = params["format"].as_str().unwrap_or("jsonl");
    let mut manifest = json!({"schema":"muteki.src.exchange-export.v1","task_id":task_id,
        "format":format,"partial":partial,"expected":ids.len(),"actual":collected.len(),
        "omissions":omissions,"filters":filters,"entries":[]});
    let (content, media, filename): (Vec<u8>, &str, String) = match format {
        "jsonl" => {
            let mut lines = String::new();
            for (exchange, request, response) in &collected {
                lines.push_str(&json!({"exchange":exchange,
                    "request_raw_base64":base64::engine::general_purpose::STANDARD.encode(request),
                    "response_raw_base64":response.as_ref().map(|bytes|base64::engine::general_purpose::STANDARD.encode(bytes))}).to_string());
                lines.push('\n');
            }
            (
                lines.into_bytes(),
                "application/x-ndjson",
                "src-exchanges.jsonl".to_owned(),
            )
        }
        "har" => {
            let mut entries = Vec::new();
            for (exchange, request, response) in &collected {
                entries.push(har_entry(exchange, request, response.as_deref())?);
            }
            let har = json!({"log":{"version":"1.2","creator":{"name":"Muteki","version":"HttpExchange.v1"},"entries":entries}});
            (
                serde_json::to_vec_pretty(&har).map_err(super::db)?,
                "application/har+json",
                "src-exchanges.har".to_owned(),
            )
        }
        "raw" if collected.len() == 1 => {
            let (exchange, request, _) = &collected[0];
            (
                request.clone(),
                "message/http",
                format!(
                    "{}-request.http",
                    exchange["exchange_id"].as_str().unwrap_or("exchange")
                ),
            )
        }
        "raw" | "zip" => {
            let mut files = Vec::new();
            for (index, (exchange, request, response)) in collected.iter().enumerate() {
                let exchange_id = exchange["exchange_id"].as_str().unwrap_or("");
                for (direction, bytes, reference) in [
                    ("request", Some(request), &exchange["request_raw_ref"]),
                    ("response", response.as_ref(), &exchange["response_raw_ref"]),
                ] {
                    let Some(bytes) = bytes else { continue };
                    let path = format!("exchanges/{:04}-{exchange_id}/{direction}.http", index + 1);
                    manifest["entries"].as_array_mut().unwrap().push(json!({"path":path,"exchange_id":exchange_id,
                        "artifact_id":reference["artifact_id"],"size":bytes.len(),"media_type":"message/http",
                        "original":matches!(exchange["protocol"].as_str(),Some("http/1.0"|"http/1.1"))}));
                    files.push((path, bytes));
                }
            }
            let mut archive = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (path, bytes) in files {
                archive.start_file(path, options).map_err(super::db)?;
                archive.write_all(bytes).map_err(super::db)?;
            }
            archive
                .start_file("manifest.json", options)
                .map_err(super::db)?;
            archive
                .write_all(&serde_json::to_vec_pretty(&manifest).map_err(super::db)?)
                .map_err(super::db)?;
            (
                archive.finish().map_err(super::db)?.into_inner(),
                "application/zip",
                format!("src-exchanges.{format}"),
            )
        }
        _ => {
            return Err(Error::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "capability_unavailable",
                format!("unsupported exchange export format {format}"),
            ));
        }
    };
    manifest["content_size"] = json!(content.len());
    Ok(
        json!({"content_base64":base64::engine::general_purpose::STANDARD.encode(&content),
        "size":content.len(),"media_type":media,"filename":filename,"manifest":manifest,"partial":partial}),
    )
}

pub(super) fn authentication_header(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(),
        "authorization" | "proxy-authorization" | "cookie" | "cookie2"
        | "x-api-key" | "x-auth-token" | "x-token" | "satoken")
}

fn validate_probe_identity(base_raw: &[u8], source_context: &str, context: &str, variant: &Value) -> Api<()> {
    let replacements = variant["replace_headers"].as_object();
    if variant["drop_auth"] == true {
        if replacements.is_some_and(|headers| headers.iter().any(|(name, value)|
            authentication_header(name) && value.as_str().is_some_and(|value| !value.is_empty()))) {
            return Err(Error::invalid("anonymous probe cannot add authentication headers"));
        }
        return Ok(());
    }
    if context == source_context { return Ok(()); }
    let (_, headers, _) = raw_parts(base_raw)?;
    let auth: Vec<_> = headers.iter().filter(|(name, _)| authentication_header(name)).collect();
    let replacement = |name: &str| replacements.and_then(|headers| headers.iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name)).and_then(|(_, value)| value.as_str()));
    if auth.iter().any(|(name, old)| replacement(name).is_none_or(|new| new == old))
        || !replacements.is_some_and(|headers| headers.iter().any(|(name, value)|
            authentication_header(name) && value.as_str().is_some_and(|value| !value.is_empty()))) {
        return Err(Error::new(StatusCode::CONFLICT, "identity_credentials_required",
            "context_id does not load credentials: changing identity requires explicit replacement of every source authentication header"));
    }
    Ok(())
}

fn build_mutated_raw_request(base_raw: &[u8], variant: &Value) -> Api<Vec<u8>> {
    let (start_line, mut headers, mut body) = raw_parts(base_raw)?;
    let mut parts: Vec<String> = start_line.split_whitespace().map(str::to_owned).collect();
    if parts.len() < 3 {
        return Err(Error::invalid("base raw HTTP request has invalid start line"));
    }
    if let Some(m) = variant["method"].as_str().filter(|v| !v.is_empty()) {
        parts[0] = m.to_ascii_uppercase();
    }
    if let Some(path) = variant["replace_path"].as_str().filter(|v| !v.is_empty()) {
        parts[1] = path.to_owned();
    }
    if variant["drop_auth"] == true {
        headers.retain(|(name, _)| !authentication_header(name));
    }
    if let Some(replacements) = variant["replace_headers"].as_object() {
        for (h_name, h_val) in replacements {
            headers.retain(|(existing, _)| !existing.eq_ignore_ascii_case(h_name));
            if let Some(val_str) = h_val.as_str() {
                headers.push((h_name.clone(), val_str.to_owned()));
            }
        }
    }
    if let Some(b_text) = variant["replace_body"].as_str() {
        body = b_text.as_bytes().to_vec();
        headers.retain(|(name, _)| !name.eq_ignore_ascii_case("content-length"));
        headers.push(("Content-Length".to_owned(), body.len().to_string()));
    }
    let mut wire = format!("{}\r\n", parts.join(" ")).into_bytes();
    for (k, v) in headers {
        wire.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    wire.extend_from_slice(b"\r\n");
    wire.extend_from_slice(&body);
    Ok(wire)
}

pub(in crate::runs::src_api::domains) async fn differential_probe(
    s: std::sync::Arc<AppState>,
    task_id: &str,
    body: &Value,
) -> Api<Value> {
    let source_exchange_id = require(body, "source_exchange_id")?;
    let source_row = rec(&s, task_id, "http_exchange", source_exchange_id)?;
    let base_raw = bytes(&s, task_id, &source_row["payload"]["request_raw_ref"])?;
    let variants = body["variants"]
        .as_array()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::invalid("variants must be a non-empty array"))?;
    if variants.len() > 8 {
        return Err(Error::invalid("variants cannot exceed 8 per probe"));
    }
    let current_policy = super::policy(&s, task_id)?;
    let auth_ver = current_policy["version"].as_u64().unwrap_or(1);
    let baseline_context = body["baseline_context_id"]
        .as_str()
        .filter(|v| !v.is_empty())
        .or_else(|| source_row["payload"]["context_id"].as_str())
        .unwrap_or("default");

    let source_context = source_row["payload"]["context_id"].as_str().filter(|v| !v.is_empty()).unwrap_or("default");
    if baseline_context != source_context {
        return Err(Error::new(StatusCode::CONFLICT, "identity_credentials_required",
            "baseline_context_id must match the captured exchange identity; choose a matching source exchange"));
    }
    // Validate all identity mutations before even sending the baseline request.
    let mut prepared_variants = Vec::new();
    for (idx, variant) in variants.iter().enumerate() {
        let label = variant["label"].as_str().filter(|v| !v.is_empty())
            .map(str::to_owned).unwrap_or_else(|| format!("variant_{}", idx + 1));
        let context = if variant["drop_auth"] == true { "anonymous" } else {
            variant["context_id"].as_str().filter(|v| !v.is_empty()).unwrap_or(baseline_context)
        };
        validate_probe_identity(&base_raw, source_context, context, variant)?;
        prepared_variants.push((label, context.to_owned(), build_mutated_raw_request(&base_raw, variant)?, variant["drop_auth"] == true));
    }

    let run_single = |raw_bytes: Vec<u8>, ctx_id: String, label: String| {
        let s = s.clone();
        let tid = task_id.to_owned();
        let src_eid = source_exchange_id.to_owned();
        let run_id = body["source_run_id"].clone();
        async move {
            let encoded = base64::engine::general_purpose::STANDARD.encode(&raw_bytes);
            let draft_out = super::create_next_draft(
                &s,
                &tid,
                &src_eid,
                Some(&encoded),
                json!({ "differential_probe_label": label }),
                false,
            )?;
            let draft_id = draft_out["draft"]["draft_id"]
                .as_str()
                .unwrap_or("")
                .to_owned();
            let draft_rev = draft_out["draft"]["revision"].as_u64().unwrap_or(1);
            let idem = format!("diffprobe-{}-{}", Uuid::new_v4().simple(), label);
            let exec_out = super::execute_draft(
                s.clone(),
                &tid,
                &json!({
                    "draft_id": draft_id,
                    "draft_revision": draft_rev,
                    "authorization_version": auth_ver,
                    "context_id": ctx_id,
                    "identity_ref": ctx_id,
                    "source_run_id": run_id,
                    "idempotency_key": idem,
                }),
            )
            .await?;
            Ok::<Value, Error>(exec_out["output"].clone())
        }
    };

    let baseline_exec = run_single(
        base_raw.clone(),
        baseline_context.to_owned(),
        "baseline".to_owned(),
    )
    .await?;
    let baseline_eid = baseline_exec["exchange_id"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            Error::conflict("baseline differential probe did not complete with an exchange_id")
        })?
        .to_owned();

    let mut variant_results = Vec::new();
    for (label, v_ctx, mutated_raw, drop_auth) in prepared_variants {
        let v_exec = run_single(mutated_raw, v_ctx.clone(), label.clone()).await?;
        let mut comparison = Value::Null;
        if let Some(v_eid) = v_exec["exchange_id"].as_str().filter(|v| !v.is_empty()) {
            comparison = compare_exchanges(
                &s,
                task_id,
                &json!({
                    "baseline_id": baseline_eid,
                    "experiment_id": v_eid,
                    "context": {
                        "baseline": { "identity": baseline_context },
                        "experiment": { "identity": v_ctx }
                    }
                }),
            )
            .unwrap_or(Value::Null);
        }
        variant_results.push(json!({
            "label": label,
            "context_id": v_ctx,
            "drop_auth": drop_auth,
            "exchange_id": v_exec["exchange_id"],
            "status_code": v_exec["exchange"]["status_code"],
            "comparison_id": comparison["comparison_id"],
            "response_identical": comparison["raw"]["response"]["identical"],
            "baseline_body_size": comparison["raw"]["response"]["baseline_size"],
            "variant_body_size": comparison["raw"]["response"]["experiment_size"],
            "changed_dimensions": comparison["changed_dimensions"],
        }));
    }

    Ok(json!({
        "baseline_exchange_id": baseline_eid,
        "baseline_context_id": baseline_context,
        "baseline_status_code": baseline_exec["exchange"]["status_code"],
        "variants": variant_results,
        "difference_is_not_vulnerability": true
    }))
}

pub(super) fn curl_export(s: &AppState, task_id: &str, params: &Value) -> Api<Value> {
    let exchange_id = require(params, "exchange_id")?;
    let include_live_auth = params["include_live_auth"] == true;
    let ex = rec(s, task_id, "http_exchange", exchange_id)?["payload"].clone();
    let raw = bytes(s, task_id, &ex["request_raw_ref"])?;
    let url = ex["observed_url"].as_str().unwrap_or("");
    let scheme = parse_url(url).map(|u| u.scheme().to_owned()).unwrap_or_else(|_| "https".to_owned());
    let parsed = parsed_request(&raw, &scheme)?;
    let method = parsed["method"].as_str().unwrap_or("GET");
    let ctx_id = ex["context_id"].as_str().unwrap_or("default");

    let sh_escape = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let mut parts = vec!["curl".to_owned(), "-i".to_owned(), "-X".to_owned(), sh_escape(method), sh_escape(url)];
    let mut redacted_headers = Vec::new();
    for pair in parsed["headers"].as_array().into_iter().flatten() {
        let Some(k) = pair.get(0).and_then(Value::as_str) else { continue };
        let Some(v) = pair.get(1).and_then(Value::as_str) else { continue };
        if k.eq_ignore_ascii_case("host") || k.eq_ignore_ascii_case("content-length") {
            continue;
        }
        let header_val = if !include_live_auth && authentication_header(k) {
            redacted_headers.push(k.to_owned());
            format!("secret://src/{task_id}/jar_{ctx_id}#{k}")
        } else {
            v.to_owned()
        };
        parts.push("-H".to_owned());
        parts.push(sh_escape(&format!("{k}: {header_val}")));
    }
    if let Some(b64) = parsed["body"]["base64"].as_str() {
        if let Ok(body_bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
            if !body_bytes.is_empty() {
                parts.push("--data-raw".to_owned());
                parts.push(sh_escape(&String::from_utf8_lossy(&body_bytes)));
            }
        }
    }
    Ok(json!({
        "exchange_id": exchange_id,
        "redacted": !include_live_auth,
        "redacted_headers": redacted_headers,
        "curl": parts.join(" "),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparison_sides_include_source_digests_and_explicit_port_identity() {
        let diff = side_diff(Some(b"abc"), Some(b"abc"), "request");
        assert_eq!(diff["baseline_sha256"],
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(diff["experiment_sha256"],diff["baseline_sha256"]);
        assert_eq!(diff["identical"],true);
        assert!(side_diff(None,Some(b"abc"),"response")["baseline_sha256"].is_null());
        assert_eq!(explicit_port("http://example.com/path"),None);
        assert_eq!(explicit_port("http://example.com:80/path"),Some(80));
        assert_eq!(explicit_port("https://[2001:db8::1]:443/path"),Some(443));
        assert_eq!(explicit_port("https://[2001:db8::1]/path"),None);
    }

    #[test]
    fn normalized_comparison_matches_source_python_samples() {
        let rules = json!({"headers":["x-trace"],"json_pointers":["/nonce"]});
        let request_a = b"POST /items?x=1 HTTP/1.1\r\nHost: example.com\r\nX-Trace: a\r\nContent-Type: application/json\r\n\r\n{\"keep\":1,\"nonce\":\"a\"}";
        let request_b = b"POST /items?x=1 HTTP/1.1\r\nHost: example.com\r\nX-Trace: b\r\nContent-Type: application/json\r\n\r\n{\"keep\":1,\"nonce\":\"b\"}";
        let request = side_diff(normalized(Some(request_a),&rules,true).as_deref(),
            normalized(Some(request_b),&rules,true).as_deref(),"normalized");
        assert_eq!(request["baseline_sha256"],
            "e632e645878d78df2f033b84c3507e301939e5d65c2c7780e6372696ed12313b");
        assert_eq!(request["baseline_size"],354);
        assert_eq!(request["identical"],true);

        let response_a = b"HTTP/1.1 200 OK\r\nX-Trace: a\r\nContent-Type: application/json\r\n\r\n{\"ok\":true,\"nonce\":\"a\"}";
        let response_b = b"HTTP/1.1 200 OK\r\nX-Trace: b\r\nContent-Type: application/json\r\n\r\n{\"ok\":true,\"nonce\":\"b\"}";
        let response = side_diff(normalized(Some(response_a),&rules,false).as_deref(),
            normalized(Some(response_b),&rules,false).as_deref(),"normalized");
        assert_eq!(response["baseline_sha256"],
            "444cec0362814e424abd65001a50dc0f2ac65670732e04353cafd290822db31b");
        assert_eq!(response["baseline_size"],222);
        assert_eq!(response["identical"],true);
    }
}

pub(super) fn decoded_body(parsed:&Value,method:&str)->Api<(Vec<u8>,Vec<String>)>{
    use std::io::Read;
    let mut body=base64::engine::general_purpose::STANDARD.decode(parsed["body"]["base64"].as_str().unwrap_or("")).map_err(|_|Error::invalid("response body encoding invalid"))?;
    let mut limitations=Vec::new();if method=="HEAD"{return Ok((body,limitations));}
    let values=|name:&str|parsed["headers"].as_array().into_iter().flatten().filter(|row|row[0].as_str().is_some_and(|value|value.eq_ignore_ascii_case(name))).filter_map(|row|row[1].as_str()).flat_map(|value|value.split(',').map(|value|value.trim().to_ascii_lowercase())).collect::<Vec<_>>();
    if values("transfer-encoding").last().is_some_and(|value|value=="chunked"){
        let decoded=(||->Result<Vec<u8>,String>{let mut output=Vec::new();let mut offset=0;loop{let tail=body.get(offset..).ok_or("incomplete chunk size")?;let end=tail.iter().position(|byte|*byte==b'\n').ok_or("missing chunk size")?;let raw=std::str::from_utf8(&tail[..end]).map_err(|_|"invalid chunk size")?.trim_end_matches('\r').split(';').next().unwrap_or("");let size=usize::from_str_radix(raw,16).map_err(|_|"invalid chunk size")?;offset+=end+1;if size==0{break;}let end=offset.checked_add(size).ok_or("chunk overflow")?;let data=body.get(offset..end).ok_or("incomplete chunk body")?;if output.len()+data.len()>64*1024*1024{return Err("chunk body exceeds limit".into());}output.extend_from_slice(data);offset=end;let tail=body.get(offset..).ok_or("missing chunk terminator")?;if tail.starts_with(b"\r\n"){offset+=2;}else if tail.starts_with(b"\n"){offset+=1;}else{return Err("missing chunk terminator".into());}}Ok(output)})();
        match decoded{Ok(decoded)=>body=decoded,Err(message)=>limitations.push(format!("chunk_decode_failed:{message}"))}
    }
    for encoding in values("content-encoding").into_iter().rev(){
        let mut decoded=Vec::new();let reader:Box<dyn Read+'_>=match encoding.as_str(){"identity"|""=>continue,"gzip"=>Box::new(flate2::read::GzDecoder::new(body.as_slice())),"deflate"=>Box::new(flate2::read::ZlibDecoder::new(body.as_slice())),_=>{limitations.push(format!("unsupported_content_encoding:{encoding}"));break;}};
        let result=reader.take(64*1024*1024+1).read_to_end(&mut decoded);
        if result.is_err(){limitations.push("content_decode_failed:invalid_compression".into());break;}
        if decoded.len()>64*1024*1024{return Err(Error::new(StatusCode::UNPROCESSABLE_ENTITY,"capture_partial","decompressed body exceeds limit"));}body=decoded;
    }Ok((body,limitations))
}
