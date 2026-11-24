//! Task identity mappings, evidence-backed verification, and managed LoginRecipe renewal.
use super::{
    Api, Error, coverage, credential_store, egress, in_policy, now, policy, rec, records, require,
    save_rec, task, verify_ref,
};
use crate::AppState;
use axum::http::StatusCode;
use base64::Engine;
use reqwest::header::HeaderName;
use ring::hmac;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::{Arc, OnceLock};
use uuid::Uuid;

fn identity(s: &AppState, id: &str, context: &str) -> Api<Value> {
    rec(s, id, "browser_identity", context)
}
fn credential(reference: &str) -> Api<&str> {
    if ["sk-", "ghp_", "gho_", "xox", "-----BEGIN"]
        .iter()
        .any(|prefix| reference.starts_with(prefix))
        || reference.chars().any(char::is_whitespace)
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "src.secret_in_config",
            "credential_ref must be a managed reference",
        ));
    }
    Ok(reference)
}
pub(super) fn ensure(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let context = require(body, "context_id")?;
    if body.get("expected_revision").is_some_and(|value|!value.is_null() && value.as_u64().is_none()){return Err(Error::invalid("expected_revision must be a nonnegative integer"));}
    let prior = records(s, id, "browser_identity")?
        .into_iter()
        .find(|v| v["natural_key"] == context);
    let revision = prior
        .as_ref()
        .and_then(|v| v["revision"].as_u64())
        .unwrap_or(0);
    if revision > 0 && body["expected_revision"].as_u64() != Some(revision) {
        return Err(Error::conflict("identity revision is stale"));
    }
    let old = prior.as_ref().map(|v| &v["payload"]);
    let conditions = body.get("success_conditions").filter(|value|!value.is_null()).cloned().unwrap_or_else(|| {
        old.map(|v| v["success_conditions"].clone())
            .unwrap_or_else(|| json!({}))
    });
    if !conditions.is_object() {
        return Err(Error::invalid(
            "identity success_conditions must be an object",
        ));
    }
    let condition_version = old
        .and_then(|v| v["condition_version"].as_u64())
        .unwrap_or(0)
        + u64::from(old.is_none_or(|v| v["success_conditions"] != conditions));
    let value = json!({"context_id":context,"account_ref":body["account_ref"].as_str().unwrap_or(""),
        "role":body["role"].as_str().unwrap_or(""),"tenant":body["tenant"].as_str().unwrap_or(""),
        "credential_ref":credential(body["credential_ref"].as_str().unwrap_or(""))?,
        "success_conditions":conditions,"condition_version":condition_version,
        "config_revision":old.and_then(|v|v["config_revision"].as_u64()).unwrap_or(0)+1,
        "session_version":old.and_then(|v|v["session_version"].as_u64()).unwrap_or(0)+1,
        "last_verification":old.map(|value|value["last_verification"].clone()),
        "browser_session_verified":false});
    save_rec(s,id,"browser_identity",context,value,Some(revision))
}

fn scoped_exchange(
    s: &AppState,
    id: &str,
    exchange_id: &str,
) -> Api<(Value, Value, &'static str, u64)> {
    let exchange = rec(s, id, "http_exchange", exchange_id)?["payload"].clone();
    let canonical_exchange_id = require(&exchange, "exchange_id")?;
    let target = require(&exchange, "observed_url")?;
    let t = task(s, id)?;
    let p = policy(s, id)?;
    if !in_policy(&p, target)
        || !t["input"]["authorization"]["targets"]
            .as_array()
            .is_some_and(|rows| {
                rows.iter().any(|v| {
                    v.as_str().is_some_and(|allowed| {
                        crate::runs::src_api::target_matches(target, allowed)
                    })
                })
            })
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "authorization_unresolved",
            "identity evidence target outside current Task scope",
        ));
    }
    let (scope, generation) = if exchange["source_kind"] == "browser" {
        let service = super::browser_runtime::service(s, id)?;
        if service["state"] != "ready"
            || exchange["execution_state"] != "completed"
            || exchange["capture_state"] != "complete"
            || exchange["status_code"] != 200
            || exchange["metadata"]["decision"]["effect"] != "allow"
            || exchange["authorization_ref"] != service["gateway_policy_digest"]
            || exchange["browser_generation"] != service["browser_generation"]
        {
            return Err(Error::new(
                StatusCode::CONFLICT,
                "src.identity_evidence_invalid",
                "browser exchange is not a current approved gateway capture",
            ));
        }
        (
            "task_browser_gateway",
            service["browser_generation"].as_u64().unwrap_or(0),
        )
    } else {
        let invocation = require(&exchange, "decision_ref")?;
        let receipt = rec(s, id, "execution_receipt", invocation)?["payload"].clone();
        if exchange["source_kind"] != "http_tool"
            || exchange["execution_state"] != "completed"
            || exchange["capture_state"] != "complete"
            || exchange["status_code"] != 200
            || receipt["dispatch_state"] != "completed"
            || receipt["outcome_known"] != true
            || receipt["external_receipt_ref"] != canonical_exchange_id
        {
            return Err(Error::new(
                StatusCode::CONFLICT,
                "src.identity_evidence_invalid",
                "identity exchange has no known completed execution",
            ));
        }
        ("scoped_http_exchange", 0)
    };
    let reference = &exchange["response_raw_ref"];
    let artifact = verify_ref(s, id, reference)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(artifact["data"].as_str().unwrap_or(""))
        .map_err(|_| Error::invalid("identity response artifact invalid"))?;
    let parsed=super::exchange_queries::parsed_response(&bytes,exchange["method"].as_str().unwrap_or("GET"))?;
    let (body,limitations)=super::exchange_queries::decoded_body(&parsed,exchange["method"].as_str().unwrap_or("GET"))?;
    if !limitations.is_empty(){return Err(Error::new(StatusCode::UNPROCESSABLE_ENTITY,"src.identity_evidence_invalid","identity response body decoding failed"))}
    let observed:Value=serde_json::from_slice(&body).map_err(|_|Error::new(StatusCode::UNPROCESSABLE_ENTITY,"src.identity_evidence_invalid","identity response is not JSON"))?;
    if !observed.is_object() {
        return Err(Error::invalid("identity response must be a JSON object"));
    }
    Ok((observed, reference.clone(), scope, generation))
}
pub(super) fn verify(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let context = require(body, "context_id")?;
    let row = identity(s, id, context)?;
    let mut mapping = row["payload"].clone();
    let version = mapping["condition_version"].as_u64().unwrap_or(0);
    if body["condition_version"]
        .as_u64()
        .is_some_and(|v| v != version)
    {
        return Err(Error::conflict("identity success conditions changed"));
    }
    if body["expected"].as_object().is_some_and(|values| {
        values
            .iter()
            .any(|(key, value)| mapping["success_conditions"][key] != *value)
    }) {
        return Err(Error::conflict(
            "identity success conditions must be revised before verification",
        ));
    }
    let supplied=body["observed"].as_object().ok_or_else(||Error::invalid("observed must be an identity observation object"))?;
    let requested_generation=body["browser_generation"].as_u64().ok_or_else(||Error::invalid("browser_generation must be a nonnegative integer"))?;
    let refs=body["evidence_refs"].as_array().ok_or_else(||Error::invalid("evidence_refs must be an array"))?;
    for reference in refs{verify_ref(s,id,reference)?;}
    let inferred=records(s,id,"http_exchange")?.into_iter().rev().find(|row|refs.iter().any(|reference|reference["artifact_id"]==row["payload"]["response_raw_ref"]["artifact_id"])).and_then(|row|row["record_id"].as_str().map(str::to_owned));
    let source=body["source_exchange_id"].as_str().filter(|value|!value.is_empty()).map(str::to_owned).or(inferred).unwrap_or_default();
    let mut observed=json!(supplied);let mut scope="unverified";let mut generation=requested_generation;let mut evidence=refs.clone();let mut proven=false;
    if !source.is_empty(){
        let(actual,reference,actual_scope,actual_generation)=scoped_exchange(s,id,&source)?;
        for(key,value)in supplied{if actual.get(key).is_some_and(|actual|actual!=value){return Err(Error::conflict("Supplied identity observation contradicts its captured response"));}}
        if actual_scope=="task_browser_gateway" && actual_generation!=requested_generation{return Err(Error::conflict("browser generation changed before identity verification"));}
        if !evidence.contains(&reference){evidence.push(reference);}scope=actual_scope;generation=if actual_scope=="task_browser_gateway"{actual_generation}else{requested_generation};proven=true;
        for(key,value)in actual.as_object().into_iter().flatten(){if observed.get(key).is_none(){observed[key]=value.clone();}}
    }else if let Some(capture)=records(s,id,"browser_screenshot")?.into_iter().rev().find(|row|row["payload"]["context_id"]==context && row["payload"]["browser_generation"]==requested_generation && row["payload"]["review_status"]=="reviewed" && refs.iter().any(|reference|reference["artifact_id"]==row["payload"]["artifact"]["artifact_id"])){
        let service=super::browser_runtime::service(s,id)?;
        if service["state"]!="ready" || service["browser_generation"]!=requested_generation{return Err(Error::conflict("Reviewed identity capture belongs to an earlier browser generation"));}
        verify_ref(s,id,&capture["payload"]["artifact"])?;scope="task_browser_reviewed_capture";proven=true;
    }
    if let Ok(context_record)=super::browser_runtime::context(s,id,&json!({"context_id":context})){if context_record["browser_generation"]!=generation{return Err(Error::conflict("Identity verification must use the current context generation"));}}
    let diagnosis = [
        "network_error",
        "challenge",
        "locked",
        "expired",
        "anonymous",
        "forbidden",
    ]
    .into_iter()
    .find(|key| observed[*key] == true);
    let mut expected = mapping["success_conditions"].clone();
    for (key, field) in [
        ("account", "account_ref"),
        ("tenant", "tenant"),
        ("role", "role"),
    ] {
        if expected.get(key).is_none() && mapping[field].as_str().is_some_and(|v| !v.is_empty()) {
            expected[key] = mapping[field].clone();
        }
    }
    let mapped = ["account", "tenant", "role"]
        .iter()
        .any(|key| expected[*key].as_str().is_some_and(|v| !v.is_empty()));
    let same = ["account", "tenant", "role"]
        .iter()
        .all(|key| expected.get(*key).is_none_or(|v| observed[*key] == *v));
    let protected = matches!(
        observed["protected_resource"].as_str(),
        Some("identity_endpoint" | "protected_page" | "subsequent_request")
    );
    let status = if let Some(reason) = diagnosis {
        reason
    } else if mapped
        && same
        && observed["session_valid"] == true
        && protected
        && !evidence.is_empty() && proven
    {
        "verified"
    } else {
        "identity_unverified"
    };
    let verification_id = format!("sidentity_verify_{}", Uuid::new_v4().simple());
    let value = json!({"verification_id":verification_id,"task_id":id,"context_id":context,
        "identity_revision":mapping["config_revision"],"browser_generation":generation,
        "condition_version":version,"expected":expected,"observed":observed,"status":status,
        "source_exchange_id":if source.is_empty(){Value::Null}else{json!(source)},"evidence_refs":evidence,
        "verification_scope":scope,"browser_session_verified":matches!(scope,"task_browser_gateway"|"task_browser_reviewed_capture") && status=="verified",
        "created_at":crate::runs::src_api::now(),"verified_at":crate::runs::src_api::now()});
    mapping["last_verification"]=value.clone();mapping["browser_session_verified"]=value["browser_session_verified"].clone();
    if !super::atomic_records(s,id,&[("identity_verification",&verification_id,value.clone(),Some(0)),("browser_identity",context,mapping,row["revision"].as_u64())])?{return Err(Error::conflict("Identity changed before verification commit"));}Ok(value)
}

pub(super) fn recover(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let context=require(body,"context_id")?;
    for field in ["profile_readable","live_session_observed"]{if !body[field].is_boolean(){return Err(Error::invalid(format!("{field} must be boolean")));}}
    let row=identity(s,id,context)?;
    let state = if body["profile_readable"] == false {
        "profile_unavailable"
    } else if body["live_session_observed"] != true {
        "session_recreated"
    } else {
        "verification_required"
    };
    let value = json!({"context_id":context,"recovery":state,"identity_revision":row["revision"],
        "verification_required":true,"session_storage_restored":false,"recorded_at":crate::runs::src_api::now()});
    save_rec(
        s,
        id,
        "identity_recovery",
        &format!("{}:{}", context, Uuid::new_v4().simple()),
        value.clone(),
        Some(0),
    )?;
    Ok(value)
}
pub(super) fn get(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    identity(s,id,require(body,"context_id")?)
}
pub(super) fn verifications(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let context = require(body, "context_id")?;
    identity(s, id, context)?;
    Ok(json!(
        records(s, id, "identity_verification")?
            .into_iter()
            .filter(|v| v["payload"]["context_id"] == context)
            .collect::<Vec<_>>()
    ))
}
pub(super) fn compare(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let left = require(body, "left_context")?;
    let right = require(body, "right_context")?;
    if left == right {
        return Err(Error::invalid("identity comparison requires two contexts"));
    }
    let l = identity(s, id, left)?["payload"].clone();
    let r = identity(s, id, right)?["payload"].clone();
    let lv = &l["last_verification"];
    let rv = &r["last_verification"];
    let left_generation=body["left_generation"].as_u64().ok_or_else(||Error::invalid("left_generation is required"))?;
    let right_generation=body["right_generation"].as_u64().ok_or_else(||Error::invalid("right_generation is required"))?;
    for(context,generation)in [(left,left_generation),(right,right_generation)]{if let Ok(current)=super::browser_runtime::context(s,id,&json!({"context_id":context})){if current["browser_generation"]!=generation{return Err(Error::conflict("Identity comparison uses an earlier browser context"));}}}
    if lv["status"] != "verified"
        || rv["status"] != "verified"
        || lv["identity_revision"] != l["config_revision"]
        || rv["identity_revision"] != r["config_revision"]
        || lv["condition_version"]!=l["condition_version"] || rv["condition_version"]!=r["condition_version"]
        || lv["browser_generation"]!=left_generation || rv["browser_generation"]!=right_generation
    {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "src.identity_unverified",
            "both identities require current evidence-backed verification",
        ));
    }
    let left_cred = l["credential_ref"].as_str().unwrap_or("");
    let right_cred = r["credential_ref"].as_str().unwrap_or("");
    let credentials_shared = if left_cred.is_empty() || right_cred.is_empty() {
        Value::Null
    } else {
        json!(left_cred == right_cred)
    };
    let same_account = lv["expected"]["account"] == rv["expected"]["account"]
        && lv["expected"]["tenant"] == rv["expected"]["tenant"];
    let ready = credentials_shared == false && !same_account;
    let mut blocked = Vec::new();
    if credentials_shared == true {
        blocked.push("identities reference the same credential");
    }
    if credentials_shared.is_null() {
        blocked.push("credential separation cannot be established from managed references");
    }
    if same_account {
        blocked.push("A/B comparison requires different accounts or tenants");
    }
    Ok(
        json!({"ready":ready,"left":lv,"right":rv,"credentials_shared":credentials_shared,
        "same_account":same_account,"blocked_reasons":blocked,
        "browser_session_verified":lv["browser_session_verified"]==true && rv["browser_session_verified"]==true}),
    )
}

fn validate_extract_spec(extract: &Value) -> Api<Value> {
    let obj = extract
        .as_object()
        .ok_or_else(|| Error::invalid("extract must be an object"))?;
    let source = obj
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("");
    match source {
        "json_body" => {
            let pointer = obj
                .get("pointer")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| v.starts_with('/'))
                .ok_or_else(|| Error::invalid("extract.pointer must start with '/' for json_body"))?;
            Ok(json!({"source": "json_body", "pointer": pointer}))
        }
        "response_header" => {
            let name = obj
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| Error::invalid("extract.name is required for response_header"))?;
            HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| Error::invalid("extract.name is not a valid HTTP header name"))?;
            Ok(json!({"source": "response_header", "name": name}))
        }
        "set_cookie" => {
            let name = obj
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| Error::invalid("extract.name is required for set_cookie"))?;
            Ok(json!({"source": "set_cookie", "name": name}))
        }
        _ => Err(Error::invalid("extract.source must be json_body, response_header, or set_cookie")),
    }
}

fn validate_inject_spec(inject: &Value) -> Api<Value> {
    let location = inject["location"].as_str().unwrap_or("header");
    let name = inject["name"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("Authorization");
    let format = inject["format"].as_str().unwrap_or("bearer");
    if !matches!(location, "header" | "query") || !matches!(format, "raw" | "bearer") {
        return Err(Error::invalid("invalid inject location or format"));
    }
    if location == "header" {
        HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| Error::invalid("inject header name invalid"))?;
    } else if !name.bytes().all(|v| v.is_ascii_alphanumeric() || matches!(v, b'_' | b'-')) {
        return Err(Error::invalid("inject query parameter invalid"));
    }
    Ok(json!({"location": location, "name": name, "format": format}))
}

pub(super) fn save_login_recipe(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let context = require(body, "context_id")?;
    let ident_row = identity(s, id, context)?;
    let login_draft_id = require(body, "login_draft_id")?;
    let draft = rec(s, id, "request_draft", login_draft_id)?["payload"].clone();
    let target_url = require(&draft, "target_url")?;
    let t = task(s, id)?;
    let p = policy(s, id)?;
    if !in_policy(&p, target_url)
        || !t["input"]["authorization"]["targets"]
            .as_array()
            .is_some_and(|rows| {
                rows.iter().any(|v| {
                    v.as_str()
                        .is_some_and(|allowed| crate::runs::src_api::target_matches(target_url, allowed))
                })
            })
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "authorization_unresolved",
            "login recipe draft target is outside current Task scope",
        ));
    }
    let target_alias = require(body, "target_credential_alias")?;
    let target_cred_ref = credential_store::managed_reference(id, target_alias)?;
    let mut login_bindings = Vec::new();
    for item in body["login_credential_bindings"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let location = require(item, "location")?;
        let name = require(item, "name")?;
        let reference = require(item, "reference")?;
        let format = item["format"].as_str().unwrap_or("raw");
        if !matches!(location, "header" | "query") || !matches!(format, "raw" | "bearer") {
            return Err(Error::invalid("invalid login_credential_bindings entry"));
        }
        credential(reference)?;
        credential_store::version(s, id, reference)?;
        login_bindings.push(json!({
            "location": location,
            "name": name,
            "reference": reference,
            "format": format
        }));
    }
    let extract = validate_extract_spec(&body["extract"])?;
    let inject = validate_inject_spec(&body["inject"])?;
    let draft_revision = body["login_draft_revision"]
        .as_u64()
        .unwrap_or_else(|| draft["revision"].as_u64().unwrap_or(1));
    if draft_revision != draft["revision"].as_u64().unwrap_or(1) {
        return Err(Error::conflict("login_draft_revision does not match current draft"));
    }
    let max_reauth = body["max_reauth_attempts"].as_u64().unwrap_or(2).clamp(1, 5);
    let threshold = body["consecutive_auth_failures_threshold"]
        .as_u64()
        .unwrap_or(2)
        .clamp(1, 5);
    let prior = rec(s, id, "login_recipe", context).ok();
    let rev = prior.as_ref().and_then(|r| r["revision"].as_u64()).unwrap_or(0);
    let recipe = json!({
        "context_id": context,
        "task_id": id,
        "login_draft_id": login_draft_id,
        "login_draft_revision": draft_revision,
        "target_url": target_url,
        "login_credential_bindings": login_bindings,
        "extract": extract,
        "target_credential_alias": target_alias,
        "target_credential_ref": target_cred_ref,
        "inject": inject,
        "max_reauth_attempts": max_reauth,
        "consecutive_auth_failures_threshold": threshold,
        "consecutive_auth_failures": 0,
        "reauth_attempts": 0,
        "reauth_exhausted": false,
        "status": "active",
        "credential_version": prior.as_ref().and_then(|r| r["payload"]["credential_version"].as_u64()),
        "last_login_exchange_id": prior.as_ref().map(|r| r["payload"]["last_login_exchange_id"].clone()).unwrap_or(Value::Null),
        "last_replayed_at": prior.as_ref().map(|r| r["payload"]["last_replayed_at"].clone()).unwrap_or(Value::Null),
        "last_error": "",
        "updated_at": now()
    });
    let saved = save_rec(s, id, "login_recipe", context, recipe, Some(rev))?["payload"].clone();
    let mut ident_payload = ident_row["payload"].clone();
    if ident_payload["credential_ref"].as_str().unwrap_or("").is_empty() {
        ident_payload["credential_ref"] = json!(target_cred_ref);
        let _ = save_rec(
            s,
            id,
            "browser_identity",
            context,
            ident_payload,
            ident_row["revision"].as_u64(),
        );
    }
    Ok(saved)
}

pub(super) fn get_login_recipe(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let context = require(body, "context_id")?;
    Ok(rec(s, id, "login_recipe", context)?["payload"].clone())
}

fn extract_token_from_response(parsed: &Value, body: &[u8], spec: &Value) -> Api<String> {
    match spec["source"].as_str().unwrap_or("") {
        "json_body" => {
            let ptr = spec["pointer"].as_str().unwrap_or("");
            let json_val: Value = serde_json::from_slice(body)
                .map_err(|_| Error::conflict("login response body is not valid JSON"))?;
            let token = json_val
                .pointer(ptr)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| Error::conflict(format!("login response missing string token at {ptr}")))?;
            Ok(token.to_owned())
        }
        "response_header" => {
            let wanted = spec["name"].as_str().unwrap_or("");
            let value = parsed["headers"]
                .as_array()
                .into_iter()
                .flatten()
                .find_map(|pair| {
                    let k = pair.get(0)?.as_str()?;
                    let v = pair.get(1)?.as_str()?;
                    k.eq_ignore_ascii_case(wanted).then(|| v.trim().to_owned())
                })
                .filter(|v| !v.is_empty())
                .ok_or_else(|| Error::conflict(format!("login response missing header `{wanted}`")))?;
            Ok(value)
        }
        "set_cookie" => {
            let wanted = spec["name"].as_str().unwrap_or("");
            let prefix = format!("{wanted}=");
            let value = parsed["headers"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|pair| {
                    let k = pair.get(0)?.as_str()?;
                    let v = pair.get(1)?.as_str()?;
                    k.eq_ignore_ascii_case("set-cookie").then_some(v)
                })
                .find_map(|cookie| {
                    let first = cookie.split(';').next()?.trim();
                    first
                        .strip_prefix(&prefix)
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned)
                })
                .ok_or_else(|| Error::conflict(format!("login response missing Set-Cookie `{wanted}`")))?;
            Ok(value)
        }
        _ => Err(Error::invalid("unsupported login extract source")),
    }
}

pub(super) async fn replay_login_recipe(s: Arc<AppState>, id: &str, body: &Value) -> Api<Value> {
    static REAUTH_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let _guard = REAUTH_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let context = require(body, "context_id")?;
    let row = rec(&s, id, "login_recipe", context)?;
    let mut recipe = row["payload"].clone();
    let force = body["force"] == true;
    if recipe["reauth_exhausted"] == true && !force {
        return Err(Error::new(
            StatusCode::CONFLICT,
            "src.identity_reauth_exhausted",
            "login recipe reauth attempts exhausted",
        ));
    }
    let p = policy(&s, id)?;
    let draft_id = require(&recipe, "login_draft_id")?.to_owned();
    let draft = rec(&s, id, "request_draft", &draft_id)?["payload"].clone();
    let draft_rev = draft["revision"].as_u64().unwrap_or(1);
    let exec_idem = format!("login-replay-{context}-{}", Uuid::new_v4().simple());
    let exec_req = json!({
        "draft_id": draft_id,
        "draft_revision": draft_rev,
        "authorization_version": p["version"],
        "context_id": context,
        "identity_ref": context,
        "credential_bindings": recipe["login_credential_bindings"],
        "actor_ref": "identity.login_recipe",
        "wait_for_approval": true,
        "idempotency_key": exec_idem
    });
    let exec_out = Box::pin(egress::execute_draft_inner(s.clone(), id, &exec_req, false)).await?;
    let output = &exec_out["output"];
    let login_exchange_id = output["exchange_id"].as_str().unwrap_or("").to_owned();
    let status_code = output["exchange"]["status_code"].as_u64().unwrap_or(0);

    let extracted = if output["execution_state"] == "completed" && (200..300).contains(&status_code) {
        let resp_ref = &output["exchange"]["response_raw_ref"];
        match verify_ref(&s, id, resp_ref).and_then(|artifact| {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(artifact["data"].as_str().unwrap_or(""))
                .map_err(|_| Error::invalid("login response artifact invalid"))?;
            let method = output["exchange"]["method"].as_str().unwrap_or("POST");
            let parsed = super::exchange_queries::parsed_response(&raw, method)?;
            let (decoded, limits) = super::exchange_queries::decoded_body(&parsed, method)?;
            if !limits.is_empty() {
                return Err(Error::conflict("login response decoding incomplete"));
            }
            extract_token_from_response(&parsed, &decoded, &recipe["extract"])
        }) {
            Ok(tok) => Ok(tok),
            Err(err) => Err(err.message),
        }
    } else {
        Err(format!(
            "login HTTP execution ended with state=`{}` status={status_code}",
            output["execution_state"].as_str().unwrap_or("unknown")
        ))
    };

    match extracted {
        Ok(secret_token) => {
            let alias = require(&recipe, "target_credential_alias")?.to_owned();
            let cred_meta = credential_store::put(
                &s,
                id,
                &json!({"alias": alias, "value": secret_token}),
            )?;
            let stamp = now();
            recipe["status"] = json!("active");
            recipe["consecutive_auth_failures"] = json!(0);
            recipe["reauth_attempts"] = json!(0);
            recipe["reauth_exhausted"] = json!(false);
            recipe["credential_version"] = cred_meta["version"].clone();
            recipe["last_login_exchange_id"] = json!(login_exchange_id);
            recipe["last_replayed_at"] = json!(stamp);
            recipe["last_error"] = json!("");
            recipe["updated_at"] = json!(stamp);
            save_rec(
                &s,
                id,
                "login_recipe",
                context,
                recipe.clone(),
                row["revision"].as_u64(),
            )?;

            let ident_row = identity(&s, id, context)?;
            let mut ident = ident_row["payload"].clone();
            ident["credential_ref"] = cred_meta["reference"].clone();
            ident["session_version"] = json!(ident["session_version"].as_u64().unwrap_or(0) + 1);
            ident["session_status"] = json!("active");
            save_rec(
                &s,
                id,
                "browser_identity",
                context,
                ident.clone(),
                ident_row["revision"].as_u64(),
            )?;
            Ok(json!({
                "context_id": context,
                "status": "refreshed",
                "credential_ref": cred_meta["reference"],
                "credential_version": cred_meta["version"],
                "session_version": ident["session_version"],
                "login_exchange_id": login_exchange_id,
                "reauth_exhausted": false
            }))
        }
        Err(reason) => {
            let attempts = recipe["reauth_attempts"].as_u64().unwrap_or(0) + 1;
            let max_attempts = recipe["max_reauth_attempts"].as_u64().unwrap_or(2);
            let exhausted = attempts >= max_attempts;
            let stamp = now();
            recipe["reauth_attempts"] = json!(attempts);
            recipe["reauth_exhausted"] = json!(exhausted);
            recipe["status"] = json!(if exhausted { "reauth_exhausted" } else { "active" });
            recipe["last_login_exchange_id"] = json!(login_exchange_id);
            recipe["last_error"] = json!(reason);
            recipe["updated_at"] = json!(stamp);
            save_rec(
                &s,
                id,
                "login_recipe",
                context,
                recipe.clone(),
                row["revision"].as_u64(),
            )?;
            if exhausted {
                let ident_row = identity(&s, id, context)?;
                let mut ident = ident_row["payload"].clone();
                ident["session_status"] = json!("reauth_exhausted");
                ident["browser_session_verified"] = json!(false);
                let _ = save_rec(
                    &s,
                    id,
                    "browser_identity",
                    context,
                    ident,
                    ident_row["revision"].as_u64(),
                );
                if !login_exchange_id.is_empty() {
                    let _ = coverage::record_blocker(
                        &s,
                        id,
                        &json!({
                            "blocker_id": format!("reauth-exhausted-{context}"),
                            "category": "authentication",
                            "target": recipe["target_url"],
                            "detail": format!("身份 `{context}` 登录配方续期连续失败 ({attempts}/{max_attempts}): {reason}"),
                            "exchange_ids": [login_exchange_id]
                        }),
                    );
                }
            }
            Err(Error::new(
                StatusCode::CONFLICT,
                if exhausted {
                    "src.identity_reauth_exhausted"
                } else {
                    "src.identity_reauth_failed"
                },
                format!("login recipe replay failed: {reason}"),
            ))
        }
    }
}

fn mark_context_auth_failed(s: &AppState, id: &str, context_id: &str, code: u64) {
    let stamp = now();
    let msg = format!("exchange returned HTTP {code}");
    if let Ok(jar_row) = rec(s, id, "session_jar", context_id) {
        let mut p = jar_row["payload"].clone();
        p["status"] = json!("auth_failed");
        p["last_probe_status_code"] = json!(code);
        p["last_probe_at"] = json!(stamp);
        p["last_probe_error"] = json!(msg);
        p["updated_at"] = json!(stamp);
        let _ = save_rec(s, id, "session_jar", context_id, p, jar_row["revision"].as_u64());
    }
    if let Ok(ident_row) = identity(s, id, context_id) {
        let mut ident = ident_row["payload"].clone();
        ident["session_status"] = json!("auth_failed");
        ident["browser_session_verified"] = json!(false);
        let _ = save_rec(
            s,
            id,
            "browser_identity",
            context_id,
            ident,
            ident_row["revision"].as_u64(),
        );
    }
}

pub(super) async fn observe_exchange_auth_status(
    s: Arc<AppState>,
    id: &str,
    context_id: &str,
    status_code: Option<u64>,
) -> Api<Option<Value>> {
    if context_id.is_empty() || matches!(context_id, "default" | "anonymous") {
        return Ok(None);
    }
    let Some(code) = status_code else {
        return Ok(None);
    };
    let recipe_row = rec(&s, id, "login_recipe", context_id).ok();
    if (200..400).contains(&code) {
        if let Some(row) = recipe_row {
            let mut recipe = row["payload"].clone();
            if recipe["consecutive_auth_failures"].as_u64().unwrap_or(0) > 0 {
                recipe["consecutive_auth_failures"] = json!(0);
                recipe["updated_at"] = json!(now());
                let _ = save_rec(
                    &s,
                    id,
                    "login_recipe",
                    context_id,
                    recipe,
                    row["revision"].as_u64(),
                );
            }
        }
        return Ok(None);
    }
    if !matches!(code, 401 | 403) {
        return Ok(None);
    }
    let Some(row) = recipe_row else {
        mark_context_auth_failed(&s, id, context_id, code);
        return Ok(Some(json!({
            "triggered": false,
            "succeeded": false,
            "status": "auth_failed",
            "status_code": code
        })));
    };
    let mut recipe = row["payload"].clone();
    let failures = recipe["consecutive_auth_failures"].as_u64().unwrap_or(0) + 1;
    let threshold = recipe["consecutive_auth_failures_threshold"]
        .as_u64()
        .unwrap_or(2);
    recipe["consecutive_auth_failures"] = json!(failures);
    recipe["updated_at"] = json!(now());
    let _ = save_rec(
        &s,
        id,
        "login_recipe",
        context_id,
        recipe.clone(),
        row["revision"].as_u64(),
    );
    if recipe["reauth_exhausted"] == true {
        mark_context_auth_failed(&s, id, context_id, code);
        return Ok(Some(json!({
            "triggered": false,
            "succeeded": false,
            "reauth_exhausted": true,
            "consecutive_auth_failures": failures
        })));
    }
    if failures < threshold {
        mark_context_auth_failed(&s, id, context_id, code);
        return Ok(Some(json!({
            "triggered": false,
            "consecutive_auth_failures": failures,
            "threshold": threshold
        })));
    }
    match replay_login_recipe(s.clone(), id, &json!({"context_id": context_id})).await {
        Ok(refreshed) => {
            if let Ok(jar_row) = rec(&s, id, "session_jar", context_id) {
                let mut p = jar_row["payload"].clone();
                p["status"] = json!("active");
                p["last_verified_at"] = json!(now());
                p["last_probe_error"] = json!("");
                p["updated_at"] = json!(now());
                let _ = save_rec(
                    &s,
                    id,
                    "session_jar",
                    context_id,
                    p,
                    jar_row["revision"].as_u64(),
                );
            }
            Ok(Some(json!({
                "triggered": true,
                "succeeded": true,
                "recipe": recipe,
                "refreshed": refreshed
            })))
        }
        Err(err) => {
            mark_context_auth_failed(&s, id, context_id, code);
            let exhausted = rec(&s, id, "login_recipe", context_id)
                .ok()
                .is_some_and(|r| r["payload"]["reauth_exhausted"] == true);
            Ok(Some(json!({
                "triggered": true,
                "succeeded": false,
                "reauth_exhausted": exhausted,
                "error_code": err.code,
                "error": err.message
            })))
        }
    }
}


fn jar_alias(context_id: &str) -> String {
    format!("jar_{:x}", Sha256::digest(context_id.as_bytes()))
}

fn totp_alias(context_id: &str) -> String {
    format!("totp_{:x}", Sha256::digest(context_id.as_bytes()))
}

fn decode_base32(input: &str) -> Api<Vec<u8>> {
    let cleaned: String = input
        .trim()
        .trim_end_matches('=')
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    let mut buffer: u32 = 0;
    let mut bits_left: u8 = 0;
    let mut out = Vec::new();
    for ch in cleaned.chars() {
        let val = match ch {
            'A'..='Z' => (ch as u8 - b'A') as u32,
            '2'..='7' => (ch as u8 - b'2' + 26) as u32,
            _ => return Err(Error::invalid("invalid Base32 character in TOTP secret")),
        };
        buffer = (buffer << 5) | val;
        bits_left += 5;
        if bits_left >= 8 {
            bits_left -= 8;
            out.push(((buffer >> bits_left) & 0xFF) as u8);
        }
    }
    Ok(out)
}

pub(super) fn generate_totp_code(secret_b32: &str, unix_secs: u64) -> Api<String> {
    let key_bytes = decode_base32(secret_b32)?;
    let counter = (unix_secs / 30).to_be_bytes();
    let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, &key_bytes);
    let sig = hmac::sign(&key, &counter);
    let bytes = sig.as_ref();
    let offset = (bytes[bytes.len() - 1] & 0x0F) as usize;
    let code_u32 = (((bytes[offset] & 0x7F) as u32) << 24)
        | ((bytes[offset + 1] as u32) << 16)
        | ((bytes[offset + 2] as u32) << 8)
        | (bytes[offset + 3] as u32);
    Ok(format!("{:06}", code_u32 % 1_000_000))
}

fn parse_cookie_header(raw: &str, domain: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for pair in raw.split(';') {
        let trimmed = pair.trim();
        let Some((name, val)) = trimmed.split_once('=') else { continue };
        let k = name.trim();
        if k.is_empty() {
            continue;
        }
        out.push(json!({
            "name": k,
            "value": val.trim(),
            "domain": domain,
            "path": "/",
            "secure": false,
            "http_only": false,
            "expires_at": Value::Null
        }));
    }
    out
}

fn load_sealed_jar(s: &AppState, id: &str, context_id: &str) -> Api<Value> {
    let alias = jar_alias(context_id);
    let reference = match rec(s,id,"session_jar",context_id) {
        Ok(row) => row["payload"]["credential_ref"].as_str().map(str::to_owned).ok_or_else(|| Error::invalid("session jar credential reference missing"))?,
        Err(error) if error.status == StatusCode::NOT_FOUND => format!("secret://src/{id}/{alias}"),
        Err(error) => return Err(error),
    };
    match credential_store::resolve(s, id, &reference, None) {
        Ok(raw) => serde_json::from_str(&raw).map_err(|_| Error::invalid("stored session jar corrupted")),
        Err(e) if e.status == StatusCode::NOT_FOUND => Ok(json!({ "cookies": [], "headers": {} })),
        Err(e) => Err(e),
    }
}

fn save_sealed_jar(
    s: &AppState,
    id: &str,
    context_id: &str,
    jar: &Value,
    status: &str,
    last_verified_at: Option<&str>,
    last_probe_status: Option<u64>,
    last_probe_error: &str,
) -> Api<Value> {
    let serialized = serde_json::to_string(jar).map_err(super::db)?;
    let fingerprint = format!("sha256:{}", &format!("{:x}", Sha256::digest(serialized.as_bytes()))[..24]);
    let alias = jar_alias(context_id);
    let cred_meta = credential_store::put(s, id, &json!({ "alias": alias, "value": serialized }))?;

    let mut expired_cookie_count = 0usize;
    let now_utc = chrono::Utc::now();
    let cookie_meta: Vec<Value> = jar["cookies"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            let expired = c["expires_at"]
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .is_some_and(|dt| dt <= now_utc);
            if expired {
                expired_cookie_count += 1;
            }
            json!({
                "name": c["name"],
                "domain": c["domain"],
                "path": c["path"],
                "secure": c["secure"] == true,
                "http_only": c["http_only"] == true,
                "expires_at": c["expires_at"],
                "expired": expired,
            })
        })
        .collect();
    let header_names: Vec<String> = jar["headers"]
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();

    let prior = rec(s, id, "session_jar", context_id).ok();
    let prior_rev = prior.as_ref().and_then(|r| r["revision"].as_u64()).unwrap_or(0);
    let prior_verified = prior
        .as_ref()
        .and_then(|r| r["payload"]["last_verified_at"].as_str())
        .map(str::to_owned);
    let prior_probe_at = prior
        .as_ref()
        .and_then(|r| r["payload"]["last_probe_at"].as_str())
        .map(str::to_owned);
    let totp_ref = prior
        .as_ref()
        .and_then(|r| r["payload"]["totp_secret_ref"].as_str())
        .unwrap_or("")
        .to_owned();
    let stamp = now();
    let last_probe_at = if last_probe_status.is_some() {
        Some(stamp.clone())
    } else {
        prior_probe_at
    };

    let meta_payload = json!({
        "context_id": context_id,
        "task_id": id,
        "credential_ref": cred_meta["reference"],
        "credential_version": cred_meta["version"],
        "session_fingerprint": fingerprint,
        "status": status,
        "cookie_meta": cookie_meta,
        "expired_cookie_count": expired_cookie_count,
        "header_names": header_names,
        "totp_configured": !totp_ref.is_empty(),
        "totp_secret_ref": if totp_ref.is_empty() { Value::Null } else { json!(totp_ref) },
        "last_verified_at": last_verified_at.map(str::to_owned).or(prior_verified),
        "last_probe_at": last_probe_at,
        "last_probe_status_code": last_probe_status,
        "last_probe_error": last_probe_error,
        "updated_at": stamp,
    });
    save_rec(s, id, "session_jar", context_id, meta_payload.clone(), Some(prior_rev))?;

    if let Ok(ident_row) = identity(s, id, context_id) {
        let mut ident = ident_row["payload"].clone();
        ident["credential_ref"] = cred_meta["reference"].clone();
        ident["session_status"] = json!(status);
        ident["session_fingerprint"] = json!(fingerprint);
        let _ = save_rec(s, id, "browser_identity", context_id, ident, ident_row["revision"].as_u64());
    }
    Ok(meta_payload)
}

pub(super) fn import_session(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let context_id = require(body, "context_id")?;
    if matches!(context_id, "default" | "anonymous") {
        return Err(Error::invalid("cannot bind session jar to anonymous/default context"));
    }
    if identity(s, id, context_id).is_err() {
        ensure(s, id, &json!({ "context_id": context_id, "account_ref": context_id }))?;
    }
    let mut jar = load_sealed_jar(s, id, context_id)?;
    let domain = body["domain"].as_str().unwrap_or("");
    if domain.trim().is_empty() { return Err(Error::invalid("session domain is required")); }
    jar["header_domain"] = json!(domain.trim().to_ascii_lowercase());
    let mut cookies = jar["cookies"].as_array().cloned().unwrap_or_default();
    if let Some(raw_cookie) = body["raw_cookie_header"].as_str().filter(|v| !v.trim().is_empty()) {
        for nc in parse_cookie_header(raw_cookie, domain) {
            let n = nc["name"].as_str().unwrap_or("");
            cookies.retain(|c| !(c["name"] == n && c["domain"] == nc["domain"]));
            cookies.push(nc);
        }
    }
    for c in body["cookies"].as_array().into_iter().flatten() {
        let Some(name) = c["name"].as_str().filter(|v| !v.is_empty()) else { continue };
        let Some(val) = c["value"].as_str() else { continue };
        let c_dom = c["domain"].as_str().unwrap_or(domain);
        cookies.retain(|existing| !(existing["name"] == name && existing["domain"] == c_dom));
        cookies.push(json!({
            "name": name,
            "value": val,
            "host_only": c["host_only"].as_bool().unwrap_or(!c_dom.starts_with('.')),
            "domain": c_dom.trim_start_matches('.'),
            "path": c["path"].as_str().unwrap_or("/"),
            "secure": c["secure"] == true,
            "http_only": c["http_only"] == true,
            "expires_at": c["expires_at"],
        }));
    }
    let mut hdr_map = jar["headers"].as_object().cloned().unwrap_or_default();
    if let Some(in_hdrs) = body["headers"].as_object() {
        for (k, v) in in_hdrs {
            if let Some(vs) = v.as_str().filter(|s| !s.is_empty()) {
                hdr_map.insert(k.clone(), json!(vs));
            }
        }
    }
    jar["cookies"] = json!(cookies);
    jar["headers"] = Value::Object(hdr_map);

    if let Some(totp_secret) = body["totp_secret"].as_str().filter(|v| !v.trim().is_empty()) {
        decode_base32(totp_secret)?;
        let alias = totp_alias(context_id);
        let t_meta = credential_store::put(s, id, &json!({ "alias": alias, "value": totp_secret.trim() }))?;
        if let Ok(prior) = rec(s, id, "session_jar", context_id) {
            let mut p = prior["payload"].clone();
            p["totp_secret_ref"] = t_meta["reference"].clone();
            p["totp_configured"] = json!(true);
            let _ = save_rec(s, id, "session_jar", context_id, p, prior["revision"].as_u64());
        } else {
            let saved_meta = save_sealed_jar(s, id, context_id, &jar, "unverified", None, None, "")?;
            let mut p = saved_meta;
            p["totp_secret_ref"] = t_meta["reference"].clone();
            p["totp_configured"] = json!(true);
            let cur_rev = rec(s, id, "session_jar", context_id).ok().and_then(|r| r["revision"].as_u64());
            save_rec(s, id, "session_jar", context_id, p.clone(), cur_rev)?;
            return Ok(p);
        }
    }
    save_sealed_jar(s, id, context_id, &jar, "unverified", None, None, "")
}

pub(super) async fn probe_session(s: Arc<AppState>, id: &str, body: &Value) -> Api<Value> {
    let context_id = require(body, "context_id")?;
    let probe_url = require(body, "probe_url")?;
    let t = task(&s, id)?;
    let p = policy(&s, id)?;
    if !in_policy(&p, probe_url)
        || !t["input"]["authorization"]["targets"]
            .as_array()
            .is_some_and(|rows| {
                rows.iter().any(|v| v.as_str().is_some_and(|a| crate::runs::src_api::target_matches(probe_url, a)))
            })
    {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "authorization_unresolved",
            "session probe URL outside Task authorization",
        ));
    }
    let url = reqwest::Url::parse(probe_url).map_err(|_| Error::invalid("invalid probe_url"))?;
    let mut hdrs = Vec::new();
    let _ = apply_jar_to_request(&s, id, context_id, &url, &mut hdrs)?;

    if let Ok(jar_meta) = rec(&s, id, "session_jar", context_id) {
        if let Some(totp_ref) = jar_meta["payload"]["totp_secret_ref"].as_str().filter(|v| !v.is_empty()) {
            if let Ok(seed) = credential_store::resolve(&s, id, totp_ref, None) {
                if let Ok(code) = generate_totp_code(&seed, chrono::Utc::now().timestamp().max(0) as u64) {
                    hdrs.push(("X-TOTP-Code".to_owned(), code));
                }
            }
        }
    }
    let client = crate::network_proxy::direct_client_builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(super::db)?;
    let method_str = body["method"].as_str().unwrap_or("GET");
    let method = reqwest::Method::from_bytes(method_str.as_bytes()).map_err(|_| Error::invalid("invalid probe method"))?;
    super::guardrails::check_runtime_guardrails(&s,id,method_str,probe_url,body["source_run_id"].as_str(),&body["budget"])?;
    let mut req = client.request(method, url.clone());
    for (k, v) in hdrs {
        req = req.header(k, v);
    }
    let jar = load_sealed_jar(&s, id, context_id)?;
    match req.send().await {
        Ok(resp) => {
            let status_code = resp.status().as_u16() as u64;
            let resp_headers: Vec<(String, String)> = resp
                .headers()
                .iter()
                .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.as_str().to_owned(), s.to_owned())))
                .collect();
            let _ = ingest_set_cookies_from_response(&s, id, context_id, probe_url, &resp_headers);
            let next_jar = load_sealed_jar(&s, id, context_id).unwrap_or(jar);
            let (jar_status, err_msg) = if (200..300).contains(&status_code) {
                ("active", "")
            } else if matches!(status_code, 401 | 403) {
                ("expired", "probe returned unauthorized/forbidden")
            } else {
                ("stale", "probe did not return 2xx active confirmation")
            };
            let stamp = now();
            save_sealed_jar(
                &s,
                id,
                context_id,
                &next_jar,
                jar_status,
                (jar_status == "active").then_some(stamp.as_str()),
                Some(status_code),
                err_msg,
            )
        }
        Err(err) => save_sealed_jar(&s, id, context_id, &jar, "stale", None, None, &err.to_string()),
    }
}

pub(super) fn session_status(s: &AppState, id: &str, params: &Value) -> Api<Value> {
    if let Some(ctx) = params["context_id"].as_str().filter(|v| !v.is_empty()) {
        let jar = rec(s, id, "session_jar", ctx).ok().map(|r| r["payload"].clone());
        let ident = identity(s, id, ctx).ok().map(|r| r["payload"].clone());
        if jar.is_none() && ident.is_none() {
            return Err(Error::new(StatusCode::NOT_FOUND, "src.record_not_found", "identity context not found"));
        }
        return Ok(json!({
            "context_id": ctx,
            "session_jar": jar,
            "identity": ident,
            "plaintext_secrets_exposed": false,
        }));
    }
    list_contexts(s, id)
}

fn count_expired_cookies(jar: Option<&Value>) -> (usize, usize) {
    let Some(cookies) = jar.and_then(|j| j["cookie_meta"].as_array()) else {
        return (0, 0);
    };
    let now_utc = chrono::Utc::now();
    let expired = cookies
        .iter()
        .filter(|c| {
            c["expires_at"]
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .is_some_and(|dt| dt <= now_utc)
        })
        .count();
    (cookies.len(), expired)
}

fn compute_effective_status(
    ident: Option<&Value>,
    jar: Option<&Value>,
    cookie_count: usize,
    expired_cookie_count: usize,
) -> &'static str {
    let raw_status = jar
        .and_then(|j| j["status"].as_str())
        .or_else(|| ident.and_then(|p| p["session_status"].as_str()))
        .unwrap_or("unverified");
    let last_probe_code = jar.and_then(|j| j["last_probe_status_code"].as_u64());
    let browser_verified = ident.is_some_and(|p| p["browser_session_verified"] == true);
    let last_verified_at = jar
        .and_then(|j| j["last_verified_at"].as_str())
        .or_else(|| {
            ident
                .and_then(|p| p["last_verification"]["verified_at"].as_str())
        })
        .filter(|s| !s.is_empty());

    if matches!(raw_status, "auth_failed" | "expired" | "reauth_exhausted")
        || matches!(last_probe_code, Some(401 | 403))
    {
        return "auth_failed";
    }
    if expired_cookie_count > 0 && cookie_count > 0 && expired_cookie_count >= cookie_count {
        return "cookies_expired";
    }
    if browser_verified
        || ident.is_some_and(|p| p["last_verification"]["status"] == "verified")
    {
        return "verified";
    }
    if raw_status == "active"
        && (last_probe_code.is_some_and(|c| (200..300).contains(&c)) || last_verified_at.is_some())
    {
        return "probe_2xx_cached";
    }
    if expired_cookie_count > 0 {
        return "cookies_expired";
    }
    "unverified"
}

pub(super) fn list_contexts(s: &AppState, id: &str) -> Api<Value> {
    let jars: std::collections::HashMap<String, Value> = records(s, id, "session_jar")
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r["payload"]["context_id"].as_str().unwrap_or("").to_owned(), r["payload"].clone()))
        .collect();
    let mut items = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for row in records(s, id, "browser_identity")? {
        let p = &row["payload"];
        let ctx = p["context_id"].as_str().unwrap_or("").to_owned();
        seen.insert(ctx.clone());
        let jar = jars.get(&ctx).cloned();
        let (cookie_count, expired_cookie_count) = count_expired_cookies(jar.as_ref());
        let effective_status =
            compute_effective_status(Some(p), jar.as_ref(), cookie_count, expired_cookie_count);
        let last_verified_at = jar
            .as_ref()
            .and_then(|j| j.get("last_verified_at"))
            .filter(|v| !v.is_null())
            .cloned()
            .or_else(|| p["last_verification"].get("verified_at").cloned())
            .unwrap_or(Value::Null);
        let last_probe_at = jar
            .as_ref()
            .and_then(|j| j.get("last_probe_at"))
            .cloned()
            .unwrap_or(Value::Null);
        items.push(json!({
            "context_id": ctx,
            "account_ref": p["account_ref"],
            "role": p["role"],
            "tenant": p["tenant"],
            "credential_ref": p["credential_ref"],
            "status": jar.as_ref().and_then(|j| j["status"].as_str()).or_else(|| p["session_status"].as_str()).unwrap_or("unverified"),
            "effective_status": effective_status,
            "session_fingerprint": jar.as_ref().map(|j| j["session_fingerprint"].clone()).unwrap_or(Value::Null),
            "cookie_count": cookie_count,
            "expired_cookie_count": expired_cookie_count,
            "header_names": jar.as_ref().map(|j| j["header_names"].clone()).unwrap_or_else(|| json!([])),
            "cookie_meta": jar.as_ref().map(|j| j["cookie_meta"].clone()).unwrap_or_else(|| json!([])),
            "totp_configured": jar.as_ref().is_some_and(|j| j["totp_configured"] == true),
            "last_verified_at": last_verified_at,
            "last_probe_at": last_probe_at,
            "last_probe_status_code": jar.as_ref().map(|j| j["last_probe_status_code"].clone()).unwrap_or(Value::Null),
            "browser_session_verified": p["browser_session_verified"] == true,
        }));
    }
    for (ctx, jar) in jars {
        if seen.contains(&ctx) {
            continue;
        }
        let (cookie_count, expired_cookie_count) = count_expired_cookies(Some(&jar));
        let effective_status =
            compute_effective_status(None, Some(&jar), cookie_count, expired_cookie_count);
        items.push(json!({
            "context_id": ctx,
            "status": jar["status"],
            "effective_status": effective_status,
            "session_fingerprint": jar["session_fingerprint"],
            "cookie_count": cookie_count,
            "expired_cookie_count": expired_cookie_count,
            "cookie_meta": jar["cookie_meta"],
            "header_names": jar["header_names"],
            "totp_configured": jar["totp_configured"],
            "last_verified_at": jar["last_verified_at"],
            "last_probe_at": jar["last_probe_at"],
            "last_probe_status_code": jar["last_probe_status_code"],
        }));
    }
    Ok(json!({ "items": items, "plaintext_secrets_exposed": false }))
}

fn cookie_domain_matches(req_host: &str, cookie_domain: &str) -> bool {
    let cd = cookie_domain.trim().trim_start_matches('.').to_ascii_lowercase();
    if cd.is_empty() { return false; }
    let h = req_host.trim().to_ascii_lowercase();
    h == cd || h.ends_with(&format!(".{cd}"))
}

pub(super) fn apply_jar_to_request(
    s: &AppState,
    id: &str,
    context_id: &str,
    url: &reqwest::Url,
    headers: &mut Vec<(String, String)>,
) -> Api<bool> {
    if context_id.is_empty() || matches!(context_id, "default" | "anonymous") {
        return Ok(false);
    }
    let jar = load_sealed_jar(s, id, context_id)?;
    let before_len = headers.len();
    headers.retain(|(name,_)| !super::exchange_queries::authentication_header(name));
    let mut changed = before_len != headers.len();
    let req_host = url.host_str().unwrap_or("");
    let req_path = url.path();
    let is_https = url.scheme().eq_ignore_ascii_case("https");
    let now_utc = chrono::Utc::now();

    let mut cookie_pairs = Vec::new();
    for c in jar["cookies"].as_array().into_iter().flatten() {
        let name = c["name"].as_str().unwrap_or("");
        let val = c["value"].as_str().unwrap_or("");
        if name.is_empty() {
            continue;
        }
        if c["secure"] == true && !is_https {
            continue;
        }
        let dom = c["domain"].as_str().unwrap_or("");
        if !cookie_domain_matches(req_host, dom) || (c["host_only"].as_bool().unwrap_or(true) && !req_host.eq_ignore_ascii_case(dom.trim_start_matches('.'))) {
            continue;
        }
        let path = c["path"].as_str().unwrap_or("/");
        if !(req_path == path || (req_path.starts_with(path) && (path.ends_with('/') || req_path.as_bytes().get(path.len()) == Some(&b'/')))) {
            continue;
        }
        if let Some(exp) = c["expires_at"].as_str().and_then(|v| chrono::DateTime::parse_from_rfc3339(v).ok()) {
            if exp <= now_utc {
                continue;
            }
        }
        cookie_pairs.push(format!("{name}={val}"));
    }
    if !cookie_pairs.is_empty() {
        let joined = cookie_pairs.join("; ");
        headers.push(("Cookie".to_owned(), joined));
        changed = true;
    }
    for (k, v) in jar["headers"].as_object().filter(|_| jar["header_domain"].as_str().is_some_and(|host| req_host.eq_ignore_ascii_case(host))).into_iter().flatten() {
        let Some(val) = v.as_str() else { continue };
        headers.retain(|(hk,_)| !hk.eq_ignore_ascii_case(k));
        headers.push((k.clone(), val.to_owned()));
        changed = true;
    }
    Ok(changed)
}

pub(super) fn ingest_set_cookies_from_response(
    s: &AppState,
    id: &str,
    context_id: &str,
    observed_url: &str,
    response_headers: &[(String, String)],
) -> Api<bool> {
    if context_id.is_empty() || matches!(context_id, "default" | "anonymous") {
        return Ok(false);
    }
    let set_cookies: Vec<&str> = response_headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
        .map(|(_, v)| v.as_str())
        .collect();
    if set_cookies.is_empty() {
        return Ok(false);
    }
    let host = reqwest::Url::parse(observed_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default();
    let mut jar = load_sealed_jar(s, id, context_id)?;
    let mut cookies = jar["cookies"].as_array().cloned().unwrap_or_default();
    for raw in set_cookies {
        let mut parts = raw.split(';');
        let Some(first) = parts.next() else { continue };
        let Some((name, val)) = first.trim().split_once('=') else { continue };
        let mut domain = host.clone();
        let mut path = "/".to_owned();
        let mut secure = false;
        let mut http_only = false;
        let mut host_only = true;
        let mut expires_at: Option<String> = None;
        for attr in parts {
            let trimmed = attr.trim();
            if let Some((ak, av)) = trimmed.split_once('=') {
                if ak.eq_ignore_ascii_case("domain") {
                    domain = av.trim().trim_start_matches('.').to_owned();
                    host_only = false;
                } else if ak.eq_ignore_ascii_case("path") {
                    path = av.trim().to_owned();
                } else if ak.eq_ignore_ascii_case("max-age") {
                    if let Ok(secs) = av.trim().parse::<i64>() { expires_at = chrono::Utc::now().checked_add_signed(chrono::Duration::seconds(secs)).map(|date| date.to_rfc3339()); }
                } else if ak.eq_ignore_ascii_case("expires") && expires_at.is_none() {
                    let raw_exp = av.trim();
                    expires_at = chrono::DateTime::parse_from_rfc2822(raw_exp)
                        .map(|d| d.with_timezone(&chrono::Utc).to_rfc3339())
                        .or_else(|_| {
                            chrono::DateTime::parse_from_rfc3339(raw_exp)
                                .map(|d| d.with_timezone(&chrono::Utc).to_rfc3339())
                        })
                        .or_else(|_| {
                            chrono::NaiveDateTime::parse_from_str(raw_exp, "%a, %d-%b-%Y %H:%M:%S GMT")
                                .map(|d| d.and_utc().to_rfc3339())
                        })
                        .or_else(|_| {
                            chrono::NaiveDateTime::parse_from_str(raw_exp, "%a, %d %b %Y %H:%M:%S GMT")
                                .map(|d| d.and_utc().to_rfc3339())
                        })
                        .ok();
                }
            } else if trimmed.eq_ignore_ascii_case("secure") {
                secure = true;
            } else if trimmed.eq_ignore_ascii_case("httponly") {
                http_only = true;
            }
        }
        if !cookie_domain_matches(&host, &domain) { continue; }
        cookies.retain(|c| !(c["name"] == name.trim() && c["domain"] == domain && c["path"] == path));
        cookies.push(json!({
            "name": name.trim(),
            "value": val.trim(),
            "domain": domain,
            "path": path,
            "secure": secure,
            "http_only": http_only,
            "expires_at": expires_at,
            "host_only": host_only,
        }));
    }
    jar["cookies"] = json!(cookies);
    let prior_meta = rec(s, id, "session_jar", context_id).ok().map(|r| r["payload"].clone());
    let preserved_status = prior_meta
        .as_ref()
        .and_then(|m| m["status"].as_str())
        .filter(|st| matches!(*st, "verified" | "auth_failed" | "expired" | "reauth_exhausted"))
        .unwrap_or("unverified");
    let preserved_probe_code = prior_meta
        .as_ref()
        .and_then(|m| m["last_probe_status_code"].as_u64());
    let preserved_probe_err = prior_meta
        .as_ref()
        .and_then(|m| m["last_probe_error"].as_str())
        .unwrap_or("");
    save_sealed_jar(
        s,
        id,
        context_id,
        &jar,
        preserved_status,
        None,
        preserved_probe_code,
        preserved_probe_err,
    )?;
    Ok(true)
}

pub(super) fn harvest_proxy_credentials(
    s: &AppState,
    id: &str,
    context_id: &str,
    observed_url: &str,
    req_headers: &[(String, String)],
    resp_headers: &[(String, String)],
) -> Api<()> {
    let mut hdr_obj = serde_json::Map::new();
    let mut cookie_raw = String::new();
    for (k, v) in req_headers {
        if matches!(
            k.to_ascii_lowercase().as_str(),
            "authorization" | "x-auth-token" | "x-token" | "satoken" | "x-api-key"
        ) {
            hdr_obj.insert(k.clone(), json!(v));
        } else if k.eq_ignore_ascii_case("cookie") {
            cookie_raw = v.clone();
        }
    }
    if !hdr_obj.is_empty() || !cookie_raw.is_empty() {
        let domain = reqwest::Url::parse(observed_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_default();
        import_session(
            s,
            id,
            &json!({
                "context_id": context_id,
                "domain": domain,
                "raw_cookie_header": cookie_raw,
                "headers": hdr_obj,
            }),
        )?;
    }
    let _ = ingest_set_cookies_from_response(s, id, context_id, observed_url, resp_headers)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[tokio::test]
    async fn imported_identity_claim_cannot_become_verified_browser_session() {
        let state = std::sync::Arc::new(AppState::new(
            crate::store::Store::open(":memory:").unwrap(),
        ));
        let target = "https://example.com/me";
        let created = crate::runs::src_api::create_task_inner(
            &state,
            &json!({"title":"Identity",
            "goal":"Verify session","authorization":{"targets":[target]},
            "idempotency_key":"identity-import-reject"}),
        )
        .await
        .unwrap();
        let id = created["output"]["task_id"].as_str().unwrap();
        ensure(
            &state,
            id,
            &json!({"context_id":"person-a","account_ref":"a",
            "success_conditions":{"account":"a"}}),
        )
        .unwrap();
        let raw = b"GET /me HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let response=b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"account\":\"a\",\"session_valid\":true,\"protected_resource\":\"identity_endpoint\"}";
        let imported = super::super::record_exchange(
            &state,
            id,
            &json!({
            "request_raw_b64":base64::engine::general_purpose::STANDARD.encode(raw),
            "response_raw_b64":base64::engine::general_purpose::STANDARD.encode(response),
            "observed_url":target,"status_code":200}),
        )
        .unwrap();
        assert!(
            verify(
                &state,
                id,
                &json!({"context_id":"person-a","source_exchange_id":imported["exchange_id"],"observed":{"account":"a"},"browser_generation":0,"evidence_refs":[]})
            )
            .is_err()
        );
        let unverified = verify(&state, id, &json!({"context_id":"person-a","observed":{"account":"a"},"browser_generation":0,"evidence_refs":[]})).unwrap();
        assert_eq!(unverified["status"], "identity_unverified");
        assert_eq!(unverified["browser_session_verified"], false);
    }
}
