use super::*;
use crate::store::Store;
use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::{Value, json};
use std::{fs, path::Path};
use tower::ServiceExt;

fn app() -> (tempfile::TempDir, Router) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("muteki.sqlite")).unwrap();
    store
        .put("system", "state_dir", &json!(dir.path()))
        .unwrap();
    (dir, router().with_state(Arc::new(AppState::new(store))))
}

async fn call(app: Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn fixture(root: &Path, version: &str) {
    let client = root.join(manifest::NAMESPACE);
    fs::create_dir_all(&client).unwrap();
    fs::write(
        client.join("ui.json"),
        r#"{"schema_version":1,"navigation":[],"command_forms":[]}"#,
    )
    .unwrap();
    fs::write(client.join("server.py"),r#"
import json,sys,os
for raw in sys.stdin:
    request=json.loads(raw);method=request.get('method');ident=request.get('id')
    if method=='initialize': result={'protocol_version':1}
    elif method=='capabilities/list': result={'projections':['default']}
    elif method=='config/validate': result={'valid':True}
    elif method=='health/read': result={'status':'healthy'}
    elif method=='projection/read': result={'data':{'ok':True}}
    elif method=='command/handle': result={'echo':request['params']['payload'],'version':os.getenv('MUTEKI_EXTENSION_VERSION')}
    else: result={}
    print(json.dumps({'jsonrpc':'2.0','id':ident,'result':result}),flush=True)
    if method=='shutdown': break
"#).unwrap();
    let plugin = json!({"$schema":manifest::PLUGIN_SCHEMA,"name":"org.example.fixture","version":version,
    "description":"Fixture extension","extensions":{manifest::NAMESPACE:{
        "manifest_version":1,"extension_version":version,"requires_core":">=1.0,<2.0",
        "origin":"installed","entrypoints":{"host":"subprocess","command":["/usr/bin/python3","-u",format!("./{}/server.py",manifest::NAMESPACE)]},
        "provides":[{"type":"ui-contribution","id":"org.example.fixture.ui","api_version":1}],
        "requires":[],"permissions":{"filesystem":[],"network":[],"secrets":[],"events_write":[]},
        "ui":format!("./{}/ui.json",manifest::NAMESPACE)
    }}});
    fs::write(root.join("plugin.json"), plugin.to_string()).unwrap();
}

#[test]
fn registry_requires_manifest_and_runtime_agreement() {
    let state_dir = tempfile::tempdir().unwrap();
    let store = Store::open(state_dir.path().join("muteki.sqlite")).unwrap();
    store.put("system", "state_dir", &json!(state_dir.path())).unwrap();
    let state = AppState::new(store);
    let manifest = json!({"provides":[
        {"type":"tool-integration","id":"org.example.tool","api_version":1},
        {"type":"platform-adapter","id":"ctfd","api_version":1},
        {"type":"platform-adapter","id":"org.example.platform","api_version":1},
        {"type":"external-agent-adapter","id":"org.example.agent","api_version":1},
        {"type":"domain-module","id":"org.example.module","api_version":1}
    ]});
    let missing = super::capabilities::registered_entries(&state, "org.example.extension", "1.0.0", &manifest, &json!({}));
    assert_eq!(missing[0]["state"], "unavailable");
    let reported = json!({"provides":[
        {"type":"tool-integration","id":"org.example.tool","api_version":1},
        {"type":"platform-adapter","id":"ctfd","api_version":1},
        {"type":"platform-adapter","id":"org.example.platform","api_version":1},
        {"type":"external-agent-adapter","id":"org.example.agent","api_version":1},
        {"type":"domain-module","id":"org.example.module","api_version":1}
    ]});
    let entries = super::capabilities::registered_entries(&state, "org.example.extension", "1.0.0", &manifest, &reported);
    assert_eq!(entries[0]["state"], "ready");
    assert_eq!(entries[1]["state"], "unavailable");
    assert_eq!(entries[1]["detail"], "platform-adapter id collides with a built-in platform");
    assert_eq!(entries[2]["state"], "ready");
    assert_eq!(entries[3]["state"], "ready");
    assert_eq!(entries[4]["state"], "ready");
    assert_eq!(entries[4]["dispatch"], "domain-module.registry");
}

#[tokio::test]
async fn preview_install_requires_confirmed_unchanged_package() {
    let (dir, app) = app();
    let source = dir.path().join("source");
    fs::create_dir_all(&source).unwrap();
    fixture(&source, "1.0.0");
    assert!(!super::source::matches_snapshot(&source, &json!({"content_sha512":"obsolete"})).unwrap());
    let (status, unpinned) = call(app.clone(), "POST", "/api/extensions/preview",
        json!({"source":{"kind":"http","url":"http://127.0.0.1:9/extension.zip","sha512":"obsolete"}})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(unpinned["error"]["code"], "extension.digest_required");
    let request = json!({"source":{"kind":"local-dir","path":source}});
    let (status, preview) = call(
        app.clone(),
        "POST",
        "/api/extensions/preview",
        request.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(
        preview["verification"]["content_sha256"]
            .as_str()
            .unwrap()
            .len(),
        64
    );
    assert!(preview["verification"]["snapshot_id"].as_str().unwrap().starts_with("snapshot-"));
    assert_eq!(preview["confirmations_required"], json!(["unsigned"]));
    let mut forged = request.clone();
    forged["source"]["trust_status"] = json!("verified");
    forged["source"]["publisher"] = json!("attacker");
    let (_, untrusted) = call(app.clone(), "POST", "/api/extensions/preview", forged).await;
    assert_eq!(untrusted["verification"]["signature"], "unsigned");
    let id = preview["preview_id"].as_str().unwrap();
    let install = json!({"source":request["source"],"preview_id":id,"confirmations":[]});
    let (status, missing) = call(
        app.clone(),
        "POST",
        "/api/extensions/install",
        install.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(missing["error"]["code"], "extension.confirmation_required");
    fs::write(source.join("extra.txt"), "changed after preview").unwrap();
    let mut confirmed = install;
    confirmed["confirmations"] = json!(["unsigned"]);
    let (status, changed) = call(app.clone(), "POST", "/api/extensions/install", confirmed).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(changed["error"]["code"], "extension.preview_source_changed");
    let (_, second) = call(
        app.clone(),
        "POST",
        "/api/extensions/preview",
        request.clone(),
    )
    .await;
    let (status, installed) = call(
        app.clone(),
        "POST",
        "/api/extensions/install",
        json!({"source":request["source"],
        "preview_id":second["preview_id"],"confirmations":["unsigned"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{installed}");
    assert_eq!(installed["receipt"]["state"], "completed");
    let (_, overview) = call(app.clone(), "GET", "/api/extensions/overview", json!({})).await;
    assert_eq!(overview["extensions"].as_array().unwrap().len(), 1);
    assert_eq!(overview["extensions"][0]["record"]["state"], "installed");
    let (_, ui) = call(
        app,
        "GET",
        "/api/extensions/org.example.fixture/ui",
        json!({}),
    )
    .await;
    assert_eq!(ui["contributions"]["schema_version"], 1);
}

#[tokio::test]
async fn rejects_script_ui_and_copies_local_links() {
    let (dir, app) = app();
    let source = dir.path().join("source");
    fs::create_dir_all(&source).unwrap();
    fixture(&source, "1.0.0");
    fs::write(
        source.join(manifest::NAMESPACE).join("ui.json"),
        r#"{"schema_version":1,"script":"alert(1)"}"#,
    )
    .unwrap();
    let (status, body) = call(
        app.clone(),
        "POST",
        "/api/extensions/preview",
        json!({"source":{"kind":"local-dir","path":source}}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "extension.ui_script_denied");
    fs::write(
        source.join(manifest::NAMESPACE).join("ui.json"),
        r#"{"schema_version":1}"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        fs::write(dir.path().join("linked-source.txt"), "local source content").unwrap();
        std::os::unix::fs::symlink(dir.path().join("linked-source.txt"), source.join("linked.txt")).unwrap();
    }
    let (status, body) = call(
        app,
        "POST",
        "/api/extensions/preview",
        json!({"source":{"kind":"local-dir","path":source}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn subprocess_handshake_invoke_health_and_disable() {
    let (dir, app) = app();
    let source = dir.path().join("source");
    fs::create_dir_all(&source).unwrap();
    fixture(&source, "1.0.0");
    let request = json!({"source":{"kind":"local-dir","path":source}});
    let (_, preview) = call(
        app.clone(),
        "POST",
        "/api/extensions/preview",
        request.clone(),
    )
    .await;
    let (status,installed)=call(app.clone(),"POST","/api/extensions/install",json!({"source":request["source"],"preview_id":preview["preview_id"],"confirmations":["unsigned"]})).await;
    assert_eq!(status, StatusCode::OK, "{installed}");
    let (status, enabled) = call(
        app.clone(),
        "POST",
        "/api/extensions/org.example.fixture/enable",
        json!({}),
    )
    .await;
    if status != StatusCode::OK {
        for entry in walkdir::WalkDir::new(dir.path().join("extensions"))
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_file() && e.path().extension().is_some_and(|v| v == "log"))
        {
            eprintln!(
                "extension log {}: {}",
                entry.path().display(),
                fs::read_to_string(entry.path()).unwrap_or_default()
            );
        }
    }
    assert_eq!(status, StatusCode::OK, "{enabled}");
    let (status, invoked) = call(
        app.clone(),
        "POST",
        "/api/extensions/org.example.fixture/invoke",
        json!({"command_type":"ext.org.example.fixture.echo","params":{"hello":"world"}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{invoked}");
    assert_eq!(invoked["result"]["echo"]["hello"], "world");
    let (_, health) = call(
        app.clone(),
        "POST",
        "/api/extensions/org.example.fixture/health/refresh",
        json!({}),
    )
    .await;
    assert_eq!(health["health"]["status"], "healthy");
    let upgrade_source = dir.path().join("upgrade");
    fs::create_dir_all(&upgrade_source).unwrap();
    fixture(&upgrade_source, "1.1.0");
    let upgrade_request = json!({"source":{"kind":"local-dir","path":upgrade_source},"extension_id":"org.example.fixture"});
    let (status, next) = call(
        app.clone(),
        "POST",
        "/api/extensions/preview",
        upgrade_request.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{next}");
    let (status, upgraded) = call(
        app.clone(),
        "POST",
        "/api/extensions/org.example.fixture/upgrade",
        json!({"source":upgrade_request["source"],
        "preview_id":next["preview_id"],"confirmations":next["confirmations_required"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{upgraded}");
    let (_, detail) = call(
        app.clone(),
        "GET",
        "/api/extensions/org.example.fixture/detail",
        json!({}),
    )
    .await;
    assert!(
        !detail["migrations"][0]["files"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == ".extension-sandbox.sb")
    );
    let (_, after_upgrade) = call(
        app.clone(),
        "POST",
        "/api/extensions/org.example.fixture/invoke",
        json!({"command_type":"ext.org.example.fixture.echo","params":{}}),
    )
    .await;
    assert_eq!(after_upgrade["result"]["version"], "1.1.0");
    let (status, rolled_back) = call(
        app.clone(),
        "POST",
        "/api/extensions/org.example.fixture/rollback",
        json!({"version":"1.0.0"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{rolled_back}");
    let (_, after_rollback) = call(
        app.clone(),
        "POST",
        "/api/extensions/org.example.fixture/invoke",
        json!({"command_type":"ext.org.example.fixture.echo","params":{}}),
    )
    .await;
    assert_eq!(after_rollback["result"]["version"], "1.0.0");
    let (_, disabled) = call(
        app,
        "POST",
        "/api/extensions/org.example.fixture/disable",
        json!({}),
    )
    .await;
    assert_eq!(disabled["receipt"]["state"], "completed");
}
