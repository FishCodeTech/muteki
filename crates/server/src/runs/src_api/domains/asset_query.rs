//! Source SRC asset query grammar and projections, with opaque artifact IDs.
use super::*;
use base64::Engine;
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use regex::Regex;
use std::{
    collections::{BTreeMap, BTreeSet},
    net::IpAddr,
    sync::OnceLock,
};

pub(super) const APPLIED: &str = "src_asset_query_applied_commands";
const REVISIONS: &str = "src_asset_query_record_revisions";
const QUERY_VERSION: &str = "src-asset-query.v1";
const FIELDS: &[&str] = &[
    "type",
    "company",
    "program",
    "task_id",
    "run_id",
    "domain",
    "ip",
    "cidr",
    "protocol",
    "port",
    "url",
    "method",
    "param_location",
    "param_name",
    "technology",
    "tag",
    "status",
    "source",
    "observed_after",
    "observed_before",
    "ingested_after",
    "ingested_before",
    "freshness",
    "kind",
    "text",
];
fn err(code: &'static str, message: impl Into<String>) -> Error {
    Error::new(StatusCode::BAD_REQUEST, code, message)
}
fn pid(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}
fn body(body: &Value) -> &Value {
    body.get("payload")
        .filter(|v| v.is_object())
        .unwrap_or(body)
}
fn arg<'a>(body: &'a Value, key: &str) -> &'a Value {
    body.get(key)
        .or_else(|| body.get("payload").and_then(|p| p.get(key)))
        .unwrap_or(&Value::Null)
}
fn string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        _ => v.to_string(),
    }
}
fn token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"\s*(>=|<=|!=|[()=~<>]|"(?:\\.|[^"])*"|'(?:\\.|[^'])*'|[^\s()=~<>!]+)"#)
            .unwrap()
    })
}
fn tokenize(expression: &str) -> Api<Vec<String>> {
    let (mut tokens, mut at) = (Vec::new(), 0);
    while at < expression.len() {
        let Some(capture) = token_re().captures_at(expression, at) else {
            if expression[at..].trim().is_empty() {
                break;
            }
            return Err(err("query_invalid", "invalid query expression"));
        };
        let whole = capture.get(0).unwrap();
        if whole.start() != at {
            return Err(err("query_invalid", "invalid query expression"));
        }
        tokens.push(capture.get(1).unwrap().as_str().to_owned());
        at = whole.end();
    }
    Ok(tokens)
}
fn unquote(value: &str) -> Api<String> {
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        return serde_json::from_str(value)
            .map_err(|_| err("query_invalid", "invalid quoted value"));
    }
    if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        return Ok(value[1..value.len() - 1]
            .replace("\\'", "'")
            .replace("\\\\", "\\"));
    }
    Ok(value.to_owned())
}
struct Parser {
    tokens: Vec<String>,
    at: usize,
}
impl Parser {
    fn peek(&self) -> String {
        self.tokens
            .get(self.at)
            .map_or(String::new(), |v| v.to_ascii_uppercase())
    }
    fn take(&mut self) -> Api<String> {
        let v = self
            .tokens
            .get(self.at)
            .cloned()
            .ok_or_else(|| err("query_invalid", "unexpected end of expression"))?;
        self.at += 1;
        Ok(v)
    }
    fn parse(mut self) -> Api<Value> {
        if self.tokens.is_empty() {
            return Ok(json!({"op":"true"}));
        }
        let v = self.or_expr()?;
        if self.at != self.tokens.len() {
            return Err(err(
                "query_invalid",
                format!("unexpected token {}", self.take()?),
            ));
        }
        Ok(v)
    }
    fn or_expr(&mut self) -> Api<Value> {
        let mut v = self.and_expr()?;
        while self.peek() == "OR" {
            self.take()?;
            v = json!({"op":"or","items":[v,self.and_expr()?]});
        }
        Ok(v)
    }
    fn and_expr(&mut self) -> Api<Value> {
        let mut v = self.not_expr()?;
        while self.at < self.tokens.len() && self.peek() != "OR" && self.peek() != ")" {
            if self.peek() == "AND" {
                self.take()?;
            }
            v = json!({"op":"and","items":[v,self.not_expr()?]});
        }
        Ok(v)
    }
    fn not_expr(&mut self) -> Api<Value> {
        if matches!(self.peek().as_str(), "NOT" | "!") {
            self.take()?;
            return Ok(json!({"op":"not","item":self.not_expr()?}));
        }
        self.primary()
    }
    fn primary(&mut self) -> Api<Value> {
        if self.peek() == "(" {
            self.take()?;
            let v = self.or_expr()?;
            if self.take()? != ")" {
                return Err(err("query_invalid", "missing closing parenthesis"));
            }
            return Ok(v);
        }
        let first = self.take()?;
        if matches!(first.as_str(), "(" | ")") {
            return Err(err("query_invalid", "unexpected parenthesis"));
        }
        if matches!(
            self.peek().as_str(),
            "=" | "~" | "!=" | ">" | "<" | ">=" | "<="
        ) {
            let operator = self.take()?;
            return condition(&first, &operator, &unquote(&self.take()?)?, false);
        }
        let raw = unquote(&first)?;
        if let Some(term) = raw.strip_prefix('-').filter(|v| !v.is_empty()) {
            let inner = if let Some((field, value)) = term.split_once(':') {
                condition(field, "=", value, true)?
            } else {
                condition("text", "~", term, true)?
            };
            return Ok(json!({"op":"not","item":inner}));
        }
        if let Some((field, value)) = raw.split_once(':') {
            if matches!(value, "contains" | "exact" | "exclude" | "prefix")
                && self.at < self.tokens.len()
            {
                let op = match value {
                    "contains" => "~",
                    "exact" => "=",
                    "exclude" => "!=",
                    _ => "prefix",
                };
                return condition(field, op, &unquote(&self.take()?)?, false);
            }
            return condition(field, "=", value, true);
        }
        condition("text", "~", &raw, true)
    }
}
fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|v| v.with_timezone(&Utc))
        .or_else(|| {
            NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|v| v.and_utc())
        })
        .or_else(|| {
            NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .ok()
                .and_then(|v| v.and_hms_opt(0, 0, 0))
                .map(|v| v.and_utc())
        })
}
fn condition(field: &str, operator: &str, value: &str, implicit: bool) -> Api<Value> {
    let field = field.to_ascii_lowercase();
    if !FIELDS.contains(&field.as_str()) {
        return Err(err(
            "query_field_unknown",
            format!("unknown query field {field}"),
        ));
    }
    if value.is_empty() {
        return Err(err(
            "query_value_missing",
            format!("missing value for {field}"),
        ));
    }
    if matches!(field.as_str(), "port" | "freshness") && value.parse::<f64>().is_err() {
        return Err(err(
            "query_value_invalid",
            format!("{field} requires a number"),
        ));
    }
    if matches!(
        field.as_str(),
        "observed_after" | "observed_before" | "ingested_after" | "ingested_before"
    ) && parse_time(value).is_none()
    {
        return Err(err(
            "query_value_invalid",
            format!("{field} requires an ISO timestamp"),
        ));
    }
    Ok(
        json!({"op":"condition","field":field,"operator":operator,"value":value,"implicit":implicit}),
    )
}
fn parse_query(expression: &str) -> Api<Value> {
    Parser {
        tokens: tokenize(expression)?,
        at: 0,
    }
    .parse()
}
fn fields(node: &Value, out: &mut Vec<String>) {
    if node["op"] == "condition" {
        if let Some(field) = node["field"].as_str() {
            if !out.iter().any(|v| v == field) {
                out.push(field.to_owned());
            }
        }
    }
    for row in node["items"].as_array().into_iter().flatten() {
        fields(row, out);
    }
    if node["item"].is_object() {
        fields(&node["item"], out);
    }
}
fn asset_url(identity: &Value) -> String {
    let Some(mut host) = identity["host"].as_str().map(str::to_owned) else {
        return String::new();
    };
    if host.contains(':') && !host.starts_with('[') {
        host = format!("[{host}]");
    }
    let scheme = identity["scheme"].as_str().unwrap_or("https");
    let default = if scheme == "http" { 80 } else { 443 };
    let port = identity["port"]
        .as_u64()
        .filter(|v| *v != default)
        .map(|v| format!(":{v}"))
        .unwrap_or_default();
    let path = identity["path"]
        .as_str()
        .filter(|v| !v.is_empty())
        .unwrap_or("/");
    let query = identity["query"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(|v| format!("?{v}"))
        .unwrap_or_default();
    format!("{scheme}://{host}{port}{path}{query}")
}
fn values(asset: &Value, field: &str) -> Vec<Value> {
    let identity = &asset["identity"];
    let profile = &asset["current_profile"];
    let judgments = &asset["judgements"];
    match field {
        "run_id" => {
            return asset["observations"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.get("source_run_id").filter(|v| !v.is_null()).cloned())
                .collect();
        }
        "source" => {
            return asset["observations"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|v| [v["source_kind"].clone(), v["source_id"].clone()])
                .filter(|v| !v.is_null())
                .collect();
        }
        _ => {}
    }
    let item = match field {
        "type" | "kind" => asset["type"].clone(),
        "company" => judgments["company_attribution"]["value"].clone(),
        "program" => judgments["program_coverage"]["value"].clone(),
        "task_id" => asset["task_id"].clone(),
        "domain" => identity
            .get("name")
            .filter(|v| !v.is_null())
            .unwrap_or(&identity["host"])
            .clone(),
        "ip" => identity
            .get("address")
            .filter(|v| !v.is_null())
            .unwrap_or(&identity["host"])
            .clone(),
        "cidr" => identity["address"].clone(),
        "protocol" => identity
            .get("protocol")
            .filter(|v| !v.is_null())
            .unwrap_or(&identity["scheme"])
            .clone(),
        "port" => identity["port"].clone(),
        "url" => json!(asset_url(identity)),
        "method" => identity["method"].clone(),
        "param_location" => identity["location"].clone(),
        "param_name" => identity["name"].clone(),
        "technology" => profile
            .get("technology")
            .filter(|v| !v.is_null())
            .unwrap_or(&profile["fingerprint"])
            .clone(),
        "tag" => asset["tags"].clone(),
        "status" => asset["lifecycle"].clone(),
        "observed_after" | "observed_before" | "freshness" => asset["latest_observed"].clone(),
        "ingested_after" | "ingested_before" => asset["updated_at"].clone(),
        _ => Value::Null,
    };
    item.as_array().cloned().unwrap_or_else(|| vec![item])
}
fn net(value: &str) -> Option<(u128, u8, u8)> {
    let (addr, prefix) = value.split_once('/').unwrap_or((value, ""));
    let addr: IpAddr = addr.parse().ok()?;
    let bits = if addr.is_ipv4() { 32 } else { 128 };
    let prefix: u8 = if prefix.is_empty() {
        bits
    } else {
        prefix.parse().ok()?
    };
    if prefix > bits {
        return None;
    }
    let integer = match addr {
        IpAddr::V4(v) => u32::from(v) as u128,
        IpAddr::V6(v) => u128::from(v),
    };
    Some((integer, prefix, bits))
}
fn compare(actual: &Value, operator: &str, expected: &str, field: &str) -> bool {
    if actual.is_null() {
        return false;
    }
    if field == "cidr" {
        let Some((left, left_prefix, width)) = net(&string(actual)) else {
            return false;
        };
        let Some((right, right_prefix, other_width)) = net(expected) else {
            return false;
        };
        if width != other_width {
            return false;
        }
        let mask = if right_prefix == 0 {
            0
        } else {
            u128::MAX << (width - right_prefix)
        };
        let inside = left_prefix >= right_prefix && left & mask == right & mask;
        return if operator == "!=" { !inside } else { inside };
    }
    if matches!(field, "port" | "freshness") {
        let left = if field == "freshness" {
            let Some(when) = parse_time(&string(actual)) else {
                return false;
            };
            (Utc::now() - when).num_seconds() as f64 / 86400.0
        } else {
            string(actual).parse::<f64>().unwrap_or(f64::NAN)
        };
        let right = expected.parse::<f64>().unwrap_or(f64::NAN);
        return match operator {
            "=" => left == right,
            "!=" => left != right,
            ">" => left > right,
            "<" => left < right,
            ">=" => left >= right,
            "<=" => left <= right,
            _ => false,
        };
    }
    if matches!(
        field,
        "observed_after" | "observed_before" | "ingested_after" | "ingested_before"
    ) {
        let (Some(left), Some(right)) = (parse_time(&string(actual)), parse_time(expected)) else {
            return false;
        };
        if field.ends_with("_after") {
            return if matches!(operator, "=" | ">" | ">=") {
                left >= right
            } else {
                left < right
            };
        }
        return if matches!(operator, "=" | "<" | "<=") {
            left <= right
        } else {
            left > right
        };
    }
    let left = string(actual);
    let (left, right) = if matches!(field, "param_name" | "url") {
        (left, expected.to_owned())
    } else {
        (
            caseless::default_case_fold_str(&left),
            caseless::default_case_fold_str(expected),
        )
    };
    match operator {
        "=" => left == right,
        "!=" => left != right,
        "~" => left.contains(&right),
        "prefix" => left.starts_with(&right),
        ">" => left > right,
        "<" => left < right,
        ">=" => left >= right,
        "<=" => left <= right,
        _ => false,
    }
}
fn matches_query(node: &Value, asset: &Value) -> bool {
    match node["op"].as_str().unwrap_or("") {
        "true" => true,
        "and" => node["items"]
            .as_array()
            .is_some_and(|rows| rows.iter().all(|v| matches_query(v, asset))),
        "or" => node["items"]
            .as_array()
            .is_some_and(|rows| rows.iter().any(|v| matches_query(v, asset))),
        "not" => !matches_query(&node["item"], asset),
        "condition" => {
            let field = node["field"].as_str().unwrap_or("");
            let operator = node["operator"].as_str().unwrap_or("");
            let expected = node["value"].as_str().unwrap_or("");
            let mut actual = values(asset, field);
            if field == "text" {
                actual.push(json!(asset.to_string()));
            }
            if operator == "!=" {
                return !actual.iter().any(|v| compare(v, "=", expected, field));
            }
            actual.iter().any(|v| compare(v, operator, expected, field))
        }
        _ => false,
    }
}
fn asset_rows(s: &AppState, id: &str) -> Api<Vec<Value>> {
    let detached: BTreeSet<String> = records(s, id, "task_asset_link")?
        .into_iter()
        .filter(|row| row["payload"]["state"] == "detached")
        .filter_map(|row| row["record_id"].as_str().map(str::to_owned))
        .collect();
    let observations = records(s, id, "asset_observation")?;
    let mut rows = Vec::new();
    for row in records(s, id, "asset")? {
        let aid = row["record_id"].as_str().unwrap_or("");
        if detached.contains(aid)
            || matches!(
                row["payload"]["lifecycle"].as_str(),
                Some("destroyed" | "merged")
            )
        {
            continue;
        }
        let mut asset = row["payload"].clone();
        let observed = observations
            .iter()
            .filter(|v| v["payload"]["asset_id"] == aid && v["payload"]["revoked_at"].is_null())
            .map(|v| v["payload"].clone())
            .collect::<Vec<_>>();
        asset["task_id"] = json!(id);
        asset["observations"] = json!(observed);
        asset["access"] = json!("task");
        asset["updated_at"] = row["updated_at"].clone();
        rows.push(asset);
    }
    for publication in publication::visible(s, id)? {
        let owner = publication["task_id"].as_str().unwrap_or("");
        for item in publication["assets"].as_array().into_iter().flatten() {
            let mut asset = item["entity"].clone();
            asset["task_id"] = json!(owner);
            asset["observations"] = item["observations"].clone();
            asset["access"] = json!("publication");
            asset["publication_id"] = publication["publication_id"].clone();
            asset["publication_version"] = publication["version"].clone();
            asset["updated_at"] = publication["published_at"].clone();
            rows.push(asset);
        }
    }
    rows.sort_by(|a, b| {
        b["latest_observed"]
            .as_str()
            .cmp(&a["latest_observed"].as_str())
            .then(b["asset_id"].as_str().cmp(&a["asset_id"].as_str()))
    });
    Ok(rows)
}
fn relation_rows(s: &AppState, id: &str) -> Api<Vec<Value>> {
    let mut rows = records(s, id, "asset_relation")?
        .into_iter()
        .map(|v| v["payload"].clone())
        .collect::<Vec<_>>();
    for publication in publication::visible(s, id)? {
        rows.extend(
            publication["relations"]
                .as_array()
                .into_iter()
                .flatten()
                .cloned(),
        );
    }
    Ok(rows)
}
fn filter(s: &AppState, id: &str, expression: &str) -> Api<(Vec<Value>, Value)> {
    let ast = parse_query(expression)?;
    let rows = asset_rows(s, id)?
        .into_iter()
        .filter(|row| matches_query(&ast, row))
        .collect();
    let mut matched = Vec::new();
    fields(&ast, &mut matched);
    Ok((rows, json!({"ast":ast,"matched_fields":matched})))
}
fn revision_key(id: &str, kind: &str, key: &str, revision: u64) -> String {
    format!("{id}:{kind}:{key}:{revision}")
}
fn write_record(
    s: &AppState,
    id: &str,
    kind: &str,
    key: &str,
    value: Value,
    expected_revision: Option<u64>,
    command_id: Option<&str>,
) -> Api<Value> {
    loop {
        if let Some(command_id) = command_id {
            if let Some(applied) = s.store.get(APPLIED, command_id).map_err(db)? {
                return Ok(applied["output"].clone());
            }
        }
        let (record_key, version, row) =
            stage_rec(s, id, kind, key, value.clone(), expected_revision)?;
        let previous = s.store.get(RECORDS, &record_key).map_err(db)?;
        let output = row["payload"].clone();
        let next = json!({"version":version+1,"values":row});
        let rev_key = revision_key(id, kind, key, version + 1);
        let old_revision = s.store.get(REVISIONS, &rev_key).map_err(db)?;
        let revision =
            json!({"task_id":id,"kind":kind,"key":key,"revision":version+1,"payload":output});
        let marker = command_id.map(|command_id| json!({"command_id":command_id,"output":output}));
        let mut conditions = vec![
            (RECORDS, record_key.as_str(), previous.as_ref()),
            (REVISIONS, rev_key.as_str(), old_revision.as_ref()),
        ];
        let mut records = vec![
            (RECORDS, record_key.as_str(), &next),
            (REVISIONS, rev_key.as_str(), &revision),
        ];
        if let (Some(command_id), Some(marker)) = (command_id, marker.as_ref()) {
            conditions.push((APPLIED, command_id, None));
            records.push((APPLIED, command_id, marker));
        }
        if s.store.put_many_if(&conditions, &records).map_err(db)? {
            return Ok(output);
        }
        if expected_revision.is_some() {
            return Err(Error::conflict("SRC record revision changed"));
        }
    }
}
fn old_payload(s: &AppState, id: &str, kind: &str, key: &str) -> Api<Option<Value>> {
    match rec(s, id, kind, key) {
        Ok(row) => Ok(Some(row["payload"].clone())),
        Err(error) if error.status == StatusCode::NOT_FOUND => Ok(None),
        Err(error) => Err(error),
    }
}
fn command(
    s: &AppState,
    id: &str,
    kind: &str,
    input: &Value,
    actor_id: &str,
    command_id: Option<&str>,
) -> Api<Value> {
    let base = arg(input, "base_revision").as_u64();
    let data = body(input);
    match kind {
        "src.query.save" => {
            let key = arg(input, "query_id")
                .as_str()
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| pid("ssavedquery"));
            let expression = data["expression"].as_str().unwrap_or("");
            parse_query(expression)?;
            let mut value = old_payload(s, id, "saved_query", &key)?.unwrap_or_else(|| json!({}));
            if let Some(map) = data.as_object() {
                for (k, v) in map {
                    value[k] = v.clone();
                }
            }
            value["query_id"] = json!(key);
            value["expression"] = json!(expression);
            value["semantic_version"] = json!(QUERY_VERSION);
            value["author"] = json!(actor_id);
            value["updated_at"] = json!(now());
            value["deleted_at"] = Value::Null;
            write_record(s, id, "saved_query", &key, value, base, command_id)
        }
        "src.query.delete" => {
            let key = require(input, "query_id")?;
            let row = rec(s, id, "saved_query", key)?;
            let Some(base) = base else {
                return Err(err("query_invalid", "base_revision is required"));
            };
            if row["revision"] != base {
                return Err(Error::conflict("saved query revision is stale"));
            }
            let mut value = row["payload"].clone();
            value["deleted_at"] = json!(now());
            write_record(s, id, "saved_query", key, value, Some(base), command_id)
        }
        "src.vocabulary.save" => {
            let key = arg(input, "term_id")
                .as_str()
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| pid("svocab"));
            if data["term"].as_str().is_none_or(|v| v.trim().is_empty()) {
                return Err(err("vocabulary_invalid", "term is required"));
            }
            let mut value =
                old_payload(s, id, "target_vocabulary", &key)?.unwrap_or_else(|| json!({}));
            if let Some(map) = data.as_object() {
                for (k, v) in map {
                    value[k] = v.clone();
                }
            }
            value["term_id"] = json!(key);
            value["author"] = json!(actor_id);
            if value["status"].as_str().is_none_or(str::is_empty) {
                value["status"] = json!("candidate");
            }
            value["updated_at"] = json!(now());
            write_record(s, id, "target_vocabulary", &key, value, base, command_id)
        }
        "src.tag.save" => {
            let key = arg(input, "tag_id")
                .as_str()
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| pid("stag"));
            let old = old_payload(s, id, "asset_tag", &key)?.unwrap_or_else(|| json!({}));
            let name = data["name"]
                .as_str()
                .or_else(|| old["name"].as_str())
                .unwrap_or("");
            if name.trim().is_empty() {
                return Err(err("tag_invalid", "tag name is required"));
            }
            let mut value = old;
            if let Some(map) = data.as_object() {
                for (k, v) in map {
                    value[k] = v.clone();
                }
            }
            value["tag_id"] = json!(key);
            value["author"] = json!(actor_id);
            value["updated_at"] = json!(now());
            write_record(s, id, "asset_tag", &key, value, base, command_id)
        }
        "src.note.save" => {
            let asset_id = require(input, "asset_id")?;
            asset_row(s, id, asset_id)?;
            let key = arg(input, "note_id")
                .as_str()
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| pid("snote"));
            let mut value = old_payload(s, id, "asset_note", &key)?.unwrap_or_else(|| json!({}));
            if let Some(map) = data.as_object() {
                for (k, v) in map {
                    value[k] = v.clone();
                }
            }
            value["note_id"] = json!(key);
            value["asset_id"] = json!(asset_id);
            value["author"] = json!(actor_id);
            if value["origin"].as_str().is_none_or(str::is_empty) {
                value["origin"] = json!("human");
            }
            value["updated_at"] = json!(now());
            write_record(s, id, "asset_note", &key, value, base, command_id)
        }
        "src.tag.assign" => assign_tags(s, id, input, command_id, actor_id),
        _ => Err(Error::invalid("unsupported SRC asset query command")),
    }
}
pub(super) fn apply_command(
    s: &AppState,
    id: &str,
    kind: &str,
    input: &Value,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    s.store.document_scope(||apply_command_inner(s,id,kind,input,command_id,actor_id)).map_err(db)?
}
pub(super) fn apply_command_inner(
    s: &AppState,
    id: &str,
    kind: &str,
    input: &Value,
    command_id: &str,
    actor_id: &str,
) -> Api<Value> {
    command(s, id, kind, input, actor_id, Some(command_id))
}
fn assign_tags(
    s: &AppState,
    id: &str,
    input: &Value,
    command_id: Option<&str>,
    actor_id: &str,
) -> Api<Value> {
    let asset_ids = input["asset_ids"]
        .as_array()
        .ok_or_else(|| err("query_invalid", "asset_ids required"))?;
    let tag_ids = input["tag_ids"]
        .as_array()
        .ok_or_else(|| err("query_invalid", "tag_ids required"))?;
    let mut names = BTreeSet::new();
    for tag in tag_ids {
        let tid = tag
            .as_str()
            .ok_or_else(|| err("query_invalid", "invalid tag ID"))?;
        let row = rec(s, id, "asset_tag", tid)?;
        if row["payload"]["deleted_at"].is_null() {
            if let Some(name) = row["payload"]["name"].as_str() {
                names.insert(name.to_owned());
            }
        }
    }
    let detach = input["detach"] == true;
    loop {
        if let Some(command_id) = command_id {
            if let Some(row) = s.store.get(APPLIED, command_id).map_err(db)? {
                return Ok(row["output"].clone());
            }
        }
        let mut updates: Vec<(String, Option<Value>, Value)> = Vec::new();
        let mut items = Vec::new();
        for asset in asset_ids {
            let Some(aid) = asset.as_str() else {
                items.push(json!({"asset_id":asset,"ok":false,
                "code":"query_invalid","message":"invalid asset ID"}));
                continue;
            };
            match asset_row(s, id, aid) {
                Ok(row) => {
                    let mut selected: BTreeSet<String> = row["payload"]["tags"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect();
                    if detach {
                        selected.retain(|v| !names.contains(v));
                    } else {
                        selected.extend(names.iter().cloned());
                    }
                    let tags = selected.into_iter().collect::<Vec<_>>();
                    let mut value = row["payload"].clone();
                    value["tags"] = json!(tags);
                    value["annotation_actor"] = json!(actor_id);
                    let key = row["natural_key"].as_str().unwrap_or(aid);
                    let version = row["revision"].as_u64().unwrap_or(0);
                    match stage_rec(s, id, "asset", key, value, Some(version)) {
                        Ok((record_key, _, next_row)) => {
                            let old = s.store.get(RECORDS, &record_key).map_err(db)?;
                            updates.push((
                                record_key,
                                old,
                                json!({"version":version+1,"values":next_row}),
                            ));
                            items.push(json!({"asset_id":aid,"ok":true,"tags":tags}));
                        }
                        Err(error) => items.push(json!({"asset_id":aid,"ok":false,
                            "code":error.code,"message":error.message})),
                    }
                }
                Err(error) => items.push(json!({"asset_id":aid,"ok":false,
                    "code":error.code,"message":error.message})),
            }
        }
        let output = json!({"items":items});
        let marker = command_id.map(|id| json!({"command_id":id,"output":output}));
        let mut expected: Vec<(&str, &str, Option<&Value>)> = updates
            .iter()
            .map(|(key, old, _)| (RECORDS, key.as_str(), old.as_ref()))
            .collect();
        let mut records: Vec<(&str, &str, &Value)> = updates
            .iter()
            .map(|(key, _, row)| (RECORDS, key.as_str(), row))
            .collect();
        if let (Some(command_id), Some(marker)) = (command_id, marker.as_ref()) {
            expected.push((APPLIED, command_id, None));
            records.push((APPLIED, command_id, marker));
        }
        if s.store.put_many_if(&expected, &records).map_err(db)? {
            return Ok(output);
        }
    }
}
fn snapshot_cursor(s: &AppState, id: &str, kind: &str, snapshot: &str, offset: usize) -> Api<String> {
    let (aggregate, scope) = if kind == "query_snapshot" {
        ("src-assets", format!("{id}:{snapshot}"))
    } else {
        ("src-fulltext", snapshot.to_owned())
    };
    let signed = super::super::cursors::signed(s, aggregate, id, offset as i64, &scope)?;
    Ok(format!("{snapshot}:{signed}"))
}
fn cursor(s: &AppState, id: &str, kind: &str, value: &str) -> Api<(Value, usize)> {
    let (snapshot, token) = value.split_once(':')
        .ok_or_else(|| err("query.cursor_invalid", "query cursor is invalid"))?;
    let row = rec(s, id, kind, snapshot)?;
    let (aggregate, scope) = if kind == "query_snapshot" {
        ("src-assets", format!("{id}:{snapshot}"))
    } else {
        ("src-fulltext", snapshot.to_owned())
    };
    let offset = super::super::cursors::after(s, token, aggregate, id, &scope)? as usize;
    Ok((row["payload"].clone(), offset))
}
fn page(s: &AppState, id: &str, kind: &str, items: &[Value], offset: usize, limit: usize, snapshot: &str) -> Api<Value> {
    let selected = items
        .iter()
        .skip(offset)
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();
    let returned = selected.len();
    let next = offset + returned;
    let next_cursor = if next < items.len() {
        Some(snapshot_cursor(s, id, kind, snapshot, next)?)
    } else {
        None
    };
    Ok(json!({"items":selected,"total":items.len(),"returned":returned,
        "next_cursor":next_cursor}))
}
fn asset_query(s: &AppState, id: &str, input: &Value) -> Api<Value> {
    let limit = input["limit"].as_u64().unwrap_or(50).clamp(1, 1000) as usize;
    let (snapshot, offset) = if let Some(cursor_value) =
        input["cursor"].as_str().filter(|v| !v.is_empty())
    {
        let (snapshot, offset) = cursor(s, id, "query_snapshot", cursor_value)?;
        for row in snapshot["items"].as_array().into_iter().flatten() {
            if let Some(publication) = row["publication_id"].as_str() {
                asset_publication::published(s, id, publication)?;
            }
        }
        (snapshot, offset)
    } else {
        let expression = input["expression"].as_str().unwrap_or("");
        let (rows, semantics) = filter(s, id, expression)?;
        let snapshot_id = pid("squerysnap");
        let watermarks = super::super::source_watermarks(s, id)?;
        let partial = watermarks
            .iter()
            .any(|v| matches!(v["state"].as_str(), Some("failed" | "pending")));
        let snapshot = json!({"snapshot_id":snapshot_id,"expression":expression,
            "semantic_version":QUERY_VERSION,"semantics":semantics,"items":rows,
            "watermarks":watermarks,"partial_reason":if partial{Some("index_lagging")}else{None},
            "created_at":now()});
        save_rec(
            s,
            id,
            "query_snapshot",
            &snapshot_id,
            snapshot.clone(),
            Some(0),
        )?;
        (snapshot, 0)
    };
    let rows = snapshot["items"].as_array().ok_or_else(|| {
        Error::new(
            StatusCode::CONFLICT,
            "src.query_snapshot_invalid",
            "saved asset query snapshot is unavailable",
        )
    })?;
    let snapshot_id = snapshot["snapshot_id"].as_str().unwrap_or("");
    let mut result = page(s, id, "query_snapshot", rows, offset, limit, snapshot_id)?;
    result["snapshot_id"] = json!(snapshot_id);
    result["semantic_version"] = json!(QUERY_VERSION);
    for field in ["expression", "semantics", "watermarks", "partial_reason"] {
        result[field] = snapshot[field].clone();
    }
    Ok(result)
}
fn saved_query_read(s: &AppState, id: &str, input: &Value) -> Api<Value> {
    let key = require(input, "query_id")?;
    let current=rec(s,id,"saved_query",key)?;
    if input["revision"].as_u64().is_some_and(|revision|current["revision"].as_u64()!=Some(revision)){return Err(Error::conflict("saved query revision is stale"));}
    Ok(current["payload"].clone())
}

fn pivot(s: &AppState, id: &str, input: &Value) -> Api<Value> {
    let asset_id = require(input, "asset_id")?;
    let assets = asset_rows(s, id)?
        .into_iter()
        .filter_map(|row| {
            let asset_id = row["asset_id"].as_str()?.to_owned();
            Some((asset_id, row))
        })
        .collect::<BTreeMap<_, _>>();
    let mut rows = Vec::new();
    for relation in relation_rows(s, id)? {
        let from = relation["from_asset_id"].as_str().unwrap_or("");
        let to = relation["to_asset_id"].as_str().unwrap_or("");
        if asset_id != from && asset_id != to {
            continue;
        }
        if input["relation_type"]
            .as_str()
            .is_some_and(|v| !v.is_empty() && relation["relation_type"] != v)
        {
            continue;
        }
        let neighbor = if asset_id == from { to } else { from };
        if let Some(asset) = assets.get(neighbor) {
            rows.push(json!({"relation":relation,"asset":asset}));
        }
    }
    rows.sort_by(|a, b| {
        a["relation"]["relation_id"]
            .as_str()
            .cmp(&b["relation"]["relation_id"].as_str())
    });
    let start = input["cursor"]
        .as_u64()
        .or_else(|| input["cursor"].as_str().and_then(|v| v.parse().ok()))
        .unwrap_or(0) as usize;
    let limit = input["limit"].as_u64().unwrap_or(50).clamp(1, 500) as usize;
    let selected = rows
        .iter()
        .skip(start)
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();
    let returned = selected.len();
    let next = start + returned;
    Ok(
        json!({"items":selected,"returned":returned,"total":rows.len(),
        "next_cursor":if next<rows.len(){Some(next)}else{None},
        "unexpanded":rows.len().saturating_sub(next)}),
    )
}
fn collect_artifact_refs(value: &Value, out: &mut Vec<Value>) {
    match value {
        Value::Object(map) => {
            if map.get("artifact_id").and_then(Value::as_str).is_some()
                && map.get("task_id").and_then(Value::as_str).is_some()
                && map.get("size").is_some()
            {
                out.push(value.clone());
            } else {
                for child in map.values() {
                    collect_artifact_refs(child, out);
                }
            }
        }
        Value::Array(rows) => {
            for child in rows {
                collect_artifact_refs(child, out)
            }
        }
        _ => {}
    }
}
fn searchable_json(value:&Value)->String{match value{
    Value::Object(fields)=>{let mut keys=fields.keys().collect::<Vec<_>>();keys.sort();format!("{{{}}}",keys.into_iter().map(|key|format!("{}: {}",json!(key),searchable_json(&fields[key]))).collect::<Vec<_>>().join(", "))},
    Value::Array(values)=>format!("[{}]",values.iter().map(searchable_json).collect::<Vec<_>>().join(", ")),
    _=>value.to_string()
}}
fn full_text_item(
    s: &AppState,
    id: &str,
    owner: &str,
    kind: &str,
    record_id: &str,
    payload: &Value,
    needle: &str,
) -> Api<Option<Value>> {
    let mut text = searchable_json(payload);
    let mut refs = Vec::new();
    collect_artifact_refs(payload, &mut refs);
    let mut checked = Vec::new();
    for reference in refs {
        match publication::readable_ref(s, id, &reference) {
            Ok(saved) => {
                if saved["media_type"].as_str().is_some_and(|v| {
                    v.starts_with("text/")
                        || [
                            "application/json",
                            "application/xml",
                            "application/x-http",
                            "application/har",
                        ]
                        .iter()
                        .any(|p| v.starts_with(p))
                }) {
                    if let Some(data) = saved["data"]
                        .as_str()
                        .and_then(|v| base64::engine::general_purpose::STANDARD.decode(v).ok())
                    {
                        text.push('\n');
                        text.push_str(&String::from_utf8_lossy(&data));
                    }
                }
                checked.push(reference);
            }
            Err(_) => {
                let mut missing = reference;
                missing["unavailable"] = json!(true);
                checked.push(missing);
            }
        }
    }
    let needle_length=needle.chars().count();
    let lowered = caseless::default_case_fold_str(&text);
    let needle = caseless::default_case_fold_str(needle);
    let Some(index) = lowered.find(&needle) else {
        return Ok(None);
    };
    let index=lowered[..index].chars().count();
    let start=index.saturating_sub(100);
    let end=index+needle_length+100;
    let snippet=text.chars().skip(start).take(end-start).collect::<String>();
    Ok(Some(
        json!({"task_id":owner,"kind":kind,"record_id":record_id,
        "snippet":snippet,"match_start":index,"artifact_refs":checked,"full_record":payload}),
    ))
}
fn full_text(s: &AppState, id: &str, input: &Value) -> Api<Value> {
    let needle = require(input, "needle")?;
    let limit = input["limit"].as_u64().unwrap_or(100).clamp(1, 1000) as usize;
    let (snapshot, offset, rows) =
        if let Some(token) = input["cursor"].as_str().filter(|v| !v.is_empty()) {
            let (snapshot, offset) = cursor(s, id, "fulltext_snapshot", token)?;
            let visible = publication::visible(s, id)?;
            let rows = snapshot["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| {
                    let Some(owner) = item["task_id"].as_str() else {
                        return false;
                    };
                    if owner == id {
                        return true;
                    }
                    visible
                        .iter()
                        .filter(|v| v["task_id"] == owner)
                        .any(|pubrow| {
                            pubrow["assets"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .any(|asset| {
                                    asset["observations"]
                                        .as_array()
                                        .into_iter()
                                        .flatten()
                                        .any(|obs| obs["observation_id"] == item["record_id"])
                                })
                        })
                })
                .cloned()
                .collect::<Vec<_>>();
            (snapshot, offset, rows)
        } else {
            let kinds: BTreeSet<String> = input["kinds"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            let defaults = [
                "asset_observation",
                "run_event",
                "fact",
                "observation",
                "step",
                "dead_end",
                "tool_call",
                "import_item",
                "import_exchange",
                "report",
            ];
            let allowed = if kinds.is_empty() {
                defaults.into_iter().map(str::to_owned).collect()
            } else {
                kinds
            };
            let mut rows = Vec::new();
            for record in records(s, id, "")? {
                let kind = record["kind"].as_str().unwrap_or("");
                if !allowed.contains(kind) {
                    continue;
                }
                if let Some(item) = full_text_item(
                    s,
                    id,
                    id,
                    kind,
                    record["record_id"].as_str().unwrap_or(""),
                    &record["payload"],
                    needle,
                )? {
                    rows.push(item);
                }
            }
            if allowed.contains("asset_observation") {
                for publication in publication::visible(s, id)? {
                    let owner = publication["task_id"].as_str().unwrap_or("");
                    for asset in publication["assets"].as_array().into_iter().flatten() {
                        for observation in asset["observations"].as_array().into_iter().flatten() {
                            if let Some(item) = full_text_item(
                                s,
                                id,
                                owner,
                                "asset_observation",
                                observation["observation_id"].as_str().unwrap_or(""),
                                observation,
                                needle,
                            )? {
                                rows.push(item);
                            }
                        }
                    }
                }
            }
            let snapshot_id = pid("sfulltext");
            let snapshot = json!({"snapshot_id":snapshot_id,"needle":needle,
            "items":rows.clone(),"created_at":now()});
            save_rec(
                s,
                id,
                "fulltext_snapshot",
                &snapshot_id,
                snapshot.clone(),
                Some(0),
            )?;
            (snapshot, 0, rows)
        };
    let sid = snapshot["snapshot_id"].as_str().unwrap_or("");
    let mut result = page(s, id, "fulltext_snapshot", &rows, offset, limit, sid)?;
    result["snapshot_id"] = json!(sid);
    result["complete"] = json!(result["next_cursor"].is_null());
    result["watermarks"] = json!(super::super::source_watermarks(s, id)?);
    Ok(result)
}
fn attachment(
    path: &str,
    bytes: &[u8],
    media: &str,
    classification: &str,
    purpose: &str,
    source: Value,
) -> Value {
    json!({"path":path,"media_type":media,"classification":classification,
        "purpose":purpose,"bytes":bytes.len(),"source":source})
}
fn export(s: &AppState, id: &str, input: &Value) -> Api<Value> {
    use std::io::Write;
    let expression = input["expression"].as_str().unwrap_or("");
    let (mut rows, semantics) = filter(s, id, expression)?;
    if let Some(selected) = input["selected_ids"].as_array() {
        let allowed: BTreeSet<&str> = selected.iter().filter_map(Value::as_str).collect();
        rows.retain(|row| {
            row["asset_id"]
                .as_str()
                .is_some_and(|v| allowed.contains(v))
        });
    }
    let format = input["export_format"]
        .as_str()
        .or_else(|| input["format"].as_str())
        .unwrap_or("json");
    let snapshot_id = pid("sexportsnap");
    let mut manifest = json!({"schema":"muteki.src.asset-export.v1","snapshot_id":snapshot_id,
        "semantic_version":QUERY_VERSION,"expression":expression,"semantics":semantics,
        "task_id":id,"created_at":now(),"expected_entries":0,"actual_entries":0,
        "omissions":[],"attachments":[]});
    let mut media = "application/json";
    let content = match format {
        "csv" => {
            media = "text/csv; charset=utf-8";
            let mut writer = csv::Writer::from_writer(Vec::new());
            writer
                .write_record([
                    "asset_id",
                    "type",
                    "identity",
                    "current_profile",
                    "first_observed",
                    "latest_observed",
                    "last_success",
                    "last_failure",
                    "sources",
                    "task_id",
                    "access",
                ])
                .map_err(db)?;
            for row in &rows {
                writer
                    .write_record([
                        string(&row["asset_id"]),
                        string(&row["type"]),
                        row["identity"].to_string(),
                        row["current_profile"].to_string(),
                        string(&row["first_observed"]),
                        string(&row["latest_observed"]),
                        string(&row["last_success"]),
                        string(&row["last_failure"]),
                        row["observations"].to_string(),
                        string(&row["task_id"]),
                        string(&row["access"]),
                    ])
                    .map_err(db)?;
            }
            manifest["roundtrip"] = json!(
                "CSV preserves the selected projection only; it does not restore the graph or evidence"
            );
            writer.into_inner().map_err(db)?
        }
        "jsonl" => {
            media = "application/jsonl";
            let mut out = String::new();
            for row in &rows {
                out.push_str(&row.to_string());
                out.push('\n');
            }
            out.into_bytes()
        }
        "markdown" => {
            media = "text/markdown; charset=utf-8";
            let mut out = format!("# SRC 资产导出 ({} 项)\n", rows.len());
            for row in &rows {
                out.push_str(&format!(
                    "\n## {}\n- asset_id: {}\n- type: {}\n- identity: {}\n",
                    row["display_name"]
                        .as_str()
                        .or_else(|| row["asset_id"].as_str())
                        .unwrap_or(""),
                    string(&row["asset_id"]),
                    string(&row["type"]),
                    row["identity"]
                ));
                if let Some(profile) = row["current_profile"].as_object().filter(|v| !v.is_empty())
                {
                    out.push_str("- profile:\n");
                    for (key, value) in profile {
                        out.push_str(&format!("  - {key}: {value}\n"));
                    }
                }
            }
            out.into_bytes()
        }
        "json" | "zip" => {
            let ids: BTreeSet<&str> = rows
                .iter()
                .filter_map(|row| row["asset_id"].as_str())
                .collect();
            let relations = relation_rows(s, id)?
                .into_iter()
                .filter(|relation| {
                    relation["from_asset_id"]
                        .as_str()
                        .is_some_and(|v| ids.contains(v))
                        && relation["to_asset_id"]
                            .as_str()
                            .is_some_and(|v| ids.contains(v))
                })
                .collect::<Vec<_>>();
            serde_json::to_vec_pretty(&json!({"schema":"muteki.src.assets.v1",
                "snapshot_id":snapshot_id,"semantic_version":QUERY_VERSION,
                "assets":rows,"relations":relations}))
            .map_err(db)?
        }
        _ => {
            return Err(err(
                "export_format_invalid",
                "asset export supports csv, json, jsonl, markdown or zip",
            ));
        }
    };
    if format != "zip" {
        let extension = match format {
            "csv" => "csv",
            "jsonl" => "jsonl",
            "markdown" => "md",
            _ => "json",
        };
        let path = format!("data.{extension}");
        manifest["attachments"] = json!([attachment(
            &path,
            &content,
            media,
            "derived",
            "asset-projection",
            Value::Null
        )]);
        manifest["expected_entries"] = json!(1);
        manifest["actual_entries"] = json!(1);
        return Ok(
            json!([{"encoding":"base64","data":base64::engine::general_purpose::STANDARD.encode(&content),
            "size":content.len()},media,manifest]),
        );
    }
    let mut archive = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    archive.start_file("assets.json", options).map_err(db)?;
    archive.write_all(&content).map_err(db)?;
    let mut attachments = vec![attachment(
        "assets.json",
        &content,
        "application/json",
        "derived",
        "asset-projection",
        Value::Null,
    )];
    let mut omissions = Vec::new();
    if input["include_artifacts"] != false {
        let mut seen = BTreeSet::new();
        for row in &rows {
            for observation in row["observations"].as_array().into_iter().flatten() {
                let mut refs = Vec::new();
                collect_artifact_refs(&observation["artifact_refs"], &mut refs);
                for reference in refs {
                    let key = reference["artifact_id"]
                        .as_str()
                        .unwrap_or("");
                    if !seen.insert(format!("{}:{key}", string(&reference["task_id"]))) {
                        continue;
                    }
                    match publication::readable_ref(s, id, &reference) {
                        Ok(saved) => {
                            let bytes = base64::engine::general_purpose::STANDARD
                                .decode(saved["data"].as_str().unwrap_or(""))
                                .map_err(|_| {
                                    Error::new(
                                        StatusCode::CONFLICT,
                                        "artifact_missing",
                                        "artifact bytes unavailable",
                                    )
                                })?;
                            let path = format!("evidence/{}", pid("item"));
                            archive.start_file(&path, options).map_err(db)?;
                            archive.write_all(&bytes).map_err(db)?;
                            attachments.push(attachment(
                                &path,
                                &bytes,
                                saved["media_type"]
                                    .as_str()
                                    .unwrap_or("application/octet-stream"),
                                "original",
                                "selected-evidence",
                                reference,
                            ));
                        }
                        Err(error) => {
                            if input["partial"] != true {
                                return Err(error);
                            }
                            omissions.push(
                                json!({"ref":reference,"code":error.code,"message":error.message}),
                            );
                        }
                    }
                }
            }
        }
    }
    let omission_count = omissions.len();
    let attachment_count = attachments.len();
    manifest["partial"] = json!(omission_count != 0);
    manifest["omissions"] = json!(omissions);
    manifest["attachments"] = json!(attachments);
    manifest["expected_entries"] = json!(attachment_count + omission_count + 1);
    manifest["actual_entries"] = json!(attachment_count + 1);
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(db)?;
    archive.start_file("manifest.json", options).map_err(db)?;
    archive.write_all(&manifest_bytes).map_err(db)?;
    let bytes = archive.finish().map_err(db)?.into_inner();
    Ok(
        json!([{"encoding":"base64","data":base64::engine::general_purpose::STANDARD.encode(&bytes),
        "size":bytes.len()},"application/zip",manifest]),
    )
}
fn fold_path_template(raw_url: &str) -> (String, String, String) {
    let Ok(parsed) = reqwest::Url::parse(raw_url) else {
        return (String::new(), "/".to_owned(), raw_url.to_owned());
    };
    let host = parsed
        .host_str()
        .map(|h| match parsed.port() {
            Some(p) => format!("{}:{}", h.to_ascii_lowercase(), p),
            None => h.to_ascii_lowercase(),
        })
        .unwrap_or_default();
    let raw_path = parsed.path();
    let mut segments = Vec::new();
    for seg in raw_path.split('/') {
        if seg.is_empty() {
            continue;
        }
        let is_uuid = seg.len() == 36
            && seg.as_bytes()[8] == b'-'
            && seg.as_bytes()[13] == b'-'
            && seg.as_bytes()[18] == b'-'
            && seg.as_bytes()[23] == b'-'
            && seg
                .bytes()
                .enumerate()
                .all(|(i, b)| matches!(i, 8 | 13 | 18 | 23) || b.is_ascii_hexdigit());
        if seg.bytes().all(|b| b.is_ascii_digit()) {
            segments.push("{int}".to_owned());
        } else if is_uuid {
            segments.push("{uuid}".to_owned());
        } else if seg.len() >= 16 && seg.bytes().all(|b| b.is_ascii_hexdigit()) {
            segments.push("{hex}".to_owned());
        } else if let Some((prefix, rest)) = seg.split_once('_') {
            if !prefix.is_empty()
                && prefix.len() <= 8
                && prefix.bytes().all(|b| b.is_ascii_alphabetic())
                && rest.len() >= 12
                && rest.bytes().all(|b| b.is_ascii_alphanumeric())
            {
                segments.push(format!("{{{prefix}_id}}"));
            } else {
                segments.push(seg.to_owned());
            }
        } else {
            segments.push(seg.to_owned());
        }
    }
    let folded = if segments.is_empty() {
        "/".to_owned()
    } else {
        format!("/{}", segments.join("/"))
    };
    (host, folded, raw_path.to_owned())
}

fn fold_routes(s: &AppState, id: &str, input: &Value) -> Api<Value> {
    let limit = input["limit"].as_u64().unwrap_or(200).clamp(1, 2000) as usize;
    let host_filter = input["host"]
        .as_str()
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| !h.is_empty());
    #[derive(Default)]
    struct GroupAcc {
        method: String,
        host: String,
        folded_path: String,
        count: usize,
        raw_paths: BTreeSet<String>,
        asset_ids: Vec<String>,
        tiers: BTreeSet<String>,
    }
    let mut groups: BTreeMap<String, GroupAcc> = BTreeMap::new();
    let mut push_route = |url: &str, method: &str, rec_id: Option<&str>, tier: Option<&str>| {
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return;
        }
        let (host, folded, raw_path) = fold_path_template(url);
        if host.is_empty() {
            return;
        }
        if let Some(ref wanted) = host_filter {
            if !host.contains(wanted.as_str()) {
                return;
            }
        }
        let method_up = if method.trim().is_empty() {
            "ANY".to_owned()
        } else {
            method.trim().to_ascii_uppercase()
        };
        let key = format!("{method_up} {host}{folded}");
        let acc = groups.entry(key).or_insert_with(|| GroupAcc {
            method: method_up,
            host,
            folded_path: folded,
            ..Default::default()
        });
        acc.count += 1;
        if acc.raw_paths.len() < 8 {
            acc.raw_paths.insert(raw_path);
        }
        if acc.asset_ids.len() < 8 {
            if let Some(aid) = rec_id.filter(|v| !v.is_empty()) {
                if !acc.asset_ids.iter().any(|existing| existing == aid) {
                    acc.asset_ids.push(aid.to_owned());
                }
            }
        }
        if let Some(t) = tier.filter(|v| !v.is_empty()) {
            acc.tiers.insert(t.to_owned());
        }
    };

    for row in records(s, id, "asset")? {
        let p = &row["payload"];
        let built_url: Option<String> = p["normalized_key"]
            .as_str()
            .or_else(|| p["value"].as_str())
            .or_else(|| p["identity"]["url"].as_str())
            .map(ToOwned::to_owned)
            .or_else(|| {
                let ident = p["identity"].as_object()?;
                let host = ident.get("host")?.as_str()?.trim();
                if host.is_empty() {
                    return None;
                }
                let scheme = ident
                    .get("scheme")
                    .and_then(Value::as_str)
                    .unwrap_or("https");
                let port = ident.get("port").and_then(Value::as_u64);
                let path = ident.get("path").and_then(Value::as_str).unwrap_or("/");
                let port_part = match (scheme, port) {
                    ("http", Some(80)) | ("https", Some(443)) | (_, None) | (_, Some(0)) => String::new(),
                    (_, Some(pnum)) => format!(":{pnum}"),
                };
                Some(format!("{scheme}://{host}{port_part}{path}"))
            });
        let Some(url) = built_url else {
            continue;
        };
        let method = p["identity"]["method"]
            .as_str()
            .or_else(|| p["current_profile"]["method"].as_str())
            .or_else(|| p["attributes"]["method"].as_str())
            .unwrap_or("ANY");
        let tier = p["current_profile"]["evidence_tier"]
            .as_str()
            .or_else(|| p["attributes"]["evidence_tier"].as_str());
        push_route(&url, method, row["record_id"].as_str(), tier);
    }

    for row in records(s, id, "http_exchange")? {
        let p = &row["payload"];
        let Some(url) = p["observed_url"].as_str() else {
            continue;
        };
        let method = p["method"].as_str().unwrap_or("GET");
        let tier = p["source_kind"].as_str().map(|_| "runtime_traffic");
        push_route(url, method, p["exchange_id"].as_str(), tier);
    }
    let mut items = groups
        .into_values()
        .map(|acc| {
            json!({
                "method": acc.method,
                "host": acc.host,
                "folded_path": acc.folded_path,
                "count": acc.count,
                "raw_sample_paths": acc.raw_paths.into_iter().collect::<Vec<_>>(),
                "sample_asset_ids": acc.asset_ids,
                "evidence_tiers": acc.tiers.into_iter().collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    items.sort_by(|a, b| {
        b["count"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["count"].as_u64().unwrap_or(0))
            .then_with(|| {
                a["folded_path"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["folded_path"].as_str().unwrap_or(""))
            })
    });
    let total = items.len();
    let truncated = total > limit;
    items.truncate(limit);
    Ok(json!({
        "groups": items,
        "total_groups": total,
        "truncated": truncated,
    }))
}

pub(super) fn query(s: &AppState, id: &str, kind: &str, input: &Value) -> Api<Value> {
    match kind {
        "src.asset.query" => asset_query(s, id, input),
        "src.asset.saved_query.read" => saved_query_read(s, id, input),
        "src.asset.full_text" => full_text(s, id, input),
        "src.asset.pivot" => pivot(s, id, input),
        "src.asset.export" => export(s, id, input),
        "src.asset.fold_routes" => fold_routes(s, id, input),
        _ => Err(Error::invalid("unsupported SRC asset query")),
    }
}
