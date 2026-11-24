//! Declarative HMAC/MD5/SHA256 signing and OS-sandboxed pure JavaScript request mutators.
use super::{Api, Error, credential_store, now, rec, records, require, save_rec};
use crate::AppState;
use axum::http::StatusCode;
use base64::Engine;
use ring::hmac;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

fn validate_secret_ref(reference: &str) -> Api<()> {
    if reference.is_empty() {
        return Ok(());
    }
    if !reference.starts_with("secret://src/") || reference.chars().any(char::is_whitespace) {
        return Err(Error::new(
            StatusCode::FORBIDDEN,
            "src.secret_in_config",
            "mutator secret_ref must be a managed secret://src/ reference",
        ));
    }
    Ok(())
}

/// Pure RFC 1321 MD5 digest for legacy API parameter signing recipes (`md5_kv_join`).
fn md5_hex(data: &[u8]) -> String {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22,
        5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20,
        4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23,
        6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
        0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
        0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
        0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
        0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
        0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
        0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
    ];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    let (mut a0, mut b0, mut c0, mut d0): (u32, u32, u32, u32) =
        (0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476);
    for chunk in msg.chunks_exact(64) {
        let mut m = [0u32; 16];
        for i in 0..16 {
            m[i] = u32::from_le_bytes([chunk[4 * i], chunk[4 * i + 1], chunk[4 * i + 2], chunk[4 * i + 3]]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((b & c) | ((!b) & d), i),
                16..=31 => ((d & b) | ((!d) & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | (!d)), (7 * i) % 16),
            };
            let temp = d;
            d = c;
            c = b;
            b = b.wrapping_add(
                a.wrapping_add(f).wrapping_add(K[i]).wrapping_add(m[g]).rotate_left(S[i]),
            );
            a = temp;
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    out.iter().map(|b| format!("{b:02x}")).collect()
}

pub(super) fn save(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let mutator_id = body["mutator_id"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("mut_{}", Uuid::new_v4().simple()));
    let name = require(body, "name")?;
    let kind = body["kind"].as_str().unwrap_or("declarative");
    if !matches!(kind, "declarative" | "sandbox_js") {
        return Err(Error::invalid("mutator kind must be declarative or sandbox_js"));
    }
    let algorithm = body["algorithm"].as_str().unwrap_or("hmac_sha256");
    if kind == "declarative"
        && !matches!(
            algorithm,
            "hmac_sha256" | "hmac_sha1" | "sha256_kv_sorted" | "md5_kv_join" | "timestamp_nonce" | "bearer_from_jar"
        )
    {
        return Err(Error::invalid("unsupported declarative signing algorithm"));
    }
    let secret_ref = body["secret_ref"].as_str().unwrap_or("");
    validate_secret_ref(secret_ref)?;
    if !secret_ref.is_empty() {
        credential_store::version(s, id, secret_ref)?;
    }
    let js_source = body["js_source"].as_str().unwrap_or("");
    if kind == "sandbox_js" && js_source.trim().is_empty() {
        return Err(Error::invalid("sandbox_js requires non-empty js_source"));
    }
    let prior = rec(s, id, "request_mutator", &mutator_id).ok();
    let rev = prior.as_ref().and_then(|r| r["revision"].as_u64()).unwrap_or(0);
    let payload = json!({
        "mutator_id": mutator_id,
        "task_id": id,
        "name": name,
        "kind": kind,
        "algorithm": algorithm,
        "secret_ref": secret_ref,
        "output_header": body["output_header"].as_str().unwrap_or("X-Signature"),
        "output_query": body["output_query"].as_str().unwrap_or(""),
        "timestamp_header": body["timestamp_header"].as_str().unwrap_or("X-Timestamp"),
        "nonce_header": body["nonce_header"].as_str().unwrap_or("X-Nonce"),
        "js_source": js_source,
        "config": if body["config"].is_object() { body["config"].clone() } else { json!({}) },
        "updated_at": now(),
    });
    save_rec(s, id, "request_mutator", &mutator_id, payload, Some(rev))
}

pub(super) fn list(s: &AppState, id: &str) -> Api<Value> {
    let items: Vec<Value> = records(s, id, "request_mutator")?
        .into_iter()
        .map(|row| row["payload"].clone())
        .collect();
    Ok(json!({ "items": items }))
}

fn find_node() -> Option<String> {
    if let Ok(path) = std::env::var("MUTEKI_NODE") {
        if !path.is_empty() && std::path::Path::new(&path).is_file() {
            return Some(path);
        }
    }
    for candidate in ["/opt/homebrew/bin/node", "/usr/local/bin/node", "/usr/bin/node"] {
        if std::path::Path::new(candidate).is_file() {
            return Some(candidate.to_owned());
        }
    }
    None
}

async fn run_sandbox_js(js_source: &str, input_json: &Value) -> Api<Value> {
    let node_bin = find_node().ok_or_else(|| {
        Error::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "mutator.node_unavailable",
            "sandboxed JS runtime (MUTEKI_NODE) not found",
        )
    })?;
    let vm_driver = r#"
const fs = require("fs");
const vm = require("vm");
const input = JSON.parse(fs.readFileSync(0, "utf8"));
const context = vm.createContext(Object.create(null), {codeGeneration:{strings:false,wasm:false}});
const payload = JSON.stringify({req: input.req, secrets: input.secrets});
const source = "const input = JSON.parse(" + JSON.stringify(payload) + "); const req = input.req; const secrets = input.secrets;\n" + input.js_source + "\nJSON.stringify(typeof mutate === 'function' ? mutate(req, secrets) : req)";
process.stdout.write(new vm.Script(source).runInContext(context, {timeout:1000}));
"#;
    let payload = json!({
        "js_source": js_source,
        "req": input_json["req"],
        "secrets": input_json["secrets"],
    });
    let payload_bytes = serde_json::to_vec(&payload).map_err(super::db)?;

    let mut cmd = if cfg!(target_os = "macos") {
        // Intentionally clears env for sandboxed JS execution.
        #[allow(clippy::disallowed_methods)]
        let mut c = tokio::process::Command::new("/usr/bin/sandbox-exec");
        c.arg("-p").arg("(version 1)(allow default)(deny network*)(deny file-read-data (subpath \"/Users\") (subpath \"/private\") (subpath \"/Volumes\"))(deny file-write*)(deny process-fork)").arg(&node_bin);
        c
    } else {
        let bwrap = crate::shell_env::resolve_program(std::ffi::OsStr::new("bwrap"));
        // Intentionally clears env for sandboxed JS execution.
        #[allow(clippy::disallowed_methods)]
        let mut c = tokio::process::Command::new(bwrap);
        c.args(["--unshare-all", "--die-with-parent", "--ro-bind", "/usr", "/usr", "--ro-bind", "/lib", "/lib", "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--"]).arg(&node_bin);
        c
    };
    cmd.arg("--permission")
        .arg("--allow-fs-read=/dev/fd/0")
        .kill_on_drop(true);
    cmd.arg("-e")
        .arg(vm_driver)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        Error::new(StatusCode::SERVICE_UNAVAILABLE, "mutator.spawn_failed", e.to_string())
    })?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&payload_bytes).await.map_err(super::db)?;
    }
    let output = tokio::time::timeout(Duration::from_secs(2), child.wait_with_output())
        .await
        .map_err(|_| Error::new(StatusCode::REQUEST_TIMEOUT, "mutator.timeout", "sandbox JS mutator timed out"))?
        .map_err(super::db)?;
    if !output.status.success() {
        let err_msg = String::from_utf8_lossy(&output.stderr);
        return Err(Error::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "mutator.execution_failed",
            format!("mutator JS process exited {}: {err_msg}", output.status),
        ));
    }
    if output.stdout.len() > 64 * 1024 {
        return Err(Error::invalid("mutator output exceeds 64 KiB"));
    }
    serde_json::from_slice(&output.stdout).map_err(|_| Error::invalid("mutator returned invalid JSON"))
}

pub(super) async fn apply_to_request(
    s: &AppState,
    task_id: &str,
    mutator_id: &str,
    method: &str,
    url: &mut reqwest::Url,
    headers: &mut Vec<(String, String)>,
    body: &mut Vec<u8>,
) -> Api<()> {
    if mutator_id.is_empty() {
        return Ok(());
    }
    let mutator = rec(s, task_id, "request_mutator", mutator_id)?["payload"].clone();
    let secret = match mutator["secret_ref"].as_str().filter(|v| !v.is_empty()) {
        Some(reference) => credential_store::resolve(s, task_id, reference, None)?,
        None => String::new(),
    };
    let ts = chrono::Utc::now().timestamp().to_string();
    let nonce = Uuid::new_v4().simple().to_string()[..12].to_owned();

    if mutator["kind"] == "sandbox_js" {
        let header_map: BTreeMap<String, String> = headers.iter().cloned().collect();
        let req_obj = json!({
            "method": method,
            "url": url.as_str(),
            "path": url.path(),
            "query": url.query().unwrap_or(""),
            "headers": header_map,
            "body": String::from_utf8_lossy(body),
            "timestamp": ts,
            "nonce": nonce,
        });
        let secrets_obj = json!({ "primary": secret });
        let mutated = run_sandbox_js(mutator["js_source"].as_str().unwrap_or(""), &json!({"req": req_obj, "secrets": secrets_obj})).await?;
        if let Some(new_hdrs) = mutated["headers"].as_object() {
            for (k, v) in new_hdrs {
                if let Some(val) = v.as_str() {
                    headers.retain(|(ek, _)| !ek.eq_ignore_ascii_case(k));
                    headers.push((k.clone(), val.to_owned()));
                }
            }
        }
        if let Some(new_query) = mutated["query"].as_object() {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in new_query {
                if let Some(val) = v.as_str() {
                    pairs.append_pair(k, val);
                }
            }
        }
        if let Some(new_body) = mutated["body"].as_str() {
            *body = new_body.as_bytes().to_vec();
        }
        return Ok(());
    }

    let ts_header = mutator["timestamp_header"].as_str().filter(|v| !v.is_empty()).unwrap_or("X-Timestamp");
    let nonce_header = mutator["nonce_header"].as_str().filter(|v| !v.is_empty()).unwrap_or("X-Nonce");
    let out_header = mutator["output_header"].as_str().unwrap_or("X-Signature");
    let out_query = mutator["output_query"].as_str().unwrap_or("");

    let mut pairs: BTreeMap<String, String> = url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    pairs.insert("timestamp".to_owned(), ts.clone());
    pairs.insert("nonce".to_owned(), nonce.clone());
    let sorted_kv: String = pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
    let body_text = String::from_utf8_lossy(body);
    let canonical = format!("{}\n{}\n{}\n{}", method.to_ascii_uppercase(), url.path(), sorted_kv, body_text);

    let sig = match mutator["algorithm"].as_str().unwrap_or("hmac_sha256") {
        "hmac_sha256" => {
            let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
            let tag = hmac::sign(&key, canonical.as_bytes());
            tag.as_ref().iter().map(|b| format!("{b:02x}")).collect::<String>()
        }
        "hmac_sha1" => {
            let key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, secret.as_bytes());
            let tag = hmac::sign(&key, canonical.as_bytes());
            base64::engine::general_purpose::STANDARD.encode(tag.as_ref())
        }
        "sha256_kv_sorted" => {
            let mut joined = sorted_kv.clone();
            if !secret.is_empty() {
                joined.push('&');
                joined.push_str(&secret);
            }
            format!("{:x}", Sha256::digest(joined.as_bytes()))
        }
        "md5_kv_join" => {
            let mut joined = sorted_kv.clone();
            if !secret.is_empty() {
                joined.push_str(&secret);
            }
            md5_hex(joined.as_bytes())
        }
        "timestamp_nonce" => String::new(),
        "bearer_from_jar" => format!("Bearer {secret}"),
        _ => String::new(),
    };

    if !ts_header.is_empty() {
        headers.retain(|(k, _)| !k.eq_ignore_ascii_case(ts_header));
        headers.push((ts_header.to_owned(), ts.clone()));
    }
    if !nonce_header.is_empty() {
        headers.retain(|(k, _)| !k.eq_ignore_ascii_case(nonce_header));
        headers.push((nonce_header.to_owned(), nonce.clone()));
    }
    if !sig.is_empty() {
        if !out_header.is_empty() {
            headers.retain(|(k, _)| !k.eq_ignore_ascii_case(out_header));
            headers.push((out_header.to_owned(), sig.clone()));
        }
        if !out_query.is_empty() {
            url.query_pairs_mut().append_pair(out_query, &sig);
        }
    }
    Ok(())
}

pub(super) async fn test_run(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let mutator_id = require(body, "mutator_id")?;
    let method = body["sample_method"].as_str().unwrap_or("GET");
    let raw_url = body["sample_url"].as_str().unwrap_or("https://example.invalid/api/v1/demo");
    let mut url = reqwest::Url::parse(raw_url).map_err(|_| Error::invalid("invalid sample_url"))?;
    let mut headers: Vec<(String, String)> = body["sample_headers"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    let mut sample_body = body["sample_body"].as_str().unwrap_or("").as_bytes().to_vec();
    apply_to_request(s, id, mutator_id, method, &mut url, &mut headers, &mut sample_body).await?;
    let applied_headers: Vec<Value> = headers
        .iter()
        .map(|(k, v)| {
            let masked = if k.eq_ignore_ascii_case("authorization") {
                format!("[REDACTED:sha256:{}]", &format!("{:x}", Sha256::digest(v.as_bytes()))[..12])
            } else {
                v.clone()
            };
            json!([k, masked])
        })
        .collect();
    Ok(json!({
        "mutator_id": mutator_id,
        "resulting_url": url.as_str(),
        "applied_headers": applied_headers,
        "body_sha256": format!("{:x}", Sha256::digest(&sample_body)),
    }))
}
