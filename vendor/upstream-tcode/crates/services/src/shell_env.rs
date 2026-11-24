//! GUI-launch environment repair.
//!
//! Desktop launches can lack the shell's PATH entries for provider CLIs.
//! Before other threads start, capture the login shell's environment: replace
//! PATH and fill missing variables without overwriting inherited configuration.

/// Printed by the shell before `env -0` so config-file noise on stdout can be
/// discarded (some rc files `echo` unconditionally).
#[cfg(unix)]
const MARKER: &str = "__TCODE_LOGIN_SHELL_ENV__";

/// How long the login shell gets to start and dump its environment. Generous —
/// nvm-heavy configs can take a while — but bounded, so a shell that blocks on
/// input can never wedge app startup.
#[cfg(unix)]
const SHELL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Session-local vars the shell reports that describe *its* session, not the
/// user's configuration; importing them would be meaningless or misleading.
#[cfg(unix)]
const SKIP_VARS: &[&str] = &["PWD", "OLDPWD", "SHLVL", "_", "PS1", "PROMPT"];

/// Merge the login shell's environment into this process. Must be called from
/// `main` before any other thread exists (it writes the process environment).
///
/// No-op on Windows (GUI apps inherit the user's registry environment there)
/// and when launched from a terminal (`TERM` set → the environment is already
/// the shell's).
pub fn import_login_shell_environment() {
    #[cfg(unix)]
    {
        if std::env::var_os("TERM").is_some() {
            return;
        }
        match capture_login_shell_env() {
            Ok(stdout) => {
                apply(parse_env_output(&stdout));
                log::info!(
                    "imported login-shell environment; PATH={}",
                    std::env::var("PATH").unwrap_or_default()
                );
            }
            Err(err) => log::warn!("could not import the login-shell environment: {err}"),
        }
    }
}

/// Run `$SHELL -l -i -c 'echo MARKER; command env -0'` and return its stdout.
///
/// Both `-l` and `-i` matter: PATH edits commonly live in the interactive rc
/// file (`.zshrc`, `config.fish`), not just the login profile. The command
/// string is deliberately the POSIX/fish common subset — fish is not
/// sh-compatible, but `echo`, `;` and `command env` mean the same thing there.
#[cfg(unix)]
fn capture_login_shell_env() -> Result<String, String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| default_shell().to_string());
    let child = crate::process::command(&shell)
        .args(["-l", "-i", "-c", &format!("echo {MARKER}; command env -0")])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|err| format!("failed to spawn `{shell}`: {err}"))?;

    // Bounded wait: collect the output on a helper thread so a shell that
    // ignores `-c` and sits on a prompt cannot wedge startup.
    let (tx, rx) = std::sync::mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let output = match rx.recv_timeout(SHELL_TIMEOUT) {
        Ok(result) => result.map_err(|err| format!("`{shell}` failed: {err}"))?,
        // Abandon the import; the detached waiter still owns the shell process.
        Err(_) => return Err(format!("`{shell}` did not exit within {SHELL_TIMEOUT:?}")),
    };
    // The thread has sent already; join so no second thread is alive while the
    // caller mutates the process environment.
    let _ = waiter.join();
    if !output.status.success() {
        return Err(format!("`{shell}` exited with {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(unix)]
fn default_shell() -> &'static str {
    if cfg!(target_os = "macos") {
        "/bin/zsh"
    } else {
        "/bin/sh"
    }
}

/// Extract the `env -0` block after [`MARKER`] into key/value pairs.
#[cfg(unix)]
fn parse_env_output(stdout: &str) -> Vec<(String, String)> {
    let Some((_, tail)) = stdout.split_once(MARKER) else {
        return Vec::new();
    };
    tail.trim_start_matches(['\r', '\n'])
        .split('\0')
        .filter_map(|entry| {
            let (key, value) = entry.split_once('=')?;
            (!key.is_empty() && !SKIP_VARS.contains(&key))
                .then(|| (key.to_string(), value.to_string()))
        })
        .collect()
}

/// Write the captured vars into the process environment: PATH always wins
/// (replacing the launchd stub is the whole point), everything else only fills
/// a gap.
#[cfg(unix)]
fn apply(vars: Vec<(String, String)>) {
    for (key, value) in vars {
        if key == "PATH" || std::env::var_os(&key).is_none() {
            // SAFETY: called from `main` before any other thread is spawned
            // (the helper thread in `capture_login_shell_env` is joined).
            unsafe { std::env::set_var(&key, &value) };
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn parses_env_after_marker_and_drops_rc_noise() {
        let stdout = format!(
            "welcome from .zshrc\n{MARKER}\nPATH=/opt/homebrew/bin:/usr/bin\0\
             CODEX_HOME=/Users/dev/.codex\0MULTI=line one\nline two\0PWD=/Users/dev\0\
             SHLVL=1\0=weird\0NOEQUALS"
        );
        let vars = parse_env_output(&stdout);
        assert_eq!(
            vars,
            vec![
                ("PATH".into(), "/opt/homebrew/bin:/usr/bin".into()),
                ("CODEX_HOME".into(), "/Users/dev/.codex".into()),
                ("MULTI".into(), "line one\nline two".into()),
            ]
        );
        assert!(parse_env_output("PATH=/usr/bin\0HOME=/root").is_empty());
    }
}
