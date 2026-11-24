//! Durable node identities. Only hashes of bearer secrets enter the database.
use crate::store::Store;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

const NODES: &str = "runner_nodes";
const ENROLLMENTS: &str = "runner_enrollments";
pub const LEASE_SECONDS: i64 = 30;
const CREDENTIAL_SECONDS: i64 = 30 * 24 * 3600;

#[derive(Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub name: String,
    pub projects: Vec<String>,
    pub state: String,
    pub credential_version: u64,
    pub credential_expires_at: i64,
    credential_hash: String,
    pub session_generation: u64,
    session_hash: Option<String>,
    pub lease_expires_at: i64,
    pub created_at: i64,
}

#[derive(Serialize, Deserialize)]
struct Enrollment {
    name: String,
    projects: Vec<String>,
    expires_at: i64,
    consumed_by: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Credential {
    pub node_id: String,
    pub secret: String,
    pub version: u64,
    pub expires_at: i64,
    pub service_id: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Lease {
    pub node_id: String,
    pub secret: String,
    pub generation: u64,
    pub credential_version: u64,
    pub expires_at: i64,
}

fn secret() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| e.to_string())?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
fn hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
fn matches(secret: &str, expected: &str) -> bool {
    let actual = hash(secret);
    actual.len() == expected.len()
        && actual
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |a, (b, c)| a | (b ^ c))
            == 0
}
fn scope_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}
fn save(store: &Store, node: &Node) -> Result<(), String> {
    store
        .put(
            NODES,
            &node.id,
            &serde_json::to_value(node).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())
}
fn load(store: &Store, id: &str) -> Result<Node, String> {
    serde_json::from_value(
        store
            .get(NODES, id)
            .map_err(|e| e.to_string())?
            .ok_or("Invalid node identity")?,
    )
    .map_err(|e| e.to_string())
}
fn audit(store: &Store, node: &str, action: &str, now: i64) -> Result<(), String> {
    let id = uuid::Uuid::new_v4().to_string();
    store
        .put(
            "runner_audit",
            &id,
            &json!({"id":id,"node_id":node,"action":action,"at":now}),
        )
        .map_err(|e| e.to_string())
}
fn transaction<T>(store: &Store, apply: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    store.document_scope(apply).map_err(|e| e.to_string())?
}

pub fn issue_enrollment(
    store: &Store,
    name: String,
    mut projects: Vec<String>,
    ttl: i64,
    now: i64,
) -> Result<String, String> {
    if name.trim().is_empty()
        || name.len() > 120
        || name.chars().any(char::is_control)
        || projects.is_empty()
        || projects.len() > 64
        || projects.iter().any(|p| !scope_id(p))
        || !(1..=900).contains(&ttl)
    {
        return Err(
            "Enrollment requires a name, explicit project scope and a lifetime of 1–900 seconds"
                .into(),
        );
    }
    projects.sort();
    projects.dedup();
    let token = secret()?;
    let row = Enrollment {
        name,
        projects,
        expires_at: now + ttl,
        consumed_by: None,
    };
    transaction(store, || {
        store
            .put(
                ENROLLMENTS,
                &hash(&token),
                &serde_json::to_value(row).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
        audit(store, "", "enrollment_issued", now)
    })?;
    Ok(token)
}

pub fn enroll(store: &Store, token: &str, now: i64) -> Result<Credential, String> {
    if token.len() != 43 {
        return Err("Invalid or expired enrollment".into());
    }
    transaction(store, || {
        let key = hash(token);
        let mut enrollment: Enrollment = serde_json::from_value(
            store
                .get(ENROLLMENTS, &key)
                .map_err(|e| e.to_string())?
                .ok_or("Invalid or expired enrollment")?,
        )
        .map_err(|e| e.to_string())?;
        if enrollment.expires_at <= now || enrollment.consumed_by.is_some() {
            return Err("Invalid or expired enrollment".into());
        }
        let token = secret()?;
        let id = uuid::Uuid::new_v4().to_string();
        let node = Node {
            id: id.clone(),
            name: enrollment.name.clone(),
            projects: enrollment.projects.clone(),
            state: "active".into(),
            credential_hash: hash(&token),
            credential_version: 1,
            credential_expires_at: now + CREDENTIAL_SECONDS,
            session_generation: 0,
            session_hash: None,
            lease_expires_at: 0,
            created_at: now,
        };
        enrollment.consumed_by = Some(id.clone());
        store
            .put(
                ENROLLMENTS,
                &key,
                &serde_json::to_value(enrollment).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
        save(store, &node)?;
        audit(store, &id, "enrolled", now)?;
        Ok(Credential {
            node_id: id,
            secret: token,
            version: 1,
            expires_at: node.credential_expires_at,
            service_id: store.installation_id().map_err(|e| e.to_string())?,
        })
    })
}

fn authenticate(store: &Store, credential: &Credential, now: i64) -> Result<Node, String> {
    let node = load(store, &credential.node_id)?;
    if !matches!(node.state.as_str(), "active" | "draining")
        || node.credential_expires_at <= now
        || credential.version != node.credential_version
        || credential.service_id != store.installation_id().map_err(|e| e.to_string())?
        || !matches(&credential.secret, &node.credential_hash)
    {
        return Err("Invalid or expired node credential".into());
    }
    Ok(node)
}

/// A credential alone cannot evict an active session. Reconnect uses its lease secret.
pub fn connect(store: &Store, credential: &Credential, now: i64) -> Result<Lease, String> {
    transaction(store, || {
        let mut node = authenticate(store, credential, now)?;
        if node.lease_expires_at > now {
            return Err(
                "Node already has an active session; resume it or wait for lease expiry".into(),
            );
        }
        let token = secret()?;
        node.session_generation = node
            .session_generation
            .checked_add(1)
            .ok_or("Session generation exhausted")?;
        node.session_hash = Some(hash(&token));
        node.lease_expires_at = (now + LEASE_SECONDS).min(node.credential_expires_at);
        save(store, &node)?;
        audit(store, &node.id, "session_started", now)?;
        Ok(Lease {
            node_id: node.id,
            secret: token,
            generation: node.session_generation,
            credential_version: node.credential_version,
            expires_at: node.lease_expires_at,
        })
    })
}

fn verify_lease(store: &Store, lease: &Lease, now: i64) -> Result<Node, String> {
    let node = load(store, &lease.node_id)?;
    if !matches!(node.state.as_str(), "active" | "draining")
        || node.credential_expires_at <= now
        || node.lease_expires_at <= now
        || node.session_generation != lease.generation
        || node.credential_version != lease.credential_version
        || !node
            .session_hash
            .as_deref()
            .is_some_and(|h| matches(&lease.secret, h))
    {
        return Err("Invalid or expired node lease".into());
    }
    Ok(node)
}
pub fn projects(store: &Store, lease: &Lease, now: i64) -> Result<Vec<String>, String> {
    Ok(verify_lease(store, lease, now)?.projects)
}
pub fn allow_staging(store: &Store, node: &str, project: &str, now: i64) -> Result<(), String> {
    let node = load(store, node)?;
    if node.state != "active"
        || node.credential_expires_at <= now
        || !node.projects.iter().any(|id| id == project)
    {
        return Err("Node is not accepting staged files for this project".into());
    }
    Ok(())
}
pub fn renew(store: &Store, lease: &Lease, now: i64) -> Result<i64, String> {
    transaction(store, || {
        let mut node = verify_lease(store, lease, now)?;
        node.lease_expires_at = (now + LEASE_SECONDS).min(node.credential_expires_at);
        save(store, &node)?;
        Ok(node.lease_expires_at)
    })
}
pub fn authorize(
    store: &Store,
    lease: &Lease,
    project: &str,
    new_work: bool,
    now: i64,
) -> Result<(), String> {
    let node = verify_lease(store, lease, now)?;
    if !node.projects.iter().any(|p| p == project) {
        return Err("Node is not authorized for this project".into());
    }
    if new_work && node.state != "active" {
        return Err("Node is draining".into());
    }
    Ok(())
}

/// Administrator recovery/rotation invalidates every previous credential and session.
pub fn rotate(store: &Store, id: &str, now: i64) -> Result<Credential, String> {
    transaction(store, || {
        let mut node = load(store, id)?;
        if node.state == "revoked" {
            return Err("Revoked nodes must be enrolled with a new identity".into());
        }
        let token = secret()?;
        node.credential_hash = hash(&token);
        node.credential_version = node
            .credential_version
            .checked_add(1)
            .ok_or("Credential version exhausted")?;
        node.credential_expires_at = now + CREDENTIAL_SECONDS;
        node.session_hash = None;
        node.lease_expires_at = 0;
        save(store, &node)?;
        audit(store, id, "credential_rotated", now)?;
        Ok(Credential {
            node_id: id.into(),
            secret: token,
            version: node.credential_version,
            expires_at: node.credential_expires_at,
            service_id: store.installation_id().map_err(|e| e.to_string())?,
        })
    })
}
pub fn set_state(store: &Store, id: &str, state: &str, now: i64) -> Result<(), String> {
    if !matches!(state, "active" | "draining" | "revoked") {
        return Err("Invalid node state".into());
    }
    transaction(store, || {
        let mut node = load(store, id)?;
        if node.state == "revoked" {
            return Err("Revocation is permanent; enroll a new identity".into());
        }
        node.state = state.into();
        if state == "revoked" {
            node.session_hash = None;
            node.lease_expires_at = 0;
        }
        save(store, &node)?;
        audit(store, id, state, now)
    })
}
/// Only a revoked identity can be forgotten, so a live credential never outlives its row.
pub fn delete(store: &Store, id: &str, now: i64) -> Result<(), String> {
    transaction(store, || {
        let node = load(store, id)?;
        if node.state != "revoked" {
            return Err("Revoke the node before removing it".into());
        }
        store.delete(NODES, id).map_err(|e| e.to_string())?;
        store.delete("runner_node_resources", id).map_err(|e| e.to_string())?;
        store.delete("runner_resource_policies", id).map_err(|e| e.to_string())?;
        audit(store, id, "removed", now)
    })
}
pub fn audit_log(store: &Store) -> Result<Vec<serde_json::Value>, String> {
    let mut rows = store.list("runner_audit").map_err(|e| e.to_string())?;
    rows.sort_by_key(|row| std::cmp::Reverse(row["at"].as_i64().unwrap_or(0)));
    Ok(rows)
}
pub fn list(store: &Store) -> Result<Vec<serde_json::Value>, String> {
    store.list(NODES).map_err(|e| e.to_string()).map(|rows| {
        rows.into_iter()
            .map(|mut row| {
                if let Some(row) = row.as_object_mut() {
                    row.remove("credential_hash");
                    row.remove("session_hash");
                }
                row
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scoped_identity_rotation_revocation_and_session_fencing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("node.sqlite")).unwrap();
        let token =
            issue_enrollment(&store, "node".into(), vec!["project-a".into()], 60, 100).unwrap();
        let credential = enroll(&store, &token, 101).unwrap();
        assert!(enroll(&store, &token, 101).is_err());
        let lease = connect(&store, &credential, 102).unwrap();
        assert!(connect(&store, &credential, 103).is_err());
        assert!(authorize(&store, &lease, "project-b", true, 103).is_err());
        authorize(&store, &lease, "project-a", true, 103).unwrap();
        set_state(&store, &credential.node_id, "draining", 104).unwrap();
        assert!(authorize(&store, &lease, "project-a", true, 105).is_err());
        authorize(&store, &lease, "project-a", false, 105).unwrap();
        renew(&store, &lease, 106).unwrap();
        let rotated = rotate(&store, &credential.node_id, 107).unwrap();
        assert!(renew(&store, &lease, 108).is_err());
        assert!(connect(&store, &credential, 108).is_err());
        let next = connect(&store, &rotated, 108).unwrap();
        assert!(next.generation > lease.generation);
        assert!(renew(&store, &next, 139).is_err());
        let third = connect(&store, &rotated, 140).unwrap();
        assert!(renew(&store, &next, 141).is_err());
        set_state(&store, &credential.node_id, "revoked", 142).unwrap();
        assert!(renew(&store, &third, 143).is_err());
        assert!(rotate(&store, &credential.node_id, 143).is_err());
        let stored = serde_json::to_string(&store.list(NODES).unwrap()).unwrap();
        for secret in [
            &credential.secret,
            &rotated.secret,
            &lease.secret,
            &third.secret,
        ] {
            assert!(!stored.contains(secret));
        }
        assert!(!list(&store).unwrap()[0].to_string().contains("_hash"));
    }
    #[test]
    fn enrollment_is_atomic_across_store_connections_and_expires() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.sqlite");
        let store = Store::open(&path).unwrap();
        let token =
            issue_enrollment(&store, "node".into(), vec!["project".into()], 10, 100).unwrap();
        let other = Store::open(&path).unwrap();
        let copy = token.clone();
        let thread = std::thread::spawn(move || enroll(&other, &copy, 101));
        let first = enroll(&store, &token, 101);
        assert_eq!(
            usize::from(first.is_ok()) + usize::from(thread.join().unwrap().is_ok()),
            1
        );
        let expired =
            issue_enrollment(&store, "expired".into(), vec!["project".into()], 1, 100).unwrap();
        assert!(enroll(&store, &expired, 101).is_err());
        assert!(issue_enrollment(&store, "node".into(), vec!["*".into()], 60, 100).is_err());
    }
}
