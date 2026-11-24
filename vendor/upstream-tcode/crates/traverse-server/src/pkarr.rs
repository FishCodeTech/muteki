//! The pkarr store: `PUT`/`GET /pkarr/<z32 endpoint id>` with signed packets
//! as `iroh::address_lookup::pkarr` sends them (relay payload = signature,
//! timestamp, DNS packet), kept in one redb file and dropped when they are
//! not refreshed for `pkarr.eviction`.
use std::{
    collections::HashMap,
    io,
    net::{IpAddr, SocketAddr},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Extension, Path as UrlPath, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use iroh_base::PublicKey;
use iroh_dns::pkarr::SignedPacket;
use iroh_metrics::{Counter, MetricsGroup};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

/// The relay payload is the signed packet without its 32-byte public key.
pub const MAX_PAYLOAD_BYTES: usize = SignedPacket::MAX_BYTES - 32;
/// Value layout: 8 bytes big-endian refresh time (unix seconds) + payload.
const RECORDS: TableDefinition<&[u8; 32], &[u8]> = TableDefinition::new("records");
const CACHE_CONTROL: &str = "public, max-age=300";

/// The socket peer of the request, set by the connection handler.
#[derive(Debug, Clone, Copy)]
pub struct PeerAddr(pub SocketAddr);

#[derive(Debug, Default, MetricsGroup)]
#[metrics(name = "pkarr")]
pub struct PkarrMetrics {
    #[metrics(help = "Packets stored")]
    pub puts: Counter,
    #[metrics(help = "PUTs rejected as invalid, stale or oversized")]
    pub put_rejected: Counter,
    #[metrics(help = "PUTs refused by the per-IP limit")]
    pub put_rate_limited: Counter,
    #[metrics(help = "Packets served")]
    pub gets: Counter,
    #[metrics(help = "GETs for unknown keys")]
    pub get_missing: Counter,
    #[metrics(help = "GETs refused by the per-IP limit")]
    pub get_rate_limited: Counter,
    #[metrics(help = "Records removed by eviction")]
    pub evicted: Counter,
}

#[derive(Debug)]
pub enum PutError {
    Stale,
    Io(io::Error),
}

impl From<io::Error> for PutError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn io_error(error: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::other(error)
}

pub struct Store {
    db: Database,
}

impl Store {
    pub fn open(path: &Path) -> io::Result<Self> {
        let db = Database::create(path).map_err(io_error)?;
        let txn = db.begin_write().map_err(io_error)?;
        txn.open_table(RECORDS).map_err(io_error)?;
        txn.commit().map_err(io_error)?;
        Ok(Self { db })
    }

    /// Stores `packet` unless the stored one is at least as recent; `now` is
    /// the refresh time eviction counts from.
    pub fn put(&self, packet: &SignedPacket, now: u64) -> Result<(), PutError> {
        let key = packet.public_key();
        let txn = self.db.begin_write().map_err(io_error)?;
        {
            let mut table = txn.open_table(RECORDS).map_err(io_error)?;
            if let Some(existing) = table.get(key.as_bytes()).map_err(io_error)? {
                let existing = SignedPacket::from_relay_payload(&key, &existing.value()[8..])
                    .map_err(io_error)?;
                if !packet.more_recent_than(&existing) {
                    return Err(PutError::Stale);
                }
            }
            let mut value = now.to_be_bytes().to_vec();
            value.extend(packet.to_relay_payload());
            table
                .insert(key.as_bytes(), value.as_slice())
                .map_err(io_error)?;
        }
        txn.commit().map_err(io_error)?;
        Ok(())
    }

    /// The stored relay payload for `key`.
    pub fn get(&self, key: &PublicKey) -> io::Result<Option<Vec<u8>>> {
        let txn = self.db.begin_read().map_err(io_error)?;
        let table = txn.open_table(RECORDS).map_err(io_error)?;
        Ok(table
            .get(key.as_bytes())
            .map_err(io_error)?
            .map(|value| value.value()[8..].to_vec()))
    }

    /// Removes records last refreshed before `now - max_age`; returns how many.
    pub fn evict(&self, now: u64, max_age: Duration) -> io::Result<usize> {
        let cutoff = now.saturating_sub(max_age.as_secs());
        let mut removed = 0;
        let txn = self.db.begin_write().map_err(io_error)?;
        {
            let mut table = txn.open_table(RECORDS).map_err(io_error)?;
            table
                .retain(|_, value| {
                    let refreshed = u64::from_be_bytes(value[..8].try_into().expect("8 bytes"));
                    let keep = refreshed >= cutoff;
                    if !keep {
                        removed += 1;
                    }
                    keep
                })
                .map_err(io_error)?;
        }
        txn.commit().map_err(io_error)?;
        Ok(removed)
    }
}

/// The limiter's source of time. Production reads `Instant::now`; a test
/// freezes it so a slow disk between two requests cannot refill a bucket
/// the test expects to be empty.
pub type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(Instant::now)
}

/// A token bucket per client address: `burst` tokens, refilled at
/// `per_second`. Idle buckets are dropped once the table grows past
/// `PRUNE_ABOVE` entries.
pub struct RateLimiter {
    per_second: f64,
    burst: f64,
    clock: Clock,
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
}

struct Bucket {
    tokens: f64,
    updated: Instant,
}

const PRUNE_ABOVE: usize = 4096;

impl RateLimiter {
    pub fn new(per_second: u32, burst: u32, clock: Clock) -> Self {
        Self {
            per_second: f64::from(per_second),
            burst: f64::from(burst),
            clock,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    pub fn allow(&self, ip: IpAddr) -> bool {
        self.allow_at(ip, (self.clock)())
    }

    pub fn allow_at(&self, ip: IpAddr, now: Instant) -> bool {
        let mut buckets = self.buckets.lock().expect("rate limiter lock");
        if buckets.len() > PRUNE_ABOVE {
            let (per_second, burst) = (self.per_second, self.burst);
            buckets.retain(|_, bucket| {
                bucket.tokens + now.duration_since(bucket.updated).as_secs_f64() * per_second
                    < burst
            });
        }
        let bucket = buckets.entry(ip).or_insert(Bucket {
            tokens: self.burst,
            updated: now,
        });
        let refill = now.duration_since(bucket.updated).as_secs_f64() * self.per_second;
        bucket.tokens = (bucket.tokens + refill).min(self.burst);
        bucket.updated = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

pub struct PkarrService {
    pub store: Store,
    pub put_limiter: RateLimiter,
    pub get_limiter: RateLimiter,
    pub trust_forwarded_for: bool,
    pub metrics: Arc<PkarrMetrics>,
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// The first `X-Forwarded-For` address when the proxy is trusted, else the
/// socket peer.
pub fn client_ip(headers: &HeaderMap, peer: SocketAddr, trust_forwarded_for: bool) -> IpAddr {
    if trust_forwarded_for
        && let Some(forwarded) = headers.get("x-forwarded-for")
        && let Ok(value) = forwarded.to_str()
        && let Some(first) = value.split(',').next()
        && let Ok(ip) = first.trim().parse::<IpAddr>()
    {
        return ip;
    }
    peer.ip()
}

/// `/pkarr/{key}` and `/{key}` (the pkarr relay specification's root form).
pub fn router(service: Arc<PkarrService>) -> Router {
    Router::new()
        .route(
            "/pkarr/{key}",
            get(get_packet).put(put_packet).options(preflight),
        )
        .route("/{key}", get(get_packet).put(put_packet).options(preflight))
        .layer(DefaultBodyLimit::max(MAX_PAYLOAD_BYTES))
        .with_state(service)
}

fn cors(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, PUT, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("*"),
    );
    response
}

async fn preflight() -> Response {
    cors(StatusCode::NO_CONTENT.into_response())
}

fn parse_key(key: &str) -> Option<PublicKey> {
    PublicKey::from_z32(key).ok()
}

fn rate_limited(what: &'static str) -> Response {
    cors(
        (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "1")],
            what,
        )
            .into_response(),
    )
}

async fn get_packet(
    State(service): State<Arc<PkarrService>>,
    UrlPath(key): UrlPath<String>,
    Extension(PeerAddr(peer)): Extension<PeerAddr>,
    headers: HeaderMap,
) -> Response {
    let ip = client_ip(&headers, peer, service.trust_forwarded_for);
    if !service.get_limiter.allow(ip) {
        service.metrics.get_rate_limited.inc();
        return rate_limited("pkarr GET rate limit");
    }
    let Some(key) = parse_key(&key) else {
        return cors((StatusCode::BAD_REQUEST, "invalid pkarr key").into_response());
    };
    let store_service = service.clone();
    let result = tokio::task::spawn_blocking(move || store_service.store.get(&key)).await;
    match result {
        Ok(Ok(Some(payload))) => {
            service.metrics.gets.inc();
            cors(
                (
                    StatusCode::OK,
                    [
                        (header::CONTENT_TYPE, "application/octet-stream"),
                        (header::CACHE_CONTROL, CACHE_CONTROL),
                    ],
                    payload,
                )
                    .into_response(),
            )
        }
        Ok(Ok(None)) => {
            service.metrics.get_missing.inc();
            cors(StatusCode::NOT_FOUND.into_response())
        }
        Ok(Err(error)) => {
            tracing::error!("pkarr get failed: {error}");
            cors(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(_) => cors(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    }
}

async fn put_packet(
    State(service): State<Arc<PkarrService>>,
    UrlPath(key): UrlPath<String>,
    Extension(PeerAddr(peer)): Extension<PeerAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let ip = client_ip(&headers, peer, service.trust_forwarded_for);
    if !service.put_limiter.allow(ip) {
        service.metrics.put_rate_limited.inc();
        return rate_limited("pkarr PUT rate limit");
    }
    let Some(key) = parse_key(&key) else {
        service.metrics.put_rejected.inc();
        return cors((StatusCode::BAD_REQUEST, "invalid pkarr key").into_response());
    };
    if body.len() > MAX_PAYLOAD_BYTES {
        service.metrics.put_rejected.inc();
        return cors(StatusCode::PAYLOAD_TOO_LARGE.into_response());
    }
    let packet = match SignedPacket::from_relay_payload(&key, &body) {
        Ok(packet) => packet,
        Err(error) => {
            service.metrics.put_rejected.inc();
            return cors((StatusCode::BAD_REQUEST, error.to_string()).into_response());
        }
    };
    let store_service = service.clone();
    let result =
        tokio::task::spawn_blocking(move || store_service.store.put(&packet, unix_now())).await;
    match result {
        Ok(Ok(())) => {
            service.metrics.puts.inc();
            cors(StatusCode::NO_CONTENT.into_response())
        }
        Ok(Err(PutError::Stale)) => {
            service.metrics.put_rejected.inc();
            cors((StatusCode::CONFLICT, "a newer packet is stored").into_response())
        }
        Ok(Err(PutError::Io(error))) => {
            tracing::error!("pkarr put failed: {error}");
            cors(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(_) => cors(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    }
}

#[cfg(test)]
mod tests {
    use iroh_base::SecretKey;

    use super::*;

    fn packet(secret: &SecretKey, value: &str) -> SignedPacket {
        SignedPacket::from_txt_strings(secret, "_iroh", [value], 30).unwrap()
    }

    fn temp_store(name: &str) -> (Store, std::path::PathBuf) {
        let path =
            std::env::temp_dir().join(format!("tcode-traverse-{name}-{}.redb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        (Store::open(&path).unwrap(), path)
    }

    #[test]
    fn eviction_drops_records_not_refreshed_within_max_age() {
        let (store, path) = temp_store("evict");
        let stale = SecretKey::generate();
        let fresh = SecretKey::generate();
        store.put(&packet(&stale, "a"), 1_000).unwrap();
        store.put(&packet(&fresh, "b"), 5_000).unwrap();
        assert_eq!(store.evict(6_000, Duration::from_secs(2_000)).unwrap(), 1);
        assert!(store.get(&stale.public()).unwrap().is_none());
        assert!(store.get(&fresh.public()).unwrap().is_some());
        // A refresh with a newer packet resets the clock.
        store.put(&packet(&fresh, "c"), 9_000).unwrap();
        assert_eq!(store.evict(10_000, Duration::from_secs(2_000)).unwrap(), 0);
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rate_limit_is_a_per_ip_token_bucket() {
        let limiter = RateLimiter::new(4, 8, system_clock());
        let start = Instant::now();
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b: IpAddr = "10.0.0.2".parse().unwrap();
        for _ in 0..8 {
            assert!(limiter.allow_at(a, start));
        }
        assert!(!limiter.allow_at(a, start));
        assert!(limiter.allow_at(b, start));
        // Half a second refills two tokens.
        let later = start + Duration::from_millis(500);
        assert!(limiter.allow_at(a, later));
        assert!(limiter.allow_at(a, later));
        assert!(!limiter.allow_at(a, later));
    }

    #[test]
    fn forwarded_for_is_only_used_when_trusted() {
        let peer: SocketAddr = "192.0.2.1:5000".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.9, 10.0.0.1".parse().unwrap());
        assert_eq!(client_ip(&headers, peer, false), peer.ip());
        assert_eq!(
            client_ip(&headers, peer, true),
            "203.0.113.9".parse::<IpAddr>().unwrap()
        );
        headers.insert("x-forwarded-for", "garbage".parse().unwrap());
        assert_eq!(client_ip(&headers, peer, true), peer.ip());
    }
}
