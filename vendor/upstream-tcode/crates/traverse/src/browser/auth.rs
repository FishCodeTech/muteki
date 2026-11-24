//! `remote.json`: the browser listener's password and the tokens it issued.
//! Native devices live in `traverse.json`; nothing here is shared with them.
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AuthStore {
    /// What a browser records this machine under; unrelated to the
    /// Traverse `EndpointId`, which no browser ever dials.
    pub host_id: Uuid,
    pub host_name: String,
    #[serde(default)]
    pub devices: Vec<Device>,
    #[serde(default)]
    password: Option<PasswordHash>,
    #[serde(skip)]
    failures: u8,
    #[serde(skip)]
    locked_until: Option<std::time::Instant>,
    #[serde(skip)]
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PasswordHash {
    salt: [u8; 16],
    hash: [u8; 32],
    iterations: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Device {
    pub id: Uuid,
    pub name: String,
    pub token_sha256_hex: String,
    pub created_unix: u64,
    /// The client's own persistent id; pairing again with it rotates this
    /// record's token instead of adding a second one. Absent for records made
    /// by clients that predate device ids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Operating system name and version as last reported by the client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

/// What a client reports about itself while pairing, logging in or saying hello.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceDetails {
    pub name: String,
    pub platform: Option<String>,
}

impl AuthStore {
    pub fn open(data_dir: &Path, host_name: &str) -> io::Result<Self> {
        fs::create_dir_all(data_dir)?;
        let path = data_dir.join("remote.json");
        match fs::read(&path) {
            Ok(bytes) => {
                let mut store: Self = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
                store.path = path;
                if store.host_name != host_name {
                    store.host_name = host_name.to_owned();
                    store.save()?;
                }
                Ok(store)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let store = Self {
                    host_id: Uuid::new_v4(),
                    host_name: host_name.to_owned(),
                    devices: Vec::new(),
                    password: None,
                    failures: 0,
                    locked_until: None,
                    path,
                };
                store.save()?;
                Ok(store)
            }
            Err(error) => Err(error),
        }
    }

    pub fn password_configured(&self) -> bool {
        self.password.is_some()
    }

    /// Whether a password is set in `data_dir`, without creating a store
    /// there.
    pub fn password_configured_at(data_dir: &Path) -> io::Result<bool> {
        match fs::read(data_dir.join("remote.json")) {
            Ok(bytes) => {
                let store: Self = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
                Ok(store.password_configured())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub fn set_password(&mut self, password: &str, revoke_tokens: bool) -> io::Result<()> {
        if password.chars().count() < 8 || password.len() > 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "password must contain at least 8 characters and at most 1024 bytes",
            ));
        }
        let mut salt = [0; 16];
        getrandom::fill(&mut salt).map_err(io::Error::other)?;
        let mut hash = [0; 32];
        pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, 600_000, &mut hash);
        // Persist before committing the in-memory change.
        let mut updated = self.clone();
        updated.password = Some(PasswordHash {
            salt,
            hash,
            iterations: 600_000,
        });
        updated.failures = 0;
        updated.locked_until = None;
        if revoke_tokens {
            updated.devices.clear();
        }
        updated.save()?;
        *self = updated;
        Ok(())
    }

    pub fn verify_password(&mut self, password: &str) -> bool {
        if let Some(until) = self.locked_until {
            if std::time::Instant::now() < until {
                return false;
            }
            self.locked_until = None;
            self.failures = 0;
        }
        let Some(expected) = &self.password else {
            return false;
        };
        let mut hash = [0; 32];
        if password.len() <= 1024 {
            pbkdf2::pbkdf2_hmac::<Sha256>(
                password.as_bytes(),
                &expected.salt,
                expected.iterations,
                &mut hash,
            );
        }
        if password.len() <= 1024 && constant_time_eq(&hash, &expected.hash) {
            self.failures = 0;
            return true;
        }
        self.failures += 1;
        if self.failures >= 5 {
            self.locked_until =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(300));
        }
        false
    }

    /// Issue a fresh token. A client that presents the `client_id` of an
    /// existing record keeps that record — its id and first-connection time —
    /// with the token rotated and its details refreshed, so the previous token
    /// stops validating; any other client gets a new record.
    pub fn issue_token(
        &mut self,
        client_id: Option<String>,
        details: DeviceDetails,
    ) -> io::Result<String> {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).map_err(io::Error::other)?;
        let token = URL_SAFE_NO_PAD.encode(bytes);
        let token_sha256_hex = hex_hash(token.as_bytes());
        let mut updated = self.clone();
        let existing = client_id.as_ref().and_then(|client_id| {
            updated
                .devices
                .iter_mut()
                .find(|device| device.client_id.as_ref() == Some(client_id))
        });
        match existing {
            Some(device) => {
                device.token_sha256_hex = token_sha256_hex;
                device.name = details.name;
                device.platform = details.platform;
            }
            None => updated.devices.push(Device {
                id: Uuid::new_v4(),
                name: details.name,
                token_sha256_hex,
                created_unix: unix_now(),
                client_id,
                platform: details.platform,
            }),
        }
        updated.save()?;
        *self = updated;
        Ok(token)
    }

    /// Reflect what a connected device now calls itself, so a rename or an OS
    /// upgrade shows on the host without pairing again. Writes only on change.
    pub fn refresh_device(&mut self, token: &str, details: DeviceDetails) -> io::Result<()> {
        let Some(index) = self.device_for_token(token) else {
            return Ok(());
        };
        let device = &self.devices[index];
        if device.name == details.name && device.platform == details.platform {
            return Ok(());
        }
        let mut updated = self.clone();
        let device = &mut updated.devices[index];
        device.name = details.name;
        device.platform = details.platform;
        updated.save()?;
        *self = updated;
        Ok(())
    }

    pub fn token_is_valid(&self, token: &str) -> bool {
        self.device_for_token(token).is_some()
    }

    /// The record `token` authenticates as, if any.
    pub fn device_id_for_token(&self, token: &str) -> Option<Uuid> {
        self.device_for_token(token)
            .map(|index| self.devices[index].id)
    }

    /// Forget a device; its token stops validating. Returns whether it was
    /// listed.
    pub fn revoke(&mut self, id: Uuid) -> io::Result<bool> {
        if !self.devices.iter().any(|device| device.id == id) {
            return Ok(false);
        }
        let mut updated = self.clone();
        updated.devices.retain(|device| device.id != id);
        updated.save()?;
        *self = updated;
        Ok(true)
    }

    fn device_for_token(&self, token: &str) -> Option<usize> {
        if token.len() != 43 {
            return None;
        }
        let candidate = Sha256::digest(token.as_bytes());
        self.devices.iter().position(|device| {
            let Some(expected) = decode_hex(&device.token_sha256_hex) else {
                return false;
            };
            constant_time_eq(candidate.as_slice(), &expected)
        })
    }

    pub(crate) fn save(&self) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        crate::identity::write_private(&self.path, &bytes)
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn hex_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    let (pairs, _) = value.as_bytes().as_chunks::<2>();
    pairs
        .iter()
        .map(|pair| {
            let pair = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(pair, 16).ok()
        })
        .collect()
}

pub(crate) fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phone(name: &str) -> DeviceDetails {
        DeviceDetails {
            name: name.into(),
            platform: Some("Android 15".into()),
        }
    }

    #[test]
    fn pairing_again_with_the_same_client_id_rotates_one_record() {
        let root = std::env::temp_dir().join(format!("tcode-repair-{}", Uuid::new_v4()));
        let mut auth = AuthStore::open(&root, "host").unwrap();
        let first = auth
            .issue_token(Some("phone-id".into()), phone("24129PN74C"))
            .unwrap();
        let record = auth.devices[0].clone();
        auth.devices[0].created_unix -= 60;
        auth.save().unwrap();
        let second = auth
            .issue_token(
                Some("phone-id".into()),
                DeviceDetails {
                    name: "Xiaomi 15".into(),
                    platform: Some("Android 16".into()),
                },
            )
            .unwrap();
        let legacy = auth.issue_token(None, phone("older app")).unwrap();
        let auth = AuthStore::open(&root, "host").unwrap();
        assert!(!auth.token_is_valid(&first));
        assert!(auth.token_is_valid(&second));
        assert!(auth.token_is_valid(&legacy));
        assert_eq!(auth.devices.len(), 2);
        assert_eq!(auth.devices[0].id, record.id);
        assert_eq!(auth.devices[0].created_unix, record.created_unix - 60);
        assert_eq!(auth.devices[0].name, "Xiaomi 15");
        assert_eq!(auth.devices[0].platform.as_deref(), Some("Android 16"));
        assert_eq!(auth.devices[1].client_id, None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hello_refreshes_details_by_token_and_saves_only_on_change() {
        let root = std::env::temp_dir().join(format!("tcode-refresh-{}", Uuid::new_v4()));
        let mut auth = AuthStore::open(&root, "host").unwrap();
        let token = auth
            .issue_token(Some("phone-id".into()), phone("Phone"))
            .unwrap();
        // Removing the file makes any write observable.
        fs::remove_file(root.join("remote.json")).unwrap();
        auth.refresh_device(&token, phone("Phone")).unwrap();
        auth.refresh_device("not-a-token", phone("Intruder"))
            .unwrap();
        assert!(!root.join("remote.json").exists());
        auth.refresh_device(
            &token,
            DeviceDetails {
                name: "Phone".into(),
                platform: Some("Android 16".into()),
            },
        )
        .unwrap();
        let reopened = AuthStore::open(&root, "host").unwrap();
        assert_eq!(reopened.devices[0].name, "Phone");
        assert_eq!(reopened.devices[0].platform.as_deref(), Some("Android 16"));
        assert_eq!(reopened.devices[0].client_id.as_deref(), Some("phone-id"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_auth_writes_do_not_rotate_or_hide_pending_device_updates() {
        let root = std::env::temp_dir().join(format!("tcode-auth-write-{}", Uuid::new_v4()));
        let mut auth = AuthStore::open(&root, "host").unwrap();
        let token = auth
            .issue_token(Some("phone-id".into()), phone("Phone"))
            .unwrap();
        // A directory in place of the temporary file deterministically fails
        // save on every platform, without relying on writable-user permissions.
        fs::create_dir(root.join("remote.tmp")).unwrap();
        assert!(
            auth.issue_token(Some("phone-id".into()), phone("Renamed"))
                .is_err()
        );
        assert!(
            auth.token_is_valid(&token),
            "a failed token rotation must keep the existing pairing"
        );
        assert!(auth.refresh_device(&token, phone("Renamed")).is_err());
        assert_eq!(auth.devices[0].name, "Phone");
        fs::remove_dir(root.join("remote.tmp")).unwrap();
        auth.refresh_device(&token, phone("Renamed")).unwrap();
        assert_eq!(
            AuthStore::open(&root, "host").unwrap().devices[0].name,
            "Renamed"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn remote_json_without_client_ids_or_platforms_still_loads() {
        let root = std::env::temp_dir().join(format!("tcode-legacy-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("remote.json"),
            br#"{
              "host_id": "3f2b8c6e-1d4a-4b9e-8c7d-2a1f0e9d8c7b",
              "host_name": "host",
              "devices": [{
                "id": "9a7c1e2d-5b6f-4a3c-8d9e-0f1a2b3c4d5e",
                "name": "old phone",
                "token_sha256_hex": "00",
                "created_unix": 1700000000
              }],
              "password": null
            }"#,
        )
        .unwrap();
        let mut auth = AuthStore::open(&root, "host").unwrap();
        assert_eq!(auth.devices[0].name, "old phone");
        assert_eq!(auth.devices[0].client_id, None);
        assert_eq!(auth.devices[0].platform, None);
        // A client id never matches a record that has none.
        auth.issue_token(Some("new-id".into()), phone("new phone"))
            .unwrap();
        assert_eq!(auth.devices.len(), 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn password_hash_persists_and_password_changes_keep_or_revoke_tokens() {
        let root = std::env::temp_dir().join(format!("tcode-password-{}", Uuid::new_v4()));
        let mut auth = AuthStore::open(&root, "host").unwrap();
        let token = auth.issue_token(None, phone("existing phone")).unwrap();
        assert!(!auth.verify_password("password"));
        assert!(auth.set_password("short", false).is_err());
        auth.set_password("test password", false).unwrap();
        let first_salt = auth.password.as_ref().unwrap().salt;
        let bytes = fs::read_to_string(root.join("remote.json")).unwrap();
        assert!(!bytes.contains("test password"));
        assert!(!bytes.contains(&token));
        assert!(bytes.contains("600000"));
        let mut auth = AuthStore::open(&root, "host").unwrap();
        assert!(auth.verify_password("test password"));
        assert!(auth.token_is_valid(&token));
        auth.set_password("new password", false).unwrap();
        assert_ne!(first_salt, auth.password.as_ref().unwrap().salt);
        assert!(!auth.verify_password("test password"));
        assert!(auth.verify_password("new password"));
        assert!(auth.token_is_valid(&token));
        auth.set_password("last password", true).unwrap();
        assert!(!auth.token_is_valid(&token));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn five_wrong_passwords_lock_out_even_the_right_password_then_recover() {
        let root = std::env::temp_dir().join(format!("tcode-lockout-{}", Uuid::new_v4()));
        let mut auth = AuthStore::open(&root, "host").unwrap();
        auth.set_password("correct password", false).unwrap();
        for _ in 0..5 {
            assert!(!auth.verify_password("wrong password"));
        }
        assert!(!auth.verify_password("correct password"));
        auth.locked_until = Some(std::time::Instant::now());
        assert!(auth.verify_password("correct password"));
        assert!(!auth.verify_password("wrong password"));
        assert!(auth.verify_password("correct password"));
        fs::remove_dir_all(root).unwrap();
    }
}
