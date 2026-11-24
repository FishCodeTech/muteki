//! One-time human-approved concurrent HTTP race condition probe (N <= 20 barrier / single-packet style).

use super::{
    egress::raw_http, identity, parse_raw_request, prepare_exchange_dispatch, proxypool,
    put_capture, request_mutator, traffic_index,
};
use crate::AppState;
use crate::runs::src_api::{
    Api, COMMANDS, Error, db, digest, now, records, require, save_rec, task,
};
use axum::http::StatusCode;
use futures_util::future::join_all;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Barrier;
use uuid::Uuid;

pub(super) async fn race_probe(
    s: Arc<AppState>,
    id: &str,
    body: &Value,
    command_id: Option<&str>,
) -> Api<Value> {
    let _ = task(&s, id)?;
    let key = body["idempotency_key"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .or(command_id)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::invalid("idempotency_key is required for race_probe"))?;
    let cmd_storage = format!("{id}:src.exchange.race_probe:{key}");
    let req_hash = digest(&json!([id, body]));
    if let Some(old) = s.store.get(COMMANDS, &cmd_storage).map_err(db)? {
        if old["request_hash"] != req_hash {
            return Err(Error::conflict(
                "idempotency_key already consumed for a different race_probe payload",
            ));
        }
        return Ok(old["output"].clone());
    }
    if records(&s, id, "race_probe_result")?
        .iter()
        .any(|r| r["payload"]["idempotency_key"].as_str() == Some(key))
    {
        return Err(Error::conflict("one-time race_probe idempotency_key already executed"));
    }

    let count = body["concurrency"].as_u64().unwrap_or(5);
    if !(2..=20).contains(&count) {
        return Err(Error::invalid(
            "race_probe concurrency must be between 2 and 20",
        ));
    }
    let n = count as usize;
    let draft_id = require(body, "draft_id")?;
    let draft_rev = body["draft_revision"]
        .as_u64()
        .ok_or_else(|| Error::invalid("draft_revision is required"))?;
    let current_policy = crate::runs::src_api::policy(&s, id)?;
    let policy_ver = body["authorization_version"]
        .as_u64()
        .filter(|v| *v > 0)
        .unwrap_or_else(|| current_policy["version"].as_u64().unwrap_or(1));
    let prepared = prepare_exchange_dispatch(&s, id, draft_id, draft_rev, policy_ver).map_err(
        |msg| Error::new(StatusCode::FORBIDDEN, "authorization_unresolved", msg),
    )?;
    super::guardrails::check_runtime_guardrails(
        &s,
        id,
        &prepared.method,
        &prepared.url,
        body["source_run_id"].as_str(),
        &body["budget"],
    )?;

    let send_url = reqwest::Url::parse(&prepared.url).map_err(db)?;
    let context_id = body["context_id"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("default");
    let (_, _, mut headers) = parse_raw_request(&prepared.raw_request, send_url.scheme())?;
    let mut body_bytes = if let Some(pos) = prepared
        .raw_request
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
    {
        prepared.raw_request[pos + 4..].to_vec()
    } else if let Some(pos) = prepared
        .raw_request
        .windows(2)
        .position(|w| w == b"\n\n")
    {
        prepared.raw_request[pos + 2..].to_vec()
    } else {
        Vec::new()
    };
    let _ = identity::apply_jar_to_request(&s, id, context_id, &send_url, &mut headers)?;
    let mut final_url = send_url.clone();
    if let Some(mutator_id) = body["mutator_id"].as_str().filter(|v| !v.is_empty()) {
        request_mutator::apply_to_request(
            &s,
            id,
            mutator_id,
            &prepared.method,
            &mut final_url,
            &mut headers,
            &mut body_bytes,
        )
        .await?;
    }
    headers.retain(|(h, _)| !h.eq_ignore_ascii_case("content-length"));
    if !body_bytes.is_empty() || !matches!(prepared.method.as_str(), "GET" | "HEAD") {
        headers.push(("Content-Length".to_owned(), body_bytes.len().to_string()));
    }
    let first_line = prepared
        .raw_request
        .split(|b| *b == b'\n')
        .next()
        .unwrap_or(&[]);
    let first_str = std::str::from_utf8(first_line)
        .map_err(|_| Error::invalid("request line is not ASCII"))?
        .trim_end_matches('\r');
    let parts = first_str.split(' ').collect::<Vec<_>>();
    let target = format!(
        "{}{}",
        super::raw_endpoint_path(final_url.as_str()),
        final_url
            .query()
            .map(|q| format!("?{q}"))
            .unwrap_or_default()
    );
    let mut wire = format!(
        "{} {target} {}\r\n",
        prepared.method,
        parts.get(2).copied().unwrap_or("HTTP/1.1")
    )
    .into_bytes();
    for (name, val) in &headers {
        wire.extend_from_slice(format!("{name}: {val}\r\n").as_bytes());
    }
    wire.extend_from_slice(b"\r\n");
    wire.extend_from_slice(&body_bytes);

    if !crate::runs::src_api::in_policy(&current_policy, final_url.as_str()) {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "authorization_unresolved",
            "Race probe target is outside current Task policy scope",
        ));
    }
    let wire = Arc::new(wire);
    let upstream = proxypool::select_upstream(&s, id)?;
    let timeout = Duration::from_secs(
        body["timeout_seconds"]
            .as_u64()
            .unwrap_or(15)
            .clamp(2, 60),
    );
    let barrier = Arc::new(Barrier::new(n));
    let tasks = (0..n).map(|slot| {
        let s_clone = s.clone();
        let url_clone = final_url.clone();
        let method_clone = prepared.method.clone();
        let wire_clone = wire.clone();
        let up_clone = upstream.clone();
        let barrier_clone = barrier.clone();
        let id_owned = id.to_owned();
        let ctx_owned = context_id.to_owned();
        async move {
            barrier_clone.wait().await;
            let started = std::time::Instant::now();
            let capture = raw_http::exchange_via_proxy(
                &url_clone,
                &method_clone,
                &wire_clone,
                timeout,
                4 * 1024 * 1024,
                up_clone.as_ref(),
            )
            .await;
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let parsed = super::exchange_queries::parsed_response(&capture.raw, &method_clone).ok();
            let status_code = parsed.as_ref().and_then(|p| p["status_code"].as_u64());
            let body_sha256 = digest(&json!(capture.raw));
            let exchange_id = format!("srace_ex_{}", Uuid::new_v4().simple());
            let mut archive_error = None;
            if let Ok(req_ref) = put_capture(&s_clone, &id_owned, &wire_clone, &format!("{exchange_id}-req.http"))
                && let Ok(resp_ref) = put_capture(&s_clone, &id_owned, &capture.raw, &format!("{exchange_id}-resp.http"))
            {
                let ex = json!({
                    "exchange_id": exchange_id,
                    "task_id": id_owned,
                    "source_kind": "http_tool",
                    "context_id": ctx_owned,
                    "declared_target": url_clone.as_str(),
                    "observed_url": url_clone.as_str(),
                    "method": method_clone,
                    "started_at": now(),
                    "finished_at": now(),
                    "request_raw_ref": req_ref,
                    "response_raw_ref": resp_ref,
                    "status_code": status_code,
                    "protocol": parsed.as_ref().and_then(|p| p["version"].as_str()).unwrap_or("http/1.1"),
                    "execution_state": if capture.complete { "completed" } else { "failed" },
                    "capture_state": if capture.complete { "complete" } else { "partial" },
                });
                if let Err(error) = save_rec(&s_clone, &id_owned, "http_exchange", &exchange_id, ex.clone(), Some(0)) { archive_error = Some(error.message); }
                if let Err(error) = traffic_index::index_exchange(
                    &s_clone,
                    &id_owned,
                    &ex,
                    &wire_clone,
                    Some(&capture.raw),
                ) { archive_error = Some(error.message); }
            } else { archive_error = Some("Race probe evidence could not be archived".to_owned()); }
            json!({
                "archive_error": archive_error,
                "slot": slot,
                "exchange_id": exchange_id,
                "status_code": status_code,
                "complete": capture.complete,
                "error": capture.error,
                "elapsed_ms": elapsed_ms,
                "response_len": capture.raw.len(),
                "response_sha256": body_sha256,
            })
        }
    });

    let results = join_all(tasks).await;
    let mut status_histogram: BTreeMap<String, usize> = BTreeMap::new();
    let mut digest_histogram: BTreeMap<String, usize> = BTreeMap::new();
    for item in &results {
        let sk = item["status_code"]
            .as_u64()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "error".to_owned());
        *status_histogram.entry(sk).or_default() += 1;
        let dk = item["response_sha256"].as_str().unwrap_or("").to_owned();
        *digest_histogram.entry(dk).or_default() += 1;
    }

    let probe_id = format!("srace_{}", Uuid::new_v4().simple());
    let output = json!({
        "probe_id": probe_id,
        "task_id": id,
        "idempotency_key": key,
        "draft_id": draft_id,
        "target_url": final_url.as_str(),
        "concurrency": n,
        "status_histogram": status_histogram,
        "distinct_body_digests": digest_histogram.len(),
        "responses_diverged": status_histogram.len() > 1 || digest_histogram.len() > 1,
        "results": results,
        "executed_at": now(),
    });
    save_rec(&s, id, "race_probe_result", &probe_id, output.clone(), Some(0))?;
    let cmd_doc = json!({
        "command_id": command_id.unwrap_or(key),
        "request_hash": req_hash,
        "output": output,
    });
    s.store.put(COMMANDS, &cmd_storage, &cmd_doc).map_err(db)?;
    Ok(output)
}
