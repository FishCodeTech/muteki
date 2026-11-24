//! Dial-time SSRF protection: resolves DNS beforehand, rejects private/loopback/link-local
//! ranges, disables HTTP redirects, and pins verified public IP addresses into reqwest.

use crate::runs::src_api::{Api, Error};
use axum::http::StatusCode;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

pub(crate) fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_forbidden_v4(v4),
        IpAddr::V6(v6) => is_forbidden_v6(v6),
    }
}

fn is_forbidden_v4(v4: Ipv4Addr) -> bool {
    let octets = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_unspecified()
        || v4.is_multicast()
        || (octets[0] == 100 && (octets[1] & 0b1100_0000) == 64)
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        || (octets[0] == 198 && (octets[1] == 18 || octets[1] == 19))
        || octets[0] >= 240
}

fn is_forbidden_v6(v6: Ipv6Addr) -> bool {
    if let Some(mapped) = v6.to_ipv4_mapped() {
        return is_forbidden_v4(mapped);
    }
    let segs = v6.segments();
    v6.is_loopback()
        || v6.is_unspecified()
        || v6.is_multicast()
        || (segs[0] & 0xfe00) == 0xfc00
        || (segs[0] & 0xffc0) == 0xfe80
}

pub(crate) async fn resolve_public_addrs(
    host: &str,
    port: u16,
    allow_private: bool,
) -> Api<Vec<SocketAddr>> {
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|err| {
            Error::new(
                StatusCode::BAD_GATEWAY,
                "ssrf_dns_failed",
                format!("DNS lookup failed for {host}: {err}"),
            )
        })?
        .collect::<Vec<SocketAddr>>();

    if addrs.is_empty() {
        return Err(Error::new(
            StatusCode::BAD_GATEWAY,
            "ssrf_dns_empty",
            format!("DNS lookup returned no addresses for {host}"),
        ));
    }

    if !allow_private {
        for addr in &addrs {
            if is_forbidden_ip(addr.ip()) {
                return Err(Error::new(
                    StatusCode::FORBIDDEN,
                    "ssrf_blocked_private_ip",
                    format!(
                        "Dial-time SSRF guard blocked private/loopback address {} resolved for {host}",
                        addr.ip()
                    ),
                ));
            }
        }
    }

    Ok(addrs)
}

pub(crate) async fn pinned_public_tcp(
    host: &str,
    port: u16,
    allow_private: bool,
) -> Api<(SocketAddr, tokio::net::TcpStream)> {
    let addrs = resolve_public_addrs(host, port, allow_private).await?;
    let mut last_err = None;
    for addr in addrs {
        match tokio::time::timeout(Duration::from_secs(8), tokio::net::TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => return Ok((addr, stream)),
            Ok(Err(err)) => last_err = Some(err.to_string()),
            Err(_) => last_err = Some("TCP connect timed out".to_owned()),
        }
    }
    Err(Error::new(
        StatusCode::BAD_GATEWAY,
        "tcp_connect_failed",
        last_err.unwrap_or_else(|| "Unable to connect to verified address".to_owned()),
    ))
}

pub(crate) async fn pinned_public_client(
    raw_url: &str,
    allow_private: bool,
) -> Api<(reqwest::Url, reqwest::Client)> {
    let url = reqwest::Url::parse(raw_url.trim())
        .map_err(|_| Error::invalid("notification URL is invalid"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::invalid("notification URL must use http or https"));
    }
    let host = url
        .host_str()
        .ok_or_else(|| Error::invalid("notification URL host is missing"))?
        .to_owned();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| Error::invalid("notification URL port is missing"))?;

    let addrs = resolve_public_addrs(&host, port, allow_private).await?;

    let client = crate::network_proxy::direct_client_builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(&host, &addrs)
        .build()
        .map_err(|err| {
            Error::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "http_client_error",
                err.to_string(),
            )
        })?;

    Ok((url, client))
}
