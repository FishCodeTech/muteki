//! A running instance: the plain and TLS listeners with the relay, pkarr
//! store and manifest, the QUIC address-discovery socket, the metrics
//! listener and the eviction timer.
use std::{
    io,
    net::SocketAddr,
    num::NonZeroU32,
    sync::Arc,
    time::{Duration, SystemTime},
};

use chrono::{DateTime, SecondsFormat, Utc};
use http::HeaderMap;
use iroh_metrics::{Registry, service::MetricsServer};
use iroh_relay::{
    KeyCache,
    defaults::DEFAULT_KEY_CACHE_CAPACITY,
    server::{
        AllowAll, ClientRateLimit, DynAccessControl, Metrics, QuicConfig, RelayService,
        ServerConfig,
    },
};
use tokio::{net::TcpListener, task::JoinSet};
use url::Url;

use crate::{
    config::Config,
    http::{Dispatch, base_router, serve_connection},
    lock::RegionLock,
    manifest,
    pkarr::{self, PkarrMetrics, PkarrService, RateLimiter, Store},
    tls::Tls,
};

/// Stale records are swept this often, or every `pkarr.eviction` if shorter.
const EVICTION_INTERVAL: Duration = Duration::from_secs(60 * 60);

pub struct Server {
    base: Url,
    http_addr: SocketAddr,
    https_addr: Option<SocketAddr>,
    quic_addr: Option<SocketAddr>,
    relay: RelayService,
    qad: Option<iroh_relay::server::Server>,
    metrics_server: Option<MetricsServer>,
    tasks: JoinSet<()>,
}

pub fn rfc3339(time: SystemTime) -> String {
    DateTime::<Utc>::from(time).to_rfc3339_opts(SecondsFormat::Secs, true)
}

impl Server {
    /// Binds every listener. `updated_at` is the manifest's `updatedAt`:
    /// the config file's modification time, or the process start.
    pub async fn spawn(config: Config, updated_at: SystemTime) -> io::Result<Self> {
        Self::spawn_with_clock(config, updated_at, pkarr::system_clock()).await
    }

    /// [`Self::spawn`] with the clock the pkarr rate limiters read. Tests
    /// freeze it so their request sequence, not the host's disk speed,
    /// decides which request is refused.
    pub async fn spawn_with_clock(
        config: Config,
        updated_at: SystemTime,
        clock: pkarr::Clock,
    ) -> io::Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        std::fs::create_dir_all(&config.data_dir)?;
        let tls = Tls::load(&config).await?.map(Arc::new);

        let http_listener = TcpListener::bind(config.http.bind).await?;
        let http_addr = http_listener.local_addr()?;
        let https_listener = match &tls {
            Some(_) => Some(TcpListener::bind(config.tls.bind).await?),
            None => None,
        };
        let https_addr = match &https_listener {
            Some(listener) => Some(listener.local_addr()?),
            None => None,
        };

        let base = manifest::public_base(&config, http_addr);
        let manifest = manifest::compose(&config, &base, rfc3339(updated_at));
        let manifest_json = Arc::new(serde_json::to_string_pretty(&manifest)?);

        let relay_metrics = Arc::new(Metrics::default());
        let access: Arc<dyn DynAccessControl> = if config.lock.enabled {
            Arc::new(RegionLock::new(&config.lock))
        } else {
            Arc::new(AllowAll)
        };
        let rate_limit = NonZeroU32::new(config.relay.rx_bytes_per_second).map(|bps| {
            let mut limit = ClientRateLimit::new(bps);
            limit.max_burst_bytes = NonZeroU32::new(config.relay.rx_max_burst_bytes);
            limit
        });
        let mut headers = HeaderMap::new();
        if tls.is_some() {
            headers.insert(
                "strict-transport-security",
                "max-age=63072000; includeSubDomains"
                    .parse()
                    .expect("static header"),
            );
        }
        let relay = RelayService::new(
            iroh_relay::server::Handlers::default(),
            headers,
            rate_limit,
            KeyCache::new(DEFAULT_KEY_CACHE_CAPACITY),
            access,
            relay_metrics.clone(),
        );

        let pkarr_metrics = Arc::new(PkarrMetrics::default());
        let pkarr = Arc::new(PkarrService {
            store: Store::open(&config.data_dir.join("pkarr.redb"))?,
            put_limiter: RateLimiter::new(
                config.pkarr.put_per_second,
                config.pkarr.put_burst,
                clock.clone(),
            ),
            get_limiter: RateLimiter::new(
                config.pkarr.get_per_second,
                config.pkarr.get_burst,
                clock,
            ),
            trust_forwarded_for: config.http.trust_forwarded_for,
            metrics: pkarr_metrics.clone(),
        });
        let router = base_router(manifest_json).merge(pkarr::router(pkarr.clone()));
        let dispatch = Dispatch::new(relay.clone(), router);

        let mut tasks = JoinSet::new();
        match https_listener {
            Some(listener) => {
                tasks.spawn(accept_loop(listener, tls.clone(), dispatch.clone()));
                // Plain HTTP beside TLS: the same routes, no relay.
                tasks.spawn(accept_loop(http_listener, None, dispatch.without_relay()));
            }
            None => {
                tasks.spawn(accept_loop(http_listener, None, dispatch));
            }
        }

        let qad = match (&tls, config.quic_enabled()) {
            (Some(tls), true) => {
                let mut quic = QuicConfig::new(config.relay.quic_bind);
                quic.server_config = Some((*tls.server_config).clone());
                let mut server_config = ServerConfig::default();
                server_config.quic = Some(quic);
                Some(
                    iroh_relay::server::Server::spawn(server_config)
                        .await
                        .map_err(io::Error::other)?,
                )
            }
            _ => None,
        };
        let quic_addr = qad.as_ref().and_then(|qad| qad.quic_addr());

        let metrics_server = match config.metrics.bind {
            Some(addr) => {
                let mut registry = Registry::default();
                registry.register(relay_metrics);
                registry.register(pkarr_metrics);
                if let Some(qad) = &qad {
                    registry
                        .sub_registry_with_label("component", "qad")
                        .register(qad.metrics().server.clone());
                }
                Some(MetricsServer::spawn(addr, Arc::new(registry)).await?)
            }
            None => None,
        };

        let eviction = config.pkarr.eviction;
        tasks.spawn(async move {
            let mut interval = tokio::time::interval(EVICTION_INTERVAL.min(eviction));
            loop {
                interval.tick().await;
                let service = pkarr.clone();
                let swept = tokio::task::spawn_blocking(move || {
                    service.store.evict(pkarr::unix_now(), eviction)
                })
                .await;
                match swept {
                    Ok(Ok(removed)) => {
                        pkarr.metrics.evicted.inc_by(removed as u64);
                        if removed > 0 {
                            tracing::info!("evicted {removed} stale pkarr records");
                        }
                    }
                    Ok(Err(error)) => tracing::error!("pkarr eviction failed: {error}"),
                    Err(_) => break,
                }
            }
        });

        tracing::info!(
            "traverse: {base} (http {http_addr}{}{}{})",
            https_addr.map_or(String::new(), |addr| format!(", https {addr}")),
            quic_addr.map_or(String::new(), |addr| format!(", qad {addr}")),
            metrics_server
                .as_ref()
                .map_or(String::new(), |server| format!(
                    ", metrics {}",
                    server.local_addr()
                )),
        );
        Ok(Self {
            base,
            http_addr,
            https_addr,
            quic_addr,
            relay,
            qad,
            metrics_server,
            tasks,
        })
    }

    /// The URL the manifest advertises for this instance.
    pub fn base(&self) -> &Url {
        &self.base
    }

    pub fn http_addr(&self) -> SocketAddr {
        self.http_addr
    }

    pub fn https_addr(&self) -> Option<SocketAddr> {
        self.https_addr
    }

    pub fn quic_addr(&self) -> Option<SocketAddr> {
        self.quic_addr
    }

    /// Runs until a listener task fails.
    pub async fn join(&mut self) {
        while let Some(result) = self.tasks.join_next().await {
            if let Err(error) = result
                && error.is_panic()
            {
                tracing::error!("server task panicked: {error}");
                return;
            }
        }
    }

    pub async fn shutdown(mut self) {
        self.tasks.abort_all();
        self.relay.shutdown().await;
        if let Some(qad) = self.qad.take() {
            let _ = qad.shutdown().await;
        }
        if let Some(server) = self.metrics_server.take() {
            server.shutdown().await;
        }
    }
}

async fn accept_loop(listener: TcpListener, tls: Option<Arc<Tls>>, dispatch: Dispatch) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                tokio::spawn(serve_connection(
                    stream,
                    peer,
                    tls.clone(),
                    dispatch.clone(),
                ));
            }
            Err(error) => {
                tracing::warn!("accept failed: {error}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}
