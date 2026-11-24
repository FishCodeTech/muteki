//! GraphQL Introspection & gRPC service schema ingestion + BFLA multi-role auth matrix.

use crate::AppState;
use crate::runs::src_api::{Api, Error, digest, now, rec, records, require, save_rec, task};
use serde_json::{Value, json};
use std::collections::BTreeSet;

fn parse_proto_text(proto: &str) -> Vec<Value> {
    let mut ops = Vec::new();
    let mut current_service = String::new();
    for raw_line in proto.lines() {
        let line = raw_line.split("//").next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("service ") {
            let name = rest.split(['{', ' ']).next().unwrap_or("").trim();
            if !name.is_empty() {
                current_service = name.to_owned();
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("rpc ") {
            if let Some((method_part, tail)) = rest.split_once('(') {
                let method = method_part.trim();
                let req_type = tail.split(')').next().unwrap_or("").trim();
                let resp_type = tail
                    .split("returns")
                    .nth(1)
                    .and_then(|s| s.split('(').nth(1))
                    .and_then(|s| s.split(')').next())
                    .unwrap_or("")
                    .trim();
                if !method.is_empty() {
                    let svc = if current_service.is_empty() {
                        "DefaultService"
                    } else {
                        current_service.as_str()
                    };
                    ops.push(json!({
                        "protocol": "grpc",
                        "kind": "rpc",
                        "service": svc,
                        "operation": format!("/{svc}/{method}"),
                        "method": method,
                        "request_type": req_type,
                        "response_type": resp_type,
                    }));
                }
            }
        }
    }
    ops
}

fn parse_graphql_introspection(schema_root: &Value) -> Vec<Value> {
    let schema = schema_root
        .get("__schema")
        .or_else(|| schema_root.get("data").and_then(|d| d.get("__schema")))
        .unwrap_or(schema_root);
    let query_name = schema["queryType"]["name"].as_str().unwrap_or("Query");
    let mutation_name = schema["mutationType"]["name"]
        .as_str()
        .unwrap_or("Mutation");
    let sub_name = schema["subscriptionType"]["name"]
        .as_str()
        .unwrap_or("Subscription");
    let mut ops = Vec::new();
    for ty in schema["types"].as_array().into_iter().flatten() {
        let ty_name = ty["name"].as_str().unwrap_or("");
        let kind = if ty_name == query_name {
            "query"
        } else if ty_name == mutation_name {
            "mutation"
        } else if ty_name == sub_name {
            "subscription"
        } else {
            continue;
        };
        for field in ty["fields"].as_array().into_iter().flatten() {
            let Some(op_name) = field["name"].as_str().filter(|v| !v.starts_with("__")) else {
                continue;
            };
            let args = field["args"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| a["name"].as_str().map(ToOwned::to_owned))
                .collect::<Vec<_>>();
            ops.push(json!({
                "protocol": "graphql",
                "kind": kind,
                "parent_type": ty_name,
                "operation": op_name,
                "args": args,
            }));
        }
    }
    ops
}

pub(super) fn ingest(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let _ = task(s, id)?;
    let protocol = match require(body, "protocol")?.trim().to_ascii_lowercase().as_str() {
        "graphql" => "graphql",
        "grpc" => "grpc",
        other => {
            return Err(Error::invalid(format!(
                "protocol must be 'graphql' or 'grpc', got '{other}'"
            )))
        }
    };
    let endpoint_url = body["endpoint_url"].as_str().unwrap_or("").trim().to_owned();
    let ops = if protocol == "graphql" {
        let schema_val = if body["schema"].is_object() {
            body["schema"].clone()
        } else if let Some(raw) = body["schema_text"].as_str() {
            serde_json::from_str(raw)
                .map_err(|err| Error::invalid(format!("GraphQL introspection JSON invalid: {err}")))?
        } else {
            return Err(Error::invalid("GraphQL schema object or schema_text JSON is required"));
        };
        parse_graphql_introspection(&schema_val)
    } else if let Some(proto_text) = body["proto_text"].as_str().filter(|v| !v.trim().is_empty()) {
        parse_proto_text(proto_text)
    } else if let Some(arr) = body["services"].as_array() {
        let mut out = Vec::new();
        for svc in arr {
            let sname = svc["name"].as_str().unwrap_or("Service");
            for m in svc["methods"].as_array().into_iter().flatten() {
                let mname = m["name"].as_str().or_else(|| m.as_str()).unwrap_or("");
                if !mname.is_empty() {
                    out.push(json!({
                        "protocol": "grpc",
                        "kind": "rpc",
                        "service": sname,
                        "operation": format!("/{sname}/{mname}"),
                        "method": mname,
                        "request_type": m["request_type"].as_str().unwrap_or(""),
                        "response_type": m["response_type"].as_str().unwrap_or(""),
                    }));
                }
            }
        }
        out
    } else {
        return Err(Error::invalid("gRPC proto_text or services[] array is required"));
    };

    let mut saved_ops = Vec::new();
    for op in &ops {
        let op_name = op["operation"].as_str().unwrap_or("");
        let kind = op["kind"].as_str().unwrap_or("op");
        let rec_id = format!(
            "ggop_{}",
            &digest(&json!([protocol, endpoint_url, kind, op_name]))[..20]
        );
        let doc = json!({
            "op_id": rec_id,
            "task_id": id,
            "endpoint_url": endpoint_url,
            "protocol": op["protocol"],
            "kind": kind,
            "service": op["service"],
            "operation": op_name,
            "args": op["args"],
            "request_type": op["request_type"],
            "response_type": op["response_type"],
            "ingested_at": now(),
        });
        let rev = rec(s, id, "graphql_grpc_operation", &rec_id)
            .ok()
            .and_then(|r| r["revision"].as_u64());
        save_rec(s, id, "graphql_grpc_operation", &rec_id, doc.clone(), rev)?;
        saved_ops.push(doc);
    }

    Ok(json!({
        "protocol": protocol,
        "endpoint_url": endpoint_url,
        "operation_count": saved_ops.len(),
        "operations": saved_ops,
    }))
}

pub(super) fn auth_matrix(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let _ = task(s, id)?;
    let ops = records(s, id, "graphql_grpc_operation")?;
    let mut contexts = BTreeSet::new();
    for c in body["contexts"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        if !c.trim().is_empty() {
            contexts.insert(c.trim().to_owned());
        }
    }
    if contexts.is_empty() {
        for r in records(s, id, "session_jar")? {
            if let Some(cid) = r["payload"]["context_id"].as_str() {
                contexts.insert(cid.to_owned());
            }
        }
    }
    let exchanges = records(s, id, "http_exchange")?;
    let mut matrix = Vec::new();
    let mut alerts = Vec::new();

    for op_row in ops {
        let op = &op_row["payload"];
        let op_name = op["operation"].as_str().unwrap_or("");
        let kind = op["kind"].as_str().unwrap_or("");
        let mut cells = serde_json::Map::new();
        let mut allowed_contexts = Vec::new();
        for ctx in &contexts {
            let matched_ex = exchanges.iter().rev().find(|ex| {
                let p = &ex["payload"];
                let ex_ctx = p["context_id"].as_str().unwrap_or("");
                let url = p["observed_url"].as_str().unwrap_or("");
                ex_ctx == ctx && (!op_name.is_empty() && url.contains(op_name))
            });
            if let Some(ex) = matched_ex {
                let status = ex["payload"]["status_code"].as_u64().unwrap_or(0);
                let access = if (200..300).contains(&status) {
                    allowed_contexts.push(ctx.clone());
                    "allowed_2xx"
                } else if matches!(status, 401 | 403) {
                    "denied_auth"
                } else {
                    "other_status"
                };
                cells.insert(
                    ctx.clone(),
                    json!({
                        "status_code": status,
                        "access": access,
                        "exchange_id": ex["payload"]["exchange_id"],
                    }),
                );
            } else {
                cells.insert(ctx.clone(), json!({"access": "untested"}));
            }
        }
        let multi_role_open = allowed_contexts.len() >= 2 && kind != "query";
        let row_doc = json!({
            "op_id": op["op_id"],
            "protocol": op["protocol"],
            "kind": kind,
            "operation": op_name,
            "cells": Value::Object(cells),
            "bfla_candidate": multi_role_open,
        });
        if multi_role_open {
            alerts.push(row_doc.clone());
        }
        matrix.push(row_doc);
    }

    Ok(json!({
        "contexts": contexts.into_iter().collect::<Vec<_>>(),
        "operation_count": matrix.len(),
        "bfla_candidate_count": alerts.len(),
        "bfla_candidates": alerts,
        "matrix": matrix,
    }))
}
