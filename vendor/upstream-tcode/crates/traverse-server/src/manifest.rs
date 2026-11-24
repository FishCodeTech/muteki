//! `GET /relays.json`: this instance and its configured peers, in the shape
//! the client parses.
use std::net::SocketAddr;

use tcode_traverse::manifest::{Manifest, ManifestRelay};
use url::Url;

use crate::config::Config;

/// Where clients reach this instance: `https://<hostname>[:port]/` when a
/// hostname is configured (a TLS-terminating proxy in front of `tls.mode =
/// "off"` still serves HTTPS), else the plain listener itself.
pub fn public_base(config: &Config, http_addr: SocketAddr) -> Url {
    match config.hostname.as_deref().filter(|host| !host.is_empty()) {
        Some(hostname) => {
            let port = if config.tls_enabled() {
                config.tls.public_port.unwrap_or(config.tls.bind.port())
            } else {
                443
            };
            let authority = if port == 443 {
                hostname.to_string()
            } else {
                format!("{hostname}:{port}")
            };
            Url::parse(&format!("https://{authority}/")).expect("hostname forms a URL")
        }
        None => Url::parse(&format!("http://{http_addr}/")).expect("socket address forms a URL"),
    }
}

pub fn compose(config: &Config, base: &Url, updated_at: String) -> Manifest {
    let quic_port = if config.quic_enabled() {
        config.relay.quic_bind.port()
    } else {
        0
    };
    // `0` marks a relay without QUIC address discovery; absent means the
    // client's default port, so a relay-only instance must send `0`.
    let mut relays = vec![ManifestRelay {
        url: base.clone(),
        quic_port: Some(quic_port),
        region: config.region.clone(),
    }];
    relays.extend(config.peers.iter().map(|peer| ManifestRelay {
        url: peer.url.clone(),
        quic_port: peer.quic_port,
        region: peer.region.clone(),
    }));
    Manifest {
        version: 1,
        updated_at: Some(updated_at),
        relays,
        pkarr: vec![base.join("pkarr").expect("relative path")],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Peer, TlsMode};

    fn json(manifest: &Manifest) -> serde_json::Value {
        serde_json::to_value(manifest).unwrap()
    }

    #[test]
    fn a_tls_instance_advertises_itself_qad_and_peers() {
        let mut config = Config::parse(
            "hostname = \"traverse.example.org\"\ncontact = \"admin@example.org\"\nregion = \"eu\"",
        )
        .unwrap();
        config.peers.push(Peer {
            url: Url::parse("https://relay-2.example.org/").unwrap(),
            quic_port: Some(7842),
            region: Some("us".into()),
        });
        let base = public_base(&config, "127.0.0.1:1".parse().unwrap());
        let manifest = compose(&config, &base, "2026-09-21T00:00:00Z".into());
        assert_eq!(
            json(&manifest),
            serde_json::json!({
                "version": 1,
                "updatedAt": "2026-09-21T00:00:00Z",
                "relays": [
                    { "url": "https://traverse.example.org/", "quic_port": 7842, "region": "eu" },
                    { "url": "https://relay-2.example.org/", "quic_port": 7842, "region": "us" }
                ],
                "pkarr": ["https://traverse.example.org/pkarr"]
            })
        );
        // What the server sends, the client accepts as is.
        assert_eq!(
            Manifest::parse(&serde_json::to_string(&manifest).unwrap()).unwrap(),
            manifest
        );
    }

    #[test]
    fn without_tls_the_relay_is_relay_only_and_a_proxied_hostname_stays_https() {
        let mut config =
            Config::parse("hostname = \"traverse.example.org\"\n[tls]\nmode = \"off\"").unwrap();
        let base = public_base(&config, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(base.as_str(), "https://traverse.example.org/");
        let manifest = compose(&config, &base, "x".into());
        assert_eq!(json(&manifest)["relays"][0]["quic_port"], 0);

        config.hostname = None;
        let base = public_base(&config, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(base.as_str(), "http://127.0.0.1:8080/");
        assert_eq!(
            compose(&config, &base, "x".into()).pkarr,
            [Url::parse("http://127.0.0.1:8080/pkarr").unwrap()]
        );

        config.tls.mode = TlsMode::Manual;
        config.tls.bind = "[::]:8443".parse().unwrap();
        config.hostname = Some("h.example".into());
        let base = public_base(&config, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(base.as_str(), "https://h.example:8443/");
        assert_eq!(
            compose(&config, &base, "x".into()).relays[0].quic_port,
            Some(7842)
        );

        // A proxy on 443 forwarding to tls.bind: the manifest names the public port.
        config.tls.public_port = Some(443);
        let base = public_base(&config, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(base.as_str(), "https://h.example/");
    }
}
