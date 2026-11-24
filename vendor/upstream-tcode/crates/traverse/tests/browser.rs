//! The browser listener over loopback: static bundle, password login and the
//! `/ws` hello, as `crates/web` speaks them.

use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tcode_traverse::HostMux;
use tcode_traverse::browser::{BrowserConfig, BrowserServer, serve, set_password};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

struct TestDir(PathBuf);

impl TestDir {
    fn new(tag: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "tcode-browser-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fake_host() -> HostMux {
    let (to_host, host_rx) =
        async_channel::unbounded::<tcode_client::attachment::AttachmentMessage>();
    let (host_tx, from_host) = async_channel::unbounded::<String>();
    std::thread::spawn(move || {
        while let Ok(message) = host_rx.recv_blocking() {
            let line = message.line;
            let request = tcode_protocol::decode_client_line(&line).unwrap();
            let id = request.id;
            if let tcode_protocol::ClientPayload::Subscribe(subscription) = &request.payload {
                let snapshot = tcode_protocol::HostMessage::Event(tcode_protocol::EventEnvelope {
                    request_id: Some(id),
                    topic: subscription.topic.clone(),
                    event: tcode_protocol::ServerEvent::IndexSnapshot(
                        tcode_protocol::IndexSnapshot {
                            summary: tcode_protocol::IndexSummary::default(),
                            sessions: Vec::new(),
                            projects: Vec::new(),
                        },
                    ),
                });
                host_tx
                    .send_blocking(tcode_protocol::encode_line(&snapshot).unwrap())
                    .unwrap();
            }
            let mut response = tcode_protocol::CommandResponse::Unit;
            if let tcode_protocol::ClientPayload::Command(
                tcode_protocol::Command::CreateProject { root },
            ) = &request.payload
            {
                let broadcast = tcode_protocol::HostMessage::Event(tcode_protocol::EventEnvelope {
                    request_id: None, topic: tcode_protocol::Topic::Index,
                    event: serde_json::from_value(json!({"type":"index_upsert_project","content":{"id":"project","name":"Shared","root":root,"created_at":1}})).unwrap(),
                });
                host_tx
                    .send_blocking(tcode_protocol::encode_line(&broadcast).unwrap())
                    .unwrap();
                response = tcode_protocol::CommandResponse::ProjectId(Some("project".into()));
            }
            host_tx
                .send_blocking(
                    tcode_protocol::encode_line(&tcode_protocol::HostMessage::Ack {
                        id,
                        result: Ok(response),
                    })
                    .unwrap(),
                )
                .unwrap();
        }
    });
    HostMux::new(to_host, from_host)
}

fn config(data_dir: PathBuf) -> BrowserConfig {
    BrowserConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        host_name: "Test Host".into(),
        data_dir,
        static_bundle: None,
        hosting: None,
    }
}

/// One bounded HTTP/1.1 request; returns the status line and body.
fn http(addr: SocketAddr, method: &str, path: &str, body: &str) -> (String, Vec<u8>) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .unwrap();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut bytes = Vec::new();
    let _ = stream.read_to_end(&mut bytes);
    let split = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8(bytes[..split].to_vec()).unwrap();
    (
        head.lines().next().unwrap().to_owned(),
        bytes[split + 4..].to_vec(),
    )
}

fn login(addr: SocketAddr, password: &str, device_name: &str) -> (String, Value) {
    let body = json!({"password": password, "device_name": device_name, "device_id": format!("{device_name}-id"), "platform": "macOS"});
    let (status, body) = http(addr, "POST", "/auth/login", &body.to_string());
    (status, serde_json::from_slice(&body).unwrap_or_default())
}

type Socket = WebSocketStream<tokio::net::TcpStream>;

/// A browser's WebSocket: the upgrade, then JSON text frames each way.
struct Browser(Socket);

impl Browser {
    async fn open(addr: SocketAddr) -> Self {
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (socket, _) = tokio_tungstenite::client_async(format!("ws://{addr}/ws"), stream)
            .await
            .unwrap();
        Self(socket)
    }

    async fn hello(addr: SocketAddr, hello: Value) -> (Self, Value) {
        let mut browser = Self::open(addr).await;
        browser.send(hello).await;
        let reply = browser.recv().await;
        (browser, reply)
    }

    async fn send(&mut self, value: Value) {
        self.0
            .send(Message::Text(value.to_string().into()))
            .await
            .unwrap();
    }

    async fn recv(&mut self) -> Value {
        loop {
            match tokio::time::timeout(Duration::from_secs(10), self.0.next())
                .await
                .expect("the listener answered in time")
            {
                Some(Ok(Message::Text(text))) => return serde_json::from_str(&text).unwrap(),
                Some(Ok(_)) => continue,
                other => panic!("socket ended: {other:?}"),
            }
        }
    }
}

fn hello(token: &str, device_name: &str) -> Value {
    json!({"type":"hello","protocol_version":tcode_protocol::PROTOCOL_VERSION,"token":token,"device":{"name":device_name,"platform":"macOS"}})
}

fn setup(server: &BrowserServer, password: &str) {
    let (status, _) = http(
        server.local_addr(),
        "POST",
        "/auth/setup",
        &json!({"password": password}).to_string(),
    );
    assert_eq!(status, "HTTP/1.1 200 OK");
}

#[test]
fn static_bundle_get_and_head_share_headers() {
    let dir = TestDir::new("static");
    let mut config = config(dir.0.clone());
    const BUNDLE: &[(&str, &[u8])] = &[
        ("/index.html", b"<html>tcode</html>"),
        ("/tcode_web_bg.wasm", b"\0asm"),
    ];
    config.static_bundle = Some(BUNDLE);
    let server = serve(fake_host(), config).unwrap();
    for (path, status, content_type, length) in [
        ("/", "200 OK", "text/html; charset=utf-8", 18),
        ("/index.html", "200 OK", "text/html; charset=utf-8", 18),
        ("/tcode_web_bg.wasm", "200 OK", "application/wasm", 4),
        ("/missing", "404 Not Found", "text/plain; charset=utf-8", 9),
    ] {
        let request = |method| {
            let mut stream = TcpStream::connect(server.local_addr()).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            write!(
                stream,
                "{method} {path} HTTP/1.1\r\nHost: localhost\r\n\r\n"
            )
            .unwrap();
            let mut result = Vec::new();
            let read = stream.read_to_end(&mut result);
            assert!(read.is_ok() || read.unwrap_err().kind() == std::io::ErrorKind::UnexpectedEof);
            String::from_utf8(result).unwrap()
        };
        let get = request("GET");
        let head = request("HEAD");
        let (headers, body) = get.split_once("\r\n\r\n").unwrap();
        assert_eq!(head, format!("{headers}\r\n\r\n"));
        assert!(headers.starts_with(&format!("HTTP/1.1 {status}")));
        assert!(headers.contains(&format!("Content-Type: {content_type}")));
        assert!(headers.contains(&format!("Content-Length: {length}")));
        assert_eq!(body.len(), length);
    }
    server.shutdown();
}

#[test]
fn two_browsers_get_their_own_acks_and_shared_broadcasts() {
    let dir = TestDir::new("two");
    let server = serve(fake_host(), config(dir.0.clone())).unwrap();
    let addr = server.local_addr();
    setup(&server, "secret password");
    let tokens: Vec<String> = ["Chrome", "Firefox"]
        .into_iter()
        .map(|name| {
            login(addr, "secret password", name).1["token"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    tcode_traverse::block_on(async {
        let (mut one, _) = Browser::hello(addr, hello(&tokens[0], "Chrome")).await;
        let (mut two, _) = Browser::hello(addr, hello(&tokens[1], "Firefox")).await;
        for (browser, id) in [(&mut one, 7), (&mut two, 7)] {
            browser
                .send(json!({"id":id,"payload":{"type":"subscribe","content":{"topic":{"type":"index"}}}}))
                .await;
            let mut kinds = vec![browser.recv().await["type"].as_str().unwrap().to_owned()];
            kinds.push(browser.recv().await["type"].as_str().unwrap().to_owned());
            kinds.sort();
            assert_eq!(kinds, ["ack", "event"]);
        }
        one.send(json!({"id":8,"payload":{"type":"command","content":{"type":"create_project","content":{"root":"/tmp/shared"}}}}))
            .await;
        let mut seen = [one.recv().await, one.recv().await];
        seen.sort_by_key(|value| value["type"].as_str().unwrap().to_owned());
        assert_eq!(seen[0]["type"], "ack");
        assert_eq!(
            seen[0]["content"]["id"], 8,
            "acks carry the browser's own id"
        );
        assert_eq!(seen[1]["content"]["event"]["type"], "index_upsert_project");
        let broadcast = two.recv().await;
        assert_eq!(
            broadcast["content"]["event"]["type"], "index_upsert_project",
            "a subscribed browser sees the other's broadcast"
        );
        // A reply to this later command is an ordered barrier: any leaked
        // ack for the first browser would precede it on the host stream.
        two.send(
            json!({"id":9,"payload":{"type":"command","content":{"type":"open_latest_session"}}}),
        )
        .await;
        loop {
            let reply = two.recv().await;
            if reply["type"] == "ack" {
                assert_eq!(reply["content"]["id"], 9, "the ack reaches only its sender");
                break;
            }
        }
    });
    server.shutdown();
}

#[test]
fn a_bind_beyond_loopback_needs_a_password_first() {
    let dir = TestDir::new("lan");
    let mux = fake_host();
    let lan = || BrowserConfig {
        listen: "0.0.0.0:0".parse().unwrap(),
        ..config(dir.0.clone())
    };
    let Err(error) = serve(mux.clone(), lan()) else {
        panic!("a LAN bind without a password must be refused");
    };
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("--password"), "{error}");
    assert!(
        !dir.0.join("remote.json").exists(),
        "a refused bind leaves nothing behind"
    );
    let early = tcode_traverse::browser::check_bind(lan().listen, &dir.0);
    assert_eq!(
        early.as_ref().map_err(|error| error.kind()),
        Err(std::io::ErrorKind::InvalidInput),
        "the same rule answers before anything is started"
    );
    set_password(&dir.0, "secret password", false).unwrap();
    tcode_traverse::browser::check_bind(lan().listen, &dir.0).unwrap();
    let server = serve(mux, lan()).unwrap();
    assert!(server.password_configured());
    server.shutdown();
}
