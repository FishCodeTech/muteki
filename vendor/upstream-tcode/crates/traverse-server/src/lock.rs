//! Region lock: an [`AccessControl`] that keeps a relay for the clients it
//! is close to. iroh-relay hands the hook only the upgrade request, not the
//! socket, so the lock reads what a reverse proxy in front of it adds:
//! `X-TCP-RTT` (microseconds, nginx `$tcpinfo_rtt`) and `X-Forwarded-For`.
//! Those headers are read only when the config trusts the proxy; a client
//! can send them itself. Otherwise every client looks far away and shares
//! the quota.
use std::{collections::HashSet, net::IpAddr, sync::Mutex, time::Duration};

use http::HeaderMap;
use ipnet::IpNet;
use iroh_base::EndpointId;
use iroh_relay::server::{Access, AccessControl, ClientRequest, ConnectionId};

use crate::config::LockConfig;

#[derive(Debug)]
pub struct RegionLock {
    trust_proxy_headers: bool,
    home_rtt_max: Duration,
    far_connection_quota: usize,
    allow_cidrs: Vec<IpNet>,
    far: Mutex<HashSet<ConnectionId>>,
}

impl RegionLock {
    pub fn new(config: &LockConfig) -> Self {
        Self {
            trust_proxy_headers: config.trust_proxy_headers,
            home_rtt_max: Duration::from_millis(u64::from(config.home_rtt_max_ms)),
            far_connection_quota: config.far_connection_quota,
            allow_cidrs: config.allow_cidrs.clone(),
            far: Mutex::new(HashSet::new()),
        }
    }
}

fn rtt(headers: &HeaderMap) -> Option<Duration> {
    let micros: u64 = headers
        .get("x-tcp-rtt")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_micros(micros))
}

fn forwarded_ip(headers: &HeaderMap) -> Option<IpAddr> {
    headers
        .get("x-forwarded-for")?
        .to_str()
        .ok()?
        .split(',')
        .next()?
        .trim()
        .parse()
        .ok()
}

impl AccessControl for RegionLock {
    async fn on_connect(&self, request: &ClientRequest) -> Access {
        if self.trust_proxy_headers {
            let headers = request.headers();
            if rtt(headers).is_some_and(|rtt| rtt <= self.home_rtt_max) {
                return Access::Allow;
            }
            if let Some(ip) = forwarded_ip(headers)
                && self.allow_cidrs.iter().any(|net| net.contains(&ip))
            {
                return Access::Allow;
            }
        }
        let mut far = self.far.lock().expect("lock");
        if far.len() < self.far_connection_quota {
            far.insert(request.connection_id());
            Access::Allow
        } else {
            Access::Deny {
                reason: Some(
                    "this relay is region-locked and its quota for far clients is in use".into(),
                ),
            }
        }
    }

    fn on_disconnect(&self, _endpoint_id: EndpointId, connection_id: ConnectionId) {
        self.far.lock().expect("lock").remove(&connection_id);
    }
}

#[cfg(test)]
mod tests {
    use iroh_base::SecretKey;
    use iroh_relay::http::ProtocolVersion;

    use super::*;

    fn request(headers: &[(&str, &str)]) -> ClientRequest {
        let mut builder = http::Request::builder().uri("/relay");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let (parts, ()) = builder.body(()).unwrap().into_parts();
        ClientRequest::new(SecretKey::generate().public(), ProtocolVersion::V2, parts)
    }

    fn lock(quota: usize) -> RegionLock {
        RegionLock::new(&LockConfig {
            enabled: true,
            trust_proxy_headers: true,
            home_rtt_max_ms: 80,
            far_connection_quota: quota,
            allow_cidrs: vec!["10.0.0.0/8".parse().unwrap()],
        })
    }

    #[tokio::test]
    async fn near_clients_and_allowed_networks_bypass_the_quota() {
        let lock = lock(0);
        assert_eq!(
            lock.on_connect(&request(&[("x-tcp-rtt", "80000")])).await,
            Access::Allow
        );
        assert_eq!(
            lock.on_connect(&request(&[("x-forwarded-for", "10.1.2.3, 203.0.113.1")]))
                .await,
            Access::Allow
        );
        assert!(matches!(
            lock.on_connect(&request(&[("x-tcp-rtt", "80001")])).await,
            Access::Deny { .. }
        ));
        assert!(matches!(
            lock.on_connect(&request(&[("x-forwarded-for", "203.0.113.1")]))
                .await,
            Access::Deny { .. }
        ));
        // No proxy headers at all: the client counts as far.
        assert!(matches!(
            lock.on_connect(&request(&[])).await,
            Access::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn untrusted_headers_never_bypass_the_quota() {
        let lock = RegionLock::new(&LockConfig {
            enabled: true,
            trust_proxy_headers: false,
            home_rtt_max_ms: 80,
            far_connection_quota: 1,
            allow_cidrs: vec!["10.0.0.0/8".parse().unwrap()],
        });
        let near = request(&[("x-tcp-rtt", "1000"), ("x-forwarded-for", "10.1.2.3")]);
        assert_eq!(lock.on_connect(&near).await, Access::Allow);
        assert!(matches!(
            lock.on_connect(&request(&[("x-tcp-rtt", "1000")])).await,
            Access::Deny { .. }
        ));
    }

    #[tokio::test]
    async fn far_clients_share_a_quota_released_on_disconnect() {
        let lock = lock(2);
        let first = request(&[("x-tcp-rtt", "250000")]);
        let second = request(&[("x-tcp-rtt", "250000")]);
        let third = request(&[]);
        assert_eq!(lock.on_connect(&first).await, Access::Allow);
        assert_eq!(lock.on_connect(&second).await, Access::Allow);
        let denied = lock.on_connect(&third).await;
        assert!(
            matches!(&denied, Access::Deny { reason: Some(reason) } if reason.contains("region-locked"))
        );
        lock.on_disconnect(first.endpoint_id(), first.connection_id());
        assert_eq!(lock.on_connect(&third).await, Access::Allow);
        // A near client's disconnect never touches the quota.
        let near = request(&[("x-tcp-rtt", "1000")]);
        assert_eq!(lock.on_connect(&near).await, Access::Allow);
        lock.on_disconnect(near.endpoint_id(), near.connection_id());
        assert!(matches!(
            lock.on_connect(&request(&[])).await,
            Access::Deny { .. }
        ));
    }
}
