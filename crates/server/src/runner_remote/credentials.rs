//! Scoped credential material: SQLite contains metadata, private version files contain secrets.
mod api;
#[cfg(test)]
mod tests;
use super::{identity, transfers::Scope};
use crate::store::Store;
pub(super) use api::{admin_router, node_router};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
const MATERIALS: &str = "runner_credential_materials";
const GRANTS: &str = "runner_credential_grants";
pub const MAX_BYTES: usize = 256 * 1024;

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Material {
    pub kind: String,
    /// Base64 regular file contents, relative to this grant's private writable home.
    pub files: BTreeMap<String, String>,
    #[serde(default)]
    pub env_files: BTreeMap<String, String>,
    #[serde(default)]
    pub env_dirs: BTreeMap<String, String>,
}
impl Material {
    pub fn validate(&self) -> Result<(), String> {
        if !matches!(self.kind.as_str(), "model" | "target")
            || self.files.is_empty()
            || self.files.len() > 32
        {
            return Err("Credential material requires model/target kind and 1–32 files".into());
        }
        let mut total = 0usize;
        for (path, content) in &self.files {
            super::transfers::validate_path(path)?;
            if path.len() > 1024 || path.split('/').count() > 8 {
                return Err("Credential file path is too deep".into());
            }
            if content.len() > MAX_BYTES * 4 / 3 + 4 {
                return Err("Credential file exceeds its bound".into());
            }
            let bytes = STANDARD
                .decode(content)
                .map_err(|_| "Invalid credential encoding")?;
            total = total
                .checked_add(bytes.len())
                .ok_or("Credential size overflow")?;
            if total > MAX_BYTES {
                return Err("Credential bundle exceeds 256 KiB".into());
            }
        }
        if self.files.keys().any(|file| {
            self.files
                .keys()
                .any(|other| other.starts_with(&format!("{file}/")))
        }) {
            return Err("Credential file paths conflict with directories".into());
        }
        for (key, path) in &self.env_files {
            if !matches!(
                key.as_str(),
                "CLAUDE_CODE_OAUTH_TOKEN_FILE"
                    | "ANTHROPIC_AUTH_TOKEN_FILE"
                    | "CURSOR_API_KEY_FILE"
                    | "CURSOR_AUTH_TOKEN_FILE"
                    | "FACTORY_API_KEY_FILE"
                    | "ANTHROPIC_API_KEY_FILE"
                    | "OPENAI_API_KEY_FILE"
                    | "OPENCODE_API_KEY_FILE"
                    | "DEEPSEEK_API_KEY_FILE"
                    | "KIMI_MODEL_API_KEY_FILE"
                    | "XAI_API_KEY_FILE"
            ) || !self.files.contains_key(path)
            {
                return Err("Unsupported credential environment-file reference".into());
            }
        }
        for (key, path) in &self.env_dirs {
            if !matches!(
                key.as_str(),
                "CODEX_HOME"
                    | "KIMI_CODE_HOME"
                    | "PI_CODING_AGENT_DIR"
                    | "GROK_HOME"
                    | "XDG_CONFIG_HOME"
                    | "XDG_DATA_HOME"
            ) {
                return Err("Unsupported credential environment-directory reference".into());
            }
            super::transfers::validate_path(path)?;
            if !self
                .files
                .keys()
                .any(|file| file.starts_with(&format!("{path}/")))
            {
                return Err("Credential directory has no selected files".into());
            }
        }
        if self.env_files.len() + self.env_dirs.len() > 16 {
            return Err("Too many credential environment references".into());
        }
        Ok(())
    }
    pub fn revision(&self) -> Result<String, String> {
        self.validate()?;
        crate::runner::canonical_digest(json!(self))
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub id: String,
    pub material_id: String,
    pub revision: String,
    pub scope: Scope,
    pub expires_at: i64,
    pub state: String,
}
#[derive(Clone)]
pub struct Vault {
    store: Store,
    root: PathBuf,
}
fn id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        Err("Invalid credential identity".into())
    } else {
        Ok(())
    }
}
impl Vault {
    pub fn open(store: Store, root: &Path) -> Result<Self, String> {
        if !root.is_absolute() {
            return Err("Credential vault requires an absolute state root".into());
        }
        Ok(Self {
            store,
            root: root.to_owned(),
        })
    }
    fn version_path(&self, material: &str, revision: &str) -> Result<PathBuf, String> {
        id(material)?;
        let hash = revision
            .strip_prefix("canonical-v1:")
            .filter(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or("Invalid credential revision")?;
        Ok(self
            .root
            .join("runner/credential-vault")
            .join(material)
            .join(format!("{hash}.json")))
    }
    pub fn metadata(&self, material: &str) -> Result<Value, String> {
        id(material)?;
        self.store
            .get(MATERIALS, material)
            .map_err(|e| e.to_string())?
            .ok_or("Unknown credential material".into())
    }
    fn prepare_version(&self, material: &str, bundle: &Material) -> Result<String, String> {
        id(material)?;
        let _lock = crate::private_files::lock(
            &self.root,
            &self
                .root
                .join("runner/credential-vault-locks")
                .join(format!("{material}.lock")),
        )?;
        if self
            .store
            .get(MATERIALS, material)
            .map_err(|e| e.to_string())?
            .is_some_and(|row| row["state"] == "revoked")
        {
            return Err("Credential material is revoked".into());
        }
        let revision = bundle.revision()?;
        let bytes = serde_json::to_vec(bundle).map_err(|e| e.to_string())?;
        let key = format!("{material}:{revision}");
        self.store.document_scope(||->Result<(),String>{
            if self.store.get("runner_credential_versions",&key).map_err(|e|e.to_string())?.is_none(){
                let versions=self.store.list("runner_credential_versions").map_err(|e|e.to_string())?;
                let used=versions.iter().try_fold(0u64,|sum,row|sum.checked_add(row["bytes"].as_u64().unwrap_or(u64::MAX))).ok_or("Credential quota accounting overflow")?;
                if versions.len()>=4096||bytes.len() as u64> (64u64*1024*1024).saturating_sub(used){return Err("Credential private-version quota exceeded".into());}
                self.store.put("runner_credential_versions",&key,&json!({"material_id":material,"revision":revision,"bytes":bytes.len()})).map_err(|e|e.to_string())?;
            }Ok(())
        }).map_err(|e|e.to_string())??;
        crate::private_files::write(&self.root, &self.version_path(material, &revision)?, &bytes)?;
        Ok(revision)
    }
    fn commit_material(
        &self,
        material: &str,
        expected: Option<&str>,
        bundle: &Material,
        revision: &str,
    ) -> Result<Value, String> {
        let prior = self
            .store
            .get(MATERIALS, material)
            .map_err(|e| e.to_string())?;
        if prior.as_ref().and_then(|row| row["revision"].as_str()) != expected {
            return Err(
                "Credential revision conflict; refresh must not overwrite newer material".into(),
            );
        }
        if prior
            .as_ref()
            .is_some_and(|row| row["state"] != "active" || row["kind"] != bundle.kind)
        {
            return Err("Credential is revoked or has a different kind".into());
        }
        let row = json!({"id":material,"kind":bundle.kind,"revision":revision,"state":"active","updated_at":chrono::Utc::now().to_rfc3339()});
        self.store
            .put(MATERIALS, material, &row)
            .map_err(|e| e.to_string())?;
        Ok(row)
    }
    pub fn put(
        &self,
        material: &str,
        expected: Option<&str>,
        bundle: &Material,
    ) -> Result<Value, String> {
        id(material)?;
        let prior = self
            .store
            .get(MATERIALS, material)
            .map_err(|e| e.to_string())?;
        if prior.as_ref().and_then(|row| row["revision"].as_str()) != expected {
            return Err("Credential revision conflict".into());
        }
        let revision = self.prepare_version(material, bundle)?;
        self.store
            .document_scope(|| -> Result<Value, String> {
                let row = self.commit_material(material, expected, bundle, &revision)?;
                for mut grant in self.store.list(GRANTS).map_err(|e| e.to_string())? {
                    if grant["material_id"] == material
                        && grant["revision"] != revision
                        && grant["state"] == "active"
                    {
                        grant["state"] = json!("revoked");
                        self.store
                            .put(
                                GRANTS,
                                grant["id"].as_str().ok_or("Grant ID missing")?,
                                &grant,
                            )
                            .map_err(|e| e.to_string())?;
                    }
                }
                Ok(row)
            })
            .map_err(|e| e.to_string())?
    }
    pub fn grant(&self, material: &str, scope: Scope, ttl: i64, now: i64) -> Result<Grant, String> {
        for value in [&scope.node_id, &scope.project_id, &scope.run_id] {
            id(value)?;
        }
        if scope.generation == 0 || !(1..=86400).contains(&ttl) {
            return Err(
                "Credential grant requires a generation and a lifetime of at most one day".into(),
            );
        }
        self.store
            .document_scope(|| -> Result<Grant, String> {
                self.authorize_scope(&scope)?;
                identity::allow_staging(&self.store, &scope.node_id, &scope.project_id, now)?;
                let row = self.metadata(material)?;
                if row["state"] != "active" {
                    return Err("Credential material is revoked".into());
                }
                let grant = Grant {
                    id: uuid::Uuid::new_v4().simple().to_string(),
                    material_id: material.into(),
                    revision: row["revision"]
                        .as_str()
                        .ok_or("Credential revision missing")?
                        .into(),
                    scope,
                    expires_at: now + ttl,
                    state: "active".into(),
                };
                self.store
                    .put(GRANTS, &grant.id, &json!(grant))
                    .map_err(|e| e.to_string())?;
                Ok(grant)
            })
            .map_err(|e| e.to_string())?
    }
    pub fn get_grant(&self, grant: &str) -> Result<Grant, String> {
        id(grant)?;
        serde_json::from_value(
            self.store
                .get(GRANTS, grant)
                .map_err(|e| e.to_string())?
                .ok_or("Unknown credential grant")?,
        )
        .map_err(|e| e.to_string())
    }
    fn authorize_scope(&self, scope: &Scope) -> Result<(), String> {
        let binding = self.store.get("runner_controller_runs", &format!("{}:{}", scope.run_id, scope.generation))
            .map_err(|e| e.to_string())?.ok_or("Credential execution binding is missing")?;
        if binding["node_id"] != scope.node_id || binding["project_id"] != scope.project_id
            || binding["generation"] != scope.generation
            || !matches!(binding["state"].as_str(), Some("preparing" | "active")) {
            return Err("Credential execution is no longer active".into());
        }
        Ok(())
    }
    pub fn revoke_scope(&self, scope: &Scope) -> Result<(), String> {
        self.store.document_scope(|| -> Result<(), String> {
            for mut grant in self.store.list(GRANTS).map_err(|e| e.to_string())? {
                if grant["scope"] == json!(scope) && grant["state"] == "active" {
                    grant["state"] = json!("revoked");
                    self.store.put(GRANTS, grant["id"].as_str().ok_or("Grant ID missing")?, &grant).map_err(|e| e.to_string())?;
                }
            }
            Ok(())
        }).map_err(|e| e.to_string())?
    }
    pub fn authorize(
        &self,
        grant: &str,
        lease: &identity::Lease,
        now: i64,
    ) -> Result<Grant, String> {
        self.store.document_scope(|| -> Result<Grant, String> {
            let grant = self.get_grant(grant)?;
            self.authorize_scope(&grant.scope)?;
            identity::authorize(&self.store, lease, &grant.scope.project_id, false, now)?;
            let material = self.metadata(&grant.material_id)?;
            if grant.state != "active"
                || grant.expires_at <= now
                || grant.scope.node_id != lease.node_id
                || material["state"] != "active"
            {
                return Err("Credential grant is revoked, expired or belongs to another node".into());
            }
            Ok(grant)
        }).map_err(|e| e.to_string())?
    }
    pub fn reveal(
        &self,
        grant: &str,
        lease: &identity::Lease,
        now: i64,
    ) -> Result<(Grant, Material), String> {
        let grant = self.authorize(grant, lease, now)?;
        let path = self.version_path(&grant.material_id, &grant.revision)?;
        let bytes = crate::private_files::read(&self.root, &path, MAX_BYTES * 2)?;
        let material: Material =
            serde_json::from_slice(&bytes).map_err(|_| "Invalid saved credential version")?;
        if material.revision()? != grant.revision {
            return Err("Credential version content does not match its revision".into());
        }
        let current = self.authorize(&grant.id, lease, chrono::Utc::now().timestamp().max(now))?;
        if current.revision != grant.revision {
            return Err("Credential version changed during retrieval".into());
        }
        Ok((grant, material))
    }
    pub fn refresh(
        &self,
        grant: &str,
        lease: &identity::Lease,
        expected: &str,
        bundle: &Material,
        now: i64,
    ) -> Result<Grant, String> {
        let (initial, old) = self.reveal(grant, lease, now)?;
        if bundle.kind != old.kind
            || bundle.env_files != old.env_files
            || bundle.env_dirs != old.env_dirs
            || bundle.files.keys().ne(old.files.keys())
        {
            return Err("Refresh cannot change the selected credential files or bindings".into());
        }
        let candidate = bundle.revision()?;
        let receipt_key = format!("{}:{expected}", initial.id);
        if let Some(receipt) = self
            .store
            .get("runner_credential_refreshes", &receipt_key)
            .map_err(|e| e.to_string())?
        {
            if receipt["revision"] != candidate {
                return Err("Refresh retry has different credential bytes".into());
            }
            return serde_json::from_value(receipt["grant"].clone()).map_err(|e| e.to_string());
        }
        if self.metadata(&initial.material_id)?["revision"] != expected {
            return Err("Credential revision conflict; newer material was preserved".into());
        }
        let revision = self.prepare_version(&initial.material_id, bundle)?;
        self.store
            .document_scope(|| -> Result<Grant, String> {
                let mut grant =
                    self.authorize(grant, lease, chrono::Utc::now().timestamp().max(now))?;
                let receipt_key = format!("{}:{expected}", grant.id);
                if let Some(receipt) = self
                    .store
                    .get("runner_credential_refreshes", &receipt_key)
                    .map_err(|e| e.to_string())?
                {
                    if receipt["revision"] != revision {
                        return Err("Refresh retry has different credential bytes".into());
                    }
                    return serde_json::from_value(receipt["grant"].clone())
                        .map_err(|e| e.to_string());
                }
                if grant.revision != expected {
                    return Err("Credential grant revision conflict".into());
                }
                self.commit_material(&grant.material_id, Some(expected), bundle, &revision)?;
                grant.revision = revision.clone();
                self.store
                    .put(GRANTS, &grant.id, &json!(grant))
                    .map_err(|e| e.to_string())?;
                self.store
                    .put(
                        "runner_credential_refreshes",
                        &receipt_key,
                        &json!({"revision":revision,"grant":grant}),
                    )
                    .map_err(|e| e.to_string())?;
                Ok(grant)
            })
            .map_err(|e| e.to_string())?
    }
    pub fn revoke_material(&self, material: &str) -> Result<Value, String> {
        id(material)?;
        self.store
            .document_scope(|| -> Result<(), String> {
                let mut row = self.metadata(material)?;
                row["state"] = json!("revoked");
                self.store
                    .put(MATERIALS, material, &row)
                    .map_err(|e| e.to_string())?;
                for mut grant in self.store.list(GRANTS).map_err(|e| e.to_string())? {
                    if grant["material_id"] == material {
                        grant["state"] = json!("revoked");
                        self.store
                            .put(
                                GRANTS,
                                grant["id"].as_str().ok_or("Grant ID missing")?,
                                &grant,
                            )
                            .map_err(|e| e.to_string())?;
                    }
                }
                Ok(())
            })
            .map_err(|e| e.to_string())??;
        let _lock = crate::private_files::lock(
            &self.root,
            &self
                .root
                .join("runner/credential-vault-locks")
                .join(format!("{material}.lock")),
        )?;
        crate::private_files::remove_tree(
            &self.root,
            &self.root.join("runner/credential-vault").join(material),
        )?;
        self.store
            .document_scope(|| -> Result<Value, String> {
                for row in self
                    .store
                    .list("runner_credential_versions")
                    .map_err(|e| e.to_string())?
                {
                    if row["material_id"] == material {
                        let revision = row["revision"]
                            .as_str()
                            .ok_or("Credential revision missing")?;
                        self.store
                            .delete(
                                "runner_credential_versions",
                                &format!("{material}:{revision}"),
                            )
                            .map_err(|e| e.to_string())?;
                    }
                }
                let mut row = self.metadata(material)?;
                row["private_data_removed"] = json!(true);
                self.store
                    .put(MATERIALS, material, &row)
                    .map_err(|e| e.to_string())?;
                Ok(row)
            })
            .map_err(|e| e.to_string())?
    }
    pub fn revoke(&self, grant: &str) -> Result<Grant, String> {
        self.store
            .document_scope(|| -> Result<Grant, String> {
                let mut grant = self.get_grant(grant)?;
                grant.state = "revoked".into();
                self.store
                    .put(GRANTS, &grant.id, &json!(grant))
                    .map_err(|e| e.to_string())?;
                Ok(grant)
            })
            .map_err(|e| e.to_string())?
    }
}
