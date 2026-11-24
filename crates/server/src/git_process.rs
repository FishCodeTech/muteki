//! Internal runner for fixed, user-requested Git workflows.
use axum::{Json, http::StatusCode};
use serde_json::{Value, json};
use std::{
    path::Path,
    process::{ExitStatus, Stdio},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt};
pub(crate) type Failure = (StatusCode, Json<Value>);
pub(crate) static WRITES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const MAX_OUTPUT: usize = 2_000_000;
pub(crate) fn redact(text: &str) -> String {
    let credentials = regex::Regex::new(r"([A-Za-z][A-Za-z0-9+.-]*://)[^\s/@]+(?::[^\s/@]*)?@")
        .expect("URL redaction regex");
    credentials
        .replace_all(text, "${1}[redacted]@")
        .into_owned()
}
pub(crate) fn failure(code: &str, message: &str) -> Failure {
    let status = match code {
        "conversation.git.timeout" => StatusCode::GATEWAY_TIMEOUT,
        "conversation.git.not_repo"
        | "conversation.git.detached_head"
        | "conversation.git.no_remote"
        | "conversation.git.no_upstream"
        | "conversation.git.nothing_to_commit"
        | "conversation.git.push_failed"
        | "conversation.git.pull_failed"
        | "conversation.git.pull_conflict"
        | "conversation.git.root_occupied" => StatusCode::CONFLICT,
        _ => StatusCode::BAD_REQUEST,
    };
    let message = redact(message);
    let hint = match code {
        "conversation.git.pull_conflict" => {
            "Resolve the conflict in a terminal, then continue or abort the Git operation"
        }
        "conversation.git.timeout" => {
            "Check network and credential configuration; the server cannot prompt for credentials"
        }
        _ => "",
    };
    (
        status,
        Json(
            json!({"detail":message,"error":{"code":code,"message":message,"recovery_hint":hint}}),
        ),
    )
}
pub(crate) struct GitOutput {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}
impl GitOutput {
    pub(crate) fn combined_output(&self) -> String {
        redact(
            &[self.stdout.trim(), self.stderr.trim()]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }
}
async fn read_output(reader: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_OUTPUT as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > MAX_OUTPUT {
        return Err(std::io::Error::other("Git output exceeds the 2 MB limit"));
    }
    Ok(bytes)
}
pub(crate) async fn run(
    root: &Path,
    args: &[&str],
    timeout_secs: u64,
    code: &str,
    check: bool,
) -> Result<GitOutput, Failure> {
    let mut command = crate::host_process::command("git");
    command
        .args([
            "--no-pager",
            "--literal-pathspecs",
            "-c",
            "core.quotePath=false",
            "-c",
            "core.fsmonitor=false",
            "-C",
        ])
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (name, default) in [
        ("GIT_EDITOR", "true"),
        ("GIT_SEQUENCE_EDITOR", "true"),
        ("GIT_ASKPASS", "false"),
        ("SSH_ASKPASS", "false"),
        ("GIT_SSH_COMMAND", "ssh -oBatchMode=yes"),
    ] {
        if std::env::var_os(name).is_none() {
            command.env(name, default);
        }
    }
    let mut child = command
        .spawn()
        .map_err(|_| failure(code, "Git is unavailable"))?;
    let stdout = child.stdout.take().expect("piped Git stdout");
    let stderr = child.stderr.take().expect("piped Git stderr");
    let operation = async {
        let (stdout, stderr) = tokio::try_join!(read_output(stdout), read_output(stderr))
            .map_err(|e| failure(code, &e.to_string()))?;
        let status = child
            .wait()
            .await
            .map_err(|_| failure(code, "Could not wait for Git"))?;
        let result = GitOutput {
            status,
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        };
        if check && !status.success() {
            let output = result.combined_output();
            return Err(failure(
                code,
                if output.is_empty() {
                    "Git operation failed"
                } else {
                    &output
                },
            ));
        }
        Ok(result)
    };
    tokio::time::timeout(Duration::from_secs(timeout_secs), operation)
        .await
        .map_err(|_| {
            failure(
                "conversation.git.timeout",
                &format!("Git operation timed out ({timeout_secs} seconds)"),
            )
        })?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_url_credentials_are_redacted() {
        let value = redact(
            "fatal: https://user:secret@example.invalid/repo and ssh://token@example.invalid/repo",
        );
        assert!(!value.contains("secret"));
        assert!(!value.contains("token"));
        assert!(value.contains("https://[redacted]@example.invalid/repo"));
    }
}
