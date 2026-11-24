//! Workspace consent and UserNotifications receipts for the native desktop.
use crate::{bridge::Context, policy::BridgeError};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::PathBuf,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    pub connection_version: u64,
    pub service_id: String,
    pub identity_id: String,
    pub origin: String,
}

impl Scope {
    pub fn key(&self) -> String {
        json!([self.origin, self.service_id, self.identity_id]).to_string()
    }

    pub fn matches_active(&self, active: &Scope) -> bool {
        !self.origin.is_empty()
            && !self.service_id.is_empty()
            && !self.identity_id.is_empty()
            && self.origin == active.origin
            && self.service_id == active.service_id
            && self.identity_id == active.identity_id
    }
}

/// Whether a notification for `thread_id` should be suppressed because that
/// thread is currently visible in the active workbench window (whether focus is
/// in the main webview or the preview webview).
#[cfg(test)]
pub fn should_suppress_foreground(
    test: bool,
    document_visible: bool,
    window_active: bool,
    visible_threads: &[String],
    thread_id: &str,
) -> bool {
    !test
        && document_visible
        && window_active
        && !thread_id.is_empty()
        && visible_threads.iter().any(|t| t == thread_id)
}

/// Per-scope sound and delivery deduplication store.
#[derive(Default, Debug, Clone)]
pub struct ScopedDedupeStore {
    sounds_by_scope: HashMap<String, HashSet<String>>,
    delivered_by_scope: HashMap<String, HashMap<String, Value>>,
}

impl ScopedDedupeStore {
    pub fn has_sound(&self, scope: &Scope, dedupe_key: &str) -> bool {
        self.sounds_by_scope
            .get(&scope.key())
            .is_some_and(|set| set.contains(dedupe_key))
    }

    pub fn record_sound(&mut self, scope: &Scope, dedupe_key: &str) {
        self.sounds_by_scope
            .entry(scope.key())
            .or_default()
            .insert(dedupe_key.to_owned());
    }

    pub fn get_delivered(&self, scope: &Scope, dedupe_key: &str) -> Option<Value> {
        self.delivered_by_scope
            .get(&scope.key())
            .and_then(|map| map.get(dedupe_key))
            .cloned()
    }

    pub fn record_delivered(&mut self, scope: &Scope, dedupe_key: &str, receipt: Value) {
        self.delivered_by_scope
            .entry(scope.key())
            .or_default()
            .insert(dedupe_key.to_owned(), receipt);
    }

    #[cfg(test)]
    pub fn clear_scope(&mut self, scope_key: &str) {
        self.sounds_by_scope.remove(scope_key);
        self.delivered_by_scope.remove(scope_key);
    }
}

fn consent_path(context: &Context) -> PathBuf {
    context.state.join("desktop/notification-consents.json")
}

pub fn choice(context: &Context, scope: &Scope) -> Result<Option<bool>, BridgeError> {
    let bytes = match fs::read(consent_path(context)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(BridgeError::new("desktop.notification_preferences_failed", error.to_string())),
    };
    let choices: BTreeMap<String, bool> = serde_json::from_slice(&bytes)
        .map_err(|error| BridgeError::new("desktop.notification_preferences_failed", error.to_string()))?;
    Ok(choices.get(&scope.key()).copied())
}

pub fn save_choice(context: &Context, scope: &Scope, allow: bool) -> Result<(), BridgeError> {
    let file = consent_path(context);
    let mut choices: BTreeMap<String, bool> = match fs::read(&file) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| BridgeError::new("desktop.notification_preferences_failed", error.to_string()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(error) => return Err(BridgeError::new("desktop.notification_preferences_failed", error.to_string())),
    };
    choices.insert(scope.key(), allow);
    let directory = file.parent().expect("notification consents have a directory");
    fs::DirBuilder::new().recursive(true).mode(0o700).create(directory)
        .map_err(|error| BridgeError::new("desktop.notification_preferences_failed", error.to_string()))?;
    let temporary = directory.join(format!(".notification-consents-{}.tmp", uuid::Uuid::new_v4()));
    let mut writer = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)
        .map_err(|error| BridgeError::new("desktop.notification_preferences_failed", error.to_string()))?;
    let result = writer.write_all(serde_json::to_string_pretty(&choices).expect("boolean choices serialize").as_bytes())
        .and_then(|()| writer.sync_all())
        .and_then(|()| fs::rename(&temporary, &file));
    if result.is_err() { let _ = fs::remove_file(&temporary); }
    result.map_err(|error| BridgeError::new("desktop.notification_preferences_failed", error.to_string()))
}

pub fn status(scope: &Scope, choice: Option<bool>, system: Result<Value, BridgeError>, delivery: Option<&Value>) -> Value {
    let workspace_permission = match choice { Some(true) => "granted", Some(false) => "denied", None => "default" };
    let mut value = json!({
        "connectionVersion":scope.connection_version,"serviceId":scope.service_id,"identityId":scope.identity_id,
        "permission":workspace_permission,"workspacePermission":workspace_permission,"host":"desktop-client",
        "code":match choice { Some(true) => "desktop.notifications_workspace_allowed", Some(false) => "desktop.notifications_workspace_denied", None => "desktop.notifications_not_requested" },
    });
    match system {
        Ok(settings) => {
            value["systemPermission"] = json!(match settings["authorizationStatus"].as_str().unwrap_or("") {
                "notDetermined" => "default", "denied" => "denied", "authorized" => "granted", "provisional" => "provisional", "ephemeral" => "ephemeral", _ => "unknown",
            });
            value["systemNotificationSettings"] = settings;
        }
        Err(error) => {
            value["systemPermission"] = json!("unknown");
            value["systemPermissionError"] = error.to_json();
        }
    }
    if let Some(delivery) = delivery { value["delivery"] = delivery.clone(); }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_scope(service: &str, identity: &str, version: u64) -> Scope {
        Scope {
            connection_version: version,
            service_id: service.into(),
            identity_id: identity.into(),
            origin: "http://127.0.0.1:8100".into(),
        }
    }

    #[test]
    fn scope_matching_and_per_scope_dedupe_isolation() {
        let s1 = sample_scope("svc-1", "id-1", 1);
        let s1_v2 = sample_scope("svc-1", "id-1", 2);
        let s2 = sample_scope("svc-2", "id-1", 2);

        assert!(s1.matches_active(&s1_v2));
        assert!(!s1.matches_active(&s2));

        let mut store = ScopedDedupeStore::default();
        store.record_sound(&s1, "dedupe-1");
        store.record_delivered(&s1, "dedupe-1", json!({"status": "shown"}));

        assert!(store.has_sound(&s1, "dedupe-1"));
        assert!(!store.has_sound(&s2, "dedupe-1"));
        assert!(store.get_delivered(&s1, "dedupe-1").is_some());
        assert!(store.get_delivered(&s2, "dedupe-1").is_none());

        store.clear_scope(&s1.key());
        assert!(!store.has_sound(&s1, "dedupe-1"));
        assert!(store.get_delivered(&s1, "dedupe-1").is_none());
    }

    #[test]
    fn foreground_suppression_covers_chat_and_src_visible_threads() {
        let visible = vec!["chat-1".to_owned(), "src-2".to_owned()];
        assert!(should_suppress_foreground(false, true, true, &visible, "chat-1"));
        assert!(should_suppress_foreground(false, true, true, &visible, "src-2"));
        assert!(!should_suppress_foreground(false, true, true, &visible, "background-3"));
        assert!(!should_suppress_foreground(true, true, true, &visible, "chat-1"));
        assert!(!should_suppress_foreground(false, true, false, &visible, "chat-1"));
    }
}
