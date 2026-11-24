use crate::{MutekiApp, api, composer};
use nsh_core::{element::Root, prelude::*};
use nsh_engine::EngineConfig;
use nsh_testing::{Headless, StubNetwork, StubResponse};
use serde_json::{Value, json};
use std::sync::Arc;
fn response(status: u16, value: Value) -> StubResponse {
    let mut response = StubResponse::default();
    response.status = status;
    response.body = value.to_string().into_bytes();
    response
}
fn catalog() -> Value {
    json!({"credentials":[
        {"credential_id":"a","label":"Credential A","http_compatible":true,"default_model":"model-a","models":[{"id":"model-a","label":"Model A"}]},
        {"credential_id":"b","label":"Credential B","http_compatible":false,"default_model":"model-b","models":[{"id":"model-b","label":"Model B"}]}
    ]})
}
fn snapshot(id: &str, title: &str) -> Value {
    json!({"thread":{"thread_id":id,"title":title,"archived":false},"state":{"status":"idle"},"runtime":composer::runtime("a","model-a"),"messages":[{"message_id":format!("msg-{id}"),"role":"assistant","text":format!("# Transcript {id}\n\n```rust\nlet value = 42;\n```")}],"turns":[]})
}
fn start() -> (Headless, Arc<StubNetwork>) {
    let routes = vec![
        (
            format!("GET {}", api::url("/api/threads")),
            response(
                200,
                json!({"threads":[{"thread_id":"a","title":"First conversation","archived":false},{"thread_id":"b","title":"Second conversation","archived":false}]}),
            ),
        ),
        (
            format!("GET {}", api::url("/api/settings/ui")),
            response(
                200,
                json!({"version":0,"values":{"defaultModel":{"credentialId":"a","modelId":"model-a"}}}),
            ),
        ),
        (
            format!("GET {}", api::url("/api/models/catalog")),
            response(200, catalog()),
        ),
        (
            format!("GET {}", api::url("/api/threads/a")),
            response(200, snapshot("a", "First conversation")),
        ),
        (
            format!("GET {}", api::url("/api/threads/b")),
            response(200, snapshot("b", "Second conversation")),
        ),
        (
            format!("POST {}", api::url("/api/threads/a/commands")),
            response(
                503,
                json!({"error":{"message":"Provider temporarily unavailable"}}),
            ),
        ),
    ];
    let mut routes = routes;
    routes.push((
        format!("POST {}", api::url("/api/threads")),
        response(503, json!({"error":{"message":"Creation unavailable"}})),
    ));
    let network = Arc::new(StubNetwork::new(routes));
    let mut config = EngineConfig::new(Root::of::<MutekiApp>());
    config.viewport = Size::new(px(1200.0), px(800.0));
    config.network = Some(network.clone());
    config.ui_sets = vec![nsh_base::registry(), crate::registry()];
    (Headless::start(config).unwrap(), network)
}
#[test]
fn switching_conversations_restores_each_draft() {
    let (mut app, _) = start();
    app.wait_for("First conversation").unwrap();
    app.press("First conversation").unwrap();
    app.wait_for("Transcript a").unwrap();
    app.type_text("id:composer", "unfinished first draft")
        .unwrap();
    app.press("Second conversation").unwrap();
    app.wait_for("Transcript b").unwrap();
    assert_eq!(app.screen().input_text("id:composer").unwrap(), Some(""));
    app.type_text("id:composer", "second draft").unwrap();
    app.press("First conversation").unwrap();
    app.wait_for("Transcript a").unwrap();
    assert_eq!(
        app.screen().input_text("id:composer").unwrap(),
        Some("unfinished first draft")
    );
    app.press("Second conversation").unwrap();
    app.wait_for("Transcript b").unwrap();
    assert_eq!(
        app.screen().input_text("id:composer").unwrap(),
        Some("second draft")
    );
}
#[test]
fn failed_send_keeps_text_and_explicit_runtime() {
    let (mut app, network) = start();
    app.wait_for("First conversation").unwrap();
    app.press("First conversation").unwrap();
    app.wait_for("Transcript a").unwrap();
    app.wait_for("Credential A").unwrap();
    app.type_text("id:composer", "keep this message").unwrap();
    app.press("发送消息").unwrap();
    app.wait_for("Provider temporarily unavailable").unwrap();
    assert_eq!(
        app.screen().input_text("id:composer").unwrap(),
        Some("keep this message")
    );
    let requests = network.requests();
    let sent = requests
        .iter()
        .find(|r| r.method == "POST")
        .expect("send request");
    let body: Value = serde_json::from_slice(match &sent.body {
        nsh_engine::HttpBody::Bytes(bytes) => bytes,
        _ => panic!("JSON body required"),
    })
    .unwrap();
    assert_eq!(body["payload"]["text"], "keep this message");
    assert_eq!(
        body["payload"]["runtime"],
        composer::runtime("a", "model-a")
    );
    assert!(!body["command_id"].as_str().unwrap().is_empty());
}
#[test]
fn whitespace_does_not_send_a_request() {
    let (mut app, network) = start();
    app.wait_for("First conversation").unwrap();
    app.type_text("id:composer", "   ").unwrap();
    app.submit("id:composer").unwrap();
    assert!(network.requests().iter().all(|r| r.method != "POST"));
}
#[test]
fn absent_or_unavailable_default_never_selects_another_credential() {
    let rows = catalog()["credentials"].as_array().unwrap().clone();
    assert_eq!(
        composer::default_choice(&json!({"values":{}}), &rows),
        (String::new(), String::new())
    );
    assert_eq!(
        composer::default_choice(
            &json!({"values":{"defaultModel":{"credentialId":"b","modelId":"model-b"}}}),
            &rows
        ),
        (String::new(), String::new())
    );
    assert_eq!(
        composer::default_choice(
            &json!({"values":{"defaultModel":{"credentialId":"a","modelId":"model-a"}}}),
            &rows
        ),
        ("a".into(), "model-a".into())
    );
    assert!(!composer::supported(&rows, "a", "unknown-model"));
}
#[test]
fn unknown_usage_is_distinct_from_measured_zero() {
    assert_eq!(crate::usage::number(&Value::Null), "未提供");
    assert_eq!(crate::usage::number(&json!(0)), "0");
}

#[test]
fn assistant_markdown_uses_headings_and_code_actions() {
    let (mut app, _) = start();
    app.wait_for("First conversation").unwrap();
    app.press("First conversation").unwrap();
    app.wait_for("Transcript a").unwrap();
    app.wait_for("复制代码").unwrap();
    assert!(!app.screen().matches("# Transcript a").unwrap());
}

#[test]
fn new_conversation_failure_keeps_draft_and_sends_selected_runtime() {
    let (mut app, network) = start();
    app.wait_for("Credential A").unwrap();
    app.type_text("id:composer", "new conversation draft")
        .unwrap();
    app.press("发送消息").unwrap();
    app.wait_for("Creation unavailable").unwrap();
    assert_eq!(
        app.screen().input_text("id:composer").unwrap(),
        Some("new conversation draft")
    );
    let requests = network.requests();
    let sent = requests.iter().find(|r| r.method == "POST").unwrap();
    let body: Value = serde_json::from_slice(match &sent.body {
        nsh_engine::HttpBody::Bytes(bytes) => bytes,
        _ => panic!("expected JSON"),
    })
    .unwrap();
    assert_eq!(body["runtime"], composer::runtime("a", "model-a"));
    assert!(!body["command_id"].as_str().unwrap().is_empty());
}
