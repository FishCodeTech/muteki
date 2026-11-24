//! Unified host child-process spawner, bundled Node resolution, and async
//! login-shell environment refresh coordination for `muteki-server`.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use crate::{AppState, agent_runtime::RuntimeError, shell_env::RefreshOutcome};

#[allow(clippy::disallowed_methods)]
pub fn std_command(program: impl AsRef<OsStr>) -> std::process::Command {
    let resolved = crate::shell_env::resolve_program(program.as_ref());
    let mut cmd = std::process::Command::new(resolved);
    for (key, value) in host_environment() {
        cmd.env(key, value);
    }
    cmd
}

pub fn host_environment() -> Vec<(String, String)> {
    let mut vars = crate::shell_env::base_vars();
    let overlay = crate::network_proxy::host_env_overlay();
    vars.retain(|(key, _)| !overlay.iter().any(|(name, _)| name == key));
    vars.extend(overlay);
    vars
}

pub fn sync_host_environment() {
    agent::configure_host_environment(host_environment());
}

pub fn command(program: impl AsRef<OsStr>) -> tokio::process::Command {
    tokio::process::Command::from(std_command(program))
}

pub fn binary_command(binary: &Path) -> Result<tokio::process::Command, RuntimeError> {
    let path_env = crate::agent_runtime::binaries::launch_path(binary)?;
    let mut cmd = command(binary);
    cmd.env("PATH", path_env);
    Ok(cmd)
}

pub fn node_path() -> PathBuf {
    if let Some(explicit) = std::env::var_os("MUTEKI_NODE")
        && !explicit.is_empty()
    {
        let path = PathBuf::from(&explicit);
        if crate::shell_env::is_executable_file(&path) {
            return path;
        }
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(contents) = exe.parent().and_then(Path::parent)
    {
        let bundled = contents.join("Resources/node/bin/node");
        if crate::shell_env::is_executable_file(&bundled) {
            return bundled;
        }
    }
    crate::shell_env::resolve_program(OsStr::new("node"))
}

pub fn node_command() -> tokio::process::Command {
    command(node_path())
}

pub fn sync_runtime_caches() {
    sync_host_environment();
    crate::agent_runtime::binaries::invalidate_cache();
    crate::agent_runtime::host_login::invalidate();
}

pub(crate) fn configured_state_dir(state: &AppState) -> Option<PathBuf> {
    state
        .store
        .get("system", "state_dir")
        .ok()
        .flatten()
        .and_then(|value| value.as_str().map(PathBuf::from))
}

pub async fn refresh_shell_env(state_dir: Option<&Path>, min_ttl: Duration) -> RefreshOutcome {
    static LOCK: OnceLock<tokio::sync::Mutex<Option<(Instant, RefreshOutcome)>>> = OnceLock::new();
    let mutex = LOCK.get_or_init(|| tokio::sync::Mutex::new(None));
    let mut guard = mutex.lock().await;

    if min_ttl > Duration::ZERO
        && crate::shell_env::is_discovered()
        && let Some((at, previous)) = guard.as_ref()
        && at.elapsed() < min_ttl
    {
        return RefreshOutcome {
            changed: false,
            discovered: previous.discovered,
            source: previous.source.clone(),
        };
    }

    let owned_dir = state_dir.map(Path::to_path_buf);
    let outcome = tokio::task::spawn_blocking(move || {
        crate::shell_env::refresh_blocking(owned_dir.as_deref())
    })
    .await
    .unwrap_or_else(|_| RefreshOutcome {
        changed: false,
        discovered: crate::shell_env::is_discovered(),
        source: "error".to_owned(),
    });

    *guard = Some((Instant::now(), outcome.clone()));
    drop(guard);

    if outcome.changed || outcome.discovered {
        sync_runtime_caches();
        crate::runner::push_shell_environment().await;
    }

    outcome
}

pub fn spawn_background_refresh(
    state: Option<Arc<AppState>>,
    state_dir: PathBuf,
    delay: Duration,
) {
    tokio::spawn(async move {
        if delay > Duration::ZERO {
            tokio::time::sleep(delay).await;
            if crate::shell_env::is_discovered() {
                return;
            }
        }
        let outcome = refresh_shell_env(Some(&state_dir), Duration::ZERO).await;
        if outcome.changed
            && let Some(app_state) = state
            && app_state.lifecycle.ready()
            && !app_state.admission.draining()
            && let Err((_, body)) = crate::runtime_api::probe_all(&app_state, "", "", false).await
        {
            tracing::warn!(error = %body.0, "Post-shell-discovery runtime probe failed");
        }
    });
}
