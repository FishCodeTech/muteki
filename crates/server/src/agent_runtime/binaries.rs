//! Engine binary resolution.
//!
//! A GUI process inherits a short `PATH`, and some engines are distributed as
//! repackages that die before parsing argv, so a PATH hit alone is not enough.
//! Resolution is: an explicit override, then the operator's `MUTEKI_<ENGINE>_BIN`
//! variable, then first-party install locations, then every `PATH` hit in order.
//! Candidates past the explicit override must answer `--version` successfully.

use super::{RuntimeError, descriptors::EngineDescriptor};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

/// `realpath` substrings that mark a repackage which must never be selected.
const BAD_REALPATH_MARKERS: [&str; 1] = ["cometix"];

const VERSION_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a `--version` result stays authoritative. An engine installed while
/// the server runs is picked up on the next window rather than never.
const RUNNABLE_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub path: PathBuf,
    /// `override`, `env`, `known-good`, `nvm` or `path`.
    pub source: &'static str,
}

fn cache() -> &'static Mutex<HashMap<PathBuf, (bool, Instant)>> {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<PathBuf, (bool, Instant)>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn invalidate_cache() {
    if let Ok(mut rows) = cache().lock() {
        rows.clear();
    }
}

fn expand_home(value: &str) -> PathBuf {
    match value.strip_prefix("~/") {
        Some(rest) => match crate::shell_env::var("HOME") {
            Some(home) => Path::new(&home).join(rest),
            None => PathBuf::from(value),
        },
        None => PathBuf::from(value),
    }
}

/// GUI launches need the resolved engine and standard user-installed tools on PATH.
/// An explicitly configured PATH remains authoritative at the call site.
pub fn launch_path(binary: &Path) -> Result<String, super::RuntimeError> {
    let mut paths = Vec::new();
    let mut current = binary.to_path_buf();
    let mut visited = Vec::new();
    while !visited.contains(&current) {
        visited.push(current.clone());
        if let Some(parent) = current.parent().filter(|path| path.is_absolute()) {
            paths.push(parent.to_path_buf());
        }
        let Ok(target) = std::fs::read_link(&current) else { break };
        current = if target.is_absolute() {
            target
        } else {
            current.parent().unwrap_or(Path::new(".")).join(target)
        };
    }
    paths.extend(crate::shell_env::path_os().iter().flat_map(std::env::split_paths));
    #[cfg(unix)]
    {
        if let Some(home) = crate::shell_env::var("HOME") {
            paths.push(PathBuf::from(home).join(".local/bin"));
        }
        paths.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"].map(PathBuf::from));
    }
    let mut unique = Vec::new();
    for path in paths {
        if !unique.contains(&path) { unique.push(path); }
    }
    std::env::join_paths(unique).map_err(|error| super::RuntimeError::new("runtime.launch_path_invalid", error.to_string()))?
        .into_string().map_err(|_| super::RuntimeError::new("runtime.launch_path_invalid", "Engine PATH is not valid Unicode"))
}

fn looks_bad(path: &Path) -> bool {
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let lowered = real.to_string_lossy().to_ascii_lowercase();
    BAD_REALPATH_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Every `basename` on `PATH`, in `PATH` order. `which` returns only the first,
/// which hides a working install behind a broken one.
fn which_all(basename: &str) -> Vec<PathBuf> {
    let Some(path) = crate::shell_env::path_os() else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for directory in std::env::split_paths(&path) {
        if !directory.is_absolute() {
            continue;
        }
        let candidate = directory.join(basename);
        if is_executable(&candidate) && !found.contains(&candidate) {
            found.push(candidate);
        }
    }
    found
}

fn collect_versioned_bins(root: &Path, sub: &str, basename: &str) -> Vec<PathBuf> {
    let mut candidates = std::fs::read_dir(root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            entry
                .ok()
                .map(|entry| entry.path().join(sub).join(basename))
        })
        .filter(|path| is_executable(path))
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.reverse();
    candidates
}

fn version_manager_all(basename: &str) -> Vec<(&'static str, PathBuf)> {
    let home = crate::shell_env::var("HOME").map(PathBuf::from);
    let mut out: Vec<(&'static str, PathBuf)> = Vec::new();
    let mut push_unique = |source: &'static str, candidate: PathBuf| {
        if is_executable(&candidate) && !out.iter().any(|(_, existing)| existing == &candidate) {
            out.push((source, candidate));
        }
    };

    if let Some(nvm_dir) = crate::shell_env::var("NVM_DIR")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".nvm")))
    {
        for candidate in collect_versioned_bins(&nvm_dir.join("versions/node"), "bin", basename) {
            push_unique("nvm", candidate);
        }
    }

    if let Some(volta) = crate::shell_env::var("VOLTA_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".volta")))
    {
        push_unique("volta", volta.join("bin").join(basename));
    }

    if let Some(fnm_multi) = crate::shell_env::var("FNM_MULTISHELL_PATH") {
        push_unique("fnm", Path::new(&fnm_multi).join("bin").join(basename));
    }
    let mut fnm_roots = Vec::new();
    if let Some(fnm_dir) = crate::shell_env::var("FNM_DIR") {
        fnm_roots.push(PathBuf::from(fnm_dir));
    }
    if let Some(h) = &home {
        fnm_roots.push(h.join(".local/share/fnm"));
        fnm_roots.push(h.join("Library/Application Support/fnm"));
    }
    for fnm_root in fnm_roots {
        push_unique("fnm", fnm_root.join("aliases/default/bin").join(basename));
        for candidate in
            collect_versioned_bins(&fnm_root.join("node-versions"), "installation/bin", basename)
        {
            push_unique("fnm", candidate);
        }
    }

    if let Some(h) = &home {
        let mise = h.join(".local/share/mise");
        push_unique("mise", mise.join("shims").join(basename));
        if let Ok(tools) = std::fs::read_dir(mise.join("installs")) {
            for tool in tools.flatten() {
                for candidate in collect_versioned_bins(&tool.path(), "bin", basename) {
                    push_unique("mise", candidate);
                }
            }
        }
        push_unique("asdf", h.join(".asdf/shims").join(basename));
    }

    if let Some(bun) = crate::shell_env::var("BUN_INSTALL")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".bun")))
    {
        push_unique("bun", bun.join("bin").join(basename));
    }

    if let Some(pnpm) = crate::shell_env::var("PNPM_HOME") {
        push_unique("pnpm", PathBuf::from(pnpm).join(basename));
    }
    if let Some(h) = &home {
        push_unique("pnpm", h.join("Library/pnpm").join(basename));
        push_unique("pnpm", h.join(".local/share/pnpm").join(basename));
        push_unique("npm-global", h.join(".npm-global/bin").join(basename));
    }

    if let Some(cargo) = crate::shell_env::var("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|h| h.join(".cargo")))
    {
        push_unique("cargo", cargo.join("bin").join(basename));
    }

    out
}

/// Does this binary actually execute? `--version` is the cheapest probe that
/// separates a real CLI from one that dies before parsing argv.
async fn runs(path: &Path) -> bool {
    if let Ok(cache) = cache().lock() {
        if let Some((result, at)) = cache.get(path) {
            if at.elapsed() < RUNNABLE_TTL {
                return *result;
            }
        }
    }
    let Ok(mut cmd) = crate::host_process::binary_command(path) else {
        return false;
    };
    let probe = cmd
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output();
    let result = match tokio::time::timeout(VERSION_TIMEOUT, probe).await {
        Ok(Ok(output)) => output.status.success(),
        Ok(Err(_)) | Err(_) => false,
    };
    if let Ok(mut cache) = cache().lock() {
        cache.insert(path.to_path_buf(), (result, Instant::now()));
    }
    result
}

/// Resolve the binary that runs `descriptor`'s engine.
///
/// `explicit` is a path configured for one runtime instance and is trusted as
/// given: an operator who names a binary is not second-guessed.
pub async fn resolve(
    descriptor: &EngineDescriptor,
    explicit: Option<&Path>,
) -> Result<Resolved, RuntimeError> {
    if let Some(path) = explicit.filter(|path| !path.as_os_str().is_empty()) {
        return Ok(Resolved {
            path: path.to_path_buf(),
            source: "override",
        });
    }
    let spec = descriptor.binary;
    if let Some(value) = crate::shell_env::var(spec.env_override) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(Resolved {
                path: expand_home(trimmed),
                source: "env",
            });
        }
    }
    for candidate in spec.known_good {
        let path = expand_home(candidate);
        if path.exists() && !looks_bad(&path) && runs(&path).await {
            return Ok(Resolved {
                path,
                source: "known-good",
            });
        }
    }
    for path in which_all(spec.basename) {
        if !looks_bad(&path) && runs(&path).await {
            return Ok(Resolved {
                path,
                source: "path",
            });
        }
    }
    for (source, path) in version_manager_all(spec.basename) {
        if !looks_bad(&path) && runs(&path).await {
            return Ok(Resolved { path, source });
        }
    }
    Err(RuntimeError::new(
        "runtime.binary_missing",
        format!(
            "`{}` was not found. Install {} or set {}.",
            spec.basename, descriptor.identity.display_name, spec.env_override
        ),
    ))
}

/// Whether the engine binary is present, without claiming anything about login
/// or connectivity.
pub async fn present(descriptor: &EngineDescriptor) -> bool {
    resolve(descriptor, None).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_is_expanded_only_for_a_leading_tilde() {
        let home = std::env::var("HOME").expect("HOME is set in tests");
        assert_eq!(expand_home("~/bin/codex"), Path::new(&home).join("bin/codex"));
        assert_eq!(expand_home("/usr/local/bin/codex"), PathBuf::from("/usr/local/bin/codex"));
        assert_eq!(expand_home("relative/codex"), PathBuf::from("relative/codex"));
    }

    #[tokio::test]
    async fn an_explicit_path_is_trusted_without_probing() {
        let descriptor = super::super::descriptors::find("codex").expect("codex");
        let resolved = resolve(descriptor, Some(Path::new("/nonexistent/codex")))
            .await
            .expect("an explicit path is accepted as given");
        assert_eq!(resolved.source, "override");
        assert_eq!(resolved.path, PathBuf::from("/nonexistent/codex"));
    }

    #[tokio::test]
    async fn a_missing_engine_reports_a_typed_error() {
        let mut descriptor = super::super::descriptors::find("codex").unwrap().clone();
        descriptor.binary = super::super::descriptors::BinarySpec {
            basename: "muteki-test-missing-engine",
            env_override: "MUTEKI_TEST_MISSING_ENGINE_BIN",
            known_good: &[],
        };
        let error = resolve(&descriptor, None).await.unwrap_err();
        assert_eq!(error.code, "runtime.binary_missing");
        assert!(error.message.contains("MUTEKI_TEST_MISSING_ENGINE_BIN"));
    }
}
