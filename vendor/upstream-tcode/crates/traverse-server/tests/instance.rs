//! One instance with TLS off on ephemeral ports: the manifest and store over
//! HTTP, then two relay-only iroh endpoints that find each other through the
//! pkarr store and talk through the relay. No external network.
use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use iroh::{
    Endpoint, EndpointAddr, RelayConfig, RelayMap, RelayMode, RelayUrl, SecretKey,
    address_lookup::{PkarrPublisher, PkarrResolver},
    endpoint::presets,
};
use iroh_dns::{endpoint_info::EndpointInfo, pkarr::SignedPacket};
use tcode_traverse_server::{Config, Server, pkarr::MAX_PAYLOAD_BYTES};
use url::Url;

const ALPN: &[u8] = b"tcode/test/1";

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("tcode-traverse-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn spawn(name: &str, configure: impl FnOnce(&mut Config)) -> Server {
    let mut config = Config::dev(temp_dir(name), SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    configure(&mut config);
    // The rate limiters see a frozen clock: buckets never refill, so the
    // bursts below are consumed by request count alone, however long a
    // slow CI disk takes between two requests.
    let frozen = Instant::now();
    Server::spawn_with_clock(
        config,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
        Arc::new(move || frozen),
    )
    .await
    .expect("server binds ephemeral ports")
}

fn packet(secret: &SecretKey, relay: &str) -> SignedPacket {
    EndpointInfo::new(secret.public())
        .with_relay_url(RelayUrl::from(Url::parse(relay).unwrap()))
        .to_pkarr_signed_packet(secret, 30)
        .unwrap()
}

#[tokio::test]
async fn manifest_and_pkarr_store_over_http() {
    let server = spawn("http", |config| {
        config.region = Some("test".into());
        config.pkarr.put_per_second = 1;
        config.pkarr.put_burst = 4;
        config.pkarr.get_per_second = 1;
        config.pkarr.get_burst = 3;
    })
    .await;
    let base = server.base().clone();
    assert_eq!(base.as_str(), format!("http://{}/", server.http_addr()));
    assert!(server.https_addr().is_none());
    assert!(server.quic_addr().is_none());
    let http = reqwest::Client::new();

    let manifest = http
        .get(base.join("relays.json").unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(manifest.status(), 200);
    assert_eq!(manifest.headers()["cache-control"], "public, max-age=300");
    let manifest: serde_json::Value = manifest.json().await.unwrap();
    assert_eq!(
        manifest,
        serde_json::json!({
            "version": 1,
            "updatedAt": "2027-01-15T08:00:00Z",
            "relays": [{ "url": base.as_str(), "quic_port": 0, "region": "test" }],
            "pkarr": [base.join("pkarr").unwrap().as_str()]
        })
    );
    let health = http
        .get(base.join("healthz").unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(health.text().await.unwrap(), "ok");

    let secret = SecretKey::generate();
    let key = secret.public().to_z32();
    let pkarr = base.join(&format!("pkarr/{key}")).unwrap();
    let older = packet(&secret, "https://old.example/");
    let newer = packet(&secret, "https://new.example/");

    let unknown = http.get(pkarr.clone()).send().await.unwrap();
    assert_eq!(unknown.status(), 404);
    assert_eq!(unknown.headers()["access-control-allow-origin"], "*");

    let put = http
        .put(pkarr.clone())
        .body(newer.to_relay_payload())
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), 204);
    let stale = http
        .put(pkarr.clone())
        .body(older.to_relay_payload())
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 409);
    let equal = http
        .put(pkarr.clone())
        .body(newer.to_relay_payload())
        .send()
        .await
        .unwrap();
    assert_eq!(equal.status(), 409);
    // Neither rejected write changes the accepted record.
    let got = http.get(pkarr.clone()).send().await.unwrap();
    assert_eq!(got.status(), 200);
    assert_eq!(got.headers()["cache-control"], "public, max-age=300");
    assert_eq!(got.bytes().await.unwrap(), newer.to_relay_payload());
    // The root form of the pkarr relay specification serves the same record.
    let root = http.get(base.join(&key).unwrap()).send().await.unwrap();
    assert_eq!(root.status(), 200);
    // Three GETs used their own burst; PUTs keep theirs.
    let limited = http.get(pkarr.clone()).send().await.unwrap();
    assert_eq!(limited.status(), 429);
    assert_eq!(limited.headers()["retry-after"], "1");

    let oversize = http
        .put(pkarr.clone())
        .body(vec![0u8; MAX_PAYLOAD_BYTES + 1])
        .send()
        .await
        .unwrap();
    assert_eq!(oversize.status(), 413);

    let mut corrupted = packet(&secret, "https://c.example/").to_relay_payload();
    corrupted[0] ^= 0x01;
    let invalid = http
        .put(pkarr.clone())
        .body(corrupted)
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 400);

    // Four PUTs used the burst (the oversize one never reached the
    // limiter); the next is refused whatever it carries.
    let limited = http
        .put(pkarr.clone())
        .body(packet(&secret, "https://d.example/").to_relay_payload())
        .send()
        .await
        .unwrap();
    assert_eq!(limited.status(), 429);
    assert_eq!(limited.headers()["retry-after"], "1");

    let preflight = http
        .request(reqwest::Method::OPTIONS, pkarr)
        .send()
        .await
        .unwrap();
    assert_eq!(preflight.status(), 204);
    assert_eq!(
        preflight.headers()["access-control-allow-methods"],
        "GET, PUT, OPTIONS"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn endpoints_meet_through_the_relay_and_the_pkarr_store() {
    let server = spawn("relay", |_| {}).await;
    let base = server.base().clone();
    let pkarr = base.join("pkarr").unwrap();
    let relay_url = RelayUrl::from(base.clone());
    let relays: RelayMap = [RelayConfig::from(relay_url.clone())].into_iter().collect();

    let host = Endpoint::builder(presets::Minimal)
        .alpns(vec![ALPN.to_vec()])
        .relay_mode(RelayMode::Custom(relays.clone()))
        .address_lookup(PkarrPublisher::builder(pkarr.clone()))
        .clear_ip_transports()
        .bind()
        .await
        .unwrap();
    let device = Endpoint::builder(presets::Minimal)
        .alpns(vec![ALPN.to_vec()])
        .relay_mode(RelayMode::Custom(relays))
        .address_lookup(PkarrResolver::builder(pkarr.clone()))
        .clear_ip_transports()
        .bind()
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), host.online())
        .await
        .expect("host reaches its home relay");
    assert_eq!(
        host.addr().relay_urls().next().cloned(),
        Some(relay_url.clone())
    );

    // The host's record reaches the store (the publisher runs in the background).
    let http = reqwest::Client::new();
    let record = base.join(&format!("pkarr/{}", host.id().to_z32())).unwrap();
    let payload = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let response = http.get(record.clone()).send().await.unwrap();
            if response.status() == 200 {
                return response.bytes().await.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("host publishes to the pkarr store");
    let published = SignedPacket::from_relay_payload(&host.id(), &payload).unwrap();
    let info = EndpointInfo::from_pkarr_signed_packet(&published).unwrap();
    assert_eq!(info.relay_urls().next(), Some(&relay_url));

    // The device knows only the id: pkarr resolves the relay, the relay carries the bytes.
    let accept = tokio::spawn({
        let host = host.clone();
        async move {
            let connection = host.accept().await.unwrap().await.unwrap();
            let mut stream = connection.accept_uni().await.unwrap();
            stream.read_to_end(64).await.unwrap()
        }
    });
    let connection = tokio::time::timeout(
        Duration::from_secs(20),
        device.connect(EndpointAddr::new(host.id()), ALPN),
    )
    .await
    .expect("connect within the timeout")
    .expect("connect through the relay");
    assert!(connection.paths().iter().all(|path| path.is_relay()));
    let mut stream = connection.open_uni().await.unwrap();
    stream.write_all(b"hello over traverse").await.unwrap();
    stream.finish().unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), accept)
            .await
            .unwrap()
            .unwrap(),
        b"hello over traverse"
    );

    connection.close(0u32.into(), b"done");
    device.close().await;
    host.close().await;
    server.shutdown().await;
}
