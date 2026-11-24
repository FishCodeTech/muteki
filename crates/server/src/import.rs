//! Explicit, private migration of persisted configuration. Never discovers host logins.
use crate::store::Store;
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use std::{fs, path::Path};

#[derive(Default, Serialize, Debug)]
pub struct ImportSummary {
    pub accounts: usize,
    pub files: usize,
    pub credentials: usize,
    pub models: usize,
    pub configurations: usize,
    pub historical_usage: usize,
    pub provider_usage_history: usize,
    pub skipped: usize,
    pub conflicts: usize,
}
pub(crate) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        && id != "."
        && id != ".."
}
fn private_dir(path: &Path) -> Result<()> {
    if path
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        bail!("Import destination cannot be a symlink");
    }
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)?;
    Ok(())
}
fn put(
    store: &Store,
    collection: &str,
    id: &str,
    value: &Value,
    summary: &mut ImportSummary,
) -> Result<bool> {
    let marker = format!("{collection}:{id}");
    let existing = store.get(collection, id)?;
    if let Some(existing) = existing {
        if existing == *value {
            summary.skipped += 1;
            return Ok(false);
        }
        summary.conflicts += 1;
        return Ok(false);
    }
    store.put(collection, id, value)?;
    store.put(
        "import_origins",
        &marker,
        &json!({"source":"legacy_python"}),
    )?;
    Ok(true)
}
fn copy_tree(
    source: &Path,
    target: &Path,
    prefix: &str,
    store: &Store,
    summary: &mut ImportSummary,
) -> Result<()> {
    private_dir(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            bail!("Symlinks in source credential storage are not supported");
        }
        let name = entry.file_name();
        let key = format!("{prefix}/{}", name.to_string_lossy());
        let dest = target.join(&name);
        if kind.is_dir() {
            copy_tree(&entry.path(), &dest, &key, store, summary)?;
        } else if kind.is_file() {
            let bytes = fs::read(entry.path())?;
            if dest.symlink_metadata().is_ok() {
                let meta = dest.symlink_metadata()?;
                if !meta.is_file() || meta.file_type().is_symlink() {
                    bail!("Unsafe credential import destination");
                }
                if fs::read(&dest)? == bytes {
                    summary.skipped += 1;
                } else {
                    summary.conflicts += 1;
                }
            } else {
                private_file(&dest, &bytes)?;
                store.put(
                    "import_files",
                    &key,
                    &json!({"source":"legacy_python"}),
                )?;
                summary.files += 1;
            }
        }
    }
    Ok(())
}
fn text(dir: &Path, name: &str) -> Option<String> {
    fs::read_to_string(dir.join(name))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}
fn json_file(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?).context("Invalid source configuration JSON")?)
}
fn account(dir: &Path, id: &str) -> Result<Value> {
    let secret = text(dir, "API_KEY").or_else(|| text(dir, "CURSOR_API_KEY"));
    let models = if dir.join("MODELS.json").exists() {
        json_file(&dir.join("MODELS.json"))?
    } else {
        json!([])
    };
    let engine = text(dir, "ENGINE").unwrap_or_else(|| {
        if dir.join("CURSOR_API_KEY").exists() {
            "cursor".into()
        } else {
            "api".into()
        }
    });
    let base = text(dir, "BASE_URL");
    Ok(
        json!({"id":id,"account_id":id,"label":id,"engine":engine,"worker_engine":engine,
        "connection":if base.is_some(){"custom"}else{"official"},"base_url":base,
        "provider":text(dir,"PROVIDER"),"revision":text(dir,"REVISION").unwrap_or_else(||"imported".into()),
        "secret":secret,"models":models,"default_model":text(dir,"DEFAULT_MODEL").or_else(||text(dir,"MODEL")),
        "present":true,"status":"imported","import_source":"legacy_python","import_account":id}),
    )
}
/// The caller must supply the source project explicitly; only its state directory is read.
pub fn run(source_root: &Path, state_dir: &Path, store: &Store) -> Result<ImportSummary> {
    let source = source_root.join("state");
    if !source.is_dir() {
        bail!("Source project state directory is missing");
    }
    private_dir(state_dir)?;
    let target = state_dir.join("imported-credentials");
    private_dir(&target)?;
    let mut summary = ImportSummary::default();
    let accounts = source.join("_secrets/accounts");
    if accounts.is_dir() {
        if accounts.symlink_metadata()?.file_type().is_symlink() {
            bail!("Source account root cannot be a symlink");
        }
        for entry in fs::read_dir(accounts)? {
            let entry = entry?;
            if entry.file_type()?.is_symlink() {
                bail!("Source account cannot be a symlink");
            }
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            if !valid_id(&id) {
                bail!("Invalid source account identifier");
            }
            let destination = target.join("accounts").join(&id);
            private_dir(&target.join("accounts"))?;
            copy_tree(
                &entry.path(),
                &destination,
                &format!("accounts/{id}"),
                store,
                &mut summary,
            )?;
            let value = account(&destination, &id)?;
            if put(store, "credential_accounts", &id, &value, &mut summary)? {
                summary.accounts += 1;
            }
        }
    }
    for name in [
        "_worker_config.json",
        "_credential_model_catalog.json",
        "_usage_model_rates.json",
    ] {
        let path = source.join(name);
        if !path.exists() {
            continue;
        }
        let metadata = path.symlink_metadata()?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            bail!("Source configuration must be a regular file");
        }
        let bytes = fs::read(&path)?;
        let destination = target.join(name);
        if destination.exists() {
            if destination.symlink_metadata()?.file_type().is_symlink() {
                bail!("Import destination cannot be a symlink");
            }
            if fs::read(&destination)? != bytes {
                summary.conflicts += 1;
            } else {
                summary.skipped += 1;
            }
        } else {
            private_file(&destination, &bytes)?;
            summary.files += 1;
        }
        let value = json_file(&destination)?;
        if put(store, "import_metadata", name, &value, &mut summary)? {
            summary.configurations += 1;
        }
        if name == "_worker_config.json" {
            for collection in ["credentials", "seats", "worker_profiles"] {
                for row in value[collection].as_array().into_iter().flatten() {
                    if let Some(id) = row["id"].as_str().filter(|s| !s.is_empty()) {
                        if put(store, collection, id, row, &mut summary)?
                            && collection == "credentials"
                        {
                            summary.credentials += 1;
                        }
                    }
                }
            }
            if value["llm_profiles"].is_object() {
                put(
                    store,
                    "settings",
                    "imported_llm_profiles",
                    &value["llm_profiles"],
                    &mut summary,
                )?;
            }
        } else if let Some(catalogs) = value["catalogs"].as_object() {
            for (key, row) in catalogs {
                if put(store, "model_catalogs", key, row, &mut summary)? {
                    summary.models += ["configured_models", "discovered_models", "verified_models"]
                        .iter()
                        .map(|k| row[*k].as_array().map_or(0, Vec::len))
                        .sum::<usize>();
                }
            }
        }
    }
    let pricing = source.join("_usage_pricing.sqlite3");
    if pricing.exists() {
        let metadata = pricing.symlink_metadata()?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            bail!("Source pricing configuration must be a regular file");
        }
        let db = rusqlite::Connection::open_with_flags(
            &pricing,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let mut statement = db.prepare("SELECT kind, identity, data FROM config WHERE kind IN ('override','reference') ORDER BY kind, identity")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (kind, identity, data) = row?;
            let raw: Value =
                serde_json::from_str(&data).context("Decode saved pricing configuration")?;
            let value =
                crate::pricing_read::config_entry(&kind, &raw).map_err(anyhow::Error::msg)?;
            if put(
                store,
                &format!("pricing_{kind}"),
                &identity,
                &value,
                &mut summary,
            )? {
                summary.configurations += 1;
            }
        }
    }
    (summary.historical_usage, summary.provider_usage_history) =
        crate::usage_history::import(&source, store)?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copies_private_credentials_and_preserves_edits_on_reimport() {
        let root = std::env::temp_dir().join(format!("muteki-import-{}", uuid::Uuid::new_v4()));
        let source = root.join("source");
        let accounts = source.join("state/_secrets/accounts/sample");
        fs::create_dir_all(accounts.join("pi-agent")).unwrap();
        fs::write(accounts.join("API_KEY"), "synthetic-test-key").unwrap();
        fs::write(accounts.join("BASE_URL"), "https://example.invalid/v1").unwrap();
        fs::write(accounts.join("DEFAULT_MODEL"), "test-model").unwrap();
        fs::write(accounts.join("pi-agent/auth.json"), "{\"test\":true}").unwrap();
        fs::write(
            source.join("state/_worker_config.json"),
            r#"{"credentials":[{"id":"account:sample","secret_ref":"sample"}],"seats":[]}"#,
        )
        .unwrap();
        let target = root.join("target");
        let store = Store::open(":memory:").unwrap();
        let first = run(&source, &target, &store).unwrap();
        assert_eq!(first.accounts, 1);
        assert_eq!(first.credentials, 1);
        assert_eq!(first.files, 5);
        assert_eq!(run(&source, &target, &store).unwrap().conflicts, 0);
        let mut account = store.get("credential_accounts", "sample").unwrap().unwrap();
        account["label"] = json!("Edited locally");
        store
            .put("credential_accounts", "sample", &account)
            .unwrap();
        fs::write(accounts.join("API_KEY"), "changed-source-key").unwrap();
        assert!(run(&source, &target, &store).unwrap().conflicts >= 2);
        assert_eq!(
            store.get("credential_accounts", "sample").unwrap().unwrap()["label"],
            "Edited locally"
        );
        assert_eq!(
            fs::read_to_string(target.join("imported-credentials/accounts/sample/API_KEY"))
                .unwrap(),
            "synthetic-test-key"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(target.join("imported-credentials/accounts/sample/API_KEY"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(target.join("imported-credentials/accounts/sample"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
}
