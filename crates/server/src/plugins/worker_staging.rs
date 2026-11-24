//! Stage only enabled, mode-authorized package snapshots for a Worker runtime.
use super::*;
#[derive(Debug, Default, Clone)]
pub struct StagedWorkerComponents {
    pub skills: Vec<PathBuf>,
    pub extensions: Vec<PathBuf>,
}

impl Registry {
    pub fn stage_worker_components(&self, workspace: &Path, engine: &str, mode: &str) -> Result<Vec<PathBuf>> {
        Ok(self.stage_worker_assets(workspace, engine, mode)?.extensions)
    }

    pub fn stage_worker_assets(&self, workspace: &Path, engine: &str, mode: &str) -> Result<StagedWorkerComponents> {
        let mut staged = StagedWorkerComponents::default();
        for record in self.enabled(engine, mode)? {
            let source = PathBuf::from(record["root"].as_str().ok_or_else(|| PluginError::invalid("Package root missing"))?);
            let id = record["id"].as_str().unwrap_or("package");
            let id = sha_hex(id.as_bytes());
            let target = workspace.join(".muteki-managed-plugins").join(id);
            for entry in walkdir::WalkDir::new(&source).follow_links(false) {
                let entry = entry.map_err(store_error)?;
                let path = entry.path();
                let relative = path.strip_prefix(&source).map_err(store_error)?;
                let destination = target.join(relative);
                if entry.file_type().is_symlink() {
                    return Err(PluginError::invalid("Managed Worker package contains a symlink"));
                }
                if entry.file_type().is_dir() {
                    std::fs::create_dir_all(&destination).map_err(store_error)?;
                } else if entry.file_type().is_file() && !destination.exists() {
                    if let Some(parent) = destination.parent() {
                        std::fs::create_dir_all(parent).map_err(store_error)?;
                    }
                    std::fs::copy(path, &destination).map_err(store_error)?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let mut permissions = std::fs::metadata(&destination).map_err(store_error)?.permissions();
                        permissions.set_mode(permissions.mode() & !0o222);
                        std::fs::set_permissions(&destination, permissions).map_err(store_error)?;
                    }
                }
            }
            for skill in record["skills"].as_array().into_iter().flatten() {
                if has_hooks(skill) && !hooks_approved(&record) {
                    continue;
                }
                if skill["requires_native"] == true && !inspect::native_skill_engines().contains(&engine.into()) {
                    continue;
                }
                let Some(relative) = skill["path"].as_str() else {
                    continue;
                };
                let path = contained(&target, relative)?;
                let directory = path.parent().ok_or_else(|| PluginError::invalid("Skill directory missing"))?;
                staged.skills.push(path.clone());
                let name = sha_hex(format!("{}:{relative}", record["id"]).as_bytes());
                let link = workspace.join(".pi/skills").join(name);
                std::fs::create_dir_all(link.parent().unwrap()).map_err(store_error)?;
                #[cfg(unix)]
                if link.symlink_metadata().is_err() {
                    let relative_dir = directory.strip_prefix(workspace).map_err(store_error)?;
                    std::os::unix::fs::symlink(Path::new("../..").join(relative_dir), &link).map_err(store_error)?;
                }
            }
            for component in record["components"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|component| strings(&component["engines"]).iter().any(|e| e == engine))
            {
                if component["kind"] == "extensions" && hooks_approved(&record) {
                    if let Some(path) = component["path"].as_str() {
                        staged.extensions.push(contained(&target, path)?);
                    }
                }
            }
        }
        Ok(staged)
    }
}
