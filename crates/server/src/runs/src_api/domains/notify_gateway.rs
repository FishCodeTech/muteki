//! 8-Channel notification gateway (DingTalk, Feishu, WeCom, Telegram, SMTP, Slack, Discord, Webhook)
//! guarded by dial-time SSRF protection, HMAC signing, secret anti-masking, chunk carryover,
//! URL/token error redaction, and SQLite SAVEPOINT isolation.

use super::{credential_store, net_guard};
use crate::AppState;
use crate::runs::src_api::{Api, Error, db, now, rec, records, require, save_rec, task};
use axum::http::StatusCode;
use base64::Engine;
use ring::hmac;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const MASK_SENTINEL: &str = "••••••••";

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

fn validate_kind(raw: &str) -> Api<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "dingtalk" => Ok("dingtalk"),
        "feishu" => Ok("feishu"),
        "wecom" => Ok("wecom"),
        "telegram" => Ok("telegram"),
        "smtp" => Ok("smtp"),
        "slack" => Ok("slack"),
        "discord" => Ok("discord"),
        "webhook" => Ok("webhook"),
        other => Err(Error::invalid(format!(
            "unsupported notify channel kind '{other}'; expected dingtalk|feishu|wecom|telegram|smtp|slack|discord|webhook"
        ))),
    }
}

fn redact_endpoint_for_error(raw_url: &str, secret: &str, detail: &str) -> String {
    let base = reqwest::Url::parse(raw_url).map_or_else(
        |_| "<endpoint>".to_owned(),
        |u| {
            let host = u.host_str().unwrap_or("unknown");
            match u.port() {
                Some(p) => format!("{}://{host}:{p}", u.scheme()),
                None => format!("{}://{host}", u.scheme()),
            }
        },
    );
    let mut cleaned = detail.to_owned();
    if !raw_url.is_empty() {
        cleaned = cleaned.replace(raw_url, &base);
    }
    if !secret.is_empty() {
        cleaned = cleaned.replace(secret, "[REDACTED]");
    }
    cleaned
}

fn opt_trimmed(v: &Value) -> String {
    v.as_str().map(str::trim).unwrap_or("").to_owned()
}

pub(super) fn save_channel(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let _ = task(s, id)?;
    let kind = validate_kind(require(body, "kind")?)?;
    let url = require(body, "url")?.trim();
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| Error::invalid("notification channel url must be a valid URL"))?;
    if kind == "smtp" && parsed.scheme() == "smtp" && body["smtp_starttls"] != true {
        return Err(Error::invalid("SMTP requires smtp_starttls=true or smtps://"));
    }
    if kind == "smtp" {
        if !matches!(parsed.scheme(), "smtp" | "smtps") || parsed.host_str().is_none() {
            return Err(Error::invalid("SMTP channel url must use smtp:// or smtps://"));
        }
    } else if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(Error::invalid("notification channel url must use http or https"));
    }

    let chat_id = opt_trimmed(&body["chat_id"]);
    if kind == "telegram" && chat_id.is_empty() {
        return Err(Error::invalid("Telegram notification channel requires chat_id"));
    }
    let smtp_from = opt_trimmed(&body["smtp_from"]);
    let smtp_to = opt_trimmed(&body["smtp_to"]);
    let smtp_username = opt_trimmed(&body["smtp_username"]);
    if kind == "smtp" && smtp_to.is_empty() {
        return Err(Error::invalid("SMTP notification channel requires smtp_to"));
    }

    let channel_id = body["channel_id"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            format!(
                "notify_{}",
                &crate::runs::src_api::digest(&json!([kind, url, chat_id, smtp_to]))[..16]
            )
        });
    let existing = rec(s, id, "notify_channel", &channel_id).ok();
    let rev = existing.as_ref().and_then(|r| r["revision"].as_u64());

    let mut credential_ref = existing
        .as_ref()
        .and_then(|r| r["payload"]["credential_ref"].as_str())
        .unwrap_or("")
        .to_owned();

    let raw_secret_opt = body["secret"].as_str().map(str::trim);
    let has_new_secret = raw_secret_opt
        .is_some_and(|v| !v.is_empty() && v != MASK_SENTINEL && v != "__UNCHANGED__");

    if let Some(prev) = existing.as_ref().map(|r| &r["payload"]) {
        if !credential_ref.is_empty() {
            let dest_changed = prev["url"].as_str().unwrap_or("") != url
                || prev["chat_id"].as_str().unwrap_or("") != chat_id
                || prev["smtp_to"].as_str().unwrap_or("") != smtp_to
                || prev["smtp_username"].as_str().unwrap_or("") != smtp_username
                || prev["smtp_from"].as_str().unwrap_or("") != smtp_from;
            if dest_changed && !has_new_secret {
                return Err(Error::new(
                    StatusCode::BAD_REQUEST,
                    "notify_secret_reentry_required",
                    "Destination changed on secret-backed notification channel; full secret/credential must be re-entered",
                ));
            }
        }
    }

    if has_new_secret {
        let raw_secret = raw_secret_opt.unwrap_or("");
        let alias = format!("notify_sec_{channel_id}");
        let saved = credential_store::put(
            s,
            id,
            &json!({
                "alias": alias,
                "value": raw_secret,
                "kind": "notify_hmac_secret",
                "label": format!("Notify secret ({kind})"),
            }),
        )?;
        credential_ref = saved["reference"].as_str().unwrap_or("").to_owned();
    }

    let doc = json!({
        "channel_id": channel_id,
        "task_id": id,
        "kind": kind,
        "label": body["label"].as_str().unwrap_or(kind),
        "url": url,
        "chat_id": if chat_id.is_empty() { Value::Null } else { json!(chat_id) },
        "smtp_from": if smtp_from.is_empty() { Value::Null } else { json!(smtp_from) },
        "smtp_to": if smtp_to.is_empty() { Value::Null } else { json!(smtp_to) },
        "smtp_username": if smtp_username.is_empty() { Value::Null } else { json!(smtp_username) },
        "smtp_starttls": body["smtp_starttls"].as_bool().unwrap_or(false),
        "enabled": body["enabled"].as_bool().unwrap_or(true),
        "allow_private_test": body["allow_private_test"].as_bool().unwrap_or(false),
        "events": body["events"].as_array().cloned().unwrap_or_else(|| vec![json!("finding_review"), json!("retest_complete")]),
        "credential_ref": if credential_ref.is_empty() { Value::Null } else { json!(credential_ref) },
        "updated_at": now(),
    });
    save_rec(s, id, "notify_channel", &channel_id, doc.clone(), rev)?;
    Ok(mask_channel(&doc))
}

fn mask_channel(ch: &Value) -> Value {
    let mut masked = ch.clone();
    let has_secret = ch["credential_ref"].as_str().is_some_and(|v| !v.is_empty());
    masked["has_secret"] = json!(has_secret);
    if has_secret {
        masked["secret_preview"] = json!(MASK_SENTINEL);
    }
    masked
}

pub(super) fn list_channels(s: &AppState, id: &str) -> Api<Value> {
    let _ = task(s, id)?;
    let carryover = records(s, id, "notify_carryover")?;
    let channels = records(s, id, "notify_channel")?
        .into_iter()
        .map(|row| {
            let mut m = mask_channel(&row["payload"]);
            let cid = m["channel_id"].as_str().unwrap_or("");
            let deferred = carryover
                .iter()
                .find(|c| c["payload"]["channel_id"].as_str() == Some(cid))
                .and_then(|c| c["payload"]["pending_chunks"].as_array())
                .map_or(0usize, Vec::len);
            m["deferred_count"] = json!(deferred);
            m
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "channel_count": channels.len(),
        "channels": channels,
    }))
}

fn channel_limit(kind: &str) -> (bool, usize) {
    match kind {
        "telegram" => (false, 4096),
        "discord" => (false, 2000),
        "dingtalk" => (true, 4000),
        "wecom" => (true, 4000),
        _ => (true, 12000),
    }
}

fn slice_by_budget(s: &str, by_bytes: bool, max_units: usize) -> &str {
    if by_bytes {
        if s.len() <= max_units {
            return s;
        }
        let mut end = 0;
        for (idx, ch) in s.char_indices() {
            if idx + ch.len_utf8() > max_units {
                break;
            }
            end = idx + ch.len_utf8();
        }
        if end == 0 {
            if let Some((_, ch)) = s.char_indices().next() {
                end = ch.len_utf8().min(s.len());
            }
        }
        if let Some(nl) = s[..end].rfind('\n') {
            if nl > max_units / 3 {
                return &s[..nl + 1];
            }
        }
        &s[..end]
    } else {
        let mut count = 0;
        let mut end = s.len();
        for (idx, _) in s.char_indices() {
            if count == max_units {
                end = idx;
                break;
            }
            count += 1;
        }
        if end < s.len() {
            if let Some(nl) = s[..end].rfind('\n') {
                if s[..nl].chars().count() > max_units / 3 {
                    return &s[..nl + 1];
                }
            }
        }
        &s[..end]
    }
}

fn split_message_chunks(kind: &str, title: &str, text: &str) -> Vec<(String, String)> {
    let (by_bytes, limit) = channel_limit(kind);
    let overhead = title.len().max(title.chars().count()) + 48;
    let body_budget = limit.saturating_sub(overhead).max(128);
    let mut chunks = Vec::new();
    let mut rest = text;
    if rest.is_empty() {
        return vec![(title.to_owned(), String::new())];
    }
    while !rest.is_empty() {
        let part = slice_by_budget(rest, by_bytes, body_budget);
        chunks.push(part.to_owned());
        rest = &rest[part.len()..];
    }
    let total = chunks.len();
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            let chunk_title = if total > 1 && i > 0 {
                format!("{title} ({}/{total})", i + 1)
            } else {
                title.to_owned()
            };
            (chunk_title, chunk)
        })
        .collect()
}

fn build_channel_request(
    ch: &Value,
    kind: &str,
    base_url: &str,
    secret: &str,
    title: &str,
    text: &str,
) -> (String, Vec<(String, String)>, Value) {
    let ts_ms = chrono::Utc::now().timestamp_millis();
    let ts_sec = chrono::Utc::now().timestamp();
    let mut target_url = base_url.to_owned();
    let mut headers = Vec::new();

    match kind {
        "dingtalk" => {
            if !secret.is_empty() {
                let sign_str = format!("{ts_ms}\n{secret}");
                let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
                let sig = base64::engine::general_purpose::STANDARD
                    .encode(hmac::sign(&key, sign_str.as_bytes()).as_ref());
                let enc_sig: String = url::form_urlencoded::byte_serialize(sig.as_bytes()).collect();
                let sep = if target_url.contains('?') { '&' } else { '?' };
                target_url = format!("{target_url}{sep}timestamp={ts_ms}&sign={enc_sig}");
            }
            (
                target_url,
                headers,
                json!({
                    "msgtype": "markdown",
                    "markdown": { "title": title, "text": format!("### {title}\n\n{text}") }
                }),
            )
        }
        "feishu" => {
            let mut body = json!({
                "msg_type": "text",
                "content": { "text": format!("{title}\n{text}") }
            });
            if !secret.is_empty() {
                let key_str = format!("{ts_sec}\n{secret}");
                let key = hmac::Key::new(hmac::HMAC_SHA256, key_str.as_bytes());
                let sig = base64::engine::general_purpose::STANDARD
                    .encode(hmac::sign(&key, b"").as_ref());
                body["timestamp"] = json!(ts_sec.to_string());
                body["sign"] = json!(sig);
            }
            (target_url, headers, body)
        }
        "wecom" => (
            target_url,
            headers,
            json!({
                "msgtype": "markdown",
                "markdown": { "content": format!("**{title}**\n{text}") }
            }),
        ),
        "telegram" => {
            let clean = target_url.trim_end_matches('/');
            let endpoint = format!("{clean}/bot{secret}/sendMessage");
            let chat_id = ch["chat_id"].as_str().unwrap_or("");
            (
                endpoint,
                headers,
                json!({
                    "chat_id": chat_id,
                    "text": format!("{title}\n{text}"),
                    "disable_web_page_preview": true
                }),
            )
        }
        "slack" => (
            target_url,
            headers,
            json!({ "text": format!("*{title}*\n{text}") }),
        ),
        "discord" => (
            target_url,
            headers,
            json!({ "content": format!("**{title}**\n{text}") }),
        ),
        _ => {
            let body = json!({
                "event": title,
                "message": text,
                "timestamp": ts_sec,
            });
            if !secret.is_empty() {
                let serialized = body.to_string();
                let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
                let sig = hex_lower(hmac::sign(&key, serialized.as_bytes()).as_ref());
                headers.push(("X-Muteki-Signature-256".to_owned(), format!("sha256={sig}")));
            }
            (target_url, headers, body)
        }
    }
}

async fn read_smtp_reply<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> Api<(u16, String)> {
    let mut full = String::new();
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|err| Error::new(StatusCode::BAD_GATEWAY, "smtp_io_error", err.to_string()))?;
        if n == 0 {
            return Err(Error::new(StatusCode::BAD_GATEWAY, "smtp_eof", "SMTP server closed connection unexpectedly"));
        }
        full.push_str(&line);
        if line.len() >= 4 && line.as_bytes()[3] == b' ' {
            let code = line[..3].parse::<u16>().unwrap_or(0);
            return Ok((code, full));
        }
        if line.len() < 4 {
            let code = line.get(..3).and_then(|s| s.parse::<u16>().ok()).unwrap_or(0);
            return Ok((code, full));
        }
    }
}

async fn send_smtp_chunk(
    ch: &Value,
    raw_url: &str,
    secret: &str,
    title: &str,
    text: &str,
    allow_private: bool,
) -> Api<u16> {
    let parsed = reqwest::Url::parse(raw_url)
        .map_err(|_| Error::invalid("SMTP URL is invalid"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| Error::invalid("SMTP host is missing"))?
        .to_owned();
    let default_port = if parsed.scheme() == "smtps" { 465 } else { 25 };
    let port = parsed.port().unwrap_or(default_port);

    let starttls = parsed.scheme() == "smtp" && ch["smtp_starttls"] == true;
    if parsed.scheme() == "smtp" && !starttls {
        return Err(Error::invalid("SMTP credentials require STARTTLS or smtps://"));
    }
    let (_addr, mut tcp) = net_guard::pinned_public_tcp(&host, port, allow_private).await?;
    if starttls {
        let mut reader = BufReader::new(&mut tcp);
        let (code, _) = read_smtp_reply(&mut reader).await?;
        if code != 220 { return Err(Error::invalid("SMTP greeting rejected")); }
        reader.get_mut().write_all(b"EHLO muteki.local\r\n").await.map_err(db)?;
        let (code, reply) = read_smtp_reply(&mut reader).await?;
        if code != 250 || !reply.to_ascii_uppercase().contains("STARTTLS") { return Err(Error::invalid("SMTP server does not offer STARTTLS")); }
        reader.get_mut().write_all(b"STARTTLS\r\n").await.map_err(db)?;
        let (code, _) = read_smtp_reply(&mut reader).await?;
        if code != 220 { return Err(Error::invalid("SMTP STARTTLS rejected")); }
    }
    if parsed.scheme() == "smtps" || starttls {
        use tokio_rustls::rustls::{ClientConfig, RootCertStore, crypto::ring::default_provider, pki_types::ServerName};
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let provider = Arc::new(default_provider());
        let cfg = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(db)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let sni = ServerName::try_from(host.clone())
            .map_err(|_| Error::invalid("SMTP host invalid for TLS SNI"))?;
        let stream = tokio_rustls::TlsConnector::from(Arc::new(cfg))
            .connect(sni, tcp)
            .await
            .map_err(|err| Error::new(StatusCode::BAD_GATEWAY, "smtp_tls_failed", redact_endpoint_for_error(raw_url, secret, &err.to_string())))?;
        run_smtp_session(stream, ch, raw_url, secret, title, text, !starttls).await
    } else {
        run_smtp_session(tcp, ch, raw_url, secret, title, text, true).await
    }
}

async fn run_smtp_session<S>(
    stream: S,
    ch: &Value,
    raw_url: &str,
    secret: &str,
    title: &str,
    text: &str,
    read_greeting: bool,
) -> Api<u16>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (r, mut w) = tokio::io::split(stream);
    let mut reader = BufReader::new(r);

    let expect = |code: u16, msg: &str, expected: &[u16]| -> Api<()> {
        if expected.contains(&code) {
            Ok(())
        } else {
            Err(Error::new(
                StatusCode::BAD_GATEWAY,
                "smtp_protocol_error",
                redact_endpoint_for_error(raw_url, secret, &format!("SMTP {code}: {msg}")),
            ))
        }
    };

    if read_greeting {
        let (code, msg) = read_smtp_reply(&mut reader).await?;
        expect(code, &msg, &[220])?;
    }

    w.write_all(b"EHLO muteki.local\r\n").await.map_err(db)?;
    let (code, msg) = read_smtp_reply(&mut reader).await?;
    expect(code, &msg, &[250])?;

    let smtp_user = ch["smtp_username"].as_str().unwrap_or("");
    if !smtp_user.is_empty() && !secret.is_empty() {
        w.write_all(b"AUTH LOGIN\r\n").await.map_err(db)?;
        let (code, msg) = read_smtp_reply(&mut reader).await?;
        expect(code, &msg, &[334])?;
        let u_b64 = base64::engine::general_purpose::STANDARD.encode(smtp_user.as_bytes());
        w.write_all(format!("{u_b64}\r\n").as_bytes()).await.map_err(db)?;
        let (code, msg) = read_smtp_reply(&mut reader).await?;
        expect(code, &msg, &[334])?;
        let p_b64 = base64::engine::general_purpose::STANDARD.encode(secret.as_bytes());
        w.write_all(format!("{p_b64}\r\n").as_bytes()).await.map_err(db)?;
        let (code, msg) = read_smtp_reply(&mut reader).await?;
        expect(code, &msg, &[235])?;
    }

    let smtp_to = ch["smtp_to"].as_str().unwrap_or("").trim();
    let smtp_from = ch["smtp_from"]
        .as_str()
        .filter(|v| !v.is_empty())
        .unwrap_or("muteki@muteki.local");
    w.write_all(format!("MAIL FROM:<{smtp_from}>\r\n").as_bytes()).await.map_err(db)?;
    let (code, msg) = read_smtp_reply(&mut reader).await?;
    expect(code, &msg, &[250])?;

    w.write_all(format!("RCPT TO:<{smtp_to}>\r\n").as_bytes()).await.map_err(db)?;
    let (code, msg) = read_smtp_reply(&mut reader).await?;
    expect(code, &msg, &[250, 251])?;

    w.write_all(b"DATA\r\n").await.map_err(db)?;
    let (code, msg) = read_smtp_reply(&mut reader).await?;
    expect(code, &msg, &[354])?;

    let mut payload = format!(
        "From: <{smtp_from}>\r\nTo: <{smtp_to}>\r\nSubject: {title}\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n"
    );
    for line in text.lines() {
        if line.starts_with('.') {
            payload.push('.');
        }
        payload.push_str(line);
        payload.push_str("\r\n");
    }
    payload.push_str(".\r\n");
    w.write_all(payload.as_bytes()).await.map_err(db)?;
    let (code, msg) = read_smtp_reply(&mut reader).await?;
    expect(code, &msg, &[250])?;

    let _ = w.write_all(b"QUIT\r\n").await;
    Ok(code)
}

async fn deliver_chunk(
    ch: &Value,
    kind: &str,
    url: &str,
    secret: &str,
    title: &str,
    text: &str,
    allow_private: bool,
) -> Api<u16> {
    if kind == "smtp" {
        return send_smtp_chunk(ch, url, secret, title, text, allow_private).await;
    }
    let (target_url, headers, payload) = build_channel_request(ch, kind, url, secret, title, text);
    let (pinned_url, client) = net_guard::pinned_public_client(&target_url, allow_private).await?;
    let mut req = client.post(pinned_url).json(&payload);
    for (k, v) in headers {
        req = req.header(k, v);
    }
    let resp = req.send().await.map_err(|err| {
        Error::new(
            StatusCode::BAD_GATEWAY,
            "notify_send_failed",
            redact_endpoint_for_error(&target_url, secret, &err.to_string()),
        )
    })?;
    Ok(resp.status().as_u16())
}

pub(super) async fn test_channel(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let _ = task(s, id)?;
    let channel_id = require(body, "channel_id")?;
    let row = rec(s, id, "notify_channel", channel_id)?;
    let ch = row["payload"].clone();
    let kind = ch["kind"].as_str().unwrap_or("webhook").to_owned();
    let url = ch["url"].as_str().unwrap_or("").to_owned();
    let allow_private = ch["allow_private_test"].as_bool().unwrap_or(false)
        && body["allow_private_test"].as_bool() == Some(true);
    let secret = ch["credential_ref"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(|cred_ref| credential_store::resolve(s, id, cred_ref, None))
        .transpose()?
        .unwrap_or_default();

    let title = body["title"].as_str().unwrap_or("Muteki SRC 通知连通性测试");
    let text = body["text"].as_str().unwrap_or("来自 Muteki SRC 工作台的验证事件");
    let chunks = split_message_chunks(&kind, title, text);
    let (first_title, first_text) = &chunks[0];
    let status = deliver_chunk(&ch, &kind, &url, &secret, first_title, first_text, allow_private).await?;
    let deferred_count = chunks.len().saturating_sub(1);
    let outcome = json!({
        "channel_id": channel_id,
        "kind": kind,
        "status_code": status,
        "delivered": (200..300).contains(&status),
        "kept": 1,
        "deferred_count": deferred_count,
        "tested_at": now(),
    });
    s.store.document_scope(|| -> Api<()> {
        let mut updated = ch.clone();
        updated["last_delivery"] = outcome.clone();
        save_rec(s, id, "notify_channel", channel_id, updated, row["revision"].as_u64())?;
        if deferred_count > 0 {
            let carry_rev = rec(s, id, "notify_carryover", channel_id)
                .ok()
                .and_then(|r| r["revision"].as_u64());
            let pending: Vec<Value> = chunks[1..]
                .iter()
                .map(|(t, body)| json!({"title": t, "text": body}))
                .collect();
            save_rec(
                s,
                id,
                "notify_carryover",
                channel_id,
                json!({
                    "channel_id": channel_id,
                    "pending_chunks": pending,
                    "retry_count": 0,
                    "updated_at": now(),
                }),
                carry_rev,
            )?;
        }
        Ok(())
    }).map_err(db)??;
    Ok(outcome)
}

pub(super) async fn flush_carryover(s: &AppState, id: &str, body: &Value) -> Api<Value> {
    let _ = task(s, id)?;
    let channel_id = require(body, "channel_id")?;
    let row = rec(s, id, "notify_channel", channel_id)?;
    let ch = row["payload"].clone();
    let kind = ch["kind"].as_str().unwrap_or("webhook").to_owned();
    let url = ch["url"].as_str().unwrap_or("").to_owned();
    let allow_private = ch["allow_private_test"].as_bool().unwrap_or(false)
        && body["allow_private_test"].as_bool() == Some(true);
    let secret = ch["credential_ref"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(|cred_ref| credential_store::resolve(s, id, cred_ref, None))
        .transpose()?
        .unwrap_or_default();

    let carry_row = rec(s, id, "notify_carryover", channel_id).ok();
    let mut pending = carry_row
        .as_ref()
        .and_then(|r| r["payload"]["pending_chunks"].as_array())
        .cloned()
        .unwrap_or_default();

    let mut flushed = 0usize;
    let mut sent_all = true;
    let mut remaining = Vec::new();
    for item in pending.drain(..) {
        if !sent_all {
            remaining.push(item);
            continue;
        }
        let t = item["title"].as_str().unwrap_or("通知续传");
        let b = item["text"].as_str().unwrap_or("");
        match deliver_chunk(&ch, &kind, &url, &secret, t, b, allow_private).await {
            Ok(code) if (200..300).contains(&code) => flushed += 1,
            Ok(_) | Err(_) => {
                sent_all = false;
                remaining.push(item);
            }
        }
    }

    let carry_rev = carry_row.as_ref().and_then(|r| r["revision"].as_u64());
    s.store.document_scope(|| -> Api<()> {
        save_rec(
            s,
            id,
            "notify_carryover",
            channel_id,
            json!({
                "channel_id": channel_id,
                "pending_chunks": remaining,
                "flushed_last": flushed,
                "updated_at": now(),
            }),
            carry_rev,
        )?;
        Ok(())
    }).map_err(db)??;

    Ok(json!({
        "channel_id": channel_id,
        "flushed_count": flushed,
        "remaining_count": remaining.len(),
        "flushed_at": now(),
    }))
}


pub(super) async fn notify_event(s: &AppState, task_id: &str, event: &str, event_id: &str) -> Api<()> {
    for row in records(s, task_id, "notify_channel")? {
        let ch = &row["payload"];
        if ch["enabled"] != true || !ch["events"].as_array().is_some_and(|events| events.iter().any(|value| value == event)) { continue; }
        let channel_id = require(ch, "channel_id")?;
        let receipt_id = crate::runs::src_api::digest(&json!([channel_id, event_id]));
        if rec(s, task_id, "notify_event_receipt", &receipt_id).is_ok() { continue; }
        let result = test_channel(s, task_id, &json!({"channel_id": channel_id, "title": format!("Muteki SRC {event}"), "text": format!("Task: {task_id}\nEvent: {event_id}\nOpen the workbench to review the evidence.")})).await;
        let result = match result { Ok(value) => value, Err(error) => json!({"delivered":false,"error":{"code":error.code,"message":error.message}}) };
        save_rec(s, task_id, "notify_event_receipt", &receipt_id, json!({"event":event,"event_id":event_id,"result":result,"created_at":now()}), Some(0))?;
    }
    Ok(())
}
