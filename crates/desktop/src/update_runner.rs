//! The private update helper and the app's own update modes.
//!
//! The helper runs from a copy inside the transaction directory, outside the
//! bundle it replaces. It waits for the app to exit, backs up the state
//! directory, swaps the bundles and waits for the new version to validate
//! itself; before the new version opens writes, any failure restores both
//! the old bundle and the backed-up state.
use crate::update_files::{self as files, Identity};
use serde_json::{Value, json};
use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

/// The state directory keeps transactions under `updates/`; backups and
/// restores leave that directory alone.
pub const KEEP: &[&str] = &["updates"];
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Prepared, WaitingForOwner, BackingUp, BackedUp, Switching, Validating,
    Validated, WritesOpen, Complete, Failed, RolledBack, RepairRequired, RecoveryRequired,
}
impl Phase {
    fn read(journal: &Value) -> Result<Self, String> {
        match journal["phase"].as_str() {
            Some("prepared") => Ok(Self::Prepared), Some("waiting-for-owner") => Ok(Self::WaitingForOwner),
            Some("backing-up") => Ok(Self::BackingUp), Some("backed-up") => Ok(Self::BackedUp),
            Some("switching") => Ok(Self::Switching), Some("validating") => Ok(Self::Validating),
            Some("validated") => Ok(Self::Validated), Some("writes-open") => Ok(Self::WritesOpen),
            Some("complete") => Ok(Self::Complete), Some("failed") => Ok(Self::Failed),
            Some("rolled-back") => Ok(Self::RolledBack), Some("repair-required") => Ok(Self::RepairRequired),
            Some("recovery-required") => Ok(Self::RecoveryRequired),
            _ => Err("Unknown update journal phase; manual recovery is required".into()),
        }
    }
    fn done(self) -> bool {
        matches!(self, Self::Prepared | Self::Complete | Self::RolledBack | Self::RepairRequired | Self::RecoveryRequired)
    }
    fn permits(self, next: Self) -> bool {
        use Phase::*;
        self == next || matches!((self,next),
            (Prepared,WaitingForOwner|BackingUp) | (WaitingForOwner,Prepared|BackingUp)
            | (BackingUp,BackedUp) | (BackedUp,Switching) | (Switching,Validating)
            | (Validating,Validated) | (Validated,WritesOpen) | (WritesOpen,Complete))
            || (!matches!(self,Complete|RolledBack|RepairRequired|RecoveryRequired)
                && matches!(next,Failed|RolledBack|RepairRequired|RecoveryRequired))
    }
}

fn checked_merge(journal: &Value, changes: Value) -> Result<Value, String> {
    let mut next = journal.clone();
    merge(&mut next, changes);
    let phase = Phase::read(&next)?;
    if let Ok(previous) = Phase::read(journal) {
        if !previous.permits(phase) { return Err(format!("Invalid update transition: {previous:?} -> {phase:?}")); }
    } else if journal["version"] == 2 || phase != Phase::RepairRequired {
        return Err("Invalid update journal phase".into());
    }
    if let Some(activated) = journal.get("activatedAt").filter(|value|!value.is_null()) {
        if next.get("activatedAt") != Some(activated) || phase == Phase::RolledBack {
            return Err("An activated update cannot restore or erase its write boundary".into());
        }
    }
    if matches!(phase,Phase::WritesOpen|Phase::Complete)
        && next["activatedAt"].as_str().is_none_or(str::is_empty) {
        return Err("Write admission requires a durable activation boundary".into());
    }
    Ok(next)
}

pub fn patch(file: &Path, patch: Value) -> Result<Value, String> {
    use std::os::fd::AsRawFd;
    let lock = std::fs::OpenOptions::new().read(true).write(true).create(true)
        .open(file.with_extension("lock")).map_err(|error|error.to_string())?;
    if unsafe { libc::flock(lock.as_raw_fd(),libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let journal = files::read_json(file).map_err(|e| format!("Cannot read {}: {e}", file.display()))?;
    let journal = checked_merge(&journal,patch)?;
    files::atomic_json(file, &journal)
        .map_err(|e| format!("Cannot write {}: {e}", file.display()))?;
    Ok(journal)
}

pub fn merge(target: &mut Value, patch: Value) {
    if let (Some(target), Value::Object(patch)) = (target.as_object_mut(), patch) {
        for (key, value) in patch {
            if value.is_null() {
                target.remove(&key);
            } else {
                target.insert(key, value);
            }
        }
    }
}

fn clean_env(command: &mut Command) -> &mut Command {
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("MUTEKI_")
            || ["NODE_OPTIONS", "NODE_PATH"].iter().any(|k| key == *k)
        {
            command.env_remove(key);
        }
    }
    command
}

/// The launch of an application bundle's executable, detached from us.
pub fn launch(
    app: &Path,
    executable: &str,
    args: &[String],
    backend_port: u16,
    ui_origin: &str,
) -> std::io::Result<Child> {
    let mut command = crate::shell_env::command(app.join("Contents/MacOS").join(executable));
    clean_env(&mut command)
        .args(args)
        .env("MUTEKI_BACKEND_PORT", backend_port.to_string())
        .env("MUTEKI_UI_ORIGIN", ui_origin)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    command.spawn()
}

fn port_open(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(300))
        .is_ok()
}

fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    done()
}

fn stop(child: &mut Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    // SAFETY: signalling our own child process.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    if !wait_until(Duration::from_secs(10), || {
        child.try_wait().ok().flatten().is_some()
    }) {
        let _ = child.kill();
    }
    let _ = child.wait();
}

/// Release pages of the trusted feed the journal recorded.
pub fn release_page_pattern(base: &str, url: &str) -> bool {
    url.strip_prefix(&format!("{base}/releases/tag/"))
        .is_some_and(|tag| files::semantic(tag.strip_prefix('v').unwrap_or(tag)))
}

fn provenance(journal: &Value) -> bool {
    let integrity = &journal["integrity"];
    let base = integrity["base"].as_str().unwrap_or("");
    journal["version"] == 2
        && journal["mode"] == "official"
        && integrity["kind"] == "github-release-asset"
        && integrity["repository"] == "FishCodeTech/muteki"
        && ((integrity["feed"] == "github" && base == crate::updates::GITHUB_BASE)
            || (integrity["feed"] == "test" && crate::updates::loopback_base(base)))
        && release_page_pattern(base, integrity["releaseUrl"].as_str().unwrap_or(""))
        && integrity["releaseUrl"].as_str().and_then(|url| url.rsplit('/').next())
            .is_some_and(|tag| tag.trim_start_matches('v') == journal["candidateIdentity"]["version"].as_str().unwrap_or(""))
        && integrity["assetName"].as_str().is_some_and(|name| name == format!("Muteki-{}-{}.zip",
            journal["candidateIdentity"]["version"].as_str().unwrap_or(""), files::host_arch()))
        && integrity["sha256"] == journal["assetDigest"]
        && journal["assetDigest"].as_str().is_some_and(|digest| {
            digest.strip_prefix("sha256:").is_some_and(|hex| {
                hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        })
}

fn launch_context(journal: &Value) -> Result<(u16, String), String> {
    let port = journal["backendPort"]
        .as_u64()
        .and_then(|value| u16::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or("Update journal has no valid backend port")?;
    let origin = journal["uiOrigin"]
        .as_str()
        .ok_or("Update journal has no UI origin")?;
    let url = reqwest::Url::parse(origin).map_err(|_| "Update journal UI origin is invalid")?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || origin != url.origin().ascii_serialization()
    {
        return Err("Update journal UI origin is invalid".into());
    }
    Ok((port, origin.to_owned()))
}

struct Run {
    file: PathBuf,
    journal: Value,
    root: PathBuf,
    current: PathBuf,
    previous: PathBuf,
    incoming: PathBuf,
    backup: PathBuf,
    switched: bool,
    backed_up: bool,
    next: Option<Child>,
}

impl Run {
    fn write(&mut self, value: Value) -> Result<(), String> {
        self.journal = patch(&self.file,value)?;
        Ok(())
    }

    fn attempt(&mut self, recover: bool) -> Result<(), String> {
        if recover {
            return Err(
                "Recovered an interrupted update before workspace writes were admitted".into(),
            );
        }
        if !matches!(Phase::read(&self.journal)?,Phase::Prepared|Phase::WaitingForOwner)
        {
            return Err("Update transaction is not prepared".into());
        }
        let owner = self.journal["ownerPid"].as_i64().unwrap_or(0);
        if !wait_until(Duration::from_secs(120), || !files::pid_alive(owner)) {
            return Err("The running app did not exit within two minutes".into());
        }
        let (port, ui_origin) = launch_context(&self.journal)?;
        if !wait_until(Duration::from_secs(30), || !port_open(port)) {
            return Err(format!("The old service still listens on port {port}"));
        }
        if !provenance(&self.journal) {
            return Err("Official update integrity provenance is missing or invalid".into());
        }
        let expected = Identity::from_json(&self.journal["identity"])
            .ok_or("The journal has no installed identity")?;
        let accepted = Identity::from_json(&self.journal["candidateIdentity"])
            .ok_or("The journal has no candidate identity")?;
        let candidate = PathBuf::from(self.journal["candidate"].as_str().unwrap_or(""));
        let identity =
            files::bundle_identity(&candidate, files::host_arch()).map_err(|e| e.message)?;
        if identity.id != expected.id
            || identity.team != accepted.team
            || identity.team != expected.team
            || identity.version != accepted.version
            || identity.executable != accepted.executable
        {
            return Err("Candidate changed after acceptance".into());
        }
        if files::bundle_hash(&candidate).map_err(|e| e.message)?
            != self.journal["sha256"].as_str().unwrap_or("")
        {
            return Err("Candidate changed after acceptance".into());
        }
        if files::bundle_hash(&self.current).map_err(|e| e.message)?
            != self.journal["previousSha256"].as_str().unwrap_or("")
        {
            return Err("Installed application changed during update preparation".into());
        }
        std::fs::create_dir(&self.backup)
            .map_err(|e| format!("Cannot create the data backup: {e}"))?;
        self.write(json!({"phase": "backing-up"}))?;
        files::copy_tree_except(&self.root, &self.backup.join("state"), KEEP)?;
        self.backed_up = true;
        self.write(json!({"phase": "backed-up", "backupComplete": true}))?;
        files::copy_bundle(&candidate, &self.incoming).map_err(|e| e.message)?;
        if files::bundle_hash(&self.incoming).map_err(|e| e.message)?
            != self.journal["sha256"].as_str().unwrap_or("")
        {
            return Err("Installed copy does not match the accepted candidate".into());
        }
        self.write(json!({"phase": "switching", "previous": self.previous}))?;
        std::fs::rename(&self.current, &self.previous)
            .map_err(|e| format!("Cannot move the installed app aside: {e}"))?;
        if let Err(error) = std::fs::rename(&self.incoming, &self.current) {
            std::fs::rename(&self.previous, &self.current)
                .map_err(|e| format!("Cannot restore the installed app: {e}"))?;
            return Err(format!("Cannot move the new app into place: {error}"));
        }
        self.switched = true;
        self.write(json!({"phase": "validating"}))?;
        let args = vec![
            format!("--muteki-environment-root={}", self.root.display()),
            format!("--muteki-update-journal={}", self.file.display()),
        ];
        self.next = Some(
            launch(&self.current, &identity.executable, &args, port, &ui_origin)
                .map_err(|e| format!("Cannot start the new app: {e}"))?,
        );
        let deadline = Instant::now() + Duration::from_secs(150);
        while Instant::now() < deadline {
            self.journal = files::read_json(&self.file).map_err(|e| e.to_string())?;
            match Phase::read(&self.journal)? {
                Phase::Validated => self.write(json!({"phase": "writes-open", "activatedAt": now()}))?,
                Phase::Complete => return Ok(()),
                Phase::Failed => {
                    return Err(self.journal["error"]
                        .as_str()
                        .unwrap_or("New application failed validation")
                        .to_owned());
                }
                _ => {}
            }
            if self
                .next
                .as_mut()
                .is_some_and(|child| child.try_wait().ok().flatten().is_some())
            {
                return Err("New application stopped during validation".into());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Err("New application did not complete startup in time".into())
    }

    fn roll_back(&mut self, error: String) -> Result<(), String> {
        let latest = files::read_json(&self.file).unwrap_or_else(|_| self.journal.clone());
        self.journal = latest;
        if self
            .journal
            .get("activatedAt")
            .is_some_and(|v| !v.is_null())
        {
            return self.write(json!({"phase": "repair-required", "error": error}));
        }
        let (port, ui_origin) = launch_context(&self.journal)?;
        if let Some(child) = self.next.as_mut() {
            stop(child);
        }
        if self.switched {
            if !wait_until(Duration::from_secs(30), || !port_open(port)) {
                return self.write(json!({"phase": "recovery-required", "error": format!("{error}; the new service still listens on port {port}")}));
            }
            if self.current.exists() {
                std::fs::remove_dir_all(&self.current)
                    .map_err(|e| format!("Cannot remove the failed app: {e}"))?;
            }
            std::fs::rename(&self.previous, &self.current)
                .map_err(|e| format!("Cannot restore the previous app: {e}"))?;
        }
        if self.backed_up {
            files::remove_tree_except(&self.root, KEEP)?;
            files::copy_tree_except(&self.backup.join("state"), &self.root, &[])?;
        }
        let _ = std::fs::remove_dir_all(&self.incoming);
        self.write(json!({"phase": "rolled-back", "error": error}))?;
        let executable = self.journal["identity"]["executable"]
            .as_str()
            .unwrap_or("Muteki")
            .to_owned();
        launch(
            &self.current,
            &executable,
            &[format!("--muteki-environment-root={}", self.root.display())],
            port,
            &ui_origin,
        )
        .map_err(|e| format!("Rolled back, but cannot restart the previous app: {e}"))?;
        Ok(())
    }
}

pub fn now() -> String {
    iso(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default())
}

/// RFC 3339 UTC text for a time since the Unix epoch.
pub fn iso(now: Duration) -> String {
    let (days, seconds) = ((now.as_secs() / 86400) as i64, now.as_secs() % 86400);
    let (y, m, d) = civil(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60,
        now.subsec_millis()
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's algorithm).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// The helper's entry point: `--muteki-update-runner=FILE [--recover]`.
pub fn run_helper(file: &Path, recover: bool) -> Result<(), String> {
    let journal =
        files::read_json(file).map_err(|e| format!("Cannot read {}: {e}", file.display()))?;
    if journal["version"] != 2 {
        return Err(
            "Legacy update journal format is unsupported; manual recovery is required"
                .into(),
        );
    }
    if matches!(Phase::read(&journal)?,Phase::Complete|Phase::RolledBack|Phase::RepairRequired|Phase::RecoveryRequired) {
        return Err("This update transaction is already settled".into());
    }
    let own = std::process::id() as i64;
    if files::helper_running(&journal) && journal["helperPid"].as_i64() != Some(own) {
        return Err("Another update coordinator is still running".into());
    }
    let id = journal["id"]
        .as_str()
        .ok_or("The journal has no id")?
        .to_owned();
    let current = PathBuf::from(
        journal["current"]
            .as_str()
            .ok_or("The journal names no installed app")?,
    );
    let root = PathBuf::from(
        journal["environmentRoot"]
            .as_str()
            .ok_or("The journal names no state directory")?,
    );
    if file.parent().and_then(Path::parent) != Some(root.join("updates").as_path())
        || file
            .parent()
            .and_then(Path::file_name)
            .is_none_or(|n| n != id.as_str())
    {
        return Err("The journal is not inside its own state directory".into());
    }
    let previous = PathBuf::from(format!("{}.previous-{id}.app", current.display()));
    let incoming = PathBuf::from(format!("{}.incoming-{id}.app", current.display()));
    let mut run = Run {
        file: file.to_owned(),
        backup: file
            .parent()
            .expect("journal directory")
            .join("data-backup"),
        switched: previous.exists(),
        backed_up: journal["backupComplete"] == true,
        journal,
        root,
        current,
        previous,
        incoming,
        next: None,
    };
    run.write(json!({"helperPid": own}))?;
    match run.attempt(recover) {
        Ok(()) => {
            println!("Update {id} complete");
            Ok(())
        }
        Err(error) => {
            eprintln!("Update {id} failed: {error}");
            run.roll_back(error)
        }
    }
}

/// The one unfinished transaction under `root`, if any.
pub fn interrupted(root: &Path) -> Result<Option<(PathBuf, Value)>, String> {
    let directory = root.join("updates");
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return Ok(None);
    };
    let mut pending = Vec::new();
    for entry in entries.flatten() {
        let file = entry.path().join("transaction.json");
        let Ok(journal) = files::read_json(&file) else {
            continue;
        };
        match Phase::read(&journal) {
            Ok(phase) if phase.done() => continue,
            Err(error) if journal["version"] == 2 => return Err(error),
            _ => {},
        }
        if journal["environmentRoot"].as_str() != Some(&*root.to_string_lossy())
            || journal["id"].as_str() != Some(&*entry.file_name().to_string_lossy())
        {
            return Err(format!(
                "Invalid pending update identity in {}",
                file.display()
            ));
        }
        pending.push((file, journal));
    }
    if pending.len() > 1 {
        return Err(
            "Multiple unfinished updates require recovery before this workspace can start".into(),
        );
    }
    Ok(pending.pop())
}

/// What a normal launch does about an interrupted update: `Ok(true)` when
/// this process must exit and leave the state to the helper.
pub fn recover_at_launch(root: &Path) -> Result<bool, String> {
    let Some((file, journal)) = interrupted(root)? else {
        return Ok(false);
    };
    if files::helper_running(&journal) {
        return Err(
            "A version update is still running. Open Muteki again after it finishes.".into(),
        );
    }
    if journal["version"] != 2 {
        patch(
            &file,
            json!({"phase": "repair-required", "error": "Legacy update journal format is unsupported; manual recovery is required."}),
        )?;
        return Ok(false);
    }
    if journal.get("activatedAt").is_some_and(|v| !v.is_null()) {
        patch(
            &file,
            json!({"phase": "repair-required", "error": "The update coordinator stopped after the new version opened writes."}),
        )?;
        return Ok(false);
    }
    let helper = file
        .parent()
        .expect("journal directory")
        .join("muteki-update-helper");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file.with_file_name("install.log"))
        .map_err(|e| e.to_string())?;
    let mut command = crate::shell_env::command(&helper);
    clean_env(&mut command)
        .arg(format!("--muteki-update-runner={}", file.display()))
        .arg("--recover")
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log)
        .process_group(0)
        .spawn()
        .map_err(|e| format!("Cannot start update recovery: {e}"))?;
    Ok(true)
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("HTTP client")
}

/// The new version's side of the switch, after its services started with
/// writes closed: prove the service answers, then open writes once the
/// helper recorded that it will no longer restore the old data.
pub fn admit(file: &Path) -> Result<(), String> {
    let journal = files::read_json(file).map_err(|e| e.to_string())?;
    if !provenance(&journal) {
        return Err("Candidate update journal lacks release metadata".into());
    }
    if journal["testFault"] == "validation" && journal["integrity"]["feed"] == "test" {
        let root = PathBuf::from(journal["environmentRoot"].as_str().unwrap_or_default());
        let _ = std::fs::write(root.join("update-test-fault.txt"), now());
        return Err("Injected validation failure (loopback test feed only)".into());
    }
    let backend = crate::backend::backend_origin();
    let ready: Value = client()
        .get(format!("{backend}/api/health"))
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.json())
        .map_err(|e| format!("Readiness check failed: {e}"))?;
    if ready["app"] != "muteki-rust" {
        return Err("The updated service is not ready".into());
    }
    let auth: Value = client()
        .get(format!("{backend}/api/auth/status"))
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.json())
        .map_err(|e| format!("Service identity check failed: {e}"))?;
    if auth["service_id"].as_str().is_none_or(str::is_empty) {
        return Err("The updated service did not name its identity".into());
    }
    patch(file, json!({"phase": "validated"}))?;
    if !wait_until(Duration::from_secs(30), || {
        files::read_json(file).is_ok_and(|j| Phase::read(&j) == Ok(Phase::WritesOpen))
    }) {
        return Err("Update coordinator did not admit writes".into());
    }
    crate::backend::set_writes(true)?;
    let ready:Value=client().get(format!("{backend}/api/readiness")).send().and_then(|r|r.error_for_status()).and_then(|r|r.json()).map_err(|error|format!("Activated service readiness failed: {error}"))?;
    if ready["ready"]!=true{return Err("Activated service core initialization is incomplete".into());}
    Ok(())
}

/// The candidate's acceptance run, with services already started on
/// private ports against an empty state directory.
pub fn self_check() -> Result<Value, String> {
    let backend = crate::backend::backend_origin();
    let ui = crate::backend::ui_origin()?;
    let ready: Value = client()
        .get(format!("{backend}/api/readiness"))
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.json())
        .map_err(|e| format!("Readiness: {e}"))?;
    if ready["ready"] != true || ready["app"] != "muteki-rust" {
        return Err("The candidate service is not ready".into());
    }
    let auth: Value = client()
        .get(format!("{backend}/api/auth/status"))
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.json())
        .map_err(|e| format!("Identity: {e}"))?;
    let service = auth["service_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("The candidate service did not name its identity")?
        .to_owned();
    let page = client()
        .get(format!("{ui}/chat"))
        .send()
        .map_err(|e| format!("Chat page: {e}"))?;
    let html = page
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/html"));
    if !page.status().is_success()
        || !html
        || !page.text().map_err(|e| e.to_string())?.contains("/_next/")
    {
        return Err("The candidate frontend did not serve the conversation page".into());
    }
    let proxied: Value = client()
        .get(format!("{ui}/api/auth/status"))
        .send()
        .and_then(|r| r.error_for_status())
        .and_then(|r| r.json())
        .map_err(|e| format!("Frontend proxy: {e}"))?;
    if proxied["service_id"].as_str() != Some(service.as_str()) {
        return Err("The candidate frontend proxies to a different service".into());
    }
    Ok(
        json!({"status": "passed", "version": crate::updates::app_version(), "serviceId": service,
        "checks": ["service readiness", "service identity", "conversation page HTML", "frontend API proxy"],
        "rendering": "not checked: the acceptance run has no window"}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_patches_merge_and_remove() {
        let mut journal = json!({"phase": "prepared", "error": "x"});
        merge(
            &mut journal,
            json!({"phase": "validating", "error": null, "helperPid": 3}),
        );
        assert_eq!(journal, json!({"phase": "validating", "helperPid": 3}));
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(19_723), (2024, 1, 1));
        assert!(release_page_pattern(
            "https://github.com/FishCodeTech/muteki",
            "https://github.com/FishCodeTech/muteki/releases/tag/v0.4.9"
        ));
        assert!(!release_page_pattern(
            "https://github.com/FishCodeTech/muteki",
            "https://github.com/Other/muteki/releases/tag/v0.4.9"
        ));
        assert!(!release_page_pattern(
            "https://github.com/FishCodeTech/muteki",
            "https://github.com/FishCodeTech/muteki/releases/tag/v0.4.9-rc"
        ));
    }

    #[test]
    fn only_one_unfinished_transaction_is_recovered() {
        let root =
            std::env::temp_dir().join(format!("muteki-upd-{}", uuid::Uuid::new_v4().simple()));
        assert_eq!(interrupted(&root), Ok(None));
        for (id, phase) in [("a", "complete"), ("b", "backing-up")] {
            std::fs::create_dir_all(root.join("updates").join(id)).unwrap();
            files::atomic_json(
                &root.join("updates").join(id).join("transaction.json"),
                &json!({"id": id, "phase": phase, "environmentRoot": root}),
            )
            .unwrap();
        }
        let (file, journal) = interrupted(&root).unwrap().unwrap();
        assert_eq!(
            (
                journal["id"].as_str(),
                file.parent().unwrap().file_name().unwrap().to_str()
            ),
            (Some("b"), Some("b"))
        );
        std::fs::create_dir_all(root.join("updates/c")).unwrap();
        files::atomic_json(
            &root.join("updates/c/transaction.json"),
            &json!({"id": "c", "phase": "switching", "environmentRoot": root}),
        )
        .unwrap();
        assert!(interrupted(&root).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_journal_is_preserved_for_manual_recovery() {
        let root = std::env::temp_dir().join(format!(
            "muteki-legacy-upd-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let directory = root.join("updates/legacy");
        std::fs::create_dir_all(&directory).unwrap();
        let file = directory.join("transaction.json");
        files::atomic_json(
            &file,
            &json!({"version":1,"id":"legacy","phase":"waiting-for-owner","environmentRoot":root}),
        )
        .unwrap();
        assert_eq!(recover_at_launch(&root), Ok(false));
        let journal = files::read_json(&file).unwrap();
        assert_eq!(journal["phase"], "repair-required");
        assert_eq!(journal["version"], 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn update_launch_keeps_only_valid_loopback_service_ports() {
        let valid = json!({"backendPort":8131,"uiOrigin":"http://127.0.0.1:3131"});
        assert_eq!(
            launch_context(&valid),
            Ok((8131, "http://127.0.0.1:3131".into()))
        );
        for invalid in [
            json!({"backendPort":0,"uiOrigin":"http://127.0.0.1:3131"}),
            json!({"backendPort":8131,"uiOrigin":"http://example.com:3131"}),
            json!({"backendPort":8131,"uiOrigin":"http://127.0.0.1:3131/?x=1"}),
        ] {
            assert!(launch_context(&invalid).is_err());
        }
    }
}
