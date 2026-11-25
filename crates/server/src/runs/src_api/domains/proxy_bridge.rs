//! Host-side client proxy bridge (WeChat Mini-Program / Desktop WeChat / Mobile HTTP CONNECT + MITM).
//! Enforces Proxy-Authorization, out-of-scope blind passthrough vs in-scope TLS MITM + capture,
//! automatic session cookie/header harvesting into the Live Session Jar, and per-task bookmarks.
use super::{db, identity, in_policy, now, policy, rec, record_exchange, records, task, traffic_index};
use crate::AppState;
use crate::runs::src_api::{Api, Error};
use axum::http::StatusCode;
use base64::Engine;
use getrandom::getrandom;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject};
use tokio_rustls::rustls::{ClientConfig, RootCertStore, ServerConfig, crypto::ring::default_provider};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use uuid::Uuid;

struct CaMaterial {
    ca_pem: String,
    ca_key_pem: String,
    ca_sha256: String,
    server_configs: Mutex<HashMap<String, Arc<ServerConfig>>>,
}

struct BridgeRuntime {
    task_id: String,
    addr: SocketAddr,
    proxy_user: String,
    proxy_pass: String,
    capture_context_id: Mutex<String>,
    ca: Arc<CaMaterial>,
    upstream_roots: Arc<RootCertStore>,
    in_scope_count: AtomicU64,
    bypassed_out_of_scope_count: AtomicU64,
    active: AtomicBool,
    stop: Arc<Notify>,
}

static BRIDGES: OnceLock<Mutex<HashMap<String, Arc<BridgeRuntime>>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<String, Arc<BridgeRuntime>>> {
    BRIDGES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn ca_dir(s: &AppState, task_id: &str) -> PathBuf {
    crate::native_chat::state_dir(s).join("src-proxy-ca").join(task_id)
}

fn ca_params() -> CertificateParams {
    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::OrganizationName, "Muteki SRC");
    dn.push(DnType::CommonName, "Muteki SRC Client Proxy CA");
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    params
}

fn internal_err(err: impl std::fmt::Display) -> Error {
    Error::new(StatusCode::INTERNAL_SERVER_ERROR, "proxy_bridge.internal", err.to_string())
}

fn ensure_ca(s: &AppState, task_id: &str) -> Api<Arc<CaMaterial>> {
    let dir = ca_dir(s, task_id);
    std::fs::create_dir_all(&dir).map_err(db)?;
    let cert_path = dir.join("ca.pem");
    let key_path = dir.join("ca.key");
    let (ca_pem, ca_key_pem) = if cert_path.is_file() && key_path.is_file() {
        (
            std::fs::read_to_string(&cert_path).map_err(db)?,
            std::fs::read_to_string(&key_path).map_err(db)?,
        )
    } else {
        let key = KeyPair::generate().map_err(internal_err)?;
        let params = ca_params();
        let pem = params
            .self_signed(&key)
            .map_err(internal_err)?
            .pem();
        let key_pem = key.serialize_pem();
        write_private_file(&key_path, key_pem.as_bytes(), 0o600)?;
        write_private_file(&cert_path, pem.as_bytes(), 0o644)?;
        (pem, key_pem)
    };
    let der = CertificateDer::from_pem_slice(ca_pem.as_bytes()).map_err(internal_err)?;
    let ca_sha256 = format!("{:x}", Sha256::digest(der.as_ref()));
    Ok(Arc::new(CaMaterial {
        ca_pem,
        ca_key_pem,
        ca_sha256,
        server_configs: Mutex::new(HashMap::new()),
    }))
}

fn write_private_file(path: &Path, data: &[u8], mode: u32) -> Api<()> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data).map_err(db)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).map_err(db)?;
    std::fs::rename(&tmp, path).map_err(db)?;
    Ok(())
}

impl CaMaterial {
    fn leaf_server_config(&self, host: &str) -> Api<Arc<ServerConfig>> {
        let normalized = host.trim().trim_matches(['[', ']']).to_ascii_lowercase();
        {
            let guard = self.server_configs.lock().unwrap();
            if let Some(cfg) = guard.get(&normalized) {
                return Ok(cfg.clone());
            }
        }
        let ca_key = KeyPair::from_pem(&self.ca_key_pem).map_err(internal_err)?;
        let issuer = Issuer::new(ca_params(), ca_key);
        let leaf_key = KeyPair::generate().map_err(internal_err)?;
        let mut params = CertificateParams::new(vec![normalized.clone()])
            .map_err(|e| Error::invalid(format!("invalid TLS host `{normalized}`: {e}")))?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, normalized.clone());
        params.distinguished_name = dn;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        let leaf_cert = params
            .signed_by(&leaf_key, &issuer)
            .map_err(internal_err)?
            .pem();
        let leaf_key_pem = leaf_key.serialize_pem();
        let mut certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(leaf_cert.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(internal_err)?;
        let ca_der = CertificateDer::from_pem_slice(self.ca_pem.as_bytes())
            .map_err(internal_err)?;
        certs.push(ca_der);
        let key_der = PrivateKeyDer::from_pem_slice(leaf_key_pem.as_bytes())
            .map_err(internal_err)?;
        let mut config = ServerConfig::builder_with_provider(Arc::new(default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(internal_err)?
            .with_no_client_auth()
            .with_single_cert(certs, key_der)
            .map_err(internal_err)?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let arc = Arc::new(config);
        self.server_configs.lock().unwrap().insert(normalized, arc.clone());
        Ok(arc)
    }
}

fn random_token() -> String {
    let mut buf = [0u8; 18];
    if getrandom(&mut buf).is_err() {
        return Uuid::new_v4().simple().to_string();
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

fn check_proxy_auth(headers: &[(String, String)], expected_user: &str, expected_pass: &str) -> bool {
    let Some((_, val)) = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("proxy-authorization")) else {
        return false;
    };
    let Some(b64) = val.trim().strip_prefix("Basic ").or_else(|| val.trim().strip_prefix("basic ")) else {
        return false;
    };
    let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) else {
        return false;
    };
    let expected = format!("{expected_user}:{expected_pass}");
    let exp = expected.as_bytes();
    if raw.len() != exp.len() {
        return false;
    }
    raw.iter().zip(exp.iter()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

fn url_in_task_scope(s: &AppState, task_id: &str, url: &str) -> bool {
    let Ok(p) = policy(s, task_id) else {
        return false;
    };
    let Ok(t) = task(s, task_id) else {
        return false;
    };
    let target_ok = t["input"]["authorization"]["targets"]
        .as_array()
        .is_some_and(|rows| {
            rows.iter()
                .any(|v| v.as_str().is_some_and(|allowed| crate::runs::src_api::target_matches(url, allowed)))
        });
    target_ok && in_policy(&p, url)
}

fn request_policy_allowed(s: &AppState, task_id: &str, url: &str, method: &str, headers: &[(String,String)]) -> bool {
    let Ok(p) = policy(s, task_id) else { return false; };
    let request = json!({"resource_kind":"http", "evaluation_point":"request", "tool_name":"src.proxy_bridge", "action":"request", "arguments":{"url":url,"method":method,"headers":headers}, "resource_facts":{"observed_url":{"value":url,"source":"proxy"},"method":{"value":method,"source":"proxy"},"headers":{"value":headers,"source":"proxy"}}});
    super::super::execution_policy::evaluate(&p, &request).is_ok_and(|v| v["effect"] == "allow")
}

async fn read_http_head<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> std::io::Result<(String, Vec<(String, String)>, Vec<u8>)> {
    let mut raw = Vec::new();
    let mut headers = Vec::new();
    let mut start = String::new();
    loop {
        let mut line = Vec::new();
        let n = reader.read_until(b'\n', &mut line).await?;
        if n == 0 {
            if raw.is_empty() {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof"));
            }
            break;
        }
        raw.extend_from_slice(&line);
        if raw.len() > 128 * 1024 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "header too large"));
        }
        let s = String::from_utf8_lossy(&line);
        let trimmed = s.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if start.is_empty() {
            start = trimmed.to_owned();
        } else if let Some((k, v)) = trimmed.split_once(':') {
            headers.push((k.trim().to_owned(), v.trim().to_owned()));
        }
    }
    Ok((start, headers, raw))
}

async fn read_http_body<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    headers: &[(String, String)],
    status_or_method_no_body: bool,
    close_delimited: bool,
) -> std::io::Result<Vec<u8>> {
    if status_or_method_no_body {
        return Ok(Vec::new());
    }
    let chunked = headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked")
    });
    if chunked {
        let mut out = Vec::new();
        loop {
            let mut line = Vec::new();
            let n = reader.read_until(b'\n', &mut line).await?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&line);
            let hex = String::from_utf8_lossy(&line)
                .trim()
                .split(';')
                .next()
                .unwrap_or("")
                .to_owned();
            let size = usize::from_str_radix(&hex, 16)
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad chunk"))?;
            if size == 0 {
                loop {
                    let mut trailer = Vec::new();
                    let tn = reader.read_until(b'\n', &mut trailer).await?;
                    if tn == 0 {
                        break;
                    }
                    out.extend_from_slice(&trailer);
                    if trailer == b"\r\n" || trailer == b"\n" {
                        break;
                    }
                }
                break;
            }
            if out.len() + size > 32 * 1024 * 1024 {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "body too large"));
            }
            let mut chunk = vec![0u8; size + 2];
            reader.read_exact(&mut chunk).await?;
            out.extend_from_slice(&chunk);
        }
        return Ok(out);
    }
    if let Some((_, len_str)) = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")) {
        let len: usize = len_str
            .trim()
            .parse()
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad content-length"))?;
        if len > 32 * 1024 * 1024 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "body too large"));
        }
        let mut body = vec![0u8; len];
        reader.read_exact(&mut body).await?;
        return Ok(body);
    }
    if close_delimited {
        let mut body = Vec::new();
        reader.take(32 * 1024 * 1024 + 1).read_to_end(&mut body).await?;
        if body.len() > 32 * 1024 * 1024 { return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "body too large")); }
        return Ok(body);
    }
    Ok(Vec::new())
}

fn build_upstream_request(start: &str, headers: &[(String, String)], body: &[u8], strip_absolute: bool) -> Vec<u8> {
    let mut parts = start.splitn(3, ' ');
    let method = parts.next().unwrap_or("GET");
    let target = parts.next().unwrap_or("/");
    let version = parts.next().unwrap_or("HTTP/1.1");
    let req_target = if strip_absolute {
        reqwest::Url::parse(target)
            .ok()
            .map(|u| {
                if let Some(q) = u.query() {
                    format!("{}?{q}", u.path())
                } else {
                    u.path().to_owned()
                }
            })
            .unwrap_or_else(|| target.to_owned())
    } else {
        target.to_owned()
    };
    let mut out = format!("{method} {req_target} {version}\r\n").into_bytes();
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("proxy-authorization") || k.eq_ignore_ascii_case("proxy-connection") {
            continue;
        }
        out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out.extend_from_slice(body);
    out
}

async fn handle_client(s: Arc<AppState>, rt: Arc<BridgeRuntime>, mut tcp: TcpStream) {
    let mut reader = BufReader::new(&mut tcp);
    let Ok((start, headers, raw_head)) = read_http_head(&mut reader).await else {
        return;
    };
    if !check_proxy_auth(&headers, &rt.proxy_user, &rt.proxy_pass) {
        let _ = reader
            .get_mut()
            .write_all(
                b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"Muteki SRC Bridge\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await;
        return;
    }
    let parts: Vec<&str> = start.split_whitespace().collect();
    if parts.len() < 2 {
        return;
    }
    let method = parts[0];
    let target = parts[1];

    if method.eq_ignore_ascii_case("CONNECT") {
        let authority = target.trim();
        let Ok(parsed_auth) = reqwest::Url::parse(&format!("https://{authority}")) else {
            return;
        };
        let Some(host) = parsed_auth.host_str().map(str::to_owned) else {
            return;
        };
        let port = parsed_auth.port().unwrap_or(443);
        let in_scope = policy(&s,&rt.task_id).ok().is_some_and(|p| {
            p["authorized_targets"].as_array().into_iter().flatten().filter_map(Value::as_str).any(|target| {
                reqwest::Url::parse(target).ok().is_some_and(|url| url.scheme() == "https" && url.host_str().is_some_and(|h| h.eq_ignore_ascii_case(&host)) && url.port_or_known_default() == Some(port))
            })
        });

        if !in_scope {
            // Blind passthrough tunnel — zero MITM, zero disk capture.
            rt.bypassed_out_of_scope_count.fetch_add(1, Ordering::Relaxed);
            let Ok(mut upstream) = TcpStream::connect((host.as_str(), port)).await else {
                let _ = tcp.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                return;
            };
            if tcp
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .is_err()
            {
                return;
            }
            let _ = tokio::io::copy_bidirectional(&mut tcp, &mut upstream).await;
            return;
        }

        let Ok(cfg) = rt.ca.leaf_server_config(&host) else {
            return;
        };
        if tcp
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .is_err()
        {
            return;
        }
        let Ok(tls_client) = TlsAcceptor::from(cfg).accept(tcp).await else {
            return;
        };
        let mut client_reader = BufReader::new(tls_client);
        let Ok((inner_start, inner_hdrs, _)) = read_http_head(&mut client_reader).await else {
            return;
        };
        let inner_body = match read_http_body(&mut client_reader, &inner_hdrs, false, false).await {
            Ok(b) => b,
            Err(_) => return,
        };
        let mut tls_client = client_reader.into_inner();
        let inner_parts: Vec<&str> = inner_start.split_whitespace().collect();
        let req_method = inner_parts.first().copied().unwrap_or("GET").to_owned();
        let req_path = inner_parts.get(1).copied().unwrap_or("/").to_owned();
        let port_suffix = if port == 443 { String::new() } else { format!(":{port}") };
        let observed_url = format!("https://{host}{port_suffix}{req_path}");
        if !url_in_task_scope(&s, &rt.task_id, &observed_url) || !request_policy_allowed(&s, &rt.task_id, &observed_url, &req_method, &inner_hdrs) {
            rt.bypassed_out_of_scope_count.fetch_add(1, Ordering::Relaxed);
            return;
        }

        let upstream_wire = build_upstream_request(&inner_start, &inner_hdrs, &inner_body, false);
        let mut client_cfg = ClientConfig::builder_with_provider(Arc::new(default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates((*rt.upstream_roots).clone())
            .with_no_client_auth();
        client_cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
        let Ok(up_tcp) = TcpStream::connect((host.as_str(), port)).await else {
            let _ = tls_client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
            return;
        };
        let Ok(sni) = ServerName::try_from(host.clone()) else {
            return;
        };
        let Ok(mut up_tls) = TlsConnector::from(Arc::new(client_cfg)).connect(sni, up_tcp).await else {
            let _ = tls_client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
            return;
        };
        if up_tls.write_all(&upstream_wire).await.is_err() || up_tls.flush().await.is_err() {
            return;
        }
        let mut up_reader = BufReader::new(up_tls);
        let Ok((resp_start, resp_hdrs, resp_raw_head)) = read_http_head(&mut up_reader).await else {
            return;
        };
        let resp_status: u16 = resp_start
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let no_body = req_method.eq_ignore_ascii_case("HEAD") || matches!(resp_status, 204 | 304) || (100..200).contains(&resp_status);
        let resp_body = match read_http_body(&mut up_reader, &resp_hdrs, no_body, true).await {
            Ok(b) => b,
            Err(_) => return,
        };
        let mut resp_wire = resp_raw_head;
        resp_wire.extend_from_slice(&resp_body);
        let _ = tls_client.write_all(&resp_wire).await;
        let _ = tls_client.flush().await;

        rt.in_scope_count.fetch_add(1, Ordering::Relaxed);
        record_and_harvest(&s, &rt, &req_method, &observed_url, &upstream_wire, &inner_hdrs, &resp_wire, &resp_hdrs, resp_status);
        return;
    }

    let Ok(req_body) = read_http_body(&mut reader, &headers, false, false).await else {
        return;
    };
    let left_over_stream = tcp;
    let Ok(parsed_url) = reqwest::Url::parse(target).or_else(|_| {
        let host = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("host"))
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        reqwest::Url::parse(&format!("http://{host}{target}"))
    }) else {
        return;
    };
    let Some(host) = parsed_url.host_str().map(str::to_owned) else {
        return;
    };
    let port = parsed_url.port_or_known_default().unwrap_or(80);
    let observed_url = parsed_url.as_str().to_owned();
    let in_scope = url_in_task_scope(&s, &rt.task_id, &observed_url);
    if in_scope && !request_policy_allowed(&s, &rt.task_id, &observed_url, method, &headers) { return; }

    let Ok(mut upstream) = TcpStream::connect((host.as_str(), port)).await else {
        let mut c = left_over_stream;
        let _ = c.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
        return;
    };
    let upstream_wire = build_upstream_request(&start, &headers, &req_body, true);
    if upstream.write_all(&upstream_wire).await.is_err() || upstream.flush().await.is_err() {
        return;
    }
    let mut up_reader = BufReader::new(upstream);
    let Ok((resp_start, resp_hdrs, resp_raw_head)) = read_http_head(&mut up_reader).await else {
        return;
    };
    let resp_status: u16 = resp_start
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let no_body = method.eq_ignore_ascii_case("HEAD") || matches!(resp_status, 204 | 304) || (100..200).contains(&resp_status);
    let Ok(resp_body) = read_http_body(&mut up_reader, &resp_hdrs, no_body, true).await else { return; };
    let mut resp_wire = resp_raw_head;
    resp_wire.extend_from_slice(&resp_body);
    let mut client = left_over_stream;
    let _ = client.write_all(&resp_wire).await;
    let _ = client.flush().await;

    if !in_scope {
        rt.bypassed_out_of_scope_count.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let _ = raw_head;
    rt.in_scope_count.fetch_add(1, Ordering::Relaxed);
    record_and_harvest(&s, &rt, method, &observed_url, &upstream_wire, &headers, &resp_wire, &resp_hdrs, resp_status);
}

fn record_and_harvest(
    s: &AppState,
    rt: &BridgeRuntime,
    method: &str,
    observed_url: &str,
    req_wire: &[u8],
    req_headers: &[(String, String)],
    resp_wire: &[u8],
    resp_headers: &[(String, String)],
    status_code: u16,
) {
    let ctx_id = rt.capture_context_id.lock().unwrap().clone();
    let effective_ctx = if ctx_id.is_empty() { "proxy_bridge".to_owned() } else { ctx_id };
    let body = json!({
        "source_kind": "proxy",
        "actor_ref": "proxy_bridge",
        "context_id": effective_ctx,
        "observed_url": observed_url,
        "method": method.to_ascii_uppercase(),
        "status_code": status_code,
        "protocol": "http/1.1",
        "execution_state": "completed",
        "capture_state": "complete",
        "request_raw_b64": base64::engine::general_purpose::STANDARD.encode(req_wire),
        "response_raw_b64": base64::engine::general_purpose::STANDARD.encode(resp_wire),
        "metadata": {
            "gateway": "src-client-proxy-bridge",
            "captured_via": "wechat_client_proxy_bridge",
        }
    });
    if let Err(err) = record_exchange(s, &rt.task_id, &body) {
        tracing::warn!(task_id = %rt.task_id, code = err.code, "proxy_bridge failed to record exchange: {}", err.message);
        return;
    }
    if effective_ctx != "proxy_bridge" {
        if let Err(err) = identity::harvest_proxy_credentials(s, &rt.task_id, &effective_ctx, observed_url, req_headers, resp_headers) {
            tracing::warn!(task_id = %rt.task_id, code = err.code, "proxy_bridge session harvest failed: {}", err.message);
        }
    }
}

pub(super) async fn start(s: Arc<AppState>, id: &str, body: &Value) -> Api<Value> {
    task(s.as_ref(), id)?;
    let existing = { registry().lock().unwrap().get(id).cloned() };
    if let Some(existing) = existing {
        if existing.active.load(Ordering::Relaxed) {
            if let Some(ctx) = body["capture_context_id"].as_str() {
                *existing.capture_context_id.lock().unwrap() = ctx.trim().to_owned();
            }
            return status(s.as_ref(), id);
        }
    }
    let ca = ensure_ca(s.as_ref(), id)?;
    let mut roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(extra_pem) = body["upstream_ca_pem"].as_str().filter(|v| !v.trim().is_empty()) {
        for cert in CertificateDer::pem_slice_iter(extra_pem.as_bytes()) {
            let der = cert.map_err(|e| Error::invalid(format!("invalid upstream_ca_pem: {e}")))?;
            roots.add(der).map_err(|e| Error::invalid(format!("invalid upstream CA: {e}")))?;
        }
    }
    let bind_ip = if body["allow_lan"] == true { "0.0.0.0" } else { "127.0.0.1" };
    let port = body["port"].as_u64().unwrap_or(0) as u16;
    let listener = TcpListener::bind(format!("{bind_ip}:{port}"))
        .await
        .map_err(|e| Error::new(StatusCode::CONFLICT, "proxy_bridge.bind_failed", e.to_string()))?;
    let addr = listener.local_addr().map_err(db)?;
    let rt = Arc::new(BridgeRuntime {
        task_id: id.to_owned(),
        addr,
        proxy_user: "muteki".to_owned(),
        proxy_pass: random_token(),
        capture_context_id: Mutex::new(
            body["capture_context_id"]
                .as_str()
                .map(str::trim)
                .unwrap_or("")
                .to_owned(),
        ),
        ca,
        upstream_roots: Arc::new(roots),
        in_scope_count: AtomicU64::new(0),
        bypassed_out_of_scope_count: AtomicU64::new(0),
        active: AtomicBool::new(true),
        stop: Arc::new(Notify::new()),
    });
    registry().lock().unwrap().insert(id.to_owned(), rt.clone());

    let s_bg = s.clone();
    let rt_bg = rt.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = rt_bg.stop.notified() => break,
                accepted = listener.accept() => {
                    let Ok((stream, _peer)) = accepted else { break; };
                    let s_conn = s_bg.clone();
                    let rt_conn = rt_bg.clone();
                    tokio::spawn(async move {
                        let _ = tokio::time::timeout(Duration::from_secs(60), handle_client(s_conn, rt_conn, stream)).await;
                    });
                }
            }
        }
        rt_bg.active.store(false, Ordering::Relaxed);
    });

    status(s.as_ref(), id)
}

pub(super) fn stop(s: &AppState, id: &str) -> Api<Value> {
    if let Some(rt) = registry().lock().unwrap().remove(id) {
        rt.active.store(false, Ordering::Relaxed);
        rt.stop.notify_waiters();
    }
    status(s, id)
}

pub(super) fn status(s: &AppState, id: &str) -> Api<Value> {
    let ca_pem_path = ca_dir(s, id).join("ca.pem");
    let ca_sha256 = if ca_pem_path.is_file() {
        std::fs::read(&ca_pem_path)
            .ok()
            .and_then(|b| CertificateDer::from_pem_slice(&b).ok())
            .map(|der| format!("{:x}", Sha256::digest(der.as_ref())))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let bookmarks: Vec<Value> = records(s, id, "proxy_bookmark")
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r["payload"]["bookmarked"] == true)
        .map(|r| r["payload"].clone())
        .collect();
    if let Some(rt) = registry().lock().unwrap().get(id).cloned() {
        let listen_host = if rt.addr.ip().is_unspecified() {
            "127.0.0.1".to_owned()
        } else {
            rt.addr.ip().to_string()
        };
        let port = rt.addr.port();
        return Ok(json!({
            "listening": rt.active.load(Ordering::Relaxed),
            "host": listen_host,
            "port": port,
            "proxy_username": rt.proxy_user,
            "proxy_password": rt.proxy_pass,
            "proxy_url": format!("http://{}:{}@{listen_host}:{port}", rt.proxy_user, rt.proxy_pass),
            "ca_fingerprint_sha256": rt.ca.ca_sha256,
            "capture_context_id": rt.capture_context_id.lock().unwrap().clone(),
            "in_scope_count": rt.in_scope_count.load(Ordering::Relaxed),
            "bypassed_out_of_scope_count": rt.bypassed_out_of_scope_count.load(Ordering::Relaxed),
            "bookmarks": bookmarks,
        }));
    }
    Ok(json!({
        "listening": false,
        "host": "127.0.0.1",
        "port": 0,
        "proxy_username": "",
        "proxy_password": "",
        "proxy_url": "",
        "ca_fingerprint_sha256": ca_sha256,
        "capture_context_id": "",
        "in_scope_count": 0,
        "bypassed_out_of_scope_count": 0,
        "bookmarks": bookmarks,
    }))
}

pub(super) fn ca_cert(s: &AppState, id: &str) -> Api<Value> {
    let ca = ensure_ca(s, id)?;
    Ok(json!({
        "pem": ca.ca_pem,
        "filename": format!("muteki-src-{id}-ca.pem"),
        "sha256": ca.ca_sha256,
        "ca_path": ca_dir(s, id).join("ca.pem").to_string_lossy(),
        "wechat_profile": "macos_desktop_and_miniprogram",
    }))
}

pub(super) fn bookmark(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let exchange_id = super::require(body, "exchange_id")?;
    rec(s, id, "http_exchange", exchange_id)?;
    let bookmarked = body["bookmarked"].as_bool().unwrap_or(true);
    let label = body["label"].as_str().unwrap_or("").trim();
    traffic_index::set_bookmark(s, id, exchange_id, bookmarked, label)?;
    let payload = json!({
        "exchange_id": exchange_id,
        "bookmarked": bookmarked,
        "label": label,
        "updated_at": now(),
    });
    let prior = rec(s, id, "proxy_bookmark", exchange_id).ok();
    let rev = prior.as_ref().and_then(|r| r["revision"].as_u64()).unwrap_or(0);
    super::save_rec(s, id, "proxy_bookmark", exchange_id, payload, Some(rev))
}
