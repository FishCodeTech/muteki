//! Persisted application settings.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[cfg(test)]
use agent::ProviderKind;
use serde::{Serialize, de::DeserializeOwned};
#[cfg(test)]
use tcode_core::settings::{EnvVar, ThemeMode};
use tcode_core::{
    control::{ControlActivityEntry, ReviewRecord},
    pull_request::HostKind,
    settings::Settings,
};

const REVIEWS_FILE: &str = "reviews.json";
const CONTROL_ACTIVITY_FILE: &str = "control_activity.json";

type Secrets = BTreeMap<String, BTreeMap<String, String>>;

/// Saved host tokens by kind, then by host. Profile ids are built-in ids or
/// slugs of ASCII alphanumerics and hyphens, so these keys never name a
/// profile, and a build that predates one keeps the entry as an unknown
/// profile's secrets.
fn token_secrets(kind: HostKind) -> &'static str {
    match kind {
        HostKind::Github => "@github",
        // One API and one token per server, whichever of the two it runs.
        HostKind::Forgejo | HostKind::Gitea => "@forgejo",
        HostKind::Gitlab => "@gitlab",
        HostKind::Bitbucket => "@bitbucket",
        // One token per organization, keyed by its host, `dev.azure.com/{organization}`.
        HostKind::Azure => "@azure",
    }
}

/// The account email saved beside a token of a kind that takes one, by host.
const EMAIL_SECRETS: &str = "@bitbucket-email";
const VERIFIED_SECRETS: &str = "@source-control-verified";

pub(crate) fn credential_fingerprint(token: &str, email: Option<&str>) -> String {
    use sha2::Digest as _;
    let mut hash = sha2::Sha256::new();
    hash.update((token.len() as u64).to_be_bytes());
    hash.update(token.as_bytes());
    if let Some(email) = email {
        hash.update([1]);
        hash.update(email.as_bytes());
    } else {
        hash.update([0]);
    }
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn saved_credential(all: &Secrets, kind: HostKind, host: &str) -> Option<(String, Option<String>)> {
    let token = all.get(token_secrets(kind))?.get(host)?.clone();
    let email = kind
        .takes_email()
        .then(|| all.get(EMAIL_SECRETS)?.get(host).cloned())
        .flatten();
    Some((token, email))
}

fn saved_fingerprint(all: &Secrets, kind: HostKind, host: &str) -> Option<String> {
    let (token, email) = saved_credential(all, kind, host)?;
    Some(credential_fingerprint(&token, email.as_deref()))
}

fn verification_key(kind: HostKind, host: &str) -> String {
    format!("{}:{host}", token_secrets(kind))
}

#[derive(Debug, Clone)]
pub struct SettingsStore {
    path: PathBuf,
    secrets_path: PathBuf,
    secrets_write: Arc<Mutex<()>>,
}

impl SettingsStore {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            path: data_dir.join("settings.json"),
            secrets_path: data_dir.join("secrets.json"),
            secrets_write: Arc::default(),
        }
    }

    pub fn load(&self) -> Settings {
        let Ok(bytes) = fs::read(&self.path) else {
            return Settings::default();
        };
        match Self::decode(&bytes) {
            Ok(settings) => settings,
            Err(err) => {
                log::warn!("failed to parse settings.json: {err}");
                Settings::default()
            }
        }
    }

    /// A read before an automatic update must not replace unreadable settings with defaults.
    pub fn try_load(&self) -> std::io::Result<Settings> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Settings::default());
            }
            Err(error) => return Err(error),
        };
        Self::decode(&bytes)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
    }

    fn decode(bytes: &[u8]) -> serde_json::Result<Settings> {
        let mut settings: Settings = serde_json::from_slice(bytes)?;
        settings.migrate_legacy();
        Ok(settings)
    }

    /// A file of the host's own beside settings.json.
    pub(crate) fn data_file(&self, name: &str) -> PathBuf {
        self.path.with_file_name(name)
    }

    /// Fails on an unreadable file so a write never replaces secrets it could not read.
    fn read_secrets(&self) -> std::io::Result<Secrets> {
        match fs::read(&self.secrets_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Secrets::new()),
            Err(error) => Err(error),
        }
    }

    /// Every stored provider secret, keyed by profile id then variable name.
    pub fn load_secrets(&self) -> BTreeMap<String, BTreeMap<String, String>> {
        self.try_load_secrets().unwrap_or_default()
    }

    fn try_load_secrets(&self) -> std::io::Result<Secrets> {
        let mut all = self.read_secrets()?;
        for kind in HostKind::ALL {
            all.remove(token_secrets(kind));
        }
        all.remove(EMAIL_SECRETS);
        all.remove(VERIFIED_SECRETS);
        Ok(all)
    }

    /// The sensitive env values for one profile id: a built-in profile's
    /// [`Settings::builtin_profile_id`] or a user profile's slug.
    pub fn profile_secrets(&self, profile_id: &str) -> BTreeMap<String, String> {
        self.try_profile_secrets(profile_id).unwrap_or_default()
    }

    pub fn try_profile_secrets(
        &self,
        profile_id: &str,
    ) -> std::io::Result<BTreeMap<String, String>> {
        Ok(self
            .try_load_secrets()?
            .remove(profile_id)
            .unwrap_or_default())
    }

    pub(crate) fn token(&self, kind: HostKind, host: &str) -> Option<String> {
        self.read_secrets()
            .ok()?
            .remove(token_secrets(kind))?
            .remove(host)
    }

    pub(crate) fn saved_credential(
        &self,
        kind: HostKind,
        host: &str,
    ) -> Option<(String, Option<String>)> {
        saved_credential(&self.read_secrets().ok()?, kind, host)
    }

    pub(crate) fn credential_verified(
        &self,
        kind: HostKind,
        host: &str,
        fingerprint: &str,
    ) -> bool {
        self.read_secrets()
            .ok()
            .and_then(|all| {
                all.get(VERIFIED_SECRETS)?
                    .get(&verification_key(kind, host))
                    .cloned()
            })
            .as_deref()
            == Some(fingerprint)
    }

    /// A response may arrive after a replacement was saved; it only changes the credential
    /// it was sent with. The same lock protects token and provider-secret writes.
    pub(crate) fn record_verification(
        &self,
        kind: HostKind,
        host: &str,
        fingerprint: &str,
        verified: bool,
    ) -> std::io::Result<()> {
        let _write = self.secrets_write.lock().unwrap();
        let mut all = self.read_secrets()?;
        if saved_fingerprint(&all, kind, host).as_deref() != Some(fingerprint) {
            return Ok(());
        }
        let key = verification_key(kind, host);
        let entries = all.entry(VERIFIED_SECRETS.to_owned()).or_default();
        if verified {
            if entries.get(&key).map(String::as_str) == Some(fingerprint) {
                return Ok(());
            }
            entries.insert(key, fingerprint.to_owned());
        } else if entries.remove(&key).is_none() {
            return Ok(());
        }
        if entries.is_empty() {
            all.remove(VERIFIED_SECRETS);
        }
        self.write_secrets(&all)
    }

    pub fn set_token(
        &self,
        kind: HostKind,
        host: &str,
        token: Option<&str>,
    ) -> std::io::Result<()> {
        self.set_credential(kind, host, token, None)
    }

    /// Saves (`Some`) or clears (`None`) the host's token. With `email`, the token is one sent
    /// with that email, and `None` keeps the token already saved; without, any saved email is
    /// cleared, so one method never outlives the other.
    pub fn set_credential(
        &self,
        kind: HostKind,
        host: &str,
        token: Option<&str>,
        email: Option<&str>,
    ) -> std::io::Result<()> {
        let host = kind
            .authority(host)
            .map_err(|refusal| std::io::Error::other(format!("{refusal:?}")))?;
        let email = email.map(str::trim).filter(|email| !email.is_empty());
        if email.is_some() && !kind.takes_email() {
            return Err(std::io::Error::other("this host takes no email"));
        }
        let _write = self.secrets_write.lock().unwrap();
        let mut all = self.read_secrets()?;
        let previous = saved_fingerprint(&all, kind, &host);
        // Only a token already sent with an email may keep going with another.
        let email_saved = all
            .get(EMAIL_SECRETS)
            .is_some_and(|emails| emails.contains_key(&host));
        let secrets = token_secrets(kind);
        let tokens = all.entry(secrets.to_owned()).or_default();
        match token.map(str::trim).filter(|token| !token.is_empty()) {
            Some(token) => {
                tokens.insert(host.clone(), token.to_owned());
            }
            None if email.is_some() && email_saved && tokens.contains_key(&host) => {}
            None if email.is_some() => {
                return Err(std::io::Error::other(
                    "no saved token to send with the email",
                ));
            }
            None => {
                tokens.remove(&host);
            }
        }
        if tokens.is_empty() {
            all.remove(secrets);
        }
        let emails = all.entry(EMAIL_SECRETS.to_owned()).or_default();
        match email {
            Some(email) => {
                emails.insert(host.clone(), email.to_owned());
            }
            None => {
                emails.remove(&host);
            }
        }
        if emails.is_empty() {
            all.remove(EMAIL_SECRETS);
        }
        if previous != saved_fingerprint(&all, kind, &host)
            && let Some(entries) = all.get_mut(VERIFIED_SECRETS)
        {
            entries.remove(&verification_key(kind, &host));
            if entries.is_empty() {
                all.remove(VERIFIED_SECRETS);
            }
        }
        self.write_secrets(&all)
    }

    /// Store (`Some`) or clear (`None`) one profile secret, by profile id.
    pub fn set_profile_secret(
        &self,
        profile_id: &str,
        name: &str,
        value: Option<&str>,
    ) -> std::io::Result<()> {
        let _write = self.secrets_write.lock().unwrap();
        let mut all = self.read_secrets()?;
        let entry = all.entry(profile_id.to_string()).or_default();
        match value {
            Some(value) => {
                entry.insert(name.to_string(), value.to_string());
            }
            None => {
                entry.remove(name);
            }
        }
        if entry.is_empty() {
            all.remove(profile_id);
        }
        self.write_secrets(&all)
    }

    /// Drop every secret stored for a profile id (used when deleting a profile).
    pub fn clear_profile_secrets(&self, profile_id: &str) -> std::io::Result<()> {
        let _write = self.secrets_write.lock().unwrap();
        let mut all = self.read_secrets()?;
        if all.remove(profile_id).is_some() {
            return self.write_secrets(&all);
        }
        Ok(())
    }

    fn write_secrets(&self, all: &Secrets) -> std::io::Result<()> {
        let data = serde_json::to_vec_pretty(all)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = self.secrets_path.with_extension("json.tmp");
        use std::io::Write as _;
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        restrict_permissions(&tmp)?;
        file.write_all(&data)?;
        fs::rename(&tmp, &self.secrets_path)?;
        restrict_permissions(&self.secrets_path)
    }

    /// The recorded conversation reviews, oldest first; unreadable records read as none.
    pub fn load_review_records(&self) -> Vec<ReviewRecord> {
        self.load_list(REVIEWS_FILE)
    }

    pub fn save_review_records(&self, records: &[ReviewRecord]) -> std::io::Result<()> {
        self.save_list(REVIEWS_FILE, records)
    }

    /// The recent `tcode_control` writes, newest first; an unreadable file reads as none.
    pub fn load_control_activity(&self) -> Vec<ControlActivityEntry> {
        self.load_list(CONTROL_ACTIVITY_FILE)
    }

    pub fn save_control_activity(&self, entries: &[ControlActivityEntry]) -> std::io::Result<()> {
        self.save_list(CONTROL_ACTIVITY_FILE, entries)
    }

    fn load_list<T: DeserializeOwned>(&self, name: &str) -> Vec<T> {
        let Ok(bytes) = fs::read(self.data_file(name)) else {
            return Vec::new();
        };
        serde_json::from_slice(&bytes).unwrap_or_else(|err| {
            log::warn!("failed to parse {name}: {err}");
            Vec::new()
        })
    }

    fn save_list<T: Serialize>(&self, name: &str, items: &[T]) -> std::io::Result<()> {
        let path = self.data_file(name);
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(items)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        fs::write(&tmp, data)?;
        fs::rename(tmp, path)
    }

    pub fn save(&self, settings: &Settings) -> std::io::Result<()> {
        let tmp = self.path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(settings)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        fs::write(&tmp, data)?;
        fs::rename(tmp, &self.path)
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_settings_updates_reject_corrupt_input() {
        let root =
            std::env::temp_dir().join(format!("tcode-settings-corrupt-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let store = SettingsStore::new(root.clone());
        fs::write(&store.path, r#"{"providers":{"codex":{"enabled":"yes"}}}"#).unwrap();
        assert_eq!(
            store.try_load().unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn loads_legacy_file_and_migrates_binary_paths() {
        // A settings.json written before the `providers` map existed must still
        // parse: its flat binary overrides migrate into the per-provider card,
        // and the newer fields default off / enabled.
        let root =
            std::env::temp_dir().join(format!("tcode-settings-legacy-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let store = SettingsStore::new(root.clone());
        fs::write(
            &store.path,
            r#"{"claude_binary":"/usr/bin/claude","theme_mode":"light","favorite_models":["opus"],"remote_hosting_enabled":true,"remote_port":47421}"#,
        )
        .unwrap();

        let loaded = store.load();
        assert_eq!(
            loaded.provider(ProviderKind::ClaudeCode).binary_path,
            Some(PathBuf::from("/usr/bin/claude"))
        );
        // The legacy keys are consumed, not echoed back.
        assert_eq!(loaded.claude_binary, None);
        assert_eq!(loaded.theme_mode, ThemeMode::Light);
        assert_eq!(loaded.favorite_models, vec!["opus".to_string()]);
        // Never-configured providers default to enabled with no overrides.
        let codex = loaded.provider(ProviderKind::Codex);
        assert!(codex.enabled);
        assert_eq!(codex.binary_path, None);
        // New fields tolerantly default to off.
        assert!(!loaded.word_wrap_diffs);
        assert!(!loaded.skip_delete_confirmation);
        // The HTTP listener's port is gone; hosting continues and the next
        // save no longer carries the port.
        assert!(loaded.remote_hosting_enabled);
        store.save(&loaded).unwrap();
        assert!(
            !fs::read_to_string(&store.path)
                .unwrap()
                .contains("remote_port")
        );
        let _ = fs::remove_dir_all(root);
    }

    /// Hosts saved before they had kinds are GitHub hosts, with their switch and account.
    #[test]
    fn github_hosts_from_before_kinds_load_as_github_source_control_hosts() {
        let root =
            std::env::temp_dir().join(format!("tcode-settings-hosts-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let store = SettingsStore::new(root.clone());
        fs::write(
            &store.path,
            r#"{"github":{"hosts":{"github.com":{"enabled":false,"account":"octocat"},"github.example.com":{}}}}"#,
        )
        .unwrap();

        let loaded = store.load();
        let hosts = &loaded.source_control.hosts;
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts["github.com"].kind, HostKind::Github);
        assert!(!hosts["github.com"].enabled);
        assert_eq!(hosts["github.com"].account.as_deref(), Some("octocat"));
        assert_eq!(hosts["github.example.com"].kind, HostKind::Github);
        assert!(hosts["github.example.com"].enabled);
        store.save(&loaded).unwrap();
        let saved = fs::read_to_string(&store.path).unwrap();
        assert!(!saved.contains("\"github\":"), "{saved}");
        assert_eq!(store.load().source_control.hosts, *hosts);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn secrets_persist_privately_and_clear_only_the_selected_profile() {
        let root =
            std::env::temp_dir().join(format!("tcode-settings-secret-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let store = SettingsStore::new(root.clone());

        let mut settings = Settings::default();
        settings.provider_mut(ProviderKind::ClaudeCode).env = vec![EnvVar {
            name: "ANTHROPIC_API_KEY".into(),
            // Sensitive rows carry no value in settings.json.
            value: String::new(),
            sensitive: true,
        }];
        store.save(&settings).unwrap();
        store
            .set_profile_secret(
                Settings::builtin_profile_id(ProviderKind::ClaudeCode),
                "ANTHROPIC_API_KEY",
                Some("sk-live"),
            )
            .unwrap();

        // settings.json never contains the secret; the reloaded row keeps its
        // name + sensitive flag and an empty value (nothing to echo back).
        let raw = fs::read_to_string(&store.path).unwrap();
        assert!(!raw.contains("sk-live"));
        let loaded = store.load();
        let env = loaded.provider(ProviderKind::ClaudeCode).env;
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].name, "ANTHROPIC_API_KEY");
        assert!(env[0].sensitive);
        assert!(env[0].value.is_empty());

        // The value is only reachable through the secrets store, which is 0600.
        let secrets = store.profile_secrets("claude");
        assert_eq!(
            secrets.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-live")
        );
        assert!(store.profile_secrets("codex").is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&store.secrets_path)
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        // Clearing removes the entry (and the now-empty provider bucket).
        store
            .set_profile_secret(
                Settings::builtin_profile_id(ProviderKind::ClaudeCode),
                "ANTHROPIC_API_KEY",
                None,
            )
            .unwrap();
        assert!(store.profile_secrets("claude").is_empty());
        // A user profile "klaude-kode" stores its own key, isolated from the
        // built-in "claude" profile.
        store
            .set_profile_secret("klaude-kode", "ANTHROPIC_API_KEY", Some("sk-kimi-xyz"))
            .unwrap();
        store
            .set_profile_secret(
                Settings::builtin_profile_id(ProviderKind::ClaudeCode),
                "ANTHROPIC_API_KEY",
                Some("sk-official"),
            )
            .unwrap();

        assert_eq!(
            store
                .profile_secrets("klaude-kode")
                .get("ANTHROPIC_API_KEY")
                .map(String::as_str),
            Some("sk-kimi-xyz")
        );
        // The built-in Claude profile keeps its `claude` bucket.
        assert_eq!(
            store
                .profile_secrets("claude")
                .get("ANTHROPIC_API_KEY")
                .map(String::as_str),
            Some("sk-official")
        );

        // Deleting a profile drops only its bucket.
        store.clear_profile_secrets("klaude-kode").unwrap();
        assert!(store.profile_secrets("klaude-kode").is_empty());
        assert_eq!(
            store
                .profile_secrets("claude")
                .get("ANTHROPIC_API_KEY")
                .map(String::as_str),
            Some("sk-official")
        );
        let _ = fs::remove_dir_all(root);
    }
}
