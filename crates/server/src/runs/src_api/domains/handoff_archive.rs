//! Secret-stripped topological Team Handoff `.tar.zst` / `.tar.gz` export & import
//! with Kahn topological ordering, decompression bomb/symlink guards, and two-phase import journal.

use crate::AppState;
use crate::runs::src_api::{
    Api, Error, RECORDS, REPORT_HEADS, db, digest, now, policy, rec, records, require, save_rec,
    task,
};
use axum::http::StatusCode;
use base64::Engine;
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{Read, Write};
use uuid::Uuid;

const MAX_DECOMPRESSED_BYTES: usize = 64 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 2048;

const ARCHIVE_KINDS: &[&str] = &[
    "asset",
    "observation",
    "dead_end",
    "target_blocker",
    "object_registration",
    "object_experiment",
    "request_mutator",
    "research_plan",
    "research_hypothesis",
    "coverage_item",
    "graphql_grpc_operation",
    "pkg_version_observation",
    "intel_advisory",
    "intel_match",
    "finding_claim",
    "finding_review",
    "finding_dedup",
    "finding_retest",
    "workbench_report_draft",
    "frozen_report",
    "workbench_suggestion",
    "workbench_todo",
];

fn kahn_topological_order() -> Api<Vec<&'static str>> {
    let edges: &[(&str, &str)] = &[
        ("object_registration", "object_experiment"),
        ("research_plan", "research_hypothesis"),
        ("asset", "research_hypothesis"),
        ("asset", "coverage_item"),
        ("research_plan", "coverage_item"),
        ("asset", "graphql_grpc_operation"),
        ("asset", "pkg_version_observation"),
        ("intel_advisory", "intel_match"),
        ("pkg_version_observation", "intel_match"),
        ("asset", "finding_claim"),
        ("research_hypothesis", "finding_claim"),
        ("coverage_item", "finding_claim"),
        ("intel_match", "finding_claim"),
        ("finding_claim", "finding_review"),
        ("finding_claim", "finding_dedup"),
        ("finding_claim", "finding_retest"),
        ("finding_claim", "workbench_report_draft"),
        ("finding_claim", "frozen_report"),
        ("finding_review", "frozen_report"),
        ("workbench_report_draft", "frozen_report"),
        ("finding_claim", "workbench_suggestion"),
        ("intel_match", "workbench_suggestion"),
        ("research_plan", "workbench_todo"),
        ("finding_claim", "workbench_todo"),
    ];

    let mut in_deg: HashMap<&'static str, usize> =
        ARCHIVE_KINDS.iter().map(|&k| (k, 0usize)).collect();
    let mut adj: HashMap<&'static str, Vec<&'static str>> =
        ARCHIVE_KINDS.iter().map(|&k| (k, Vec::new())).collect();

    for &(src, dst) in edges {
        if let (Some(children), Some(deg)) = (adj.get_mut(src), in_deg.get_mut(dst)) {
            children.push(dst);
            *deg += 1;
        }
    }

    let mut q: VecDeque<&'static str> = ARCHIVE_KINDS
        .iter()
        .copied()
        .filter(|k| in_deg.get(k).copied().unwrap_or(0) == 0)
        .collect();
    let mut order = Vec::with_capacity(ARCHIVE_KINDS.len());
    while let Some(node) = q.pop_front() {
        order.push(node);
        if let Some(children) = adj.get(node) {
            for &next in children {
                if let Some(d) = in_deg.get_mut(next) {
                    *d = d.saturating_sub(1);
                    if *d == 0 {
                        q.push_back(next);
                    }
                }
            }
        }
    }
    if order.len() != ARCHIVE_KINDS.len() {
        return Err(Error::conflict("handoff dependency DAG contains a cycle"));
    }
    Ok(order)
}

fn write_tar_entry<W: Write>(tar: &mut W, name: &str, data: &[u8]) -> std::io::Result<()> {
    let mut hdr = [0u8; 512];
    let name_bytes = name.as_bytes();
    let copy_len = name_bytes.len().min(100);
    hdr[..copy_len].copy_from_slice(&name_bytes[..copy_len]);
    hdr[100..108].copy_from_slice(b"0000644\0");
    hdr[108..116].copy_from_slice(b"0000000\0");
    hdr[116..124].copy_from_slice(b"0000000\0");
    let size_oct = format!("{:011o}\0", data.len());
    hdr[124..136].copy_from_slice(size_oct.as_bytes());
    hdr[136..148].copy_from_slice(b"00000000000\0");
    hdr[148..156].fill(b' ');
    hdr[156] = b'0';
    hdr[257..263].copy_from_slice(b"ustar\0");
    hdr[263..265].copy_from_slice(b"00");
    let cksum: u32 = hdr.iter().map(|&b| u32::from(b)).sum();
    let cksum_oct = format!("{cksum:06o}\0 ");
    hdr[148..156].copy_from_slice(cksum_oct.as_bytes());
    tar.write_all(&hdr)?;
    tar.write_all(data)?;
    let pad = (512 - (data.len() % 512)) % 512;
    if pad > 0 {
        tar.write_all(&vec![0u8; pad])?;
    }
    Ok(())
}

fn finish_tar<W: Write>(tar: &mut W) -> std::io::Result<()> {
    tar.write_all(&[0u8; 1024])
}

fn read_tar_entries(tar_bytes: &[u8]) -> Api<Vec<(String, Vec<u8>)>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 512 <= tar_bytes.len() {
        let hdr = &tar_bytes[pos..pos + 512];
        pos += 512;
        if hdr.iter().all(|&b| b == 0) {
            break;
        }
        if out.len() >= MAX_ARCHIVE_ENTRIES {
            return Err(Error::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "archive_too_many_entries",
                "handoff tar archive exceeds entry count limit",
            ));
        }
        let typeflag = hdr[156];
        if !matches!(typeflag, 0 | b'0') {
            return Err(Error::new(
                StatusCode::BAD_REQUEST,
                "archive_unsafe_entry",
                format!("unsupported/unsafe tar entry typeflag '{typeflag}' (symlinks and special files are rejected)"),
            ));
        }
        let name_end = hdr[..100].iter().position(|&b| b == 0).unwrap_or(100);
        let name = String::from_utf8_lossy(&hdr[..name_end]).into_owned();
        if name.starts_with('/') || name.contains("..") || name.contains('\\') || name.is_empty() {
            return Err(Error::new(
                StatusCode::BAD_REQUEST,
                "archive_unsafe_entry",
                format!("unsafe tar entry path '{name}'"),
            ));
        }
        let size_str = std::str::from_utf8(&hdr[124..136])
            .unwrap_or("0")
            .trim_matches(['\0', ' ']);
        let size = usize::from_str_radix(size_str, 8)
            .map_err(|_| Error::invalid("invalid tar entry size"))?;
        if size > tar_bytes.len().saturating_sub(pos) {
            return Err(Error::invalid("truncated handoff tar archive"));
        }
        let data = tar_bytes[pos..pos + size].to_vec();
        let blocks = size.div_ceil(512) * 512;
        pos += blocks;
        out.push((name, data));
    }
    Ok(out)
}

fn strip_secrets_recursive(val: &mut Value) {
    match val {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                let lower = k.to_ascii_lowercase();
                if matches!(
                    lower.as_str(),
                    "authorization"
                        | "cookie"
                        | "set-cookie"
                        | "password"
                        | "secret"
                        | "totp_seed"
                        | "proxy_password"
                        | "api_key"
                ) {
                    *v = json!("[stripped_on_handoff]");
                } else {
                    strip_secrets_recursive(v);
                }
            }
        }
        Value::Array(arr) => {
            if arr.len() == 2 && arr[0].as_str().is_some_and(|name| matches!(name.to_ascii_lowercase().as_str(), "authorization" | "cookie" | "set-cookie" | "x-api-key" | "x-auth-token" | "proxy-authorization")) {
                arr[1] = json!("[stripped_on_handoff]");
            } else {
                for item in arr { strip_secrets_recursive(item); }
            }
        }
        _ => {}
    }
}

pub(super) fn export_archive(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let format = body["format"].as_str().unwrap_or("tar.zst");
    let use_gzip = match format {
        "tar.zst" => false,
        "tar.gz" => true,
        other => {
            return Err(Error::invalid(format!(
                "unsupported archive format '{other}'; expected tar.zst or tar.gz"
            )));
        }
    };
    let topo_order = kahn_topological_order()?;
    let mut task_doc = task(s, id)?;
    strip_secrets_recursive(&mut task_doc);
    let mut pol = policy(s, id)?;
    strip_secrets_recursive(&mut pol);

    let mut tar_buf = Vec::new();
    let mut manifest_counts = serde_json::Map::new();

    let task_bytes = serde_json::to_vec_pretty(&task_doc).map_err(db)?;
    write_tar_entry(&mut tar_buf, "handoff/task.json", &task_bytes).map_err(db)?;
    let pol_bytes = serde_json::to_vec_pretty(&pol).map_err(db)?;
    write_tar_entry(&mut tar_buf, "handoff/policy.json", &pol_bytes).map_err(db)?;

    for kind in &topo_order {
        let rows = records(s, id, kind)?;
        manifest_counts.insert((*kind).to_owned(), json!(rows.len()));
        let mut ndjson = Vec::new();
        for mut row in rows {
            strip_secrets_recursive(&mut row);
            serde_json::to_writer(&mut ndjson, &row).map_err(db)?;
            ndjson.push(b'\n');
        }
        write_tar_entry(&mut tar_buf, &format!("handoff/records/{kind}.jsonl"), &ndjson).map_err(db)?;
    }

    let manifest = json!({
        "schema": "muteki-src-handoff.v2",
        "source_task_id": id,
        "compression": if use_gzip { "tar.gz" } else { "tar.zst" },
        "secrets_stripped": true,
        "topological_order": topo_order,
        "record_counts": Value::Object(manifest_counts),
        "exported_at": now(),
    });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(db)?;
    write_tar_entry(&mut tar_buf, "handoff/manifest.json", &manifest_bytes).map_err(db)?;
    finish_tar(&mut tar_buf).map_err(db)?;

    let out_bytes = if use_gzip {
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(&tar_buf).map_err(db)?;
        enc.finish().map_err(db)?
    } else {
        zstd::stream::encode_all(&tar_buf[..], 3).map_err(db)?
    };
    let sha256 = digest(&json!(out_bytes));
    let ext = if use_gzip { "tar.gz" } else { "tar.zst" };
    let media_type = if use_gzip {
        "application/gzip"
    } else {
        "application/zstd"
    };

    Ok(json!({
        "filename": format!("src-handoff-{id}.{ext}"),
        "format": ext,
        "media_type": media_type,
        "sha256": sha256,
        "size": out_bytes.len(),
        "manifest": manifest,
        "archive_b64": base64::engine::general_purpose::STANDARD.encode(&out_bytes),
    }))
}

fn decompress_bounded(raw: &[u8]) -> Api<(&'static str, Vec<u8>)> {
    let (fmt, mut reader): (&'static str, Box<dyn Read>) =
        if raw.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
            let dec = zstd::stream::read::Decoder::new(raw)
                .map_err(|err| Error::invalid(format!("invalid zstd handoff archive: {err}")))?;
            ("tar.zst", Box::new(dec))
        } else if raw.starts_with(&[0x1F, 0x8B]) {
            ("tar.gz", Box::new(GzDecoder::new(raw)))
        } else {
            return Err(Error::invalid("archive must be compressed as .tar.zst or .tar.gz"));
        };
    let mut tar_bytes = Vec::new();
    let read_n = reader
        .by_ref()
        .take((MAX_DECOMPRESSED_BYTES + 1) as u64)
        .read_to_end(&mut tar_bytes)
        .map_err(|err| Error::invalid(format!("failed to decompress handoff archive: {err}")))?;
    if read_n > MAX_DECOMPRESSED_BYTES {
        return Err(Error::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "archive_size_exceeded",
            "handoff archive exceeds 64 MiB uncompressed limit",
        ));
    }
    Ok((fmt, tar_bytes))
}

fn annotate_missing_artifacts(s: &AppState, id: &str, val: &mut Value) -> Api<()> {
    match val {
        Value::Object(map) => {
            if let Some(aid) = map
                .get("artifact_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
            {
                let exists = s
                    .store
                    .get("src_artifacts", &format!("{id}:{aid}"))
                    .map_err(db)?
                    .is_some();
                if !exists {
                    map.insert("original_available".to_owned(), json!(false));
                    map.insert(
                        "missing_reason".to_owned(),
                        json!("not_bundled_in_handoff_archive"),
                    );
                }
            }
            for child in map.values_mut() {
                annotate_missing_artifacts(s, id, child)?;
            }
        }
        Value::Array(arr) => {
            for item in arr {
                annotate_missing_artifacts(s, id, item)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn mark_journal_failed(
    s: &AppState,
    id: &str,
    journal_id: &str,
    compression: &str,
    archive_sha256: &str,
    topo_order: &[&str],
    err: &Error,
) {
    let rev = rec(s, id, "handoff_import_journal", journal_id)
        .ok()
        .and_then(|r| r["revision"].as_u64());
    let _ = save_rec(
        s,
        id,
        "handoff_import_journal",
        journal_id,
        json!({
            "journal_id": journal_id,
            "phase": "failed",
            "compression": compression,
            "archive_sha256": archive_sha256,
            "topological_order": topo_order,
            "error": {
                "code": err.code,
                "message": err.message,
            },
            "failed_at": now(),
        }),
        rev,
    );
}

pub(super) fn import_archive(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let _ = task(s, id)?;
    let b64 = require(body, "archive_b64")?;
    let comp_bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|_| Error::invalid("archive_b64 must be valid base64"))?;
    let archive_sha256 = digest(&json!(comp_bytes));
    let (compression, tar_bytes) = decompress_bounded(&comp_bytes)?;
    let topo_order = kahn_topological_order()?;
    let journal_id = format!("hjournal-{}", Uuid::new_v4().simple());

    save_rec(
        s,
        id,
        "handoff_import_journal",
        &journal_id,
        json!({
            "journal_id": journal_id,
            "phase": "prepared",
            "compression": compression,
            "archive_sha256": archive_sha256,
            "topological_order": topo_order,
            "started_at": now(),
        }),
        Some(0),
    )?;

    let inner = || -> Api<Value> {
        let entries = read_tar_entries(&tar_bytes)?;

        let mut seen_paths = std::collections::HashSet::new();
        for (name, _) in &entries {
            let valid_record = name
                .strip_prefix("handoff/records/")
                .and_then(|rest| rest.strip_suffix(".jsonl"))
                .is_some_and(|kind| ARCHIVE_KINDS.contains(&kind));
            if !matches!(
                name.as_str(),
                "handoff/manifest.json" | "handoff/task.json" | "handoff/policy.json"
            ) && !valid_record
            {
                return Err(Error::invalid(format!(
                    "unexpected tar entry path '{name}' in handoff archive"
                )));
            }
            if !seen_paths.insert(name.clone()) {
                return Err(Error::invalid(format!(
                    "duplicate tar entry path '{name}' in handoff archive"
                )));
            }
        }

        let manifest_bytes = entries
            .iter()
            .find(|(name, _)| name == "handoff/manifest.json")
            .ok_or_else(|| Error::invalid("handoff manifest is missing"))?;
        let manifest: Value = serde_json::from_slice(&manifest_bytes.1)
            .map_err(|error| Error::invalid(format!("invalid handoff manifest: {error}")))?;
        if manifest["schema"] != "muteki-src-handoff.v2" {
            return Err(Error::invalid("unsupported handoff schema"));
        }
        if manifest["secrets_stripped"] != true {
            return Err(Error::invalid("handoff manifest must assert secrets_stripped == true"));
        }
        if manifest["compression"].as_str() != Some(compression) {
            return Err(Error::invalid(format!(
                "handoff manifest compression {:?} does not match actual '{}'",
                manifest["compression"], compression
            )));
        }
        let manifest_order: Vec<&str> = manifest["topological_order"]
            .as_array()
            .ok_or_else(|| Error::invalid("handoff manifest topological_order must be an array"))?
            .iter()
            .map(|v| {
                v.as_str()
                    .ok_or_else(|| Error::invalid("handoff topological_order entries must be strings"))
            })
            .collect::<Api<Vec<_>>>()?;
        if manifest_order != topo_order {
            return Err(Error::invalid(
                "handoff manifest topological_order does not match Kahn order",
            ));
        }

        let task_bytes = entries
            .iter()
            .find(|(name, _)| name == "handoff/task.json")
            .ok_or_else(|| Error::invalid("handoff/task.json is missing"))?;
        let task_doc: Value = serde_json::from_slice(&task_bytes.1)
            .map_err(|err| Error::invalid(format!("invalid handoff/task.json: {err}")))?;
        if !task_doc.is_object() {
            return Err(Error::invalid("handoff/task.json must be a JSON object"));
        }

        let policy_bytes = entries
            .iter()
            .find(|(name, _)| name == "handoff/policy.json")
            .ok_or_else(|| Error::invalid("handoff/policy.json is missing"))?;
        let policy_doc: Value = serde_json::from_slice(&policy_bytes.1)
            .map_err(|err| Error::invalid(format!("invalid handoff/policy.json: {err}")))?;
        if !policy_doc.is_object() {
            return Err(Error::invalid("handoff/policy.json must be a JSON object"));
        }

        let mut staged_by_kind: BTreeMap<String, Vec<(String, Value)>> = BTreeMap::new();
        for (name, bytes) in entries {
            if !name.starts_with("handoff/records/") || !name.ends_with(".jsonl") {
                continue;
            }
            let kind = name
                .trim_start_matches("handoff/records/")
                .trim_end_matches(".jsonl");
            let text = std::str::from_utf8(&bytes)
                .map_err(|err| Error::invalid(format!("non-UTF-8 JSONL in {name}: {err}")))?;
            let vec_rows = staged_by_kind.entry(kind.to_owned()).or_default();
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                let mut row: Value = serde_json::from_str(line)
                    .map_err(|err| Error::invalid(format!("invalid jsonl row in {name}: {err}")))?;
                strip_secrets_recursive(&mut row);
                let rec_id = row["natural_key"]
                    .as_str()
                    .filter(|v| !v.is_empty())
                    .or_else(|| row["record_id"].as_str().and_then(|s| s.rsplit(':').next()))
                    .unwrap_or("")
                    .to_owned();
                if rec_id.is_empty() {
                    return Err(Error::invalid(format!(
                        "handoff jsonl row in {name} is missing record identifier"
                    )));
                }
                let mut payload = row["payload"].clone();
                if !payload.is_object() {
                    return Err(Error::invalid(format!(
                        "handoff jsonl row in {name} must have an object payload"
                    )));
                }
                annotate_missing_artifacts(s, id, &mut payload)?;
                vec_rows.push((rec_id, payload));
            }
        }

        for kind in &topo_order {
            let count = staged_by_kind.get(*kind).map_or(0, Vec::len) as u64;
            if manifest["record_counts"][*kind].as_u64() != Some(count) {
                return Err(Error::invalid(format!("handoff count mismatch for {kind}")));
            }
        }

        let planned_counts: serde_json::Map<String, Value> = staged_by_kind
            .iter()
            .map(|(k, v)| (k.clone(), json!(v.len())))
            .collect();
        let jrev0 = rec(s, id, "handoff_import_journal", &journal_id)?["revision"].as_u64();
        save_rec(
            s,
            id,
            "handoff_import_journal",
            &journal_id,
            json!({
                "journal_id": journal_id,
                "phase": "prepared",
                "compression": compression,
                "archive_sha256": archive_sha256,
                "topological_order": topo_order,
                "planned_counts": planned_counts,
                "started_at": now(),
            }),
            jrev0,
        )?;

        let (imported_counts, total_imported, total_skipped) = s
            .store
            .document_scope(|| -> Api<_> {
                let mut imported_counts = serde_json::Map::new();
                let mut total_skipped = 0usize;
                let mut total_imported = 0usize;
                for kind in &topo_order {
                    let Some(rows) = staged_by_kind.get(*kind) else {
                        continue;
                    };
                    let mut kind_count = 0usize;
                    for (rec_id, payload) in rows {
                        let key = super::super::rec_key(id, kind, rec_id);
                        if s.store.get(RECORDS, &key).map_err(db)?.is_some() {
                            total_skipped += 1;
                            continue;
                        }
                        if *kind == "frozen_report"
                            && let Some(finding_id) = payload["finding_id"].as_str().filter(|v| !v.is_empty())
                        {
                            let head_key = format!("{id}:{finding_id}");
                            let existing_head = s.store.get(REPORT_HEADS, &head_key).map_err(db)?;
                            let imported_seq = payload["report_seq"].as_u64().unwrap_or(1);
                            let imported_report_id = payload["report_id"].as_str().unwrap_or(rec_id);
                            let existing_lineage = existing_head.as_ref().and_then(|h| {
                                h["finding_id"].as_str().map(str::to_owned)
                            });
                            let existing_report_id = existing_head.as_ref().and_then(|h| {
                                h["last_report_id"].as_str().map(str::to_owned)
                            });
                            let supersedes = payload["supersedes"].as_str();
                            let lineage_conflict = existing_head.as_ref().is_some_and(|h| {
                                let head_seq = h["seq"].as_u64().unwrap_or(0);
                                if existing_lineage.as_deref().is_some_and(|f| f != finding_id) {
                                    return true;
                                }
                                if head_seq > 0
                                    && existing_report_id.as_deref().is_some_and(|rid| {
                                        rid != imported_report_id && supersedes != Some(rid)
                                    })
                                    && imported_seq <= head_seq
                                {
                                    return true;
                                }
                                let same_finding_conflict = records(s, id, "frozen_report")
                                    .unwrap_or_default()
                                    .into_iter()
                                    .any(|existing| {
                                        existing["payload"]["finding_id"] == finding_id
                                            && existing["payload"]["report_seq"] == imported_seq
                                            && existing["payload"]["report_id"] != imported_report_id
                                    });
                                same_finding_conflict
                            });
                            if lineage_conflict {
                                return Err(Error::conflict(format!(
                                    "frozen_report head lineage conflict for finding {finding_id}"
                                )));
                            }
                            let head_seq = existing_head
                                .as_ref()
                                .and_then(|h| h["seq"].as_u64())
                                .unwrap_or(0);
                            if imported_seq > head_seq {
                                let next_head = json!({
                                    "seq": imported_seq,
                                    "finding_id": finding_id,
                                    "last_report_id": imported_report_id
                                });
                                s.store
                                    .put_many_if(
                                        &[(REPORT_HEADS, &head_key, existing_head.as_ref())],
                                        &[(REPORT_HEADS, &head_key, &next_head)],
                                    )
                                    .map_err(db)?;
                            }
                        }
                        save_rec(s, id, kind, rec_id, payload.clone(), Some(0))?;
                        kind_count += 1;
                        total_imported += 1;
                    }
                    imported_counts.insert((*kind).to_owned(), json!(kind_count));
                }

                let jrev = rec(s, id, "handoff_import_journal", &journal_id)?["revision"].as_u64();
                save_rec(
                    s,
                    id,
                    "handoff_import_journal",
                    &journal_id,
                    json!({
                        "journal_id": journal_id,
                        "phase": "committed",
                        "compression": compression,
                        "archive_sha256": archive_sha256,
                        "topological_order": topo_order,
                        "imported_counts": Value::Object(imported_counts.clone()),
                        "total_imported": total_imported,
                        "total_skipped": total_skipped,
                        "completed_at": now(),
                    }),
                    jrev,
                )?;

                Ok((imported_counts, total_imported, total_skipped))
            })
            .map_err(db)??;

        let _ = super::traffic_index::backfill_missing(s, id);

        Ok(json!({
            "imported_into_task_id": id,
            "journal_id": journal_id,
            "phase": "committed",
            "compression": compression,
            "topological_order": topo_order,
            "total_imported": total_imported,
            "counts": Value::Object(imported_counts),
            "total_skipped": total_skipped,
            "imported_at": now(),
        }))
    };

    match inner() {
        Ok(res) => Ok(res),
        Err(err) => {
            mark_journal_failed(s, id, &journal_id, compression, &archive_sha256, &topo_order, &err);
            Err(err)
        }
    }
}
