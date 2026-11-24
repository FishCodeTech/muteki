//! Child-process constructors that never flash a console window on Windows,
//! and never hand a child this process's own standard handles.
//!
//! Provider CLIs are spawned from a GUI app; a plain `Command` on Windows
//! allocates a console for the child, which pops a black box on screen.
//! `CREATE_NO_WINDOW` suppresses it. No-ops elsewhere.

use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use smol::channel::Receiver;

use crate::AgentError;

const STDERR_TAIL_LINES: usize = 20;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Keep this process's own standard handles out of every child it starts.
///
/// Windows gives a child each inheritable handle of its parent, not only the
/// stdio its `Command` names, so a child that outlives the parent keeps open
/// the pipes whoever runs the parent reads from. A child spawned with
/// `Stdio::inherit` still gets them: std hands it an inheritable duplicate.
/// No-op elsewhere.
pub fn keep_own_stdio_from_children() {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle as _;
        use windows::Win32::Foundation::{
            HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation,
        };
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            for handle in [
                std::io::stdin().as_raw_handle(),
                std::io::stdout().as_raw_handle(),
                std::io::stderr().as_raw_handle(),
            ] {
                // SAFETY: these are this process's standard handles, open for
                // its lifetime; a missing one only makes the call fail, which
                // leaves nothing to keep from a child.
                let _ = unsafe {
                    SetHandleInformation(HANDLE(handle), HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))
                };
            }
        });
    }
}

static SUPERVISOR:std::sync::OnceLock<(std::path::PathBuf,Vec<std::ffi::OsString>)>=std::sync::OnceLock::new();
static HOST_ENV: std::sync::OnceLock<std::sync::RwLock<Vec<(String, String)>>> =
    std::sync::OnceLock::new();

pub fn configure_supervisor(executable:std::path::PathBuf,args:Vec<std::ffi::OsString>){let _=SUPERVISOR.set((executable,args));}

/// Stores allowlisted host environment variables (including the live login-shell
/// `PATH`) for child processes spawned via the `agent` crate.
pub fn configure_host_environment(vars: Vec<(String, String)>) {
    let lock = HOST_ENV.get_or_init(|| std::sync::RwLock::new(Vec::new()));
    let mut guard = lock.write().unwrap_or_else(|poison| poison.into_inner());
    *guard = vars;
}

pub(crate) fn host_environment() -> Vec<(String, String)> {
    HOST_ENV
        .get()
        .map(|lock| lock.read().unwrap_or_else(|poison| poison.into_inner()).clone())
        .unwrap_or_default()
}

pub(crate) fn host_path() -> Option<std::ffi::OsString> {
    if let Some(lock) = HOST_ENV.get() {
        let guard = lock.read().unwrap_or_else(|poison| poison.into_inner());
        if let Some((_, value)) = guard.iter().find(|(key, value)| key == "PATH" && !value.is_empty())
        {
            return Some(std::ffi::OsString::from(value));
        }
    }
    std::env::var_os("PATH")
}

pub fn supervised_command(program:impl AsRef<std::ffi::OsStr>)->std::process::Command{command(program)}

/// A blocking `std::process::Command` with the console suppressed.
pub(crate) fn command<S: AsRef<std::ffi::OsStr>>(program: S) -> std::process::Command {
    keep_own_stdio_from_children();
    let program_ref = program.as_ref();
    let resolved_program: std::path::PathBuf = match program_ref.to_str() {
        Some(raw) if !raw.is_empty() && !raw.contains(['/', '\\']) => {
            crate::find_on_path(raw).unwrap_or_else(|| std::path::PathBuf::from(program_ref))
        }
        _ => std::path::PathBuf::from(program_ref),
    };
    #[cfg_attr(not(windows), allow(unused_mut))]
    // Approved constructor: this is the helper every other site must use.
    #[allow(clippy::disallowed_methods)]
    let mut cmd = if let Some((exe,args))=SUPERVISOR.get() {
        let mut cmd=std::process::Command::new(exe);cmd.args(args).arg(&resolved_program);
        #[cfg(unix)]{use std::os::unix::process::CommandExt;cmd.process_group(0);}
        cmd
    }else{std::process::Command::new(&resolved_program)};
    for (key, value) in host_environment() {
        cmd.env(key, value);
    }
    // The control server's credentials do not belong to provider processes.
    // Callers add their selected provider and Task-scoped bridge credentials
    // explicitly after construction, so those still reach the intended CLI.
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(|name| name.starts_with("MUTEKI_")
            && (name.ends_with("_TOKEN") || name.contains("PASSWORD") || name.contains("SECRET")))
        {
            cmd.env_remove(key);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// An async (`smol`) command with the console suppressed. `async_process`
/// exposes no `creation_flags`, so the flag rides in through the `From` impl.
pub(crate) fn async_command<S: AsRef<std::ffi::OsStr>>(program: S) -> smol::process::Command {
    smol::process::Command::from(command(program))
}

/// Runs blocking work on `smol`'s blocking pool. The one approved
/// `smol::unblock` site in this crate, so callers are easy to audit.
#[allow(clippy::disallowed_methods)]
pub(crate) fn unblock<T, F>(f: F) -> smol::Task<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    smol::unblock(f)
}

pub(crate) async fn probe_version(
    binary: &std::path::Path,
    launch_env: &crate::LaunchEnv,
    provider: crate::ProviderKind,
) -> Option<(u32, u32, u32)> {
    let mut command = async_command(binary);
    command.arg("--version");
    if provider == crate::ProviderKind::ClaudeCode {
        command
            .env_remove("CLAUDECODE")
            .env_remove("CLAUDE_CODE_ENTRYPOINT");
    }
    for (key, value) in launch_env.pairs(provider) {
        command.env(key, value);
    }
    let output = command.output().await.ok()?;
    crate::parse_semver(&String::from_utf8_lossy(&output.stdout))
}

pub(crate) enum ChildOutput {
    Line(String),
    Eof,
    Error(String),
}

pub(crate) fn spawn_line_reader(
    stdout: impl Read + Send + 'static,
    thread_name: &str,
    error_prefix: Option<&'static str>,
    skip_empty: bool,
) -> (Receiver<ChildOutput>, std::io::Result<()>) {
    let (sender, receiver) = smol::channel::unbounded();
    let spawned = std::thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(line) => {
                        if (!skip_empty || !line.trim().is_empty())
                            && sender.send_blocking(ChildOutput::Line(line)).is_err()
                        {
                            return;
                        }
                    }
                    Err(err) => {
                        let error = match error_prefix {
                            Some(prefix) => format!("{prefix}: {err}"),
                            None => err.to_string(),
                        };
                        let _ = sender.send_blocking(ChildOutput::Error(error));
                        return;
                    }
                }
            }
            let _ = sender.send_blocking(ChildOutput::Eof);
        })
        .map(|_| ());
    (receiver, spawned)
}

pub(crate) fn send_json(
    writer: &mut impl Write,
    value: &Value,
    serialization_context: Option<&str>,
) -> Result<(), AgentError> {
    serde_json::to_writer(&mut *writer, value).map_err(|err| {
        AgentError::Protocol(match serialization_context {
            Some(context) => format!("{context}: {err}"),
            None => err.to_string(),
        })
    })?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

/// A rolling tail of process output used to enrich startup and exit errors.
#[derive(Clone, Default)]
pub(crate) struct StderrTail {
    lines: Arc<Mutex<Vec<String>>>,
    readers: Arc<Mutex<Vec<std::thread::JoinHandle<()>>>>,
}

impl StderrTail {
    pub(crate) fn spawn(
        &self,
        reader: impl Read + Send + 'static,
        thread_name: &str,
        log_prefix: &'static str,
    ) -> std::io::Result<()> {
        let tail = self.clone();
        let reader = std::thread::Builder::new()
            .name(thread_name.into())
            .spawn(move || {
                for line in BufReader::new(reader).lines().map_while(Result::ok) {
                    log::debug!("{log_prefix}: {line}");
                    tail.push(line);
                }
            })?;
        self.readers.lock().unwrap().push(reader);
        Ok(())
    }

    pub(crate) fn push(&self, line: String) {
        let mut lines = self.lines.lock().unwrap();
        if lines.len() == STDERR_TAIL_LINES {
            lines.remove(0);
        }
        lines.push(line);
    }

    /// Joins spawned readers; stop or reap their child before collecting diagnostics.
    pub(crate) fn append_to(&self, mut message: String, separator: &str) -> String {
        let tail = self.text();
        if !tail.trim().is_empty() {
            message.push_str(separator);
            message.push_str(&tail);
        }
        message
    }

    fn text(&self) -> String {
        let readers = std::mem::take(&mut *self.readers.lock().unwrap());
        for reader in readers {
            let _ = reader.join();
        }
        self.lines.lock().unwrap().join("\n")
    }
}

#[cfg(test)]
pub(crate) const TEST_ECHO_READY: &str = "TCODE_TEST_ECHO_READY";

#[cfg(test)]
pub(crate) fn test_echo_command() -> std::process::Command {
    // Reuse the test executable so actor fixtures need no shell or PATH tools.
    let mut child = command(std::env::current_exe().unwrap());
    child
        .args([
            "--exact",
            "process::tests::line_reader_preserves_record_framing_and_reports_invalid_utf8",
            "--quiet",
            "--nocapture",
        ])
        .env("TCODE_TEST_ECHO_CHILD", "1");
    child
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_reader_preserves_record_framing_and_reports_invalid_utf8() {
        if std::env::var_os("TCODE_TEST_ECHO_CHILD").is_some() {
            let mut output = std::io::stdout().lock();
            writeln!(output, "{TEST_ECHO_READY}").unwrap();
            output.flush().unwrap();
            for line in std::io::stdin().lock().lines() {
                writeln!(output, "{}", line.unwrap()).unwrap();
                output.flush().unwrap();
            }
            return;
        }
        let record = "{\"text\":\"a\u{2028}b\"}";
        for skip_empty in [false, true] {
            let (lines, spawned) = spawn_line_reader(
                std::io::Cursor::new(format!("\n{record}\r\nlast").into_bytes()),
                "test-json-lines",
                None,
                skip_empty,
            );
            spawned.unwrap();
            if !skip_empty {
                assert!(
                    matches!(lines.recv_blocking().unwrap(), ChildOutput::Line(line) if line.is_empty())
                );
            }
            assert!(
                matches!(lines.recv_blocking().unwrap(), ChildOutput::Line(line) if line == record)
            );
            assert!(
                matches!(lines.recv_blocking().unwrap(), ChildOutput::Line(line) if line == "last")
            );
            assert!(matches!(lines.recv_blocking().unwrap(), ChildOutput::Eof));
        }
        let (lines, spawned) = spawn_line_reader(
            std::io::Cursor::new(vec![0xff, b'\n']),
            "test-invalid-utf8",
            Some("provider output"),
            true,
        );
        spawned.unwrap();
        assert!(
            matches!(lines.recv_blocking().unwrap(), ChildOutput::Error(message) if message.starts_with("provider output:"))
        );
        assert!(lines.recv_blocking().is_err());
    }
}

/// Bounded metadata subprocess; cancellation also closes the supervised group.
pub(crate) async fn bounded_output(command:&mut smol::process::Command, timeout:std::time::Duration, limit:u64)->Result<std::process::Output,AgentError>{
    use smol::io::AsyncReadExt;
    command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
    let mut child=command.spawn().map_err(AgentError::Io)?;
    let stdout=child.stdout.take().ok_or_else(||AgentError::Protocol("Metadata stdout unavailable".into()))?;
    let stderr=child.stderr.take().ok_or_else(||AgentError::Protocol("Metadata stderr unavailable".into()))?;
    let read=async |stream:Box<dyn smol::io::AsyncRead+Unpin+Send>|->Result<Vec<u8>,AgentError>{let mut bytes=Vec::new();stream.take(limit+1).read_to_end(&mut bytes).await.map_err(AgentError::Io)?;if bytes.len() as u64>limit{return Err(AgentError::Protocol("Metadata output exceeded its limit".into()));}Ok(bytes)};
    let result=smol::future::race(async{
        let(status,(stdout,stderr))=smol::future::zip(child.status(),smol::future::zip(read(Box::new(stdout)),read(Box::new(stderr)))).await;
        Ok(std::process::Output{status:status.map_err(AgentError::Io)?,stdout:stdout?,stderr:stderr?})
    },async{smol::Timer::after(timeout).await;Err(AgentError::Protocol("Metadata subprocess timed out".into()))}).await;
    if result.is_err(){let _=child.kill();let _=child.status().await;}result
}
