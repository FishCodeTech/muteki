//! Machine and device identities. Each is an iroh secret key; the public
//! `EndpointId` is what pairing records and what the transport authenticates.
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use iroh::{EndpointId, SecretKey};
use serde::{Deserialize, Serialize};

use crate::group::{Declined, GroupRecord, Membership};

/// One device allowed to open `tcode/1` connections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRecord {
    /// The device's `EndpointId`.
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    pub created_unix: u64,
    pub access: DeviceGrant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
pub enum DeviceGrant {
    Full,
    Spaces(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpaceRecord {
    pub id: String,
    pub name: String,
    pub created_unix: u64,
    pub project_ids: Vec<String>,
    pub secret: String,
    pub link_enabled: bool,
    pub link_failures: u8,
}

/// `traverse.json`: this machine's key, name, allow list and pairing switch.
#[derive(Debug, Clone)]
pub struct HostIdentity {
    pub host_name: String,
    secret_key: SecretKey,
    pub devices: Vec<DeviceRecord>,
    pub spaces: Vec<SpaceRecord>,
    pub policy_revision: u64,
    pub pairing_enabled: bool,
    pub group: Option<GroupRecord>,
    path: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct HostFile {
    v: u32,
    host_name: String,
    secret_key: String,
    #[serde(default)]
    devices: Vec<DeviceRecord>,
    #[serde(default)]
    spaces: Vec<SpaceRecord>,
    #[serde(default)]
    policy_revision: u64,
    #[serde(default = "enabled")]
    pairing_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    group: Option<GroupRecord>,
}

/// A machine outside any group writes version 3, which an older build still
/// reads; version 4 adds the group.
const VERSION_WITHOUT_GROUP: u32 = 3;
const VERSION: u32 = 4;

fn enabled() -> bool {
    true
}

pub const HOST_FILE: &str = "traverse.json";

impl HostIdentity {
    /// Load the machine identity, or create one.
    pub fn load_or_create(data_dir: &Path, host_name: &str) -> io::Result<Self> {
        fs::create_dir_all(data_dir)?;
        let path = data_dir.join(HOST_FILE);
        match fs::read(&path) {
            Ok(bytes) => {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&bytes).map_err(io::Error::other)?;
                let migrated = value["v"] == 2;
                if migrated {
                    if let Some(devices) = value["devices"].as_array_mut() {
                        for device in devices {
                            let record = device.as_object_mut().ok_or_else(|| {
                                io::Error::new(io::ErrorKind::InvalidData, "invalid device record")
                            })?;
                            record.insert("access".into(), serde_json::json!({"type": "full"}));
                        }
                    }
                    value["spaces"] = serde_json::json!([]);
                    value["v"] = 3.into();
                }
                let file: HostFile = serde_json::from_value(value).map_err(io::Error::other)?;
                if file.v != VERSION && file.v != VERSION_WITHOUT_GROUP {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{HOST_FILE} has unsupported version {}", file.v),
                    ));
                }
                if file.devices.iter().any(|device| matches!(&device.access, DeviceGrant::Spaces(ids) if ids.is_empty() || ids.iter().any(|id| !tcode_client::pairing::valid_space_id(id)))) {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid device access"));
                }
                let mut identity = Self {
                    host_name: file.host_name,
                    secret_key: parse_secret_key(&file.secret_key)?,
                    devices: file.devices,
                    spaces: file.spaces,
                    policy_revision: file.policy_revision,
                    pairing_enabled: file.pairing_enabled,
                    group: file.group,
                    path,
                };
                if migrated || identity.host_name != host_name {
                    identity.host_name = host_name.to_owned();
                    identity.save()?;
                }
                Ok(identity)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let identity = Self {
                    host_name: host_name.to_owned(),
                    secret_key: SecretKey::generate(),
                    devices: Vec::new(),
                    spaces: Vec::new(),
                    policy_revision: 0,
                    pairing_enabled: true,
                    group: None,
                    path,
                };
                identity.save()?;
                Ok(identity)
            }
            Err(error) => Err(error),
        }
    }

    pub fn secret_key(&self) -> &SecretKey {
        &self.secret_key
    }

    pub fn endpoint_id(&self) -> EndpointId {
        self.secret_key.public()
    }

    pub fn save(&self) -> io::Result<()> {
        let file = HostFile {
            v: if self.group.is_some() {
                VERSION
            } else {
                VERSION_WITHOUT_GROUP
            },
            host_name: self.host_name.clone(),
            secret_key: encode_hex(&self.secret_key.to_bytes()),
            devices: self.devices.clone(),
            spaces: self.spaces.clone(),
            policy_revision: self.policy_revision,
            pairing_enabled: self.pairing_enabled,
            group: self.group.clone(),
        };
        write_private(&self.path, &serde_json::to_vec_pretty(&file)?)
    }

    pub fn is_paired(&self, id: &EndpointId) -> bool {
        let id = id.to_string();
        self.devices.iter().any(|device| device.id == id)
    }

    /// Admit a device, keeping the original pairing time when it pairs again.
    pub fn admit(&mut self, id: &EndpointId, name: String, platform: Option<String>) {
        let id = id.to_string();
        match self.devices.iter_mut().find(|device| device.id == id) {
            Some(device) => {
                device.name = name;
                device.platform = platform;
            }
            None => self.devices.push(DeviceRecord {
                id,
                name,
                platform,
                created_unix: now_unix(),
                access: DeviceGrant::Full,
            }),
        }
    }

    /// Bring the allow list in line with the group: admit its member
    /// devices (or, while `accept_new` is off, record them as declined) and
    /// remove its removed ones, however they were paired. A space member is
    /// left as it is, and so is `local`, the device sharing this machine's
    /// data directory, which reaches it without the network. Returns the
    /// devices removed, whose connections the caller closes.
    pub fn apply_group(&mut self, accept_new: bool, local: Option<&str>) -> Vec<String> {
        let Some(group) = &mut self.group else {
            return Vec::new();
        };
        let mut removed = Vec::new();
        for device in &group.state.devices {
            if Some(device.id.as_str()) == local {
                continue;
            }
            let listed = self
                .devices
                .iter()
                .position(|record| record.id == device.id);
            match (device.entry.state, listed) {
                (Membership::Removed, Some(index)) => {
                    self.devices.remove(index);
                    removed.push(device.id.clone());
                }
                (Membership::Member, None) => {
                    let declined = group.declined.iter().any(|declined| {
                        declined.id == device.id && declined.counter == device.entry.counter
                    });
                    if declined {
                        continue;
                    }
                    if !accept_new {
                        group.declined.retain(|declined| declined.id != device.id);
                        group.declined.push(Declined {
                            id: device.id.clone(),
                            counter: device.entry.counter,
                        });
                        continue;
                    }
                    self.devices.push(DeviceRecord {
                        id: device.id.clone(),
                        name: device.name.clone(),
                        platform: device.platform.clone(),
                        created_unix: now_unix(),
                        access: DeviceGrant::Full,
                    });
                }
                _ => {}
            }
        }
        let members = &group.state.devices;
        group.declined.retain(|declined| {
            members.iter().any(|device| {
                device.id == declined.id
                    && device.entry.counter == declined.counter
                    && device.entry.is_member()
            })
        });
        removed
    }

    /// Remove a device from the allow list. Returns whether it was listed.
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.devices.len();
        self.devices.retain(|device| device.id != id);
        self.devices.len() != before
    }
}

/// `device.json`: this device's key and how it introduces itself. Cloning
/// shares one identity; the client endpoint built on it is shared too. The
/// endpoint's relays and lookups follow the machines in `hosts.json`; see
/// [`DeviceIdentity::hosts_changed`].
#[derive(Clone)]
pub struct DeviceIdentity {
    inner: Arc<DeviceInner>,
}

pub(crate) struct DeviceInner {
    pub(crate) secret_key: SecretKey,
    pub(crate) data_dir: PathBuf,
    details: Mutex<Details>,
    pub(crate) lan: Mutex<crate::lan::LanOptions>,
    pub(crate) endpoint: tokio::sync::OnceCell<crate::client::ClientEndpoint>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Details {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    platform: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct DeviceFile {
    v: u32,
    secret_key: String,
    #[serde(flatten)]
    details: Details,
}

pub const DEVICE_FILE: &str = "device.json";

impl DeviceIdentity {
    pub fn load_or_create(data_dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(data_dir)?;
        let path = data_dir.join(DEVICE_FILE);
        let (secret_key, details) = match fs::read(&path) {
            Ok(bytes) => {
                let file: DeviceFile = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
                if file.v != 1 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("{DEVICE_FILE} has unsupported version {}", file.v),
                    ));
                }
                (parse_secret_key(&file.secret_key)?, file.details)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let secret_key = SecretKey::generate();
                let file = DeviceFile {
                    v: 1,
                    secret_key: encode_hex(&secret_key.to_bytes()),
                    details: Details::default(),
                };
                write_private(&path, &serde_json::to_vec_pretty(&file)?)?;
                (secret_key, Details::default())
            }
            Err(error) => return Err(error),
        };
        Ok(Self {
            inner: Arc::new(DeviceInner {
                secret_key,
                data_dir: data_dir.to_owned(),
                details: Mutex::new(details),
                lan: Mutex::new(crate::lan::LanOptions::default()),
                endpoint: tokio::sync::OnceCell::new(),
            }),
        })
    }

    pub fn endpoint_id(&self) -> EndpointId {
        self.inner.secret_key.public()
    }

    pub fn data_dir(&self) -> &Path {
        &self.inner.data_dir
    }

    /// What this device tells machines about itself.
    pub fn claim(&self) -> crate::wire::DeviceClaim {
        let details = self.inner.details.lock().unwrap();
        crate::wire::DeviceClaim {
            name: details.name.clone().unwrap_or_else(|| "Tcode".into()),
            platform: details.platform.clone(),
        }
    }

    /// Update the name and platform sent with pairing and hello.
    pub fn set_details(&self, name: String, platform: Option<String>) {
        let mut details = self.inner.details.lock().unwrap();
        let next = Details {
            name: Some(name),
            platform,
        };
        if details.name == next.name && details.platform == next.platform {
            return;
        }
        *details = next.clone();
        let file = DeviceFile {
            v: 1,
            secret_key: encode_hex(&self.inner.secret_key.to_bytes()),
            details: next,
        };
        let result = serde_json::to_vec_pretty(&file)
            .map_err(io::Error::other)
            .and_then(|bytes| write_private(&self.inner.data_dir.join(DEVICE_FILE), &bytes));
        if let Err(error) = result {
            log::error!("could not write {DEVICE_FILE}: {error}");
        }
    }

    /// How the LAN lookup observes the network; read when the endpoint
    /// binds on first use, so a platform sets it before connecting.
    pub fn set_lan_options(&self, options: crate::lan::LanOptions) {
        *self.inner.lan.lock().unwrap() = options;
    }

    pub(crate) fn inner(&self) -> &DeviceInner {
        &self.inner
    }

    pub(crate) fn from_inner(inner: Arc<DeviceInner>) -> Self {
        Self { inner }
    }

    pub(crate) fn downgrade(&self) -> std::sync::Weak<DeviceInner> {
        Arc::downgrade(&self.inner)
    }
}

/// The device of `device.json` in `data_dir`, if this data directory has
/// one: on a desktop, the window's identity beside the machine's.
pub fn existing_device(data_dir: &Path) -> Option<(EndpointId, crate::wire::DeviceClaim)> {
    let bytes = fs::read(data_dir.join(DEVICE_FILE)).ok()?;
    let file: DeviceFile = serde_json::from_slice(&bytes).ok()?;
    let id = parse_secret_key(&file.secret_key).ok()?.public();
    Some((
        id,
        crate::wire::DeviceClaim {
            name: file.details.name.unwrap_or_else(|| "Tcode".into()),
            platform: file.details.platform,
        },
    ))
}

/// The machine id of `traverse.json` in `data_dir`, if this data directory
/// hosts a machine.
pub fn existing_machine_id(data_dir: &Path) -> Option<EndpointId> {
    let bytes = fs::read(data_dir.join(HOST_FILE)).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    Some(
        parse_secret_key(value["secret_key"].as_str()?)
            .ok()?
            .public(),
    )
}

pub(crate) fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn parse_secret_key(hex: &str) -> io::Result<SecretKey> {
    let bytes = decode_hex(hex)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid secret key"))?;
    Ok(SecretKey::from_bytes(&bytes))
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_hex(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut bytes = [0; 32];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}

/// Write `bytes` to `path` atomically, readable by the owner only.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension("tmp");
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    use io::Write as _;
    let mut file = options.open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(temporary, path)
}
