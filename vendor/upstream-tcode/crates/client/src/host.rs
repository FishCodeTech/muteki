//! Platform services shared by every tcode client.
//!
//! This contract deliberately contains no UI-runtime types. Adapters await the
//! returned local futures and marshal their results onto their own UI thread.

use std::{future::Future, io, pin::Pin};

use futures_lite::{AsyncRead, AsyncWrite};
use serde::{Deserialize, Serialize};

use crate::{
    ConnectionState,
    pairing::{PairInvite, PairedHost},
};

/// A future which may remain on the thread that created it.
pub type HostFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// A live link to a host: NDJSON lines in both directions plus connection state.
pub struct Transport {
    pub originals: Option<std::sync::Arc<dyn crate::original::OriginalOpener>>,
    pub to_host: crate::outgoing::Outgoing,
    pub from_host: async_channel::Receiver<String>,
    pub state: async_channel::Receiver<ConnectionState>,
    /// Native routes follow this attachment's authenticated endpoint even when
    /// persisting it fails. Local and fixed-origin browser transports use None.
    pub current_host: Option<LiveHost>,
}

/// The transport publishes its current pairing before emitting Syncing.
/// Consumers take an atomic snapshot; saved hosts are only restart storage. A
/// transport that can carry raw tunnels to the machine attaches its
/// [`TunnelOpener`] here, so Preview reaches the machine over the same
/// authenticated link as the protocol.
#[derive(Clone)]
pub struct LiveHost {
    host: std::sync::Arc<std::sync::Mutex<PairedHost>>,
    tunnels: Option<std::sync::Arc<dyn TunnelOpener>>,
}

impl LiveHost {
    pub fn new(host: PairedHost) -> Self {
        Self {
            host: std::sync::Arc::new(std::sync::Mutex::new(host)),
            tunnels: None,
        }
    }

    pub fn with_tunnels(host: PairedHost, tunnels: std::sync::Arc<dyn TunnelOpener>) -> Self {
        Self {
            tunnels: Some(tunnels),
            ..Self::new(host)
        }
    }

    pub fn snapshot(&self) -> PairedHost {
        self.host.lock().unwrap().clone()
    }

    /// Called by the owning transport only after authenticating the endpoint.
    pub fn authenticated(&self, host: &PairedHost) {
        *self.host.lock().unwrap() = host.clone();
    }

    pub fn tunnels(&self) -> Option<std::sync::Arc<dyn TunnelOpener>> {
        self.tunnels.clone()
    }
}

/// One raw byte tunnel to a TCP service on the paired machine. Closing the
/// write half tells the machine to shut down its write side to the service;
/// end of stream on the read half means the service closed its side.
pub struct Tunnel {
    pub read: Box<dyn AsyncRead + Send + Unpin>,
    pub write: Box<dyn AsyncWrite + Send + Unpin>,
}

impl std::fmt::Debug for Tunnel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Tunnel")
    }
}

pub type TunnelFuture = Pin<Box<dyn Future<Output = io::Result<Tunnel>> + Send>>;

/// Opens tunnels to `host:port` as dialled from the paired machine, over the
/// attachment's current connection. Fails at once while the attachment is
/// reconnecting; tunnels opened on an earlier connection end with it.
pub trait TunnelOpener: Send + Sync {
    fn open(&self, host: &str, port: u16) -> TunnelFuture;
}

/// What a client says about itself when pairing and connecting; the host
/// keeps one device record per `id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceIdentity {
    pub id: String,
    pub name: String,
    /// Operating system name and version, such as `Android 15` or `macOS 26.0`.
    pub platform: Option<String>,
}

/// Hosts accept a device id of at most this many bytes; longer or
/// control-bearing values are treated as absent.
pub const MAX_DEVICE_ID_LEN: usize = 64;

/// Whether a stored or received device id is one a host will accept.
pub fn valid_device_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_DEVICE_ID_LEN && !id.chars().any(char::is_control)
}

/// The client's persistent device id: the stored value when it is usable,
/// otherwise a freshly minted UUID that `store` persists for the next read.
pub fn persistent_device_id(stored: Option<String>, store: impl FnOnce(&str)) -> String {
    if let Some(id) = stored.filter(|id| valid_device_id(id)) {
        return id;
    }
    let id = uuid::Uuid::new_v4().to_string();
    store(&id);
    id
}

/// Preferences which belong to the client and are never sent to the host.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientPreferences {
    pub appearance: Option<String>,
    pub language: Option<String>,
    pub device_name: Option<String>,
    /// Opaque, client-local UI restoration state. The shell owns its schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub navigation: Option<serde_json::Value>,
    /// Per-image ceiling, in MiB, this device applies to attachments it sends
    /// across the internet; `None` is
    /// [`tcode_core::attachments::DEFAULT_REMOTE_BYTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_attachment_limit_mib: Option<u32>,
}

/// One image the platform's own picker handed back, already read into memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickedImage {
    pub name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// Persistence, pairing, transport, and platform facilities for a tcode client.
pub trait ClientHost: 'static {
    /// Native dictation supplied by this client platform, when available.
    fn speech_recognizer(&self) -> Option<std::rc::Rc<dyn crate::speech::SpeechRecognizer>> {
        None
    }

    /// The platform's word segmentation, for a double-clicked word.
    fn word_segmenter(&self) -> Option<std::rc::Rc<dyn crate::words::WordSegmenter>> {
        crate::words::system()
    }

    /// Name this device presents to hosts while pairing and connecting.
    fn device_name(&self) -> String;

    /// Stable, client-generated id (see [`persistent_device_id`]) so a host
    /// keeps one record for this device however often it pairs again.
    fn device_id(&self) -> String;

    /// Operating system name and version shown next to the device name on hosts.
    fn device_platform(&self) -> Option<String>;

    fn device_identity(&self) -> DeviceIdentity {
        DeviceIdentity {
            id: self.device_id(),
            name: self.device_name(),
            platform: self.device_platform(),
        }
    }

    fn load_preferences(&self) -> ClientPreferences {
        ClientPreferences::default()
    }

    fn save_preferences(&self, _preferences: &ClientPreferences) {}

    fn outbox_storage(&self, _host_id: &str) -> Option<std::sync::Arc<dyn crate::outbox::Storage>> {
        None
    }

    fn load_hosts(&self) -> Vec<PairedHost>;
    fn save_hosts(&self, hosts: &[PairedHost]);

    /// Replace one pairing while preserving unrelated saved machines. Native
    /// adapters make these mutations atomic with transport address updates.
    fn remember_host(&self, host: PairedHost) {
        let mut hosts = self.load_hosts();
        crate::pairing::remember_host(&mut hosts, host);
        self.save_hosts(&hosts);
    }

    fn remove_host(&self, host_id: &str) {
        let mut hosts = self.load_hosts();
        hosts.retain(|host| host.host_id != host_id);
        self.save_hosts(&hosts);
    }

    fn stamp_connected(&self, host_id: &str, timestamp: u64) {
        let mut hosts = self.load_hosts();
        if let Some(host) = hosts.iter_mut().find(|host| host.host_id == host_id) {
            host.last_connected_unix = Some(timestamp);
            self.save_hosts(&hosts);
        }
    }

    /// `Some(id)` of the host to reconnect to on launch.
    fn last_host_id(&self) -> Option<String>;
    fn set_last_host_id(&self, host_id: Option<&str>);

    /// A browser is signed in with the machine that served it and can reach
    /// no other: the shell offers it no way to add or re-pair a machine, and
    /// [`ClientHost::pair`] is never called on it.
    fn fixed_machine(&self) -> bool {
        false
    }

    /// Exchange the invitation's secret for a pairing with exactly the
    /// machine the invitation names. `address` is the machine's IP address
    /// as the user typed it after an attempt found no path to the machine;
    /// it is dialed at the invitation's port.
    fn pair(
        &self,
        invite: PairInvite,
        address: Option<std::net::IpAddr>,
    ) -> HostFuture<'_, Result<PairedHost, String>> {
        let _ = (invite, address);
        Box::pin(async { Err("this client cannot add machines".into()) })
    }

    /// Open a reconnecting link. Dropping the returned channels ends it.
    fn connect(&self, host: &PairedHost) -> Transport;

    fn supports_qr(&self) -> bool {
        false
    }

    fn scan_qr(&self) -> HostFuture<'_, Result<String, String>> {
        Box::pin(async { Err("unsupported".into()) })
    }

    /// Whether [`pick_images`](Self::pick_images) opens the platform's own
    /// media picker. Desktops attach by paste and drop instead.
    fn supports_image_picker(&self) -> bool {
        false
    }

    /// Let the user choose up to `limit` images in the platform's media
    /// picker. Dismissing the picker resolves to an empty list; `Err` is a
    /// real failure to show.
    fn pick_images(&self, limit: usize) -> HostFuture<'_, Result<Vec<PickedImage>, String>> {
        let _ = limit;
        Box::pin(async { Err("unsupported".into()) })
    }

    /// Whether [`ClientHost::deliver_artifact`] can actually hand a produced
    /// file to the user here (a browser download, a share sheet). Views ask
    /// before offering the action, so a client without one shows Copy instead of
    /// a button that silently does nothing.
    fn supports_artifact_delivery(&self) -> bool {
        false
    }

    /// Hand finished bytes to the platform's own delivery path. `Err` is a real
    /// failure worth reporting; callers must check
    /// [`ClientHost::supports_artifact_delivery`] first.
    fn deliver_artifact(&self, _name: &str, _mime: &str, _bytes: &[u8]) -> Result<(), String> {
        Err("this client cannot save files".into())
    }

    /// Open a path in the user's external editor. `None` means this client has
    /// no editor integration; the path is always one this client can reach.
    fn open_in_editor(&self, _path: &std::path::Path) -> Option<Result<(), String>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_is_reused_when_valid_and_minted_and_stored_otherwise() {
        let stored = std::cell::Cell::new(None);
        let keep = "3f2b8c6e-1d4a-4b9e-8c7d-2a1f0e9d8c7b".to_owned();
        assert_eq!(
            persistent_device_id(Some(keep.clone()), |id| stored.set(Some(id.to_owned()))),
            keep
        );
        assert_eq!(stored.take(), None, "a usable id must not be rewritten");
        for damaged in [
            None,
            Some(String::new()),
            Some("a\u{0}b".into()),
            Some("x".repeat(65)),
        ] {
            let minted = persistent_device_id(damaged, |id| stored.set(Some(id.to_owned())));
            assert!(valid_device_id(&minted));
            assert_eq!(stored.take().as_deref(), Some(minted.as_str()));
        }
    }
}
