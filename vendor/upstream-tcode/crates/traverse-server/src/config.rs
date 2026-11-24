//! The TOML configuration of one Traverse instance. `TEMPLATE` is what
//! `--print-default-config` writes; it must parse to [`Config::default`].
use std::{
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    time::Duration,
};

use ipnet::IpNet;
use serde::Deserialize;
use url::Url;

pub const TEMPLATE: &str = r#"# tcode-traverse configuration.
#
# Public name of this instance. The manifest advertises the relay as
# https://<hostname>/ and the pkarr store as https://<hostname>/pkarr; with
# tls.mode = "letsencrypt" it is also the certificate name.
hostname = "traverse.example.org"
# Account e-mail for Let's Encrypt (letsencrypt mode only).
contact = "admin@example.org"
# Certificates and the pkarr store live here.
data_dir = "/var/lib/tcode-traverse"
# Free-form region label shown in the manifest.
region = "eu-central"

[http]
# Plain HTTP listener: captive-portal probes and, with tls.mode = "off", every
# route including the relay.
bind = "[::]:80"
# Trust X-Forwarded-For for the per-IP pkarr limits. Enable only behind a
# reverse proxy that overwrites the header.
trust_forwarded_for = false

[tls]
# letsencrypt | manual | reloading | off
#   letsencrypt: ACME with TLS-ALPN-01 on the tls.bind port; needs hostname and contact.
#   manual:      cert and key are PEM files read once at start.
#   reloading:   like manual, re-read once a day.
#   off:         serve plain HTTP on http.bind (behind a TLS-terminating proxy, or for development).
mode = "letsencrypt"
bind = "[::]:443"
cert = ""
key = ""
# ACME directory URL; unset means Let's Encrypt production.
# acme_directory = "https://acme-staging-v02.api.letsencrypt.org/directory"
# Port in the advertised https://<hostname> URL; unset means the tls.bind
# port. Set 443 when a reverse proxy on 443 forwards TLS to tls.bind.
# public_port = 443

[relay]
# QUIC address discovery lets clients learn their public address. It needs
# TLS, so it is skipped with tls.mode = "off".
quic_addr_discovery = true
quic_bind = "[::]:7842"
# Per-connection limit on bytes received from a relay client; 0 disables.
rx_bytes_per_second = 2_000_000
rx_max_burst_bytes = 4_000_000

[pkarr]
# Per-IP token buckets for PUT and GET /pkarr/<key>.
put_per_second = 4
put_burst = 8
get_per_second = 20
get_burst = 40
# Records not refreshed for this long are removed (s, m, h or d).
eviction = "7d"

[lock]
# Region lock: clients with RTT <= home_rtt_max_ms or an address in
# allow_cidrs are always admitted; everyone else shares far_connection_quota
# concurrent connections. RTT and address come from X-TCP-RTT (microseconds,
# nginx: $tcpinfo_rtt) and X-Forwarded-For on the relay upgrade request, read
# only with trust_proxy_headers = true: enable it only behind a reverse proxy
# that overwrites both. Without it every client counts as far, so the lock
# needs a positive far_connection_quota.
enabled = false
trust_proxy_headers = false
home_rtt_max_ms = 80
far_connection_quota = 64
allow_cidrs = []

[metrics]
# Prometheus text endpoint on a private address; off when bind is absent.
bind = "127.0.0.1:9090"

# Other instances to list in the manifest.
# [[peers]]
# url = "https://relay-2.example.org/"
# quic_port = 7842
# region = "us-east"
"#;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub contact: Option<String>,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub http: HttpConfig,
    #[serde(default)]
    pub tls: TlsConfig,
    #[serde(default)]
    pub relay: RelayConfig,
    #[serde(default)]
    pub pkarr: PkarrConfig,
    #[serde(default)]
    pub lock: LockConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
    #[serde(default)]
    pub peers: Vec<Peer>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    #[serde(default = "default_http_bind")]
    pub bind: SocketAddr,
    #[serde(default)]
    pub trust_forwarded_for: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    LetsEncrypt,
    Manual,
    Reloading,
    Off,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    #[serde(default = "default_tls_mode")]
    pub mode: TlsMode,
    #[serde(default = "default_tls_bind")]
    pub bind: SocketAddr,
    #[serde(default)]
    pub cert: String,
    #[serde(default)]
    pub key: String,
    /// ACME directory other than Let's Encrypt production (staging, pebble).
    #[serde(default)]
    pub acme_directory: Option<String>,
    /// Port advertised in the manifest URLs when it differs from `bind`, for
    /// a TLS listener behind a proxy that forwards a public port to it.
    #[serde(default)]
    pub public_port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayConfig {
    #[serde(default = "default_true")]
    pub quic_addr_discovery: bool,
    #[serde(default = "default_quic_bind")]
    pub quic_bind: SocketAddr,
    #[serde(default = "default_rx_bytes_per_second")]
    pub rx_bytes_per_second: u32,
    #[serde(default = "default_rx_max_burst_bytes")]
    pub rx_max_burst_bytes: u32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PkarrConfig {
    #[serde(default = "default_put_per_second")]
    pub put_per_second: u32,
    #[serde(default = "default_put_burst")]
    pub put_burst: u32,
    #[serde(default = "default_get_per_second")]
    pub get_per_second: u32,
    #[serde(default = "default_get_burst")]
    pub get_burst: u32,
    #[serde(
        default = "default_eviction",
        deserialize_with = "deserialize_duration"
    )]
    pub eviction: Duration,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub trust_proxy_headers: bool,
    #[serde(default = "default_home_rtt_max_ms")]
    pub home_rtt_max_ms: u32,
    #[serde(default = "default_far_connection_quota")]
    pub far_connection_quota: usize,
    #[serde(default)]
    pub allow_cidrs: Vec<IpNet>,
}

/// Absent, or present without `bind`, means no metrics listener.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    #[serde(default)]
    pub bind: Option<SocketAddr>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    pub url: Url,
    #[serde(default)]
    pub quic_port: Option<u16>,
    #[serde(default)]
    pub region: Option<String>,
}

fn default_true() -> bool {
    true
}
fn default_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/tcode-traverse")
}
fn default_http_bind() -> SocketAddr {
    SocketAddr::from((Ipv6Addr::UNSPECIFIED, 80))
}
fn default_tls_mode() -> TlsMode {
    TlsMode::LetsEncrypt
}
fn default_tls_bind() -> SocketAddr {
    SocketAddr::from((Ipv6Addr::UNSPECIFIED, 443))
}
fn default_quic_bind() -> SocketAddr {
    SocketAddr::from((Ipv6Addr::UNSPECIFIED, 7842))
}
fn default_put_per_second() -> u32 {
    4
}
fn default_put_burst() -> u32 {
    8
}
fn default_get_per_second() -> u32 {
    20
}
fn default_get_burst() -> u32 {
    40
}
fn default_rx_bytes_per_second() -> u32 {
    2_000_000
}
fn default_rx_max_burst_bytes() -> u32 {
    4_000_000
}
fn default_eviction() -> Duration {
    Duration::from_secs(7 * 24 * 60 * 60)
}
fn default_home_rtt_max_ms() -> u32 {
    80
}
fn default_far_connection_quota() -> usize {
    64
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            bind: default_http_bind(),
            trust_forwarded_for: false,
        }
    }
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            mode: default_tls_mode(),
            bind: default_tls_bind(),
            cert: String::new(),
            key: String::new(),
            acme_directory: None,
            public_port: None,
        }
    }
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            quic_addr_discovery: true,
            quic_bind: default_quic_bind(),
            rx_bytes_per_second: default_rx_bytes_per_second(),
            rx_max_burst_bytes: default_rx_max_burst_bytes(),
        }
    }
}

impl Default for PkarrConfig {
    fn default() -> Self {
        Self {
            put_per_second: default_put_per_second(),
            put_burst: default_put_burst(),
            get_per_second: default_get_per_second(),
            get_burst: default_get_burst(),
            eviction: default_eviction(),
        }
    }
}

impl Default for LockConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            trust_proxy_headers: false,
            home_rtt_max_ms: default_home_rtt_max_ms(),
            far_connection_quota: default_far_connection_quota(),
            allow_cidrs: Vec::new(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hostname: Some("traverse.example.org".into()),
            contact: Some("admin@example.org".into()),
            data_dir: default_data_dir(),
            region: Some("eu-central".into()),
            http: HttpConfig::default(),
            tls: TlsConfig::default(),
            relay: RelayConfig::default(),
            pkarr: PkarrConfig::default(),
            lock: LockConfig::default(),
            metrics: MetricsConfig {
                bind: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 9090))),
            },
            peers: Vec::new(),
        }
    }
}

impl Config {
    pub fn parse(toml: &str) -> Result<Self, String> {
        let config: Self = toml::from_str(toml).map_err(|error| error.to_string())?;
        config.validate()?;
        Ok(config)
    }

    /// Development defaults: plain HTTP on loopback, no ACME, no QAD, metrics
    /// off, everything under `data_dir`.
    pub fn dev(data_dir: PathBuf, http_bind: SocketAddr) -> Self {
        Self {
            hostname: None,
            contact: None,
            data_dir,
            region: None,
            http: HttpConfig {
                bind: http_bind,
                trust_forwarded_for: false,
            },
            tls: TlsConfig {
                mode: TlsMode::Off,
                ..TlsConfig::default()
            },
            metrics: MetricsConfig::default(),
            ..Self::default()
        }
    }

    fn validate(&self) -> Result<(), String> {
        let hostname = self.hostname.as_deref().unwrap_or("");
        match self.tls.mode {
            TlsMode::LetsEncrypt => {
                if hostname.is_empty() {
                    return Err("tls.mode = \"letsencrypt\" needs hostname".into());
                }
                if self.contact.as_deref().unwrap_or("").is_empty() {
                    return Err("tls.mode = \"letsencrypt\" needs contact".into());
                }
            }
            TlsMode::Manual | TlsMode::Reloading => {
                if self.tls.cert.is_empty() || self.tls.key.is_empty() {
                    return Err(format!(
                        "tls.mode = \"{}\" needs tls.cert and tls.key",
                        match self.tls.mode {
                            TlsMode::Manual => "manual",
                            _ => "reloading",
                        }
                    ));
                }
            }
            TlsMode::Off => {}
        }
        if self.pkarr.put_per_second == 0 || self.pkarr.put_burst == 0 {
            return Err("pkarr.put_per_second and pkarr.put_burst must be positive".into());
        }
        if self.pkarr.get_per_second == 0 || self.pkarr.get_burst == 0 {
            return Err("pkarr.get_per_second and pkarr.get_burst must be positive".into());
        }
        if self.pkarr.eviction.is_zero() {
            return Err("pkarr.eviction must be positive".into());
        }
        if self.relay.rx_bytes_per_second == 0 && self.relay.rx_max_burst_bytes != 0 {
            return Err("relay.rx_max_burst_bytes needs relay.rx_bytes_per_second".into());
        }
        if self.lock.enabled
            && !self.lock.trust_proxy_headers
            && self.lock.far_connection_quota == 0
        {
            return Err("lock.enabled without lock.trust_proxy_headers treats every client as far, so lock.far_connection_quota must be positive or nothing can connect".into());
        }
        Ok(())
    }

    /// Whether the relay listener itself terminates TLS.
    pub fn tls_enabled(&self) -> bool {
        self.tls.mode != TlsMode::Off
    }

    pub fn quic_enabled(&self) -> bool {
        self.tls_enabled() && self.relay.quic_addr_discovery
    }
}

/// `"7d"`, `"12h"`, `"30m"`, `"90s"`; a bare number is seconds.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let (digits, unit) = match text.find(|c: char| !c.is_ascii_digit()) {
        Some(index) => text.split_at(index),
        None => (text, "s"),
    };
    let value: u64 = digits
        .parse()
        .map_err(|_| format!("invalid duration {text:?}"))?;
    let seconds = match unit.trim() {
        "s" => value,
        "m" => value * 60,
        "h" => value * 60 * 60,
        "d" => value * 24 * 60 * 60,
        _ => return Err(format!("invalid duration {text:?}: use s, m, h or d")),
    };
    Ok(Duration::from_secs(seconds))
}

fn deserialize_duration<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Duration, D::Error> {
    let text = String::deserialize(deserializer)?;
    parse_duration(&text).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_to_the_defaults() {
        assert_eq!(Config::parse(TEMPLATE).unwrap(), Config::default());
    }

    #[test]
    fn durations_and_validation() {
        assert_eq!(parse_duration("7d").unwrap(), Duration::from_secs(604_800));
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert!(parse_duration("7w").is_err());
        assert!(parse_duration("").is_err());

        let error = Config::parse("[tls]\nmode = \"manual\"").unwrap_err();
        assert!(error.contains("tls.cert"), "{error}");
        let error = Config::parse("[tls]\nmode = \"letsencrypt\"").unwrap_err();
        assert!(error.contains("hostname"), "{error}");
        let error = Config::parse("[pkarr]\nput_burst = 0\n[tls]\nmode = \"off\"").unwrap_err();
        assert!(error.contains("put_burst"), "{error}");
        assert!(Config::parse("unknown = 1\n[tls]\nmode = \"off\"").is_err());
        let error = Config::parse(
            "[tls]\nmode = \"off\"\n[lock]\nenabled = true\nfar_connection_quota = 0",
        )
        .unwrap_err();
        assert!(error.contains("trust_proxy_headers"), "{error}");
        assert!(
            Config::parse(
                "[tls]\nmode = \"off\"\n[lock]\nenabled = true\ntrust_proxy_headers = true\nfar_connection_quota = 0",
            )
            .is_ok()
        );

        let config = Config::parse(
            "[tls]\nmode = \"off\"\n[lock]\nenabled = true\nallow_cidrs = [\"10.0.0.0/8\"]\n[metrics]\n[[peers]]\nurl = \"https://relay-2.example.org/\"\nquic_port = 0",
        )
        .unwrap();
        assert!(config.lock.enabled);
        assert_eq!(config.lock.allow_cidrs, ["10.0.0.0/8".parse().unwrap()]);
        assert_eq!(config.metrics.bind, None);
        assert_eq!(config.peers[0].quic_port, Some(0));
        assert!(!config.quic_enabled());
    }
}
