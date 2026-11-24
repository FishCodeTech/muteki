//! The machine side: one iroh endpoint that pairs devices over `tcode/pair/1`,
//! serves paired devices over `tcode/1`, bridging each main stream into the
//! [`HostMux`], and keeps its group in step with the other members over
//! `tcode/group/1`.
use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use iroh::{
    Endpoint, EndpointId, RelayMap, RelayMode, TransportAddr, Watcher as _,
    endpoint::{Connection, SendStream, presets},
    protocol::{AcceptError, ProtocolHandler, Router},
};
use tcode_client::pairing::{
    MAX_ADDRS, PairInvite, TRAVERSE_OFF, TRAVERSE_OFFICIAL, encode_secret, pair_url, parse_pair_url,
};
use tcode_protocol::{
    DeviceAccess, GroupAction, GroupInfo, GroupMachineInfo, HostedDevice, HostingAction,
    HostingState, PathInfo, Principal, ProtocolError, SpaceAction, SpaceInfo, TraverseRelayStatus,
    TraverseSourceStatus,
};

use crate::{
    group::{
        ALPN_GROUP, GroupLine, GroupMachine, GroupRecord, GroupRejection, GroupState,
        JoiningDevice, JoiningMachine, MAX_GROUP_LINE, MAX_GROUP_MACHINES, Membership, Roster,
    },
    identity::{DeviceGrant, DeviceRecord, HostIdentity, SpaceRecord, existing_device, now_unix},
    lan,
    manifest::{Manifest, ManifestLoader, ManifestSource, live},
    mux::HostMux,
    runtime::block_on,
    wire::{self, ClientLine, DeviceClaim, HelloRejection, HostLine, LineReader, PairRejection},
};

pub const INVITATION_LIFETIME: Duration = Duration::from_secs(5 * 60);
/// Wrong secrets an invitation survives; a cheap defence in depth behind
/// 128 bits of entropy.
pub const MAX_PAIRING_FAILURES: u8 = 5;

pub struct HostConfig {
    pub host_name: String,
    pub data_dir: PathBuf,
    /// Every Traverse instance this machine publishes to: the union of their
    /// relays, and every one's lookup. Empty is none: no relay and no
    /// wide-area lookup: devices reach the machine on the LAN (DNS-SD) or at
    /// an address the user types.
    pub traverse: Vec<ManifestSource>,
    /// Whether this host may pair devices at all. The user's persisted
    /// pairing switch applies on top of it.
    pub pairing_enabled: bool,
    /// A fixed UDP port instead of a random one, so the invitation's port
    /// and firewall rules survive restarts.
    pub bind_port: Option<u16>,
}

/// A minted invitation: the link is the secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invitation {
    pub invite: PairInvite,
    pub expires_at: Instant,
}

impl Invitation {
    pub fn remaining(&self) -> Duration {
        self.expires_at.saturating_duration_since(Instant::now())
    }

    /// The `tcode://pair?…` link to scan or paste.
    pub fn url(&self) -> String {
        pair_url(&self.invite)
    }
}

/// One paired device, as shown by the hosting UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub platform: Option<String>,
    pub created_unix: u64,
    pub access: DeviceAccess,
    /// How the device reaches this machine while connected; `None` offline.
    pub live: Option<PathInfo>,
}

/// What invitations say about where this machine is at one moment. No IP
/// address: see `docs/pair-link.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointAddrSnapshot {
    pub id: String,
    /// The home relay, once the endpoint has one.
    pub relay: Option<String>,
    /// The bound UDP port.
    pub port: u16,
}

struct ActiveInvitation {
    invitation: Invitation,
    failures: u8,
}

/// Everything revocation and pairing must see atomically: the allow list,
/// the active invitation and the connections currently admitted.
struct State {
    identity: HostIdentity,
    invitation: Option<ActiveInvitation>,
    live: HashMap<EndpointId, Vec<Connection>>,
}

struct Shared {
    endpoint: Endpoint,
    mux: HostMux,
    state: Mutex<State>,
    /// The relay map the endpoint was bound with. iroh shares it with the
    /// endpoint, so it always reads as what the endpoint dials; empty until
    /// a source's manifest arrives, and always with no source.
    relays: RelayMap,
    /// The manifest in effect for each source, in configured order; `None`
    /// until it first loads. Held across a whole apply so two sources'
    /// refreshes cannot interleave their changes to the endpoint.
    manifests: tokio::sync::Mutex<Vec<Option<Arc<Manifest>>>>,
    /// Told the invitation in effect after every change; see
    /// [`TraverseHost::invitation_events`].
    listeners: Mutex<Vec<async_channel::Sender<Option<Invitation>>>>,
    /// Ends the active invitation when its lifetime runs out.
    expiry: Mutex<Option<tokio::task::AbortHandle>>,
    /// What invitations say about this machine's Traverse instances; see
    /// [`PairInvite::traverse`].
    traverse: Vec<String>,
    /// One per source, in configured order: the manifest each has in hand
    /// and how its last fetch went.
    loaders: Vec<ManifestLoader>,
    /// Whether this machine's record reads back from each pkarr URL.
    lookup_checks: Arc<live::LookupChecks>,
    /// Told when a manifest fetch or a lookup check ends; with the
    /// endpoint's own watchers it prompts a status event.
    status_changed: Arc<tokio::sync::Notify>,
    /// See [`TraverseHost::traverse_events`].
    status_listeners: Mutex<Vec<async_channel::Sender<Vec<TraverseSourceStatus>>>>,
    allow_pairing: bool,
    /// Where `traverse.json` lives; on a desktop also `device.json` and
    /// `hosts.json`, which the group keeps current for the window.
    data_dir: PathBuf,
    /// Prompts the group sync loop to dial every member now.
    group_wake: Arc<tokio::sync::Notify>,
}

pub struct TraverseHost {
    shared: Arc<Shared>,
    router: Router,
    /// The loop that keeps the group in step with the other members.
    group_sync: tokio::task::AbortHandle,
    /// The manifest refresh loops, one per source, ended with the host.
    refresh: Vec<tokio::task::AbortHandle>,
    /// The loop behind [`TraverseHost::traverse_events`].
    status: tokio::task::AbortHandle,
    /// This machine's DNS-SD record, withdrawn with the host.
    advertisement: Option<lan::Advertisement>,
}

impl TraverseHost {
    /// Bind the endpoint and start accepting connections.
    pub fn start(mux: HostMux, config: HostConfig) -> io::Result<TraverseHost> {
        let identity = HostIdentity::load_or_create(&config.data_dir, &config.host_name)?;
        let secret_key = identity.secret_key().clone();
        let traverse = invite_traverse(&config.traverse);
        block_on(async move {
            let loaders: Vec<ManifestLoader> = config
                .traverse
                .iter()
                .map(|source| ManifestLoader::new(source.clone(), &config.data_dir))
                .collect();
            // The copies in hand start the endpoint; only a self-hosted
            // instance with nothing cached waits for one fetch, all of them
            // at once. A source that fails is left out rather than holding
            // the machine back: the other sources and the LAN still work,
            // and its refresh loop applies the manifest once the instance
            // answers. The official manifest is never substituted.
            let startups: Vec<_> = loaders
                .iter()
                .map(|loader| {
                    let loader = loader.clone();
                    tokio::spawn(async move { loader.startup().await })
                })
                .collect();
            let mut manifests = Vec::with_capacity(startups.len());
            for (startup, loader) in startups.into_iter().zip(&loaders) {
                manifests.push(match startup.await.map_err(io::Error::other)? {
                    Ok(manifest) => Some(manifest),
                    Err(error) => {
                        log::warn!(
                            "starting without the Traverse manifest from {}: {error}; retrying in the background",
                            loader.source().url()
                        );
                        None
                    }
                });
            }
            let relays = union_relays(&manifests);
            // An empty custom map keeps the relay transport, so relays can
            // be inserted live; `Disabled` would have none to insert into.
            let relay_mode = if loaders.is_empty() {
                RelayMode::Disabled
            } else {
                RelayMode::Custom(relays.clone())
            };
            let build = |ipv6: bool| -> io::Result<iroh::endpoint::Builder> {
                let mut builder =
                    Endpoint::builder(presets::Minimal).relay_mode(relay_mode.clone());
                builder = builder
                    .secret_key(secret_key.clone())
                    .transport_config(wire::transport_config());
                if let Some(port) = config.bind_port {
                    let v4 = std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, port));
                    let v6 = std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port));
                    builder = builder
                        .clear_ip_transports()
                        .bind_addr(v4)
                        .map_err(io::Error::other)?;
                    if ipv6 {
                        builder = builder.bind_addr(v6).map_err(io::Error::other)?;
                    }
                }
                Ok(builder)
            };
            let endpoint = match build(true)?.bind().await {
                Ok(endpoint) => endpoint,
                // A fixed port binds both families; fall back to IPv4 alone
                // where IPv6 is unavailable.
                Err(error) if config.bind_port.is_some() => {
                    log::debug!("dual-stack bind failed, trying IPv4 only: {error}");
                    build(false)?.bind().await.map_err(bind_error)?
                }
                Err(error) => return Err(bind_error(error)),
            };
            let status_changed = Arc::new(tokio::sync::Notify::new());
            let lookup_checks = Arc::new(live::LookupChecks::new(status_changed.clone()));
            live::install_lookups(
                &endpoint,
                manifests.iter().flatten().map(AsRef::as_ref),
                Some(&lookup_checks),
                None,
            );
            let advertisement = advertise(&endpoint, &identity.host_name);
            let shared = Arc::new(Shared {
                endpoint: endpoint.clone(),
                mux,
                state: Mutex::new(State {
                    identity,
                    invitation: None,
                    live: HashMap::new(),
                }),
                relays,
                manifests: tokio::sync::Mutex::new(manifests),
                listeners: Mutex::new(Vec::new()),
                expiry: Mutex::new(None),
                traverse,
                loaders,
                lookup_checks,
                status_changed,
                status_listeners: Mutex::new(Vec::new()),
                allow_pairing: config.pairing_enabled,
                data_dir: config.data_dir.clone(),
                group_wake: Arc::new(tokio::sync::Notify::new()),
            });
            // A fetched manifest is applied to the running endpoint, whether
            // it refreshes the one in hand or is the first to arrive. The
            // loop holds no reference that would keep a stopped host alive.
            let refresh = shared
                .loaders
                .iter()
                .enumerate()
                .map(|(index, loader)| {
                    let shared = Arc::downgrade(&shared);
                    loader.spawn_refresh(move |manifest| {
                        let shared = shared.upgrade();
                        async move {
                            let Some(shared) = shared else {
                                return;
                            };
                            if let Some(manifest) = manifest {
                                shared.apply_manifest(index, manifest).await;
                            }
                            shared.status_changed.notify_one();
                        }
                    })
                })
                .collect();
            let status = tokio::spawn(watch_status(Arc::downgrade(&shared))).abort_handle();
            let router = Router::builder(endpoint)
                .accept(wire::ALPN_PAIR, PairHandler(shared.clone()))
                .accept(wire::ALPN_MAIN, MainHandler(shared.clone()))
                .accept(ALPN_GROUP, GroupHandler(shared.clone()))
                .spawn();
            shared.group_wake.notify_one();
            let group_sync = tokio::spawn(group_sync_loop(
                Arc::downgrade(&shared),
                shared.group_wake.clone(),
            ))
            .abort_handle();
            Ok(TraverseHost {
                shared,
                router,
                group_sync,
                refresh,
                status,
                advertisement,
            })
        })
    }

    pub fn endpoint_id(&self) -> String {
        self.shared.endpoint.id().to_string()
    }

    /// Wait until the endpoint has a home relay, up to `timeout`. Only worth
    /// calling before minting an invite that should work off the LAN; with
    /// no relay configured or no route to one it simply returns at `timeout`.
    pub fn wait_online(&self, timeout: Duration) {
        let endpoint = self.shared.endpoint.clone();
        block_on(async move {
            let _ = tokio::time::timeout(timeout, endpoint.online()).await;
        });
    }

    /// The id, home relay and port invitations carry right now.
    pub fn addr(&self) -> EndpointAddrSnapshot {
        self.shared.snapshot()
    }

    /// The direct addresses the endpoint knows for itself right now, for a
    /// person to read off this machine and type on a device that found no
    /// path; invitations never carry them.
    pub fn direct_addrs(&self) -> Vec<std::net::SocketAddr> {
        self.shared.endpoint.addr().ip_addrs().copied().collect()
    }

    /// Mint an invitation, replacing any active one.
    pub fn new_invitation(&self) -> Invitation {
        self.shared.mint()
    }

    /// The active invitation and its remaining lifetime, carrying where the
    /// machine is reachable right now: the endpoint may have found its
    /// relay since the mint. `None` while pairing is disabled.
    pub fn invitation(&self) -> Option<(Invitation, Duration)> {
        let addr = self.shared.snapshot();
        let state = self.shared.state.lock().unwrap();
        self.shared.current_invitation(&state, &addr)
    }

    /// Every change to the invitation, carrying what is in effect after it:
    /// the invitation minted, or `None` once it is used, expires or pairing
    /// is turned off. Each call gets its own stream of every later change.
    pub fn invitation_events(&self) -> async_channel::Receiver<Option<Invitation>> {
        let (sender, receiver) = async_channel::unbounded();
        self.shared.listeners.lock().unwrap().push(sender);
        receiver
    }

    pub fn pairing_enabled(&self) -> bool {
        self.shared.allow_pairing && self.shared.state.lock().unwrap().identity.pairing_enabled
    }

    pub fn set_pairing_enabled(&self, enabled: bool) {
        self.shared.set_pairing_enabled(enabled);
    }

    /// Paired devices, oldest first, with how each connected one reaches us.
    pub fn devices(&self) -> Vec<DeviceInfo> {
        self.shared.devices()
    }

    /// Remove a device and close every connection it holds. A new
    /// connection from it is refused from this call on. When the allow list
    /// cannot be written the device stays paired and connected, so a
    /// revocation never appears to have happened without lasting.
    pub fn revoke(&self, id: &str) -> io::Result<()> {
        self.shared.revoke(id)
    }

    pub fn spaces(&self) -> Vec<SpaceInfo> {
        let addr = self.shared.snapshot();
        let state = self.shared.state.lock().unwrap();
        self.shared.spaces(&state, &addr)
    }

    pub fn create_space(&self, name: String) -> io::Result<String> {
        self.shared
            .space_action(SpaceAction::Create { name })
            .map(|id| id.unwrap())
    }

    pub fn rename_space(&self, id: &str, name: String) -> io::Result<()> {
        self.shared
            .space_action(SpaceAction::Rename {
                id: id.into(),
                name,
            })
            .map(|_| ())
    }

    pub fn delete_space(&self, id: &str) -> io::Result<()> {
        self.shared
            .space_action(SpaceAction::Delete { id: id.into() })
            .map(|_| ())
    }

    pub fn set_space_projects(&self, id: &str, project_ids: Vec<String>) -> io::Result<()> {
        self.shared
            .space_action(SpaceAction::SetProjects {
                id: id.into(),
                project_ids,
            })
            .map(|_| ())
    }

    pub fn regenerate_space_link(&self, id: &str) -> io::Result<()> {
        self.shared
            .space_action(SpaceAction::RegenerateLink { id: id.into() })
            .map(|_| ())
    }

    pub fn set_space_link_enabled(&self, id: &str, enabled: bool) -> io::Result<()> {
        self.shared
            .space_action(SpaceAction::SetLinkEnabled {
                id: id.into(),
                enabled,
            })
            .map(|_| ())
    }

    pub fn space_link_url(&self, id: &str) -> Option<String> {
        self.spaces().into_iter().find(|space| space.id == id)?.link
    }

    pub fn move_member(&self, device_id: &str, space_id: &str) -> io::Result<()> {
        self.shared
            .space_action(SpaceAction::MoveMember {
                device_id: device_id.into(),
                space_id: space_id.into(),
            })
            .map(|_| ())
    }

    pub fn remove_member(&self, device_id: &str, regenerate_link: bool) -> io::Result<()> {
        self.shared
            .space_action(SpaceAction::RemoveMember {
                device_id: device_id.into(),
                regenerate_link,
            })
            .map(|_| ())
    }

    /// How each Traverse source is doing right now, in configured order.
    pub fn traverse_status(&self) -> Vec<TraverseSourceStatus> {
        self.shared.traverse_status()
    }

    /// The Traverse status after every change, at most once per
    /// [`STATUS_COALESCE`]. Each call gets its own stream of every later
    /// change.
    pub fn traverse_events(&self) -> async_channel::Receiver<Vec<TraverseSourceStatus>> {
        let (sender, receiver) = async_channel::unbounded();
        self.shared.status_listeners.lock().unwrap().push(sender);
        receiver
    }

    /// Answer a hosting query from a client of any transport.
    pub fn hosting(&self, action: HostingAction) -> Result<HostingState, ProtocolError> {
        self.shared.hosting(action)
    }

    /// A handle that answers hosting queries from any thread, for a caller
    /// that must not wait on the UI thread: joining a group dials another
    /// machine.
    pub fn hosting_handle(&self) -> HostingHandle {
        HostingHandle(self.shared.clone())
    }

    pub fn shutdown(self) {
        self.group_sync.abort();
        drop(self.advertisement);
        for refresh in self.refresh {
            refresh.abort();
        }
        self.status.abort();
        if let Some(expiry) = self.shared.expiry.lock().unwrap().take() {
            expiry.abort();
        }
        let router = self.router;
        block_on(async move {
            let _ = router.shutdown().await;
        });
    }
}

/// The endpoint's bind failure, with the kind of the socket error under it,
/// so a caller can tell a port another process holds from other failures.
fn bind_error(error: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    let kind = std::iter::successors(
        Some(&error as &(dyn std::error::Error + 'static)),
        |error| error.source(),
    )
    .find_map(|error| error.downcast_ref::<io::Error>())
    .map_or(io::ErrorKind::Other, io::Error::kind);
    io::Error::new(kind, error)
}

/// The endpoint's UDP port: the IPv4 socket's, else the IPv6 one's. With
/// [`HostConfig::bind_port`] both families share it.
fn bound_port(endpoint: &Endpoint) -> Option<u16> {
    let bound = endpoint.bound_sockets();
    let v4 = bound.iter().find(|addr| addr.is_ipv4());
    let v6 = bound.iter().find(|addr| addr.is_ipv6());
    v4.or(v6).map(|addr| addr.port())
}

/// Advertise the endpoint's UDP port on the LAN; see [`lan`]. Without a
/// port shared by both address families only IPv4 is advertised, since one
/// SRV record carries one port. Unavailable mDNS is logged, not fatal.
fn advertise(endpoint: &Endpoint, host_name: &str) -> Option<lan::Advertisement> {
    let port = bound_port(endpoint)?;
    let ipv6 = endpoint
        .bound_sockets()
        .iter()
        .any(|addr| addr.is_ipv6() && addr.port() == port);
    match lan::Advertisement::start(&endpoint.id(), host_name, port, ipv6) {
        Ok(advertisement) => Some(advertisement),
        Err(error) => {
            log::warn!("not advertising this machine on the LAN: {error}");
            None
        }
    }
}

/// What invitations list for the Traverse instances this machine publishes
/// to, in configured order; see [`PairInvite::traverse`]. The official
/// instance alone is the empty list, which the link leaves out.
fn invite_traverse(sources: &[ManifestSource]) -> Vec<String> {
    match sources {
        [] => vec![TRAVERSE_OFF.to_owned()],
        [ManifestSource::Official] => Vec::new(),
        sources => sources.iter().map(source_name).collect(),
    }
}

/// A source as invitations and the hosting status name it.
fn source_name(source: &ManifestSource) -> String {
    match source {
        ManifestSource::Official => TRAVERSE_OFFICIAL.to_owned(),
        ManifestSource::Custom(base) => base.to_string(),
    }
}

/// The shortest gap between two [`TraverseHost::traverse_events`]: a burst
/// of changes is one event.
pub const STATUS_COALESCE: Duration = Duration::from_secs(1);

/// Tell the status listeners what changed: woken by a new network report,
/// a home relay's connection, a manifest fetch or a lookup check, then
/// coalesced, and only sent when the status differs from the last sent.
async fn watch_status(shared: std::sync::Weak<Shared>) {
    let Some((mut report, mut home, changed)) = shared.upgrade().map(|shared| {
        (
            shared.endpoint.net_report(),
            shared.endpoint.home_relay_status(),
            shared.status_changed.clone(),
        )
    }) else {
        return;
    };
    let mut last = None;
    loop {
        tokio::select! {
            updated = report.updated() => if updated.is_err() { return },
            updated = home.updated() => if updated.is_err() { return },
            () = changed.notified() => {}
        }
        tokio::time::sleep(STATUS_COALESCE).await;
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let status = shared.traverse_status();
        if last.as_ref() != Some(&status) {
            shared
                .status_listeners
                .lock()
                .unwrap()
                .retain(|listener| listener.try_send(status.clone()).is_ok());
            last = Some(status);
        }
    }
}

/// Every source's relays in one map. A relay two sources list keeps the
/// first source's configuration.
fn union_relays(manifests: &[Option<Arc<Manifest>>]) -> RelayMap {
    let union = RelayMap::empty();
    for map in manifests
        .iter()
        .flatten()
        .map(|manifest| manifest.relay_map())
    {
        for config in map.relays::<Vec<Arc<_>>>() {
            if !union.contains(&config.url) {
                union.insert(config.url.clone(), config);
            }
        }
    }
    union
}

impl Shared {
    /// Put `manifest` in effect for the source at `index`, leaving the
    /// others' as they are: the endpoint moves to the union of every
    /// source's relays through `insert_relay`/`remove_relay`, and its
    /// publishers and resolvers are rebuilt for every source.
    async fn apply_manifest(&self, index: usize, manifest: Arc<Manifest>) {
        log::info!("applying a Traverse manifest");
        let mut manifests = self.manifests.lock().await;
        manifests[index] = Some(manifest);
        live::sync_relays(&self.endpoint, &self.relays, &union_relays(&manifests)).await;
        live::install_lookups(
            &self.endpoint,
            manifests.iter().flatten().map(AsRef::as_ref),
            Some(&self.lookup_checks),
            None,
        );
    }

    fn traverse_status(&self) -> Vec<TraverseSourceStatus> {
        let report = self.endpoint.net_report().get();
        let latency = |url: &iroh::RelayUrl| {
            report.as_ref().and_then(|report| {
                report
                    .relay_latency
                    .iter()
                    .filter(|(_, relay, _)| *relay == url)
                    .map(|(_, _, latency)| latency)
                    .min()
            })
        };
        let home = self.endpoint.home_relay_status().get();
        self.loaders
            .iter()
            .map(|loader| {
                let (manifest, status) = loader.status();
                let relays = manifest
                    .iter()
                    .flat_map(|manifest| &manifest.relays)
                    .map(|relay| {
                        let url = iroh::RelayUrl::from(relay.url.clone());
                        let home = home.iter().find(|status| status.url() == &url);
                        TraverseRelayStatus {
                            url: relay.url.to_string(),
                            region: relay.region.clone(),
                            latency_ms: latency(&url)
                                .map(|latency| latency.as_millis().min(u32::MAX.into()) as u32),
                            home: home.is_some(),
                            error: home
                                .filter(|status| !status.is_connected())
                                .and_then(|status| status.last_error())
                                .map(ToString::to_string),
                        }
                    })
                    .collect();
                let lookups = manifest
                    .iter()
                    .flat_map(|manifest| manifest.pkarr_urls())
                    .map(|url| self.lookup_checks.status(url))
                    .collect();
                TraverseSourceStatus {
                    source: source_name(loader.source()),
                    manifest: status,
                    relays,
                    lookups,
                }
            })
            .collect()
    }

    fn mint(self: &Arc<Self>) -> Invitation {
        let mut random = [0_u8; tcode_client::pairing::SECRET_BYTES];
        getrandom::fill(&mut random).expect("the OS random source is available");
        let addr = self.snapshot();
        let mut state = self.state.lock().unwrap();
        let invitation = Invitation {
            invite: PairInvite {
                host_id: addr.id,
                secret: encode_secret(&random),
                space: None,
                group: group_id(&state.identity),
                traverse: self.traverse.clone(),
                relay: addr.relay,
                port: addr.port,
            },
            expires_at: Instant::now() + INVITATION_LIFETIME,
        };
        state.invitation = Some(ActiveInvitation {
            invitation: invitation.clone(),
            failures: 0,
        });
        // The timer holds no reference that would keep a stopped host alive,
        // and checks it is still ending the invitation it was set for.
        let shared = Arc::downgrade(self);
        let secret = invitation.invite.secret.clone();
        let expires_at = invitation.expires_at + Duration::from_millis(10);
        let expiry = crate::runtime::runtime().spawn(async move {
            tokio::time::sleep_until(expires_at.into()).await;
            if let Some(shared) = shared.upgrade() {
                shared.expire(&secret);
            }
        });
        if let Some(previous) = self.expiry.lock().unwrap().replace(expiry.abort_handle()) {
            previous.abort();
        }
        self.notify(Some(invitation.clone()));
        invitation
    }

    /// The invitation `secret` ran out of time.
    fn expire(&self, secret: &str) {
        let mut state = self.state.lock().unwrap();
        let expired = state.invitation.as_ref().is_some_and(|active| {
            active.invitation.invite.secret == secret && active.invitation.remaining().is_zero()
        });
        if expired {
            state.invitation = None;
            self.notify(None);
        }
    }

    /// Under the state lock, so listeners hear changes in the order they
    /// happened.
    fn notify(&self, invitation: Option<Invitation>) {
        self.listeners
            .lock()
            .unwrap()
            .retain(|listener| listener.try_send(invitation.clone()).is_ok());
    }

    /// The unexpired invitation with `addr` as its routing hints. The
    /// secret never changes; only the relay the link names does.
    fn current_invitation(
        &self,
        state: &State,
        addr: &EndpointAddrSnapshot,
    ) -> Option<(Invitation, Duration)> {
        if !self.allow_pairing || !state.identity.pairing_enabled {
            return None;
        }
        let active = state.invitation.as_ref()?;
        let remaining = active.invitation.remaining();
        if remaining.is_zero() {
            return None;
        }
        let invitation = Invitation {
            // The group can change while an invitation is live.
            invite: PairInvite {
                relay: addr.relay.clone(),
                port: addr.port,
                group: group_id(&state.identity),
                ..active.invitation.invite.clone()
            },
            expires_at: active.invitation.expires_at,
        };
        Some((invitation, remaining))
    }

    fn snapshot(&self) -> EndpointAddrSnapshot {
        let endpoint = &self.endpoint;
        EndpointAddrSnapshot {
            id: endpoint.id().to_string(),
            relay: endpoint.addr().relay_urls().next().map(ToString::to_string),
            port: bound_port(endpoint).unwrap_or_default(),
        }
    }

    fn set_pairing_enabled(&self, enabled: bool) {
        let mut state = self.state.lock().unwrap();
        let previous = state.identity.clone();
        state.identity.pairing_enabled = enabled;
        state.identity.policy_revision += 1;
        if let Err(error) = state.identity.save() {
            state.identity = previous;
            log::error!("could not persist the pairing switch: {error}");
            return;
        }
        if !enabled && state.invitation.take().is_some() {
            self.notify(None);
        }
    }

    fn devices(&self) -> Vec<DeviceInfo> {
        let state = self.state.lock().unwrap();
        state
            .identity
            .devices
            .iter()
            .map(|device| {
                let live = device
                    .id
                    .parse::<EndpointId>()
                    .ok()
                    .and_then(|id| state.live.get(&id))
                    .and_then(|connections| connections.first())
                    .map(path_info);
                DeviceInfo {
                    id: device.id.clone(),
                    name: device.name.clone(),
                    platform: device.platform.clone(),
                    created_unix: device.created_unix,
                    access: device_access(device),
                    live,
                }
            })
            .collect()
    }

    fn spaces(&self, state: &State, addr: &EndpointAddrSnapshot) -> Vec<SpaceInfo> {
        state
            .identity
            .spaces
            .iter()
            .map(|space| SpaceInfo {
                id: space.id.clone(),
                name: space.name.clone(),
                created_unix: space.created_unix,
                project_ids: space.project_ids.clone(),
                link: (self.allow_pairing
                    && state.identity.pairing_enabled
                    && space.link_enabled
                    && space.link_failures < MAX_PAIRING_FAILURES)
                    .then(|| {
                        pair_url(&PairInvite {
                            host_id: addr.id.clone(),
                            secret: space.secret.clone(),
                            space: Some(space.id.clone()),
                            group: None,
                            traverse: self.traverse.clone(),
                            relay: addr.relay.clone(),
                            port: addr.port,
                        })
                    }),
                link_enabled: space.link_enabled,
                link_dead: space.link_failures >= MAX_PAIRING_FAILURES,
                members: state
                    .identity
                    .devices
                    .iter()
                    .filter(|device| member_of(device, &space.id))
                    .map(|device| hosted_device(state, device))
                    .collect(),
            })
            .collect()
    }

    fn space_action(&self, action: SpaceAction) -> io::Result<Option<String>> {
        let mut state = self.state.lock().unwrap();
        let previous = state.identity.clone();
        let mut affected = Vec::new();
        let mut created = None;
        match action {
            SpaceAction::Create { name } => {
                let name = space_name(name)?;
                let id = uuid::Uuid::new_v4().to_string();
                state.identity.spaces.push(SpaceRecord {
                    id: id.clone(),
                    name,
                    created_unix: now_unix(),
                    project_ids: Vec::new(),
                    secret: new_secret(),
                    link_enabled: true,
                    link_failures: 0,
                });
                created = Some(id);
            }
            SpaceAction::MoveMember {
                device_id,
                space_id,
            } => {
                if !state
                    .identity
                    .spaces
                    .iter()
                    .any(|space| space.id == space_id)
                {
                    return Err(missing("space"));
                }
                let device = state
                    .identity
                    .devices
                    .iter_mut()
                    .find(|device| device.id == device_id)
                    .ok_or_else(|| missing("device"))?;
                device.access = DeviceGrant::Spaces(vec![space_id]);
                affected.push(device_id);
            }
            SpaceAction::RemoveMember {
                device_id,
                regenerate_link,
            } => {
                if regenerate_link {
                    let device = state
                        .identity
                        .devices
                        .iter()
                        .find(|device| device.id == device_id)
                        .ok_or_else(|| missing("device"))?;
                    let DeviceGrant::Spaces(ids) = &device.access else {
                        return Err(missing("member space"));
                    };
                    let ids = ids.clone();
                    for space in &mut state.identity.spaces {
                        if ids.contains(&space.id) {
                            space.secret = new_secret();
                            space.link_failures = 0;
                            space.link_enabled = true;
                        }
                    }
                }
                state.identity.remove(&device_id);
                affected.push(device_id);
            }
            action => {
                let id = match &action {
                    SpaceAction::Rename { id, .. }
                    | SpaceAction::Delete { id }
                    | SpaceAction::SetProjects { id, .. }
                    | SpaceAction::RegenerateLink { id }
                    | SpaceAction::SetLinkEnabled { id, .. } => id,
                    _ => unreachable!(),
                };
                let index = state
                    .identity
                    .spaces
                    .iter()
                    .position(|space| &space.id == id)
                    .ok_or_else(|| missing("space"))?;
                if matches!(
                    action,
                    SpaceAction::Rename { .. }
                        | SpaceAction::Delete { .. }
                        | SpaceAction::SetProjects { .. }
                ) {
                    affected = state
                        .identity
                        .devices
                        .iter()
                        .filter(|device| member_of(device, id))
                        .map(|device| device.id.clone())
                        .collect();
                }
                match action {
                    SpaceAction::Rename { name, .. } => {
                        state.identity.spaces[index].name = space_name(name)?
                    }
                    SpaceAction::Delete { id } => {
                        state.identity.spaces.remove(index);
                        state.identity.devices.retain_mut(|device| {
                            if let DeviceGrant::Spaces(ids) = &mut device.access {
                                ids.retain(|space| space != &id);
                                !ids.is_empty()
                            } else {
                                true
                            }
                        });
                    }
                    SpaceAction::SetProjects { project_ids, .. } => {
                        state.identity.spaces[index].project_ids = project_ids
                    }
                    SpaceAction::RegenerateLink { .. } => {
                        let space = &mut state.identity.spaces[index];
                        space.secret = new_secret();
                        space.link_failures = 0;
                        space.link_enabled = true;
                    }
                    SpaceAction::SetLinkEnabled { enabled, .. } => {
                        state.identity.spaces[index].link_enabled = enabled
                    }
                    _ => unreachable!(),
                }
            }
        }
        state.identity.policy_revision += 1;
        if let Err(error) = state.identity.save() {
            state.identity = previous;
            return Err(error);
        }
        for id in affected {
            let code = if state.identity.devices.iter().any(|device| device.id == id) {
                4
            } else {
                3
            };
            close_device(&mut state, &id, code, b"space changed");
        }
        Ok(created)
    }

    fn principal(&self, remote: EndpointId) -> Option<Principal> {
        let state = self.state.lock().unwrap();
        let device = state
            .identity
            .devices
            .iter()
            .find(|device| device.id == remote.to_string())?;
        match &device.access {
            DeviceGrant::Full => Some(Principal::Full),
            DeviceGrant::Spaces(ids) => {
                let [id] = ids.as_slice() else {
                    return None;
                };
                let space = state.identity.spaces.iter().find(|space| &space.id == id)?;
                Some(Principal::Space {
                    policy_revision: state.identity.policy_revision,
                    space_id: space.id.clone(),
                    space_name: space.name.clone(),
                    project_ids: space.project_ids.clone(),
                    device_id: device.id.clone(),
                    device_name: device.name.clone(),
                })
            }
        }
    }

    fn revoke(&self, id: &str) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        let previous = state.identity.clone();
        let by = self.endpoint.id().to_string();
        if let Some(group) = &mut state.identity.group
            && let Some(device) = group
                .state
                .device(id)
                .filter(|device| device.entry.is_member())
        {
            let (name, platform) = (device.name.clone(), device.platform.clone());
            group
                .state
                .set_device(id, &name, platform.as_deref(), Membership::Removed, &by);
            return self.commit(&mut state, previous).inspect_err(|error| {
                log::error!("could not persist the revocation: {error}");
            });
        }
        if !state.identity.remove(id) {
            return Ok(());
        }
        state.identity.policy_revision += 1;
        if let Err(error) = state.identity.save() {
            log::error!("could not persist the revocation: {error}");
            state.identity = previous;
            return Err(error);
        }
        close_device(&mut state, id, 3, b"revoked");
        Ok(())
    }

    fn hosting(self: &Arc<Self>, action: HostingAction) -> Result<HostingState, ProtocolError> {
        let mut created_space_id = None;
        match action {
            HostingAction::Spaces(action) => {
                created_space_id = self.space_action(action).map_err(|error| ProtocolError {
                    code: "space_update_failed".into(),
                    message: error.to_string(),
                })?;
            }
            HostingAction::State => {}
            HostingAction::SetEnabled(enabled) => {
                self.set_pairing_enabled(enabled);
                if enabled && self.allow_pairing {
                    self.mint();
                }
            }
            HostingAction::NewInvitation => {
                if self.allow_pairing && self.state.lock().unwrap().identity.pairing_enabled {
                    self.mint();
                }
            }
            HostingAction::RevokeDevice(id) => self.revoke(&id).map_err(revoke_error)?,
            HostingAction::Group(action) => {
                self.group_action(action).map_err(|error| ProtocolError {
                    code: "group_update_failed".into(),
                    message: error.to_string(),
                })?;
            }
        }
        let addr = self.snapshot();
        let state = self.state.lock().unwrap();
        let enabled = self.allow_pairing && state.identity.pairing_enabled;
        let (invite, expires_in_secs) = self
            .current_invitation(&state, &addr)
            .map(|(invitation, remaining)| (Some(invitation.url()), remaining.as_secs()))
            .unwrap_or((None, 0));
        Ok(HostingState {
            created_space_id,
            spaces: self.spaces(&state, &addr),
            enabled,
            expires_in_secs,
            host_id: addr.id.clone(),
            host_name: state.identity.host_name.clone(),
            invite,
            devices: state
                .identity
                .devices
                .iter()
                .map(|device| hosted_device(&state, device))
                .collect(),
            traverse: self.traverse_status(),
            group: self.group_info(&state, &addr),
        })
    }

    /// Exchange an invitation's secret for a place on the allow list.
    /// Expiry, single use and the failure budget are judged under the one
    /// lock every requester shares, and the device is on disk before it is
    /// told so.
    fn pair(
        &self,
        remote: EndpointId,
        secret: &str,
        device: &DeviceClaim,
        space: Option<&str>,
    ) -> HostLine {
        let mut state = self.state.lock().unwrap();
        if !self.allow_pairing || !state.identity.pairing_enabled {
            return HostLine::PairRejected {
                reason: PairRejection::Disabled,
            };
        }
        if let Some(space_id) = space {
            let Some(index) = state
                .identity
                .spaces
                .iter()
                .position(|space| space.id == space_id)
            else {
                return HostLine::PairRejected {
                    reason: PairRejection::SpaceUnavailable,
                };
            };
            let space = &state.identity.spaces[index];
            if !space.link_enabled || space.link_failures >= MAX_PAIRING_FAILURES {
                return HostLine::PairRejected {
                    reason: PairRejection::SpaceUnavailable,
                };
            }
            if state.identity.is_paired(&remote) {
                return HostLine::PairRejected {
                    reason: PairRejection::AlreadyMember,
                };
            }
            let previous = state.identity.clone();
            if !constant_time_eq(space.secret.as_bytes(), secret.as_bytes()) {
                state.identity.spaces[index].link_failures += 1;
                state.identity.policy_revision += 1;
                if let Err(error) = state.identity.save() {
                    log::error!("could not record the space link failure: {error}");
                    state.identity = previous;
                    return HostLine::PairRejected {
                        reason: PairRejection::Busy,
                    };
                }
                return HostLine::PairRejected {
                    reason: PairRejection::Invalid,
                };
            }
            let space_name = space.name.clone();
            let (name, platform) = device.normalized();
            state.identity.admit(&remote, name, platform);
            state
                .identity
                .devices
                .iter_mut()
                .find(|device| device.id == remote.to_string())
                .unwrap()
                .access = DeviceGrant::Spaces(vec![space_id.into()]);
            state.identity.policy_revision += 1;
            if let Err(error) = state.identity.save() {
                log::error!("could not record the space member: {error}");
                state.identity = previous;
                return HostLine::PairRejected {
                    reason: PairRejection::Busy,
                };
            }
            return HostLine::Paired {
                host_name: state.identity.host_name.clone(),
                space_name: Some(space_name),
                group: None,
            };
        }
        if !self.take_invitation(&mut state, secret) {
            return HostLine::PairRejected {
                reason: PairRejection::Invalid,
            };
        }
        let (name, platform) = device.normalized();
        let previous = state.identity.clone();
        state.identity.admit(&remote, name, platform);
        self.introduce(&mut state.identity, &remote.to_string());
        if let Err(error) = self.commit(&mut state, previous) {
            log::error!("could not record the paired device: {error}");
            return HostLine::PairRejected {
                reason: PairRejection::Busy,
            };
        }
        HostLine::Paired {
            host_name: state.identity.host_name.clone(),
            space_name: None,
            group: roster(&state.identity, &remote.to_string()),
        }
    }

    /// Spend the active invitation on `secret`: single use, unexpired, and
    /// dropped after [`MAX_PAIRING_FAILURES`] wrong secrets. Devices and
    /// joining machines share it.
    fn take_invitation(&self, state: &mut State, secret: &str) -> bool {
        match state.invitation.as_mut() {
            None => false,
            Some(active) if active.invitation.remaining().is_zero() => {
                state.invitation = None;
                self.notify(None);
                false
            }
            Some(active) => {
                if constant_time_eq(
                    active.invitation.invite.secret.as_bytes(),
                    secret.as_bytes(),
                ) {
                    state.invitation = None;
                    self.notify(None);
                    true
                } else {
                    active.failures += 1;
                    if active.failures >= MAX_PAIRING_FAILURES {
                        state.invitation = None;
                        self.notify(None);
                    }
                    false
                }
            }
        }
    }

    /// Record a full device this machine admitted as a member of its group,
    /// made by this machine.
    fn introduce(&self, identity: &mut HostIdentity, id: &str) {
        let by = self.endpoint.id().to_string();
        let Some(group) = &mut identity.group else {
            return;
        };
        let Some(device) = identity
            .devices
            .iter()
            .find(|device| device.id == id && device.access == DeviceGrant::Full)
        else {
            return;
        };
        group.state.set_device(
            id,
            &device.name,
            device.platform.as_deref(),
            Membership::Member,
            &by,
        );
    }

    /// Apply the group to the allow list and write it, or put `previous`
    /// back and change nothing. Removed devices are disconnected; when the
    /// group's machines change, full devices reconnect to learn them; and the
    /// other members hear of any change to the group.
    fn commit(&self, state: &mut State, previous: HostIdentity) -> io::Result<()> {
        let local = existing_device(&self.data_dir).map(|(id, _)| id.to_string());
        let accept = self.allow_pairing && state.identity.pairing_enabled;
        let removed = state.identity.apply_group(accept, local.as_deref());
        state.identity.policy_revision += 1;
        let machines_before = member_machines(&previous);
        let group_before = previous.group.as_ref().map(|group| group.state.clone());
        if let Err(error) = state.identity.save() {
            state.identity = previous;
            return Err(error);
        }
        for id in &removed {
            close_device(state, id, 3, b"revoked");
        }
        if member_machines(&state.identity) != machines_before {
            let full: Vec<String> = state
                .identity
                .devices
                .iter()
                .filter(|device| device.access == DeviceGrant::Full)
                .map(|device| device.id.clone())
                .collect();
            for id in full {
                close_device(state, &id, 4, b"group changed");
            }
            self.write_local_hosts(
                state
                    .identity
                    .group
                    .as_ref()
                    .map(|group| group.state.roster()),
            );
        }
        if state.identity.group.as_ref().map(|group| &group.state) != group_before.as_ref() {
            self.group_wake.notify_one();
        }
        Ok(())
    }

    /// Keep the window sharing this data directory current with the group's
    /// machines, as a device of the group would be after connecting.
    fn write_local_hosts(&self, roster: Option<Roster>) {
        let Some(roster) = roster else {
            return;
        };
        if existing_device(&self.data_dir).is_none() {
            return;
        }
        let local = self.endpoint.id().to_string();
        if let Err(error) = crate::hosts::update_hosts(&self.data_dir, |hosts| {
            crate::hosts::apply_roster(hosts, &local, Some(&roster), Some(&local));
        }) {
            log::error!("could not save the group's machines: {error}");
        }
    }

    /// Admit a `tcode/1` connection if its peer is paired, registering it
    /// under the same lock so a concurrent revocation either sees it or
    /// refuses it.
    fn admit(&self, connection: &Connection) -> bool {
        let remote = connection.remote_id();
        let mut state = self.state.lock().unwrap();
        if !state.identity.is_paired(&remote) {
            return false;
        }
        state
            .live
            .entry(remote)
            .or_default()
            .push(connection.clone());
        true
    }

    fn release(&self, connection: &Connection) {
        let mut state = self.state.lock().unwrap();
        if let Some(connections) = state.live.get_mut(&connection.remote_id()) {
            connections.retain(|live| live.stable_id() != connection.stable_id());
            if connections.is_empty() {
                state.live.remove(&connection.remote_id());
            }
        }
    }

    /// The hosting list shows what the device calls itself now. It is
    /// already authenticated, so a failed write is not a reason to refuse.
    fn refresh_device(&self, remote: EndpointId, device: &DeviceClaim) {
        if !device.is_valid() {
            return;
        }
        let mut state = self.state.lock().unwrap();
        if !state.identity.is_paired(&remote) {
            return;
        }
        let (name, platform) = device.normalized();
        let changed = state.identity.devices.iter().any(|record| {
            record.id == remote.to_string() && (record.name != name || record.platform != platform)
        });
        if !changed {
            return;
        }
        let previous = state.identity.clone();
        state.identity.admit(&remote, name, platform);
        state.identity.policy_revision += 1;
        if let Err(error) = state.identity.save() {
            state.identity = previous;
            log::warn!("could not record the connecting device's details: {error}");
        }
    }

    fn host_name(&self) -> String {
        self.state.lock().unwrap().identity.host_name.clone()
    }
}

/// How often the group is synced when nothing prompts it.
const GROUP_SYNC_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Budget for reaching another member and exchanging one line each way.
const GROUP_DIAL_BUDGET: Duration = Duration::from_secs(20);

/// Answers hosting queries from any thread; see [`TraverseHost::hosting_handle`].
#[derive(Clone)]
pub struct HostingHandle(Arc<Shared>);

impl HostingHandle {
    pub fn hosting(&self, action: HostingAction) -> Result<HostingState, ProtocolError> {
        self.0.hosting(action)
    }
}

/// Where a connection reached its peer: its relay and direct addresses.
struct Hints {
    relay: Option<String>,
    addrs: Vec<String>,
}

/// Only the selected direct path is kept: the other paths are candidates
/// that may never have answered, and see [`Shared::this_machine`] for what
/// a wrong candidate costs.
fn connection_hints(connection: &Connection) -> Hints {
    let mut hints = Hints {
        relay: None,
        addrs: Vec::new(),
    };
    for path in connection.paths().iter() {
        match path.remote_addr() {
            TransportAddr::Ip(addr) if path.is_selected() => hints.addrs.push(addr.to_string()),
            TransportAddr::Relay(url) => hints.relay = Some(url.to_string()),
            _ => {}
        }
    }
    hints
}

/// What a machine's invitation names while the machine is in a group.
fn group_id(identity: &HostIdentity) -> Option<String> {
    identity.group.as_ref().map(|group| group.state.id.clone())
}

/// The group's machines, for `device` while it is a member of the group:
/// a full device left out of the group would only learn of machines that
/// refuse it.
fn roster(identity: &HostIdentity, device: &str) -> Option<Roster> {
    let group = identity.group.as_ref()?;
    group
        .state
        .device(device)
        .is_some_and(|device| device.entry.is_member())
        .then(|| group.state.roster())
}

fn member_machines(identity: &HostIdentity) -> Vec<String> {
    identity
        .group
        .iter()
        .flat_map(|group| group.state.member_machines())
        .map(|machine| machine.id.clone())
        .collect()
}

fn group_name(name: String) -> io::Result<String> {
    if !crate::group::valid_name(&name) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid group name",
        ));
    }
    Ok(name.trim().into())
}

fn rejection_error(reason: GroupRejection) -> io::Error {
    let message = match reason {
        GroupRejection::Invalid => "the invitation is wrong, expired or already used",
        GroupRejection::Disabled => "that machine is not accepting new devices",
        GroupRejection::Busy => "that machine could not record the change; try again",
        GroupRejection::NotMember => "that machine is not in this group",
        GroupRejection::Full => "the group has as many machines as it can hold",
    };
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

/// Run `future` to completion from a hosting query, which may come from the
/// UI thread or from a task on the shared runtime.
fn run_blocking<F: std::future::Future>(future: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(future)),
        Err(_) => block_on(future),
    }
}

impl Shared {
    /// This machine as it reports itself to the group: no direct addresses.
    /// An interface address is only meaningful on its own network (every
    /// Docker host's bridge is 172.17.0.1), and iroh 1.3 fails a whole dial,
    /// relay included, when any candidate address is answered by a different
    /// endpoint, then keeps failing for that id until the dialling endpoint
    /// restarts. Members and devices get only addresses a connection to this
    /// machine actually selected.
    fn this_machine(&self, identity: &HostIdentity, addr: &EndpointAddrSnapshot) -> GroupMachine {
        GroupMachine::new(
            addr.id.clone(),
            identity.host_name.clone(),
            self.traverse.clone(),
            addr.relay.clone(),
            Vec::new(),
        )
    }

    fn group_info(&self, state: &State, addr: &EndpointAddrSnapshot) -> Option<GroupInfo> {
        let group = state.identity.group.as_ref()?;
        Some(GroupInfo {
            id: group.state.id.clone(),
            name: group.state.name.clone(),
            machines: group
                .state
                .member_machines()
                .map(|machine| GroupMachineInfo {
                    id: machine.id.clone(),
                    name: machine.name.clone(),
                    this_machine: machine.id == addr.id,
                })
                .collect(),
            declined: group
                .declined
                .iter()
                .filter_map(|declined| group.state.device(&declined.id))
                .map(|device| HostedDevice {
                    access: DeviceAccess::Full,
                    id: device.id.clone(),
                    name: device.name.clone(),
                    created_unix: device.entry.changed_unix,
                    platform: device.platform.clone(),
                    path: None,
                })
                .collect(),
        })
    }

    fn roster(&self, device: EndpointId) -> Option<Roster> {
        roster(&self.state.lock().unwrap().identity, &device.to_string())
    }

    /// The other member machines, to sync with.
    fn peers(&self) -> Vec<GroupMachine> {
        let id = self.endpoint.id().to_string();
        let state = self.state.lock().unwrap();
        state
            .identity
            .group
            .iter()
            .flat_map(|group| group.state.member_machines())
            .filter(|machine| machine.id != id)
            .cloned()
            .collect()
    }

    fn group_action(self: &Arc<Self>, action: GroupAction) -> io::Result<()> {
        let action = match action {
            GroupAction::Join { link, address } => {
                return self.join_group(&link, address.as_deref());
            }
            GroupAction::Leave => return self.leave_group(),
            action => action,
        };
        let by = self.endpoint.id().to_string();
        let addr = self.snapshot();
        let mut state = self.state.lock().unwrap();
        let previous = state.identity.clone();
        let mut removed = None;
        let no_group = || io::Error::new(io::ErrorKind::NotFound, "this machine is not in a group");
        match action {
            GroupAction::Create { name } => {
                if state.identity.group.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "this machine is already in a group",
                    ));
                }
                let mut group = GroupState::new(new_secret(), group_name(name)?);
                group.set_machine(
                    self.this_machine(&state.identity, &addr),
                    Membership::Member,
                    &by,
                );
                state.identity.group = Some(GroupRecord {
                    state: group,
                    declined: Vec::new(),
                });
                self.bring_devices(&mut state.identity);
            }
            GroupAction::RemoveMachine(id) => {
                if id == by {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "leave the group to remove this machine",
                    ));
                }
                let group = state.identity.group.as_mut().ok_or_else(no_group)?;
                let machine = group
                    .state
                    .machine(&id)
                    .filter(|machine| machine.entry.is_member())
                    .cloned()
                    .ok_or_else(|| missing("machine"))?;
                group
                    .state
                    .set_machine(machine.clone(), Membership::Removed, &by);
                removed = Some(machine);
            }
            GroupAction::Admit(id) => {
                let group = state.identity.group.as_mut().ok_or_else(no_group)?;
                let device = group
                    .state
                    .device(&id)
                    .filter(|device| device.entry.is_member())
                    .cloned()
                    .ok_or_else(|| missing("group device"))?;
                group.declined.retain(|declined| declined.id != id);
                if !state.identity.devices.iter().any(|record| record.id == id) {
                    state.identity.devices.push(DeviceRecord {
                        id,
                        name: device.name,
                        platform: device.platform,
                        created_unix: now_unix(),
                        access: DeviceGrant::Full,
                    });
                }
            }
            GroupAction::Join { .. } | GroupAction::Leave => unreachable!(),
        }
        self.commit(&mut state, previous)?;
        // The members no longer sync with it, so it is told once now; if it
        // is unreachable it learns from the next member it syncs with.
        if let Some(machine) = removed {
            crate::runtime::runtime().spawn(self.clone().sync_with(machine));
        }
        Ok(())
    }

    /// Bring every full-access device and the device sharing this data
    /// directory into the group just created.
    fn bring_devices(&self, identity: &mut HostIdentity) {
        let full: Vec<String> = identity
            .devices
            .iter()
            .filter(|device| device.access == DeviceGrant::Full)
            .map(|device| device.id.clone())
            .collect();
        for id in full {
            self.introduce(identity, &id);
        }
        if let Some((id, claim)) = existing_device(&self.data_dir)
            && let Some(group) = &mut identity.group
        {
            let (name, platform) = claim.normalized();
            group.state.set_device(
                &id.to_string(),
                &name,
                platform.as_deref(),
                Membership::Member,
                &self.endpoint.id().to_string(),
            );
        }
    }

    /// Join the group of another member with its invitation: that machine
    /// admits this one and every full-access device it brings, and answers
    /// with the group.
    fn join_group(self: &Arc<Self>, link: &str, address: Option<&str>) -> io::Result<()> {
        let invalid =
            |message: &str| io::Error::new(io::ErrorKind::InvalidInput, message.to_owned());
        let invite = parse_pair_url(link)
            .filter(|invite| invite.group.is_some())
            .ok_or_else(|| invalid("not the invitation of a machine in a group"))?;
        let group_id = invite.group.clone().unwrap_or_default();
        let typed: Vec<String> = match address {
            Some(address) => vec![
                std::net::SocketAddr::new(
                    address
                        .trim()
                        .parse()
                        .map_err(|_| invalid("invalid address"))?,
                    invite.port,
                )
                .to_string(),
            ],
            None => Vec::new(),
        };
        let id = self.endpoint.id().to_string();
        if invite.host_id == id {
            return Err(invalid("that is this machine's own invitation"));
        }
        let addr = self.snapshot();
        let joining = {
            let state = self.state.lock().unwrap();
            if state.identity.group.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "this machine is already in a group",
                ));
            }
            let mut brought: Vec<JoiningDevice> = state
                .identity
                .devices
                .iter()
                .filter(|device| device.access == DeviceGrant::Full)
                .map(|device| JoiningDevice {
                    id: device.id.clone(),
                    name: device.name.clone(),
                    platform: device.platform.clone(),
                })
                .collect();
            if let Some((device_id, claim)) = existing_device(&self.data_dir) {
                let (name, platform) = claim.normalized();
                brought.push(JoiningDevice {
                    id: device_id.to_string(),
                    name,
                    platform,
                });
            }
            let this = self.this_machine(&state.identity, &addr);
            JoiningMachine {
                name: this.name,
                traverse: this.traverse,
                relay: this.relay,
                addrs: this.addrs,
                devices: brought,
            }
        };
        let line = GroupLine::Join {
            group: group_id.clone(),
            secret: invite.secret.clone(),
            machine: joining,
        };
        let (reply, hints) =
            run_blocking(self.dial_group(&invite.host_id, invite.relay.as_deref(), &typed, &line))?;
        let joined = match reply {
            GroupLine::Joined { state }
                if state.is_valid() && state.id == group_id && state.is_member_machine(&id) =>
            {
                state
            }
            GroupLine::Rejected { reason } => return Err(rejection_error(reason)),
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unexpected group reply {other:?}"),
                ));
            }
        };
        let mut state = self.state.lock().unwrap();
        if state.identity.group.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "this machine is already in a group",
            ));
        }
        let previous = state.identity.clone();
        let mut joined = joined;
        joined.learn_hints(&invite.host_id, hints.relay, hints.addrs);
        state.identity.group = Some(GroupRecord {
            state: joined,
            declined: Vec::new(),
        });
        self.commit(&mut state, previous)
    }

    /// Tell the other members this machine is leaving, then forget the
    /// group. Every device stays on the allow list, now as directly paired.
    fn leave_group(self: &Arc<Self>) -> io::Result<()> {
        let id = self.endpoint.id().to_string();
        let leaving = {
            let state = self.state.lock().unwrap();
            let Some(group) = &state.identity.group else {
                return Ok(());
            };
            let mut leaving = group.state.clone();
            if let Some(this) = leaving.machine(&id).cloned() {
                leaving.set_machine(this, Membership::Removed, &id);
            }
            leaving
        };
        let peers: Vec<GroupMachine> = leaving
            .member_machines()
            .filter(|machine| machine.id != id)
            .cloned()
            .collect();
        // A member unreachable now learns of it from the next member it
        // syncs with.
        run_blocking(async {
            let line = GroupLine::Sync { state: leaving };
            let tells: Vec<_> = peers
                .into_iter()
                .map(|peer| {
                    let shared = self.clone();
                    let line = line.clone();
                    tokio::spawn(async move {
                        let told = shared
                            .dial_group(&peer.id, peer.relay.as_deref(), &peer.addrs, &line)
                            .await;
                        if let Err(error) = told {
                            log::debug!("could not tell {} this machine left: {error}", peer.id);
                        }
                    })
                })
                .collect();
            for tell in tells {
                let _ = tell.await;
            }
        });
        let mut state = self.state.lock().unwrap();
        let previous = state.identity.clone();
        self.forget_group(&mut state.identity);
        let gone = previous.group.as_ref().map(|group| group.state.roster());
        self.commit(&mut state, previous)?;
        self.write_local_hosts(gone.map(|mut roster| {
            for machine in &mut roster.machines {
                machine.member = false;
            }
            roster
        }));
        Ok(())
    }

    fn forget_group(&self, identity: &mut HostIdentity) {
        identity.group = None;
    }

    /// Dial `id` on `tcode/group/1`, send `line` and read the answer.
    async fn dial_group(
        &self,
        id: &str,
        relay: Option<&str>,
        addrs: &[String],
        line: &GroupLine,
    ) -> io::Result<(GroupLine, Hints)> {
        let addr = crate::client::dial_addr(id, relay, addrs)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid machine id"))?;
        let exchange = async {
            let connection = self
                .endpoint
                .connect(addr, ALPN_GROUP)
                .await
                .map_err(|error| io::Error::other(format!("{error:#}")))?;
            let (mut send, recv) = connection.open_bi().await.map_err(io::Error::other)?;
            wire::write_line(&mut send, line).await?;
            send.finish().map_err(io::Error::other)?;
            let mut reader = wire::reader(recv);
            let reply = wire::read_control_within::<GroupLine>(&mut reader, MAX_GROUP_LINE).await;
            let hints = connection_hints(&connection);
            connection.close(0_u32.into(), b"done");
            Ok((reply?, hints))
        };
        tokio::time::timeout(GROUP_DIAL_BUDGET, exchange)
            .await
            .map_err(|_| timed_out("group exchange"))?
    }

    /// Exchange states with one member and keep what outranks ours.
    async fn sync_with(self: Arc<Self>, peer: GroupMachine) {
        let Some(ours) = self
            .state
            .lock()
            .unwrap()
            .identity
            .group
            .as_ref()
            .map(|group| group.state.clone())
        else {
            return;
        };
        let line = GroupLine::Sync { state: ours };
        match self
            .dial_group(&peer.id, peer.relay.as_deref(), &peer.addrs, &line)
            .await
        {
            Ok((GroupLine::Sync { state }, hints)) => {
                let mut guard = self.state.lock().unwrap();
                if let Err(error) = self.absorb(&mut guard, &peer.id, state, hints) {
                    log::warn!("could not record the group state from {}: {error}", peer.id);
                }
            }
            Ok((reply, _)) => log::debug!("group member {} answered {reply:?}", peer.id),
            Err(error) => log::debug!("could not reach group member {}: {error}", peer.id),
        }
    }

    /// Merge a member's state and the hints of the connection that carried
    /// it. A state that removes this machine ends its membership as leaving
    /// would.
    fn absorb(
        &self,
        state: &mut State,
        peer: &str,
        theirs: GroupState,
        hints: Hints,
    ) -> io::Result<()> {
        let id = self.endpoint.id().to_string();
        let Some(group) = &state.identity.group else {
            return Ok(());
        };
        if theirs.id != group.state.id || !theirs.is_valid() {
            return Ok(());
        }
        let previous = state.identity.clone();
        let Some(group) = &mut state.identity.group else {
            return Ok(());
        };
        let changed = group.state.merge(theirs);
        let hinted = group.state.learn_hints(peer, hints.relay, hints.addrs);
        if !group.state.is_member_machine(&id) {
            let gone = group.state.roster();
            self.forget_group(&mut state.identity);
            self.commit(state, previous)?;
            self.write_local_hosts(Some(Roster {
                machines: gone
                    .machines
                    .into_iter()
                    .map(|machine| crate::group::RosterMachine {
                        member: false,
                        ..machine
                    })
                    .collect(),
                ..gone
            }));
            return Ok(());
        }
        if changed {
            self.commit(state, previous)
        } else if hinted {
            state
                .identity
                .save()
                .inspect_err(|_| state.identity = previous)
        } else {
            Ok(())
        }
    }

    /// Bring this machine's own entry up to date with its name and routing
    /// hints, so members dial it where it is.
    fn refresh_this_machine(&self) {
        let addr = self.snapshot();
        let mut state = self.state.lock().unwrap();
        let this = self.this_machine(&state.identity, &addr);
        let Some(group) = &state.identity.group else {
            return;
        };
        let Some(known) = group.state.machine(&this.id) else {
            return;
        };
        if !known.entry.is_member()
            || (&known.name, &known.traverse, &known.relay, &known.addrs)
                == (&this.name, &this.traverse, &this.relay, &this.addrs)
        {
            return;
        }
        let previous = state.identity.clone();
        let by = this.id.clone();
        if let Some(group) = &mut state.identity.group {
            group.state.set_machine(this, Membership::Member, &by);
        }
        if let Err(error) = self.commit(&mut state, previous) {
            log::warn!("could not record this machine's group entry: {error}");
        }
    }

    /// Admit a machine presenting this machine's invitation, with the devices
    /// it brings. A device the group removed stays removed.
    fn accept_join(
        &self,
        remote: &str,
        group_id: &str,
        secret: &str,
        machine: JoiningMachine,
        hints: Hints,
    ) -> GroupLine {
        let rejected = |reason| GroupLine::Rejected { reason };
        let by = self.endpoint.id().to_string();
        let mut state = self.state.lock().unwrap();
        let Some(record) = &state.identity.group else {
            return rejected(GroupRejection::NotMember);
        };
        if record.state.id != group_id || remote == by || !machine.is_valid() {
            return rejected(GroupRejection::NotMember);
        }
        if !self.allow_pairing || !state.identity.pairing_enabled {
            return rejected(GroupRejection::Disabled);
        }
        if !record.state.is_member_machine(remote)
            && record.state.member_machines().count() >= MAX_GROUP_MACHINES
        {
            return rejected(GroupRejection::Full);
        }
        if !self.take_invitation(&mut state, secret) {
            return rejected(GroupRejection::Invalid);
        }
        let previous = state.identity.clone();
        let Some(group) = &mut state.identity.group else {
            return rejected(GroupRejection::NotMember);
        };
        let mut addrs = machine.addrs.clone();
        for addr in hints.addrs {
            if !addrs.contains(&addr) {
                addrs.insert(0, addr);
            }
        }
        addrs.truncate(MAX_ADDRS);
        group.state.set_machine(
            GroupMachine::new(
                remote.to_owned(),
                machine.name.trim().to_owned(),
                machine.traverse.clone(),
                machine.relay.clone().or(hints.relay),
                addrs,
            ),
            Membership::Member,
            &by,
        );
        for device in &machine.devices {
            if group.state.device(&device.id).is_some() {
                continue;
            }
            group.state.set_device(
                &device.id,
                device.name.trim(),
                device.platform.as_deref(),
                Membership::Member,
                &by,
            );
        }
        if let Err(error) = self.commit(&mut state, previous) {
            log::error!("could not record the joining machine: {error}");
            return rejected(GroupRejection::Busy);
        }
        match &state.identity.group {
            Some(group) => GroupLine::Joined {
                state: group.state.clone(),
            },
            None => rejected(GroupRejection::Busy),
        }
    }

    /// Answer a member's state with this machine's, merged. A machine the
    /// group removed is told so and nothing it sends is taken.
    fn accept_sync(&self, remote: &str, theirs: GroupState, hints: Hints) -> GroupLine {
        let mut state = self.state.lock().unwrap();
        let Some(group) = &state.identity.group else {
            return GroupLine::Rejected {
                reason: GroupRejection::NotMember,
            };
        };
        if theirs.id != group.state.id {
            return GroupLine::Rejected {
                reason: GroupRejection::NotMember,
            };
        }
        match group.state.machine(remote) {
            None => {
                return GroupLine::Rejected {
                    reason: GroupRejection::NotMember,
                };
            }
            // Its own removal is all a removed machine is told.
            Some(machine) if !machine.entry.is_member() => {
                return GroupLine::Sync {
                    state: GroupState {
                        machines: vec![machine.clone()],
                        devices: Vec::new(),
                        ..GroupState::new(group.state.id.clone(), group.state.name.clone())
                    },
                };
            }
            Some(_) => {}
        }
        if let Err(error) = self.absorb(&mut state, remote, theirs, hints) {
            log::error!("could not record the group state from {remote}: {error}");
            return GroupLine::Rejected {
                reason: GroupRejection::Busy,
            };
        }
        match &state.identity.group {
            Some(group) => GroupLine::Sync {
                state: group.state.clone(),
            },
            None => GroupLine::Rejected {
                reason: GroupRejection::NotMember,
            },
        }
    }
}

/// Keep the group in step with the other members: at start, whenever this
/// machine's group changes, and every [`GROUP_SYNC_INTERVAL`] for members
/// that were unreachable. Ends with the host.
async fn group_sync_loop(shared: std::sync::Weak<Shared>, wake: Arc<tokio::sync::Notify>) {
    loop {
        tokio::select! {
            () = wake.notified() => {}
            () = tokio::time::sleep(GROUP_SYNC_INTERVAL) => {}
        }
        let Some(shared) = shared.upgrade() else {
            return;
        };
        shared.refresh_this_machine();
        let rounds: Vec<_> = shared
            .peers()
            .into_iter()
            .map(|peer| tokio::spawn(shared.clone().sync_with(peer)))
            .collect();
        drop(shared);
        for round in rounds {
            let _ = round.await;
        }
    }
}

fn new_secret() -> String {
    let mut random = [0_u8; tcode_client::pairing::SECRET_BYTES];
    getrandom::fill(&mut random).expect("the OS random source is available");
    encode_secret(&random)
}

fn space_name(name: String) -> io::Result<String> {
    if name.trim().is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid space name",
        ));
    }
    Ok(name.trim().into())
}

fn missing(kind: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("unknown {kind}"))
}

fn member_of(device: &DeviceRecord, space: &str) -> bool {
    matches!(&device.access, DeviceGrant::Spaces(ids) if ids.iter().any(|id| id == space))
}

fn device_access(device: &DeviceRecord) -> DeviceAccess {
    match &device.access {
        DeviceGrant::Full => DeviceAccess::Full,
        DeviceGrant::Spaces(ids) => DeviceAccess::Space {
            space_id: ids.first().cloned().unwrap_or_default(),
        },
    }
}

fn hosted_device(state: &State, device: &DeviceRecord) -> HostedDevice {
    HostedDevice {
        access: device_access(device),
        id: device.id.clone(),
        name: device.name.clone(),
        created_unix: device.created_unix,
        platform: device.platform.clone(),
        path: device
            .id
            .parse::<EndpointId>()
            .ok()
            .and_then(|id| state.live.get(&id))
            .and_then(|connections| connections.first())
            .map(path_info),
    }
}

fn close_device(state: &mut State, id: &str, code: u32, reason: &[u8]) {
    let connections = id
        .parse::<EndpointId>()
        .ok()
        .and_then(|id| state.live.remove(&id))
        .unwrap_or_default();
    for connection in connections {
        connection.close(code.into(), reason);
    }
}

/// A revocation the machine could not record, as the hosting query reports
/// it: the device is still paired.
pub(crate) fn revoke_error(error: io::Error) -> ProtocolError {
    ProtocolError {
        code: "revoke_failed".into(),
        message: format!(
            "the machine could not record the revocation; the device is still paired: {error}"
        ),
    }
}

/// How `connection` is carried right now, judged by its selected path. A
/// relay selected while a direct path is already open is a relay still
/// waiting for that path to prove itself.
pub(crate) fn path_info(connection: &Connection) -> PathInfo {
    let paths = connection.paths();
    let selected = paths
        .iter()
        .find(|path| path.is_selected())
        .or_else(|| paths.iter().next());
    let direct_open = paths
        .iter()
        .any(|path| matches!(path.remote_addr(), TransportAddr::Ip(_)));
    match selected.map(|path| path.remote_addr().clone()) {
        Some(TransportAddr::Ip(addr)) => PathInfo {
            direct: true,
            relay: None,
            lan: on_own_network(addr.ip()),
            probing_direct: false,
        },
        Some(TransportAddr::Relay(url)) => PathInfo {
            direct: false,
            relay: Some(url.to_string()),
            lan: false,
            probing_direct: direct_open,
        },
        _ => PathInfo {
            direct: false,
            relay: None,
            lan: false,
            probing_direct: direct_open,
        },
    }
}

/// Whether a peer at `ip` was reached without crossing the internet:
/// private, link-local or loopback addresses.
fn on_own_network(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => ip.is_private() || ip.is_link_local() || ip.is_loopback(),
        std::net::IpAddr::V6(ip) => {
            ip.is_unique_local() || ip.is_unicast_link_local() || ip.is_loopback()
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Clone)]
struct PairHandler(Arc<Shared>);

impl std::fmt::Debug for PairHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairHandler")
    }
}

impl ProtocolHandler for PairHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let remote = connection.remote_id();
        let (mut send, recv) = tokio::time::timeout(wire::CONTROL_TIMEOUT, connection.accept_bi())
            .await
            .map_err(|_| AcceptError::from_err(timed_out("pairing stream")))??;
        let mut reader = wire::reader(recv);
        let reply = match wire::read_control::<ClientLine>(&mut reader).await {
            Ok(ClientLine::Pair {
                secret,
                device,
                space,
            }) if device.is_valid()
                && secret.len() <= wire::MAX_CONTROL_LINE
                && space
                    .as_deref()
                    .is_none_or(tcode_client::pairing::valid_space_id) =>
            {
                self.0.pair(remote, &secret, &device, space.as_deref())
            }
            Ok(_) => HostLine::Refused {
                reason: "expected a pair line".into(),
            },
            Err(error) => {
                connection.close(2_u32.into(), b"protocol");
                return Err(AcceptError::from_err(error));
            }
        };
        wire::write_line(&mut send, &reply).await?;
        send.finish()?;
        // Let the reply drain before closing; the device closes once it
        // has read it.
        let _ = tokio::time::timeout(wire::CONTROL_TIMEOUT, connection.closed()).await;
        connection.close(0_u32.into(), b"done");
        Ok(())
    }
}

#[derive(Clone)]
struct GroupHandler(Arc<Shared>);

impl std::fmt::Debug for GroupHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GroupHandler")
    }
}

impl ProtocolHandler for GroupHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let remote = connection.remote_id().to_string();
        let (mut send, recv) = tokio::time::timeout(wire::CONTROL_TIMEOUT, connection.accept_bi())
            .await
            .map_err(|_| AcceptError::from_err(timed_out("group stream")))??;
        let mut reader = wire::reader(recv);
        let line = match wire::read_control_within::<GroupLine>(&mut reader, MAX_GROUP_LINE).await {
            Ok(line) => line,
            Err(error) => {
                connection.close(2_u32.into(), b"protocol");
                return Err(AcceptError::from_err(error));
            }
        };
        let hints = connection_hints(&connection);
        let reply = match line {
            GroupLine::Join {
                group,
                secret,
                machine,
            } => self.0.accept_join(&remote, &group, &secret, machine, hints),
            GroupLine::Sync { state } => self.0.accept_sync(&remote, state, hints),
            _ => GroupLine::Rejected {
                reason: GroupRejection::NotMember,
            },
        };
        wire::write_line(&mut send, &reply).await?;
        send.finish()?;
        let _ = tokio::time::timeout(wire::CONTROL_TIMEOUT, connection.closed()).await;
        connection.close(0_u32.into(), b"done");
        Ok(())
    }
}

#[derive(Clone)]
struct MainHandler(Arc<Shared>);

impl std::fmt::Debug for MainHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MainHandler")
    }
}

impl ProtocolHandler for MainHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let shared = self.0.clone();
        if !shared.admit(&connection) {
            // Say why before closing, so the device stops retrying and asks
            // to pair again instead of treating this as a network fault.
            if let Ok(Ok((mut send, recv))) =
                tokio::time::timeout(wire::CONTROL_TIMEOUT, connection.accept_bi()).await
            {
                let mut reader = wire::reader(recv);
                let _ = wire::read_control::<ClientLine>(&mut reader).await;
                let _ = wire::write_line(
                    &mut send,
                    &HostLine::HelloRejected {
                        reason: HelloRejection::Unpaired,
                    },
                )
                .await;
                let _ = send.finish();
                let _ = tokio::time::timeout(wire::CONTROL_TIMEOUT, connection.closed()).await;
            }
            connection.close(1_u32.into(), b"unpaired");
            return Ok(());
        }
        let _released = Release {
            shared: shared.clone(),
            connection: connection.clone(),
        };
        let policy = tcode_client::attachment::AttachmentPolicy::network();
        let hello_done = Arc::new(AtomicBool::new(false));
        let principal = Arc::new(Mutex::new(None));
        let open_streams = Arc::new(AtomicUsize::new(0));
        let tunnels = Arc::new(AtomicUsize::new(0));
        loop {
            let accepted = tokio::select! {
                accepted = connection.accept_bi() => accepted,
                _ = connection.closed() => break,
            };
            let Ok((send, recv)) = accepted else {
                break;
            };
            if open_streams.fetch_add(1, Ordering::AcqRel) >= wire::MAX_STREAMS as usize {
                open_streams.fetch_sub(1, Ordering::AcqRel);
                drop((send, recv));
                continue;
            }
            let stream = StreamTask {
                shared: shared.clone(),
                connection: connection.clone(),
                policy,
                hello_done: hello_done.clone(),
                principal: principal.clone(),
                open_streams: open_streams.clone(),
                tunnels: tunnels.clone(),
            };
            tokio::spawn(async move {
                let result = stream.run(send, wire::reader(recv)).await;
                stream.open_streams.fetch_sub(1, Ordering::AcqRel);
                if let Err(error) = result {
                    log::debug!("main stream ended: {error}");
                }
            });
        }
        Ok(())
    }
}

/// Unregisters a connection when its accept task ends, however it ends.
struct Release {
    shared: Arc<Shared>,
    connection: Connection,
}

impl Drop for Release {
    fn drop(&mut self) {
        self.shared.release(&self.connection);
    }
}

struct StreamTask {
    policy: tcode_client::attachment::AttachmentPolicy,
    shared: Arc<Shared>,
    connection: Connection,
    hello_done: Arc<AtomicBool>,
    principal: Arc<Mutex<Option<Principal>>>,
    open_streams: Arc<AtomicUsize>,
    tunnels: Arc<AtomicUsize>,
}

/// One of the connection's [`wire::MAX_TUNNELS`] tunnel slots, held while a
/// tunnel is served.
struct TunnelSlot(Arc<AtomicUsize>);

impl TunnelSlot {
    fn take(tunnels: &Arc<AtomicUsize>) -> Option<Self> {
        if tunnels.fetch_add(1, Ordering::AcqRel) >= wire::MAX_TUNNELS {
            tunnels.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(Self(tunnels.clone()))
    }
}

impl Drop for TunnelSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl StreamTask {
    async fn run(&self, mut send: SendStream, mut reader: LineReader) -> io::Result<()> {
        let first = match wire::read_control::<ClientLine>(&mut reader).await {
            Ok(first) => first,
            Err(error) => {
                return Err(error);
            }
        };
        match first {
            ClientLine::Hello {
                protocol_version,
                device,
            } => {
                if protocol_version != tcode_protocol::PROTOCOL_VERSION {
                    wire::write_line(
                        &mut send,
                        &HostLine::HelloRejected {
                            reason: HelloRejection::Protocol,
                        },
                    )
                    .await?;
                    send.finish()?;
                    return Ok(());
                }
                if self.hello_done.swap(true, Ordering::AcqRel) {
                    return refuse(send, "one main stream per connection").await;
                }
                self.shared
                    .refresh_device(self.connection.remote_id(), &device);
                let Some(principal) = self.shared.principal(self.connection.remote_id()) else {
                    wire::write_line(
                        &mut send,
                        &HostLine::HelloRejected {
                            reason: HelloRejection::Unpaired,
                        },
                    )
                    .await?;
                    send.finish()?;
                    return Ok(());
                };
                *self.principal.lock().unwrap() = Some(principal.clone());
                let group = matches!(principal, Principal::Full)
                    .then(|| self.shared.roster(self.connection.remote_id()))
                    .flatten();
                wire::write_line(
                    &mut send,
                    &HostLine::HelloOk {
                        host_name: self.shared.host_name(),
                        protocol_version: tcode_protocol::PROTOCOL_VERSION,
                        group,
                    },
                )
                .await?;
                self.bridge(send, reader, principal).await
            }
            ClientLine::Connect { host, port } => {
                if !self.hello_done.load(Ordering::Acquire) {
                    return refuse(send, "hello required").await;
                }
                if !matches!(*self.principal.lock().unwrap(), Some(Principal::Full)) {
                    return refuse(send, "space members cannot open tunnels").await;
                }
                if !wire::valid_tunnel_target(&host, port) {
                    return refuse(send, "invalid tunnel target").await;
                }
                let Some(_slot) = TunnelSlot::take(&self.tunnels) else {
                    return refuse(send, "too many tunnels").await;
                };
                crate::tunnel::serve(send, reader, &host, port).await
            }
            ClientLine::Original { target } => {
                let principal = self.principal.lock().unwrap().clone();
                let Some(principal) = principal else {
                    return refuse(send, "hello required").await;
                };
                crate::original::serve(
                    &self.shared.mux,
                    self.connection.remote_id().to_string(),
                    principal,
                    target,
                    self.policy,
                    send,
                )
                .await
            }
            ClientLine::Pair { .. } => refuse(send, "pairing uses tcode/pair/1").await,
        }
    }

    /// Pump NDJSON between the stream and one mux attachment until either
    /// side ends. Retained-command keys are scoped to the device so its
    /// dedup cache cannot be shared with or spoofed by another device.
    async fn bridge(
        &self,
        send: SendStream,
        reader: LineReader,
        principal: Principal,
    ) -> io::Result<()> {
        let mut send = wire::LineWriter::new(send);
        let mut reader = wire::LineStream::new(reader);
        let full = matches!(principal, Principal::Full);
        let attachment = self.shared.mux.attach(principal, self.policy);
        let scope = self.connection.remote_id().to_string();
        let (outbound, outbound_rx) = async_channel::unbounded::<String>();
        let writer = async {
            loop {
                let line = tokio::select! {
                    line = attachment.from_host.recv() => line,
                    line = outbound_rx.recv() => line,
                };
                let line = line.map_err(|_| {
                    io::Error::new(io::ErrorKind::ConnectionAborted, "host attachment closed")
                })?;
                send.write_line(&line).await?;
            }
        };
        let reader = async {
            while let Some(line) = reader.read_line(wire::MAX_LINE).await? {
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&line) else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "client line is not JSON",
                    ));
                };
                if let Ok(tcode_protocol::ClientMessage {
                    principal: _,
                    id,
                    payload:
                        tcode_protocol::ClientPayload::Query(tcode_protocol::Query::Hosting { action }),
                    ..
                }) = serde_json::from_value(value.clone())
                {
                    let reply = tcode_protocol::HostMessage::QueryResult {
                        id,
                        result: if full {
                            self.shared
                                .hosting(action)
                                .map(tcode_protocol::QueryResponse::Hosting)
                        } else {
                            Err(ProtocolError::out_of_scope(
                                "hosting is available only to full devices",
                            ))
                        },
                    };
                    let reply = tcode_protocol::encode_line(&reply)
                        .map_err(|error| io::Error::other(error.message))?;
                    let _ = outbound.send(reply).await;
                    continue;
                }
                if let Some(key) = value.get("key").and_then(serde_json::Value::as_str) {
                    if !valid_key(key) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "client line carries an invalid key",
                        ));
                    }
                    value["key"] = format!("{scope}:{key}").into();
                }
                let mut line = value.to_string();
                line.push('\n');
                attachment.to_host.send(line).await.map_err(|_| {
                    io::Error::new(io::ErrorKind::ConnectionAborted, "host attachment closed")
                })?;
            }
            Ok(())
        };
        let result = tokio::select! {
            result = futures_lite::future::race(reader, writer) => result,
            _ = attachment.to_host.closed() => Err(io::Error::new(
                io::ErrorKind::ConnectionAborted, "host attachment closed",
            )),
        };
        attachment.to_host.close();
        send.finish();
        if result
            .as_ref()
            .is_err_and(|error| error.kind() == io::ErrorKind::InvalidData)
        {
            self.connection.close(2_u32.into(), b"protocol");
        }
        result
    }
}

async fn refuse(mut send: SendStream, reason: &str) -> io::Result<()> {
    wire::write_line(
        &mut send,
        &HostLine::Refused {
            reason: reason.into(),
        },
    )
    .await?;
    send.finish()?;
    Ok(())
}

/// A client-minted key is one UUID; the host adds the device scope itself.
pub(crate) fn valid_key(key: &str) -> bool {
    key.len() == 36
        && key.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn timed_out(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, format!("{what} timed out"))
}

#[cfg(test)]
mod tests {
    use url::Url;

    use tcode_protocol::TraverseManifestState;

    use super::*;

    #[test]
    fn native_loopback_connections_report_lan_on_both_sides() {
        block_on(async {
            let server = Endpoint::builder(presets::Minimal)
                .relay_mode(RelayMode::Disabled)
                .clear_ip_transports()
                .bind_addr("127.0.0.1:0")
                .unwrap()
                .alpns(vec![wire::ALPN_MAIN.to_vec()])
                .bind()
                .await
                .unwrap();
            let client = Endpoint::builder(presets::Minimal)
                .relay_mode(RelayMode::Disabled)
                .clear_ip_transports()
                .bind_addr("127.0.0.1:0")
                .unwrap()
                .bind()
                .await
                .unwrap();
            let (outgoing, incoming) = tokio::time::timeout(Duration::from_secs(10), async {
                tokio::join!(client.connect(server.addr(), wire::ALPN_MAIN), async {
                    server.accept().await.unwrap().await.unwrap()
                })
            })
            .await
            .expect("the loopback connection establishes");
            let outgoing = outgoing.unwrap();
            let expected = PathInfo {
                direct: true,
                relay: None,
                lan: true,
                probing_direct: false,
            };
            assert_eq!(path_info(&outgoing), expected);
            assert_eq!(path_info(&incoming), expected);
            client.close().await;
            server.close().await;
        });
    }

    /// A self-hosted instance that does not answer at start: the machine
    /// runs with no relay and no lookup, and the manifest is applied to the
    /// running endpoint by the refresh that first reaches it. The loop
    /// itself is paced in minutes, so the test runs one refresh by hand
    /// through the same apply path.
    #[test]
    fn a_self_hosted_machine_starts_without_its_manifest_and_applies_it_when_it_arrives() {
        let dir = std::env::temp_dir().join(format!(
            "tcode-host-late-manifest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let manifest_path = dir.join("relays.json");
        let (to_host, _host_rx) =
            async_channel::unbounded::<tcode_client::attachment::AttachmentMessage>();
        let (_host_tx, from_host) = async_channel::unbounded::<String>();
        let mut host = TraverseHost::start(
            HostMux::new(to_host, from_host),
            HostConfig {
                host_name: "Late".into(),
                data_dir: dir.clone(),
                traverse: vec![ManifestSource::Custom(
                    Url::from_file_path(&manifest_path).unwrap(),
                )],
                pairing_enabled: true,
                bind_port: None,
            },
        )
        .expect("the machine starts without its manifest");
        let endpoint = host.shared.endpoint.clone();
        assert!(host.shared.relays.is_empty(), "no relay to dial yet");
        assert_eq!(endpoint.address_lookup().unwrap().len(), 0, "no lookup yet");
        let source = Url::from_file_path(&manifest_path).unwrap().to_string();
        assert_eq!(
            host.new_invitation().invite.traverse,
            std::slice::from_ref(&source)
        );
        // The hosting status says why the source has nothing to offer yet.
        let [status] = <[_; 1]>::try_from(host.hosting(HostingAction::State).unwrap().traverse)
            .expect("one source");
        assert_eq!(status.source, source);
        assert_eq!(status.manifest.state, TraverseManifestState::Failed);
        assert!(
            status
                .manifest
                .error
                .as_deref()
                .is_some_and(|error| error.contains("relays.json")),
            "{:?}",
            status.manifest.error
        );
        assert!(status.relays.is_empty() && status.lookups.is_empty());

        std::fs::write(
            &manifest_path,
            r#"{"version":1,"relays":[{"url":"https://relay.self-hosted.test/","region":"eu"}],"pkarr":["https://relay.self-hosted.test/pkarr"]}"#,
        )
        .unwrap();
        let loader = host.shared.loaders[0].clone();
        // The failed startup fetch paces the next attempt; the loop would wait
        // it out. The loop is stopped before the stamp is reset: its first
        // pass runs as soon as the runtime schedules it, and once unpaced it
        // would fetch the written manifest itself, leaving this refresh with
        // nothing to return.
        for refresh in host.refresh.drain(..) {
            refresh.abort();
        }
        loader.state().last_attempt_ms = None;
        let manifest = block_on(loader.refresh()).expect("the manifest arrived");
        block_on(host.shared.apply_manifest(0, manifest));
        assert_eq!(
            host.shared.relays.urls::<Vec<_>>(),
            [iroh::RelayUrl::from(
                Url::parse("https://relay.self-hosted.test/").unwrap()
            )]
        );
        assert_eq!(
            endpoint.address_lookup().unwrap().len(),
            2,
            "a pkarr publisher and resolver"
        );
        let [status] = <[_; 1]>::try_from(host.hosting(HostingAction::State).unwrap().traverse)
            .expect("one source");
        assert_eq!(status.source, source);
        assert_eq!(
            (status.manifest.state, status.manifest.error),
            (TraverseManifestState::Live, None),
            "fetched by this run, and the failure is behind it"
        );
        assert!(status.manifest.fetched_unix.is_some());
        assert_eq!(
            status
                .relays
                .iter()
                .map(|relay| (relay.url.as_str(), relay.region.as_deref()))
                .collect::<Vec<_>>(),
            [("https://relay.self-hosted.test/", Some("eu"))]
        );
        assert_eq!(
            status
                .lookups
                .iter()
                .map(|lookup| lookup.url.as_str())
                .collect::<Vec<_>>(),
            ["https://relay.self-hosted.test/pkarr"]
        );
        host.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Several enabled sources: the endpoint dials the union of their relays
    /// and publishes to and resolves through every one's pkarr URLs. A
    /// source that does not answer at start is left out without holding
    /// the others back, and its manifest joins them when it arrives.
    #[test]
    fn several_sources_serve_the_union_of_their_relays_and_every_lookup() {
        let dir = std::env::temp_dir().join(format!(
            "tcode-host-sources-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = |name: &str| {
            format!(
                r#"{{"version":1,"relays":[{{"url":"https://{name}.test/"}},{{"url":"https://shared.test/"}}],"pkarr":["https://{name}.test/pkarr"]}}"#
            )
        };
        let paths = ["a", "b", "late"].map(|name| dir.join(format!("{name}.json")));
        std::fs::write(&paths[0], manifest("a")).unwrap();
        std::fs::write(&paths[1], manifest("b")).unwrap();
        let relay_urls = |host: &TraverseHost| {
            let mut urls: Vec<String> = host
                .shared
                .relays
                .urls::<Vec<_>>()
                .iter()
                .map(ToString::to_string)
                .collect();
            urls.sort();
            urls
        };
        let (to_host, _host_rx) =
            async_channel::unbounded::<tcode_client::attachment::AttachmentMessage>();
        let (_host_tx, from_host) = async_channel::unbounded::<String>();
        let mut host = TraverseHost::start(
            HostMux::new(to_host, from_host),
            HostConfig {
                host_name: "Sources".into(),
                data_dir: dir.clone(),
                traverse: paths
                    .iter()
                    .map(|path| ManifestSource::Custom(Url::from_file_path(path).unwrap()))
                    .collect(),
                pairing_enabled: true,
                bind_port: None,
            },
        )
        .expect("the machine starts without one source's manifest");
        let endpoint = host.shared.endpoint.clone();
        assert_eq!(
            relay_urls(&host),
            ["https://a.test/", "https://b.test/", "https://shared.test/"]
        );
        assert_eq!(
            endpoint.address_lookup().unwrap().len(),
            4,
            "a pkarr publisher and resolver for each answering source"
        );

        std::fs::write(&paths[2], manifest("late")).unwrap();
        let loader = host.shared.loaders[2].clone();
        // As with a single source: the loops stop before the failed
        // attempt's pacing is reset, so this refresh is the one that fetches.
        for refresh in host.refresh.drain(..) {
            refresh.abort();
        }
        loader.state().last_attempt_ms = None;
        let late = block_on(loader.refresh()).expect("the manifest arrived");
        block_on(host.shared.apply_manifest(2, late));
        assert_eq!(
            relay_urls(&host),
            [
                "https://a.test/",
                "https://b.test/",
                "https://late.test/",
                "https://shared.test/"
            ]
        );
        assert_eq!(endpoint.address_lookup().unwrap().len(), 6);
        host.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }
}
