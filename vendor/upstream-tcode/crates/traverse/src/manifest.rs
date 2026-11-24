//! A Traverse instance describes itself with a manifest:
//!
//! ```json
//! { "version": 1,
//!   "updatedAt": "2026-09-21T00:00:00Z",
//!   "relays": [ { "url": "https://relay.example/", "quic_port": 7842, "region": "eu" } ],
//!   "pkarr":  [ "https://traverse.example/pkarr/" ] }
//! ```
//!
//! A relay without `quic_port` uses iroh's default QUIC address-discovery
//! port; `"quic_port": 0` marks a relay that only forwards (no UDP). Lookup
//! is pkarr over HTTPS only: a machine publishes to and a device resolves
//! through every `pkarr` URL.
//!
//! The official manifest is `traverse_manifest.json` next to this file,
//! bundled into every build and re-fetched from [`OFFICIAL_MANIFEST_URL`]; a
//! self-hosted instance serves `<base>/relays.json`. Both go through one
//! [`ManifestLoader`]: preference is remote, then the disk cache, then the
//! bundle — except that a bundle whose `updatedAt` is newer than the cache
//! outranks it — and invalid data never replaces a usable copy. A self-hosted
//! instance has no bundle: with no cache and no fetch there is no manifest,
//! never the official one, and its endpoint runs without relay or lookup
//! until a refresh brings it.
use std::{
    fs,
    io::{self, Read as _},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use iroh::{RelayConfig, RelayMap, RelayUrl};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tcode_client::pairing::{TRAVERSE_OFF, TRAVERSE_OFFICIAL};
use tcode_protocol::{TraverseManifestState, TraverseManifestStatus};
use url::Url;

pub const OFFICIAL_MANIFEST_URL: &str = "https://raw.githubusercontent.com/Tryanks/tcode/main/crates/traverse/src/traverse_manifest.json";
/// Debug builds may point the official fetch elsewhere (`file://` included)
/// to exercise the fetch path before the manifest is published.
#[cfg(debug_assertions)]
pub const MANIFEST_URL_ENV: &str = "TCODE_TRAVERSE_MANIFEST_URL";
const BUNDLED: &str = include_str!("traverse_manifest.json");
/// How long a fetched manifest stays fresh.
pub const TTL_MS: u64 = 60 * 60 * 1000;
/// Minimum gap between attempts after a failure, so an offline machine does
/// not pay a network timeout on every check.
pub const RETRY_MS: u64 = 5 * 60 * 1000;
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    /// ISO-8601 UTC edit date. Same-format timestamps order lexicographically,
    /// and an undated manifest counts as older than any dated one.
    #[serde(rename = "updatedAt", default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    pub relays: Vec<ManifestRelay>,
    #[serde(default)]
    pub pkarr: Vec<Url>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestRelay {
    pub url: Url,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quic_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ManifestError {
    Json(String),
    Version(u32),
    /// A relay or pkarr URL is not `https`, or the relay URL is malformed.
    Url(String),
    NoRelays,
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(error) => write!(f, "invalid manifest: {error}"),
            Self::Version(version) => write!(f, "unsupported manifest version {version}"),
            Self::Url(url) => write!(f, "manifest URL must be https: {url}"),
            Self::NoRelays => f.write_str("manifest lists no relays"),
        }
    }
}

impl std::error::Error for ManifestError {}

impl Manifest {
    pub fn parse(json: &str) -> Result<Self, ManifestError> {
        serde_json::from_str(json)
            .map_err(|error| ManifestError::Json(error.to_string()))
            .and_then(Self::from_value)
    }

    pub fn from_value(value: Value) -> Result<Self, ManifestError> {
        let manifest: Self = serde_json::from_value(value)
            .map_err(|error| ManifestError::Json(error.to_string()))?;
        if manifest.version != 1 {
            return Err(ManifestError::Version(manifest.version));
        }
        if manifest.relays.is_empty() {
            return Err(ManifestError::NoRelays);
        }
        for url in manifest
            .relays
            .iter()
            .map(|relay| &relay.url)
            .chain(&manifest.pkarr)
        {
            if url.scheme() != "https" || url.host_str().is_none() {
                return Err(ManifestError::Url(url.to_string()));
            }
        }
        Ok(manifest)
    }

    /// The relay map iroh dials for `RelayMode::Custom`.
    pub fn relay_map(&self) -> RelayMap {
        self.relays
            .iter()
            .map(|relay| {
                // `RelayQuicConfig` is not re-exported by iroh; start from the
                // default QUIC configuration and adjust or drop it.
                let mut config = RelayConfig::from(RelayUrl::from(relay.url.clone()));
                match relay.quic_port {
                    Some(0) => config.quic = None,
                    Some(port) => {
                        if let Some(quic) = &mut config.quic {
                            quic.port = port;
                        }
                    }
                    None => {}
                }
                config
            })
            .collect()
    }

    pub fn pkarr_urls(&self) -> &[Url] {
        &self.pkarr
    }
}

/// The copy of the official manifest built into this binary.
pub fn bundled() -> Manifest {
    Manifest::parse(BUNDLED).expect("bundled Traverse manifest is valid")
}

/// Whose manifest: the official service's or a self-hosted instance's.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ManifestSource {
    Official,
    /// Base URL of the instance; a `file://` base names the manifest file
    /// itself.
    Custom(Url),
}

impl ManifestSource {
    /// The instances behind what a device stores per machine, see
    /// [`tcode_client::pairing::PairInvite::traverse`]: an empty list is the
    /// official service, [`TRAVERSE_OFF`] no service at all. An entry that
    /// is not a URL names nothing.
    pub fn from_traverse(traverse: &[String]) -> Vec<Self> {
        if traverse.is_empty() {
            return vec![Self::Official];
        }
        let mut sources = Vec::new();
        for entry in traverse {
            let source = match entry.as_str() {
                TRAVERSE_OFF => None,
                TRAVERSE_OFFICIAL => Some(Self::Official),
                base => Url::parse(base).ok().map(Self::Custom),
            };
            if let Some(source) = source
                && !sources.contains(&source)
            {
                sources.push(source);
            }
        }
        sources
    }

    /// Where the manifest is fetched from.
    pub fn url(&self) -> Url {
        match self {
            Self::Official => {
                #[cfg(debug_assertions)]
                if let Some(url) = std::env::var_os(MANIFEST_URL_ENV)
                    .and_then(|value| Url::parse(&value.to_string_lossy()).ok())
                {
                    return url;
                }
                Url::parse(OFFICIAL_MANIFEST_URL).expect("static manifest URL is valid")
            }
            Self::Custom(base) if base.scheme() == "file" => base.clone(),
            Self::Custom(base) => {
                let mut base = base.clone();
                if !base.path().ends_with('/') {
                    base.set_path(&format!("{}/", base.path()));
                }
                base.join("relays.json").unwrap_or(base)
            }
        }
    }

    pub fn cache_key(&self) -> String {
        match self {
            Self::Official => "official".into(),
            Self::Custom(base) => format!("{:016x}", fnv1a(base.as_str().as_bytes())),
        }
    }

    /// `<data_dir>/traverse-manifest-<key>.json`.
    pub fn cache_path(&self, data_dir: &Path) -> PathBuf {
        data_dir.join(format!("traverse-manifest-{}.json", self.cache_key()))
    }
}

/// FNV-1a: a stable file-name hash, not a security boundary.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// On-disk shape of the last good remote manifest.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheFile {
    pub fetched_at_ms: u64,
    pub manifest: Value,
}

/// In-memory manifest plus the fetch bookkeeping that paces refreshes.
pub struct ManifestState {
    /// `None` for a self-hosted instance with nothing cached yet.
    manifest: Option<Arc<Manifest>>,
    /// Wall-clock millis of the fetch that produced `manifest`; `None` for
    /// the bundle. Persisted with the disk cache so a restart does not
    /// refetch.
    fetched_at_ms: Option<u64>,
    /// `manifest` was fetched by this process rather than read from disk.
    fetched_live: bool,
    /// Why the last fetch failed, until one succeeds.
    last_error: Option<String>,
    pub(crate) last_attempt_ms: Option<u64>,
    disk_loaded: bool,
}

impl ManifestState {
    pub fn new(source: ManifestSource) -> Self {
        let manifest = match &source {
            ManifestSource::Official => Some(Arc::new(bundled())),
            ManifestSource::Custom(_) => None,
        };
        Self {
            manifest,
            fetched_at_ms: None,
            fetched_live: false,
            last_error: None,
            last_attempt_ms: None,
            disk_loaded: false,
        }
    }

    pub fn current(&self) -> Option<Arc<Manifest>> {
        self.manifest.clone()
    }

    /// Adopt the disk cache once, unless the bundle is newer by `updatedAt`.
    pub fn load_cache(&mut self, path: &Path) {
        if std::mem::replace(&mut self.disk_loaded, true) {
            return;
        }
        let Some(cache) = fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<CacheFile>(&text).ok())
        else {
            return;
        };
        match Manifest::from_value(cache.manifest) {
            Ok(manifest)
                if self
                    .manifest
                    .as_ref()
                    .is_none_or(|bundle| manifest.updated_at >= bundle.updated_at) =>
            {
                self.manifest = Some(Arc::new(manifest));
                self.fetched_at_ms = Some(cache.fetched_at_ms);
            }
            Ok(_) => log::info!("bundled Traverse manifest is newer than the cache"),
            Err(error) => log::warn!("ignoring cached Traverse manifest: {error}"),
        }
    }

    /// Whether a fetch is due now; records the attempt when it is.
    pub fn begin_fetch(&mut self, now_ms: u64) -> bool {
        // A timestamp in the future means the wall clock moved backwards;
        // treat it as expired so the refetch rewrites both timestamps.
        let within = |since: Option<u64>, window: u64| {
            since.is_some_and(|since| now_ms >= since && now_ms - since < window)
        };
        if within(self.fetched_at_ms, TTL_MS) || within(self.last_attempt_ms, RETRY_MS) {
            return false;
        }
        self.last_attempt_ms = Some(now_ms);
        true
    }

    /// Replace the manifest with a fetched one; an undecodable or invalid
    /// body leaves the current manifest in place. Returns the manifest and
    /// its raw value for the cache.
    pub fn install(&mut self, now_ms: u64, body: &[u8]) -> Result<(Arc<Manifest>, Value), String> {
        let value: Value = serde_json::from_slice(body).map_err(|error| error.to_string())?;
        let manifest =
            Arc::new(Manifest::from_value(value.clone()).map_err(|error| error.to_string())?);
        self.manifest = Some(manifest.clone());
        self.fetched_at_ms = Some(now_ms);
        self.fetched_live = true;
        Ok((manifest, value))
    }

    /// Where the copy in effect came from, as a hosting machine reports it.
    pub fn status(&self) -> TraverseManifestStatus {
        let state = match (&self.manifest, self.fetched_at_ms) {
            (None, _) => TraverseManifestState::Failed,
            (Some(_), None) => TraverseManifestState::Bundled,
            (Some(_), Some(_)) if self.fetched_live => TraverseManifestState::Live,
            (Some(_), Some(_)) => TraverseManifestState::Cached,
        };
        TraverseManifestStatus {
            state,
            fetched_unix: self
                .manifest
                .as_ref()
                .and(self.fetched_at_ms)
                .map(|ms| ms / 1000),
            error: self.last_error.clone(),
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn fetch(url: &Url) -> Result<Vec<u8>, String> {
    if url.scheme() == "file" {
        let path = url
            .to_file_path()
            .map_err(|()| "invalid file URL".to_owned())?;
        let body = fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        if body.len() as u64 > MAX_RESPONSE_BYTES {
            return Err("manifest exceeds 1 MiB".into());
        }
        return Ok(body);
    }
    let response = ureq::get(url.as_str())
        .set("User-Agent", "tcode")
        .timeout(FETCH_TIMEOUT)
        .call()
        .map_err(|error| error.to_string())?;
    let mut body = Vec::new();
    response
        .into_reader()
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut body)
        .map_err(|error| error.to_string())?;
    if body.len() as u64 > MAX_RESPONSE_BYTES {
        return Err("manifest exceeds 1 MiB".into());
    }
    Ok(body)
}

struct LoaderInner {
    source: ManifestSource,
    cache_path: PathBuf,
    state: Mutex<ManifestState>,
}

/// One manifest source shared by everything that builds on it: the copy in
/// effect is always available without waiting on the network, and
/// [`refresh`](Self::refresh) reports when a fetch changed it.
#[derive(Clone)]
pub struct ManifestLoader {
    inner: Arc<LoaderInner>,
}

impl ManifestLoader {
    /// Adopt the cache under `data_dir` (bundle first for the official
    /// service). Nothing is fetched here.
    pub fn new(source: ManifestSource, data_dir: &Path) -> Self {
        let cache_path = source.cache_path(data_dir);
        let mut state = ManifestState::new(source.clone());
        state.load_cache(&cache_path);
        Self {
            inner: Arc::new(LoaderInner {
                source,
                cache_path,
                state: Mutex::new(state),
            }),
        }
    }

    pub fn source(&self) -> &ManifestSource {
        &self.inner.source
    }

    /// The manifest in effect and where it came from.
    pub fn status(&self) -> (Option<Arc<Manifest>>, TraverseManifestStatus) {
        let state = self.state();
        (state.current(), state.status())
    }

    pub(crate) fn state(&self) -> std::sync::MutexGuard<'_, ManifestState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The manifest in effect; never waits on the network. A self-hosted
    /// instance with nothing cached is an error, never the official service.
    pub fn current(&self) -> io::Result<Arc<Manifest>> {
        self.state().current().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "no cached Traverse manifest for {} at {}",
                    self.inner.source.url(),
                    self.inner.cache_path.display()
                ),
            )
        })
    }

    /// The manifest to start with: the copy in hand, or — only when there is
    /// none — one fetch now. Must run on the runtime.
    pub async fn startup(&self) -> io::Result<Arc<Manifest>> {
        if let Ok(manifest) = self.current() {
            return Ok(manifest);
        }
        self.refresh().await;
        self.current()
    }

    /// Fetch when the TTL and retry gap allow, on a blocking thread. Returns
    /// the new manifest when the fetch produced one different from the copy
    /// in effect.
    pub async fn refresh(&self) -> Option<Arc<Manifest>> {
        let now = now_ms();
        let (due, previous) = {
            let mut state = self.state();
            (state.begin_fetch(now), state.current())
        };
        if !due {
            return None;
        }
        let url = self.inner.source.url();
        // The lock is not held across the fetch: `current()` must stay
        // instant while a 10 s timeout plays out.
        let fetched = tokio::task::spawn_blocking(move || fetch(&url))
            .await
            .unwrap_or_else(|error| Err(error.to_string()));
        let installed = {
            let mut state = self.state();
            let installed = fetched.and_then(|body| state.install(now, &body));
            state.last_error = installed.as_ref().err().cloned();
            installed
        };
        match installed {
            Ok((manifest, value)) => {
                let cache = CacheFile {
                    fetched_at_ms: now,
                    manifest: value,
                };
                if let Err(error) = serde_json::to_vec(&cache)
                    .map_err(io::Error::other)
                    .and_then(|bytes| fs::write(&self.inner.cache_path, bytes))
                {
                    log::warn!("could not cache the Traverse manifest: {error}");
                }
                (previous.as_deref() != Some(&*manifest)).then_some(manifest)
            }
            Err(error) => {
                log::warn!(
                    "Traverse manifest refresh from {} failed: {error}; keeping the {}",
                    self.inner.source.url(),
                    match (&self.inner.source, previous.is_some()) {
                        (ManifestSource::Official, _) => "cached or bundled copy",
                        (ManifestSource::Custom(_), true) => "cached copy",
                        (ManifestSource::Custom(_), false) => "instance unreachable",
                    }
                );
                None
            }
        }
    }

    /// Keep refreshing every [`RETRY_MS`] (the TTL paces actual fetches);
    /// `apply` runs after each pass, with the manifest when a fetch changed
    /// it. Aborted when the returned handle is dropped by its owner.
    pub fn spawn_refresh<F, Fut>(&self, apply: F) -> tokio::task::AbortHandle
    where
        F: Fn(Option<Arc<Manifest>>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let loader = self.clone();
        crate::runtime::runtime()
            .spawn(async move {
                loop {
                    let manifest = loader.refresh().await;
                    apply(manifest).await;
                    tokio::time::sleep(Duration::from_millis(RETRY_MS)).await;
                }
            })
            .abort_handle()
    }
}

/// Live application of a manifest to a bound endpoint.
pub(crate) mod live {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use iroh::{
        Endpoint, EndpointId, RelayMap, RelayUrl,
        address_lookup::{
            AddressLookup, AddressLookupBuilder as _, DEFAULT_REPUBLISH_INTERVAL, EndpointData,
            PkarrPublisher, PkarrRelayClient, PkarrResolver,
        },
        endpoint_info::EndpointInfo,
    };
    use tcode_protocol::{TraverseLookupState, TraverseLookupStatus};
    use tokio::sync::Notify;
    use url::Url;

    use super::Manifest;

    /// Move the endpoint's relay map from `previous` to `wanted`: relays no
    /// longer listed are removed, new or changed ones inserted.
    pub(crate) async fn sync_relays(endpoint: &Endpoint, previous: &RelayMap, wanted: &RelayMap) {
        for url in previous.urls::<Vec<_>>() {
            if !wanted.contains(&url) {
                endpoint.remove_relay(&url).await;
            }
        }
        for config in wanted.relays::<Vec<Arc<_>>>() {
            if previous.get(&config.url).as_ref() != Some(&config) {
                endpoint.insert_relay(config.url.clone(), config).await;
            }
        }
    }

    /// Replace every address-lookup service with those of `manifests`: a
    /// pkarr resolver per pkarr URL and, on a machine (`publish`), a
    /// publisher whose record is read back into `publish`. A device keeps
    /// its `lan` lookup through the replacement.
    pub(crate) fn install_lookups<'a>(
        endpoint: &Endpoint,
        manifests: impl IntoIterator<Item = &'a Manifest>,
        publish: Option<&Arc<LookupChecks>>,
        lan: Option<crate::lan::LanLookup>,
    ) {
        let Ok(services) = endpoint.address_lookup() else {
            return;
        };
        services.clear();
        if let Some(lan) = lan {
            services.add(lan);
        }
        for manifest in manifests {
            for pkarr in manifest.pkarr_urls() {
                if let Some(checks) = publish {
                    match PkarrPublisher::builder(pkarr.clone()).into_address_lookup(endpoint) {
                        Ok(publisher) => match CheckedPublisher::new(
                            publisher,
                            endpoint,
                            pkarr.clone(),
                            checks.clone(),
                        ) {
                            Some(publisher) => services.add(publisher),
                            None => log::warn!("could not add pkarr publisher {pkarr}"),
                        },
                        Err(error) => log::warn!("could not add pkarr publisher {pkarr}: {error}"),
                    }
                }
                match PkarrResolver::builder(pkarr.clone()).into_address_lookup(endpoint) {
                    Ok(resolver) => services.add(resolver),
                    Err(error) => log::warn!("could not add pkarr resolver {pkarr}: {error}"),
                }
            }
        }
    }

    /// The last read-back of this machine's record from each pkarr URL it
    /// publishes to.
    pub(crate) struct LookupChecks {
        outcomes: Mutex<HashMap<Url, TraverseLookupStatus>>,
        /// Told after every check.
        changed: Arc<Notify>,
    }

    impl LookupChecks {
        pub(crate) fn new(changed: Arc<Notify>) -> Self {
            Self {
                outcomes: Mutex::new(HashMap::new()),
                changed,
            }
        }

        pub(crate) fn status(&self, url: &Url) -> TraverseLookupStatus {
            self.outcomes
                .lock()
                .unwrap()
                .get(url)
                .cloned()
                .unwrap_or_else(|| TraverseLookupStatus {
                    url: url.to_string(),
                    state: TraverseLookupState::Pending,
                    checked_unix: None,
                    error: None,
                })
        }

        fn record(&self, url: &Url, result: Result<(), String>) {
            if let Err(error) = &result {
                log::warn!("this machine's record does not read back from {url}: {error}");
            }
            let status = TraverseLookupStatus {
                url: url.to_string(),
                state: match result {
                    Ok(()) => TraverseLookupState::Ok,
                    Err(_) => TraverseLookupState::Failed,
                },
                checked_unix: Some(now_unix()),
                error: result.err(),
            };
            self.outcomes.lock().unwrap().insert(url.clone(), status);
            self.changed.notify_one();
        }
    }

    /// The first read-back waits this long for a publish to prompt it.
    const FIRST_CHECK: Duration = Duration::from_secs(30);
    /// Time for the publisher's PUT, which it sends in the background, to
    /// land before the record is read back.
    const PUBLISH_SETTLE: Duration = Duration::from_secs(5);
    /// How old a record may be: the publisher rewrites it every
    /// [`DEFAULT_REPUBLISH_INTERVAL`], so an older one means its writes are
    /// failing.
    const MAX_RECORD_AGE: Duration = Duration::from_secs(DEFAULT_REPUBLISH_INTERVAL.as_secs() + 60);

    /// iroh's pkarr publisher only logs whether its writes land, so each one
    /// is paired with a read-back of the record through the same pkarr URL:
    /// shortly after every publish and once per republish interval.
    #[derive(Debug)]
    struct CheckedPublisher<P> {
        inner: P,
        /// The relays the last publish named.
        published: Arc<Mutex<Vec<RelayUrl>>>,
        nudge: Arc<Notify>,
        check: tokio::task::AbortHandle,
    }

    impl<P: AddressLookup> CheckedPublisher<P> {
        fn new(inner: P, endpoint: &Endpoint, url: Url, checks: Arc<LookupChecks>) -> Option<Self> {
            let client = PkarrRelayClient::new(
                url.clone(),
                endpoint.tls_config().clone(),
                endpoint.dns_resolver().ok()?.clone(),
            );
            let published = Arc::new(Mutex::new(Vec::new()));
            let nudge = Arc::new(Notify::new());
            let check = crate::runtime::runtime()
                .spawn(read_back(
                    client,
                    endpoint.id(),
                    url,
                    published.clone(),
                    nudge.clone(),
                    checks,
                ))
                .abort_handle();
            Some(Self {
                inner,
                published,
                nudge,
                check,
            })
        }
    }

    impl<P> Drop for CheckedPublisher<P> {
        fn drop(&mut self) {
            self.check.abort();
        }
    }

    impl<P: AddressLookup> AddressLookup for CheckedPublisher<P> {
        fn publish(&self, data: &EndpointData) {
            self.inner.publish(data);
            *self.published.lock().unwrap() = data.relay_urls().cloned().collect();
            self.nudge.notify_one();
        }
    }

    async fn read_back(
        client: PkarrRelayClient,
        id: EndpointId,
        url: Url,
        published: Arc<Mutex<Vec<RelayUrl>>>,
        nudge: Arc<Notify>,
        checks: Arc<LookupChecks>,
    ) {
        let mut wait = FIRST_CHECK;
        loop {
            tokio::select! {
                _ = nudge.notified() => {}
                _ = tokio::time::sleep(wait) => {}
            }
            wait = DEFAULT_REPUBLISH_INTERVAL;
            tokio::time::sleep(PUBLISH_SETTLE).await;
            let expected = published.lock().unwrap().clone();
            let result = match client.resolve(id).await {
                Ok(packet) => {
                    let age = Duration::from_micros(
                        now_micros().saturating_sub(packet.timestamp().as_micros()),
                    );
                    match EndpointInfo::from_pkarr_signed_packet(&packet) {
                        Err(error) => Err(error.to_string()),
                        Ok(_) if age > MAX_RECORD_AGE => Err(format!(
                            "the record was last written {} min ago",
                            age.as_secs() / 60
                        )),
                        Ok(info)
                            if !expected
                                .iter()
                                .all(|relay| info.relay_urls().any(|named| named == relay)) =>
                        {
                            Err("the record names another relay".into())
                        }
                        Ok(_) => Ok(()),
                    }
                }
                Err(error) => Err(error_chain(&error)),
            };
            checks.record(&url, result);
        }
    }

    /// An error and its causes on one line; iroh's lookup error names only
    /// the service at the top.
    fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
        let mut text = Vec::new();
        let mut next = error.source();
        while let Some(cause) = next {
            text.push(cause.to_string());
            next = cause.source();
        }
        if text.is_empty() {
            error.to_string()
        } else {
            text.join(": ")
        }
    }

    fn now_micros() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_micros() as u64)
    }

    fn now_unix() -> u64 {
        now_micros() / 1_000_000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tcode-manifest-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn manifest_json(updated_at: &str, relay: &str) -> String {
        format!(r#"{{"version":1,"updatedAt":"{updated_at}","relays":[{{"url":"{relay}"}}]}}"#)
    }

    fn write_cache(path: &Path, fetched_at_ms: u64, manifest: &str) {
        let cache = CacheFile {
            fetched_at_ms,
            manifest: serde_json::from_str(manifest).unwrap(),
        };
        fs::write(path, serde_json::to_vec(&cache).unwrap()).unwrap();
    }

    fn first_relay(state: &ManifestState) -> String {
        state.current().unwrap().relays[0].url.to_string()
    }

    #[test]
    fn bundle_lists_iroh_default_relays_and_the_n0_pkarr_relay() {
        let bundle = bundled();
        assert_eq!(
            bundle.relay_map(),
            iroh::RelayMode::Default.relay_map(),
            "the official manifest must match iroh's default relays"
        );
        assert_eq!(
            bundle
                .pkarr_urls()
                .iter()
                .map(Url::as_str)
                .collect::<Vec<_>>(),
            [iroh::address_lookup::N0_DNS_PKARR_RELAY_PROD]
        );
        assert!(bundle.updated_at.is_some());
    }

    #[test]
    fn manifest_becomes_relay_map_and_pkarr_urls() {
        let manifest = Manifest::parse(
            r#"{"version":1,"relays":[{"url":"https://relay.example/","quic_port":7842,"region":"eu"},{"url":"https://tcp-only.example/","quic_port":0},{"url":"https://default.example/"}],"pkarr":["https://traverse.example/pkarr/"]}"#,
        )
        .unwrap();
        let map = manifest.relay_map();
        assert_eq!(map.len(), 3);
        let relay = |url: &str| map.get(&RelayUrl::from(Url::parse(url).unwrap())).unwrap();
        assert_eq!(
            relay("https://relay.example/").quic.as_ref().unwrap().port,
            7842
        );
        assert!(relay("https://tcp-only.example/").quic.is_none());
        assert_eq!(
            relay("https://default.example/")
                .quic
                .as_ref()
                .unwrap()
                .port,
            iroh::defaults::DEFAULT_RELAY_QUIC_PORT
        );
        assert_eq!(
            manifest.pkarr_urls(),
            [Url::parse("https://traverse.example/pkarr/").unwrap()]
        );
    }

    #[test]
    fn malformed_manifests_are_rejected() {
        assert!(matches!(
            Manifest::parse("not json"),
            Err(ManifestError::Json(_))
        ));
        assert_eq!(
            Manifest::parse(r#"{"version":2,"relays":[{"url":"https://relay.example/"}]}"#),
            Err(ManifestError::Version(2))
        );
        assert_eq!(
            Manifest::parse(r#"{"version":1,"relays":[]}"#),
            Err(ManifestError::NoRelays)
        );
        assert_eq!(
            Manifest::parse(r#"{"version":1,"relays":[{"url":"http://relay.example/"}]}"#),
            Err(ManifestError::Url("http://relay.example/".into()))
        );
        assert_eq!(
            Manifest::parse(
                r#"{"version":1,"relays":[{"url":"https://relay.example/"}],"pkarr":["ftp://x/"]}"#
            ),
            Err(ManifestError::Url("ftp://x/".into()))
        );
        assert!(matches!(
            Manifest::parse(r#"{"version":1,"relays":[{"url":"relay"}]}"#),
            Err(ManifestError::Json(_))
        ));
    }

    #[test]
    fn sources_name_their_fetch_url_and_cache_file() {
        let dir = Path::new("/data");
        assert_eq!(
            ManifestSource::Official.cache_path(dir),
            dir.join("traverse-manifest-official.json")
        );
        let base = ManifestSource::Custom(Url::parse("https://traverse.example/tcode").unwrap());
        assert_eq!(
            base.url().as_str(),
            "https://traverse.example/tcode/relays.json"
        );
        assert_eq!(
            ManifestSource::Custom(Url::parse("https://traverse.example/").unwrap())
                .url()
                .as_str(),
            "https://traverse.example/relays.json"
        );
        let cache = base.cache_path(dir);
        let name = cache.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("traverse-manifest-") && name.ends_with(".json"));
        assert_eq!(name.len(), "traverse-manifest-.json".len() + 16);
        assert_ne!(
            ManifestSource::Custom(Url::parse("https://other.example/").unwrap()).cache_path(dir),
            cache
        );
        let file = ManifestSource::Custom(Url::parse("file:///tmp/relays.json").unwrap());
        assert_eq!(file.url().as_str(), "file:///tmp/relays.json");
        let from = |entries: &[&str]| {
            ManifestSource::from_traverse(
                &entries.iter().map(|&e| e.to_owned()).collect::<Vec<_>>(),
            )
        };
        assert_eq!(from(&[]), [ManifestSource::Official]);
        assert_eq!(from(&[TRAVERSE_OFF]), []);
        assert_eq!(from(&["not a url"]), []);
        assert_eq!(
            from(&[
                "https://traverse.example/",
                TRAVERSE_OFFICIAL,
                "https://traverse.example/"
            ]),
            [
                ManifestSource::Custom(Url::parse("https://traverse.example/").unwrap()),
                ManifestSource::Official
            ]
        );
    }

    #[test]
    fn official_cache_is_adopted_unless_the_bundle_is_newer_and_invalid_caches_are_ignored() {
        let dir = temp_dir("official");
        let path = ManifestSource::Official.cache_path(&dir);
        let bundle_date = bundled().updated_at.unwrap();

        // Older than the bundle: ignored.
        write_cache(
            &path,
            42,
            &manifest_json("2000-01-01T00:00:00Z", "https://old.example/"),
        );
        let mut state = ManifestState::new(ManifestSource::Official);
        state.load_cache(&path);
        assert_eq!(state.current().unwrap().relay_map(), bundled().relay_map());
        assert_eq!(state.fetched_at_ms, None);

        // Same date or newer: adopted, together with its fetch time.
        write_cache(
            &path,
            42,
            &manifest_json(&bundle_date, "https://cached.example/"),
        );
        let mut state = ManifestState::new(ManifestSource::Official);
        state.load_cache(&path);
        assert_eq!(first_relay(&state), "https://cached.example/");
        assert_eq!(state.fetched_at_ms, Some(42));

        // Loaded once: a later cache write is not re-read.
        write_cache(
            &path,
            43,
            &manifest_json("2999-01-01T00:00:00Z", "https://later.example/"),
        );
        state.load_cache(&path);
        assert_eq!(first_relay(&state), "https://cached.example/");

        // Invalid cache: bundle stays.
        for broken in [
            "{}",
            r#"{"fetchedAtMs":1,"manifest":{"version":1,"relays":[]}}"#,
            r#"{"fetchedAtMs":1,"manifest":{"version":9,"relays":[{"url":"https://x/"}]}}"#,
        ] {
            fs::write(&path, broken).unwrap();
            let mut state = ManifestState::new(ManifestSource::Official);
            state.load_cache(&path);
            assert_eq!(state.current().unwrap().relay_map(), bundled().relay_map());
        }
        let loader = ManifestLoader::new(ManifestSource::Official, &dir);
        assert_eq!(loader.current().unwrap().relay_map(), bundled().relay_map());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_custom_base_uses_its_cache_and_never_the_official_manifest() {
        let dir = temp_dir("custom");
        let base = ManifestSource::Custom(Url::parse("https://traverse.example/").unwrap());
        let loader = ManifestLoader::new(base.clone(), &dir);
        let error = loader.current().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(
            error
                .to_string()
                .contains("https://traverse.example/relays.json"),
            "{error}"
        );

        write_cache(
            &base.cache_path(&dir),
            7,
            &manifest_json("2000-01-01T00:00:00Z", "https://self-hosted.example/"),
        );
        let loader = ManifestLoader::new(base.clone(), &dir);
        assert_eq!(
            loader.current().unwrap().relays[0].url.as_str(),
            "https://self-hosted.example/"
        );
        // A self-hosted cache is adopted however old it is: there is no bundle to outrank it.
        assert_eq!(loader.state().fetched_at_ms, Some(7));

        fs::write(base.cache_path(&dir), "garbage").unwrap();
        assert!(ManifestLoader::new(base, &dir).current().is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fetches_are_paced_by_ttl_and_retry_gap() {
        let mut state = ManifestState::new(ManifestSource::Official);
        assert!(state.begin_fetch(1_000));
        assert!(!state.begin_fetch(1_000 + RETRY_MS - 1), "retry gap");
        assert!(state.begin_fetch(1_000 + RETRY_MS));
        state.fetched_at_ms = Some(10_000);
        assert!(!state.begin_fetch(10_000 + TTL_MS - 1), "fresh");
        assert!(state.begin_fetch(10_000 + TTL_MS));
        // A future timestamp (clock moved backwards) counts as expired.
        state.fetched_at_ms = Some(u64::MAX);
        state.last_attempt_ms = Some(u64::MAX);
        assert!(state.begin_fetch(5));
    }

    #[test]
    fn a_fetched_manifest_replaces_the_copy_in_hand_unless_invalid() {
        let mut state = ManifestState::new(ManifestSource::Official);
        assert!(state.install(1_000, b"{ not json").is_err());
        assert!(
            state
                .install(1_000, br#"{"version":1,"relays":[]}"#)
                .is_err()
        );
        assert_eq!(state.current().unwrap().relay_map(), bundled().relay_map());
        assert_eq!(state.fetched_at_ms, None);
        // The remote wins even when dated before the bundle.
        let (manifest, _) = state
            .install(
                2_000,
                manifest_json("2000-01-01T00:00:00Z", "https://remote.example/").as_bytes(),
            )
            .unwrap();
        assert_eq!(manifest.relays[0].url.as_str(), "https://remote.example/");
        assert_eq!(first_relay(&state), "https://remote.example/");
        assert_eq!(state.fetched_at_ms, Some(2_000));
    }

    #[test]
    fn a_file_source_is_fetched_cached_and_reported_only_when_it_changes() {
        let dir = temp_dir("file");
        let path = dir.join("relays.json");
        fs::write(
            &path,
            manifest_json("2000-01-01T00:00:00Z", "https://first.example/"),
        )
        .unwrap();
        let base = ManifestSource::Custom(Url::from_file_path(&path).unwrap());
        let loader = ManifestLoader::new(base.clone(), &dir);
        assert!(loader.current().is_err());
        let started = crate::block_on(loader.startup()).unwrap();
        assert_eq!(started.relays[0].url.as_str(), "https://first.example/");
        assert!(base.cache_path(&dir).is_file(), "the fetch is cached");
        // Fresh: no fetch, so no change reported even though the file changed.
        fs::write(
            &path,
            manifest_json("2000-01-01T00:00:00Z", "https://second.example/"),
        )
        .unwrap();
        assert!(crate::block_on(loader.refresh()).is_none());
        loader.state().fetched_at_ms = None;
        loader.state().last_attempt_ms = None;
        let changed = crate::block_on(loader.refresh()).unwrap();
        assert_eq!(changed.relays[0].url.as_str(), "https://second.example/");
        loader.state().fetched_at_ms = None;
        loader.state().last_attempt_ms = None;
        assert!(
            crate::block_on(loader.refresh()).is_none(),
            "an unchanged manifest is not reported"
        );
        // The next process starts from the cache.
        assert_eq!(
            ManifestLoader::new(base, &dir).current().unwrap().relays[0]
                .url
                .as_str(),
            "https://second.example/"
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
