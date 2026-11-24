//! Desktop updates from the project's GitHub Releases: check, download,
//! accept the candidate, and hand the switch to the update helper.
use crate::{
    backend,
    native::{self, Notice},
    update_files::{self as files, UpdateError},
    update_runner,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Mutex, OnceLock, mpsc},
    time::{Duration, Instant},
};

pub const GITHUB_BASE: &str = "https://github.com/FishCodeTech/muteki";
const GITHUB_API: &str = "https://api.github.com/repos/FishCodeTech/muteki/releases/latest";
const GITHUB_HOSTS: &[&str] = &[
    "github.com",
    "release-assets.githubusercontent.com",
    "objects.githubusercontent.com",
];
const REPOSITORY_PATH: &str = "/FishCodeTech/muteki";
/// Points the updater at a release server on this machine instead of GitHub,
/// for verifying installs end to end. Only loopback http origins are accepted.
const TEST_FEED: &str = "MUTEKI_UPDATE_TEST_FEED";
/// With the test feed only: `validation` makes the new version fail its
/// startup validation so the rollback path runs.
const TEST_FAULT: &str = "MUTEKI_UPDATE_TEST_FAULT";

/// Where this process runs and which state it owns.
pub struct Setup {
    pub bundle: Option<PathBuf>,
    pub channel: &'static str,
    pub root: PathBuf,
}

static SETUP: OnceLock<Setup> = OnceLock::new();

pub fn configure(setup: Setup) {
    let _ = SETUP.set(setup);
}

fn setup() -> Result<&'static Setup, UpdateError> {
    SETUP
        .get()
        .ok_or_else(|| UpdateError::new("update.unsupported", "更新尚未初始化。"))
}

/// The version in the running bundle's Info.plist; the crate version when
/// running outside a bundle.
pub fn app_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| files::bundle_of(&exe))
            .and_then(|app| {
                let out = crate::shell_env::command("/usr/libexec/PlistBuddy")
                    .args(["-c", "Print :CFBundleShortVersionString"])
                    .arg(app.join("Contents/Info.plist"))
                    .output()
                    .ok()?;
                let version = String::from_utf8_lossy(&out.stdout).trim().to_owned();
                (out.status.success() && files::semantic(&version)).then_some(version)
            })
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned())
    })
}

fn loopback_origin(url: &reqwest::Url) -> bool {
    url.scheme() == "http"
        && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
        && url.port().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}

/// A test feed's repository base: `http://127.0.0.1:PORT/FishCodeTech/muteki`.
pub fn loopback_base(base: &str) -> bool {
    reqwest::Url::parse(base).is_ok_and(|url| {
        loopback_origin(&url)
            && url.path() == REPOSITORY_PATH
            && base == format!("{}{REPOSITORY_PATH}", url.origin().ascii_serialization())
    })
}

struct Feed {
    kind: &'static str,
    api: String,
    base: String,
}

impl Feed {
    fn current() -> Result<Feed, UpdateError> {
        let Ok(origin) = std::env::var(TEST_FEED) else {
            return Ok(Feed {
                kind: "github",
                api: GITHUB_API.into(),
                base: GITHUB_BASE.into(),
            });
        };
        let url = reqwest::Url::parse(&origin)
            .ok()
            .filter(|url| loopback_origin(url) && url.path() == "/")
            .ok_or_else(|| {
                UpdateError::new(
                    "update.test_feed_invalid",
                    format!("{TEST_FEED} must be a plain http://127.0.0.1:PORT origin."),
                )
            })?;
        let origin = url.origin().ascii_serialization();
        Ok(Feed {
            kind: "test",
            api: format!("{origin}/repos{REPOSITORY_PATH}/releases/latest"),
            base: format!("{origin}{REPOSITORY_PATH}"),
        })
    }

    fn trusted(&self, url: &reqwest::Url) -> bool {
        match self.kind {
            "github" => {
                url.scheme() == "https"
                    && url
                        .host_str()
                        .is_some_and(|host| GITHUB_HOSTS.contains(&host))
            }
            _ => reqwest::Url::parse(&self.base).is_ok_and(|base| base.origin() == url.origin()),
        }
    }

    fn client(&self, timeout: u64) -> Result<reqwest::blocking::Client, UpdateError> {
        let builder = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(timeout))
            .user_agent("Muteki-Desktop-Updater");
        let builder = if self.kind == "test" {
            builder.no_proxy()
        } else {
            builder
        };
        builder
            .build()
            .map_err(|e| UpdateError::new("update.release_check_failed", e.to_string()))
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Release {
    version: String,
    asset_name: String,
    size: u64,
    digest: String,
    download: String,
    page: String,
}

fn safe_tag(tag: &str) -> bool {
    files::semantic(tag.strip_prefix('v').unwrap_or(tag))
}

/// The latest release's stable version and trusted asset for this machine.
fn checked_release(feed: &Feed, release: &Value) -> Result<Release, UpdateError> {
    let invalid = |message: &str| UpdateError::new("update.release_invalid", message);
    let tag = release["tag_name"].as_str().unwrap_or("");
    if release["draft"] == true || release["prerelease"] == true || !safe_tag(tag) {
        return Err(invalid(
            "Latest GitHub release is not a stable semantic version.",
        ));
    }
    let version = tag.strip_prefix('v').unwrap_or(tag).to_owned();
    let page = format!("{}/releases/tag/{tag}", feed.base);
    if release["html_url"].as_str() != Some(page.as_str()) {
        return Err(invalid(
            "GitHub release URL does not match the trusted repository and tag.",
        ));
    }
    let asset_name = format!("Muteki-{version}-{}.zip", files::host_arch());
    let download = format!("{}/releases/download/{tag}/{asset_name}", feed.base);
    let asset = release["assets"].as_array().and_then(|assets| {
        assets
            .iter()
            .find(|a| a["name"] == asset_name.as_str() && a["state"] == "uploaded")
    });
    let unavailable = || {
        UpdateError::new(
            "update.asset_unavailable",
            format!(
                "Trusted {} release asset is unavailable.",
                files::host_arch()
            ),
        )
    };
    let asset = asset.ok_or_else(unavailable)?;
    let size = asset["size"].as_u64().unwrap_or(0);
    let digest = asset["digest"].as_str().unwrap_or("");
    let digest = digest
        .strip_prefix("sha256:")
        .filter(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()));
    if asset["browser_download_url"].as_str() != Some(download.as_str())
        || size == 0
        || size > files::MAX_ASSET
        || digest.is_none()
    {
        return Err(unavailable());
    }
    Ok(Release {
        version,
        asset_name,
        size,
        digest: digest.unwrap().to_ascii_lowercase(),
        download,
        page,
    })
}

fn fetch_release(feed: &Feed) -> Result<Value, UpdateError> {
    let failed = |message: String| UpdateError::new("update.release_check_failed", message);
    let response = feed
        .client(30)?
        .get(&feed.api)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .map_err(|e| failed(format!("GitHub release check failed: {e}")))?;
    if !response.status().is_success() {
        return Err(failed(format!(
            "GitHub release check failed (HTTP {}).",
            response.status().as_u16()
        )));
    }
    response
        .json()
        .map_err(|e| failed(format!("GitHub release metadata is invalid: {e}")))
}

struct Prepared {
    file: PathBuf,
    version: String,
}

struct State {
    status: Value,
    release: Option<Release>,
    prepared: Option<Prepared>,
    busy: bool,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut guard = STATE.lock().unwrap_or_else(|p| p.into_inner());
    let state = guard.get_or_insert_with(|| State {
        status: last_install()
            .unwrap_or_else(|| json!({"state": "idle", "currentVersion": app_version()})),
        release: None,
        prepared: None,
        busy: false,
    });
    f(state)
}

fn publish(patch: Value) -> Value {
    let status = with(|state| {
        update_runner::merge(&mut state.status, patch);
        state.status["currentVersion"] = json!(app_version());
        state.status.clone()
    });
    native::notify(Notice::Event("update-status".into(), status.clone()));
    status
}

fn fail(error: &UpdateError) -> Value {
    json!({"state": "error", "error": {"code": error.code, "message": error.message}, "message": error.message})
}

pub fn status() -> Value {
    with(|state| {
        state.status["currentVersion"] = json!(app_version());
        state.status.clone()
    })
}

/// Whether this process may replace its own bundle; the reason when not.
fn install_refusal() -> Option<String> {
    let Ok(setup) = setup() else {
        return Some("更新尚未初始化。".into());
    };
    let Some(bundle) = setup.bundle.as_ref() else {
        return Some("安装更新仅支持打包后的 macOS 应用。".into());
    };
    if setup.channel != "stable" {
        return Some("工作区开发包与候选版本不能安装更新；请使用安装后的 Muteki.app。".into());
    }
    if backend::control_token().is_none() {
        return Some("当前后端服务不是由此应用启动的，不能安全切换版本。".into());
    }
    let parent = bundle.parent().map(Path::to_path_buf).unwrap_or_default();
    let writable = std::ffi::CString::new(parent.to_string_lossy().as_bytes())
        // SAFETY: access(2) on a NUL-terminated path.
        .is_ok_and(|path| unsafe { libc::access(path.as_ptr(), libc::W_OK) } == 0);
    if !writable {
        return Some(format!(
            "没有写入 {} 的权限，无法替换应用。",
            parent.display()
        ));
    }
    None
}

pub fn check() -> Value {
    if with(|state| std::mem::replace(&mut state.busy, true)) {
        return status();
    }
    publish(
        json!({"state": "checking", "error": null, "progress": null, "message": null, "lastInstall": null}),
    );
    let result = Feed::current().and_then(|feed| {
        let release = checked_release(&feed, &fetch_release(&feed)?)?;
        Ok((feed, release))
    });
    let status = match result {
        Ok((feed, release)) => {
            let current = files::compare_versions(&release.version, app_version()).is_le();
            let refusal = install_refusal();
            let patch = json!({
                "state": if current { "current" } else { "available" },
                "latestVersion": release.version, "assetName": release.asset_name,
                "releaseUrl": release.page, "checkedAt": update_runner::now(), "canInstall": refusal.is_none(),
                "installReason": refusal, "feed": feed.kind, "progress": 0,
                "message": if current { Value::from("已是最新版本。") } else if feed.kind == "test" { Value::from("发现新版本（本机测试更新源）。") } else { Value::Null },
            });
            with(|state| {
                state.release = Some(release);
                state.prepared = None;
            });
            publish(patch)
        }
        Err(error) => publish(fail(&error)),
    };
    with(|state| state.busy = false);
    status
}

/// The page's install action: download and accept, ask, then switch.
pub fn install() -> Result<Value, UpdateError> {
    if with(|state| std::mem::replace(&mut state.busy, true)) {
        return Err(UpdateError::new("update.busy", "更新流程正在进行中。"));
    }
    let result = install_inner();
    with(|state| state.busy = false);
    result
}

fn install_inner() -> Result<Value, UpdateError> {
    let ready = with(|state| state.status["state"] == "ready" && state.prepared.is_some());
    let version =
        with(|state| state.prepared.as_ref().map(|p| p.version.clone()).or_else(|| state.release.as_ref().map(|r| r.version.clone())))
            .ok_or_else(|| UpdateError::new("update.not_available", "请先检查可用更新。"))?;
    let (sender, receiver) = mpsc::channel();
    native::on_main(move || {
        let chosen = native::alert(
            false,
            &format!("确认安装版本 {version}？"),
            "将下载并验证更新包、运行新版本启动检查，通过后保存草稿、备份数据并重启。",
            &["稍后", "应用更新"],
        );
        let _ = sender.send(chosen);
    });
    if receiver.recv().unwrap_or(0) != 1 {
        return Ok(status());
    }
    if !ready {
        if let Err(error) = download_and_prepare() {
            return Ok(publish(fail(&error)));
        }
    }
    match apply() {
        Ok(status) => Ok(status),
        Err(error) => Ok(publish(
            json!({"state": "ready", "error": {"code": error.code, "message": error.message}, "message": error.message}),
        )),
    }
}

fn download_and_prepare() -> Result<(), UpdateError> {
    if let Some(reason) = install_refusal() {
        return Err(UpdateError::new("update.unsupported", reason));
    }
    let checked = with(|state| {
        (state.status["state"] == "available")
            .then(|| state.release.clone())
            .flatten()
    })
    .ok_or_else(|| {
        UpdateError::new(
            "update.not_available",
            "Check for an available update before installing.",
        )
    })?;
    publish(json!({"state": "downloading", "progress": 0, "error": null}));
    let feed = Feed::current()?;
    let release = checked_release(&feed, &fetch_release(&feed)?).ok();
    if release.as_ref() != Some(&checked) {
        return Err(UpdateError::new(
            "update.release_changed",
            "Release metadata changed since the update check. Check again.",
        ));
    }
    let setup = setup()?;
    let updates = setup.root.join("updates");
    files::private_dir(&updates)?;
    let work = updates.join(format!(".download-{}", uuid::Uuid::new_v4().simple()));
    files::private_dir(&work)?;
    let result = (|| {
        let zip = work.join(&checked.asset_name);
        publish(
            json!({"message": if feed.kind == "test" { "正在从本机测试更新源下载安装包…" } else { "正在从 GitHub 下载官方安装包…" }, "progress": 0}),
        );
        download(&feed, &checked, &zip)?;
        publish(
            json!({"state": "validating", "progress": 100, "message": "正在检查安装包结构与签名…"}),
        );
        let bytes = std::fs::read(&zip)
            .map_err(|e| UpdateError::new("update.archive_invalid", e.to_string()))?;
        files::assert_zip_safe(&bytes)?;
        drop(bytes);
        let extracted = work.join("extracted");
        files::private_dir(&extracted)?;
        let out = crate::shell_env::command("/usr/bin/ditto")
            .args(["-x", "-k"])
            .arg(&zip)
            .arg(&extracted)
            .output()
            .map_err(|e| UpdateError::new("update.archive_invalid", e.to_string()))?;
        if !out.status.success() {
            return Err(UpdateError::new(
                "update.archive_invalid",
                format!(
                    "Cannot extract the release: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ));
        }
        let entries: Vec<_> = std::fs::read_dir(&extracted)
            .map_err(|e| UpdateError::new("update.archive_invalid", e.to_string()))?
            .flatten()
            .map(|e| e.file_name())
            .collect();
        if entries.len() != 1 || entries[0] != "Muteki.app" {
            return Err(UpdateError::new(
                "update.archive_invalid",
                "Release archive must contain exactly one application bundle.",
            ));
        }
        let source = extracted.join("Muteki.app");
        let identity = files::bundle_identity(&source, files::host_arch())?;
        if identity.id != files::BUNDLE_ID || identity.version != checked.version {
            return Err(UpdateError::new(
                "update.identity_mismatch",
                "Downloaded application identity or version does not match the checked release.",
            ));
        }
        let bundle = setup.bundle.as_ref().expect("checked by install_refusal");
        let current = files::bundle_identity(bundle, files::host_arch())?;
        if identity.id != current.id || identity.team != current.team {
            return Err(UpdateError::new(
                "update.signing_mismatch",
                "Release signing identity does not match the installed application.",
            ));
        }
        if !files::compare_versions(&identity.version, app_version()).is_gt() {
            return Err(UpdateError::new(
                "update.not_newer",
                "更新版本必须高于当前版本。",
            ));
        }
        publish(json!({"message": "正在运行候选版本的启动验收…"}));
        let prepared = prepare(setup, bundle, &source, &current, &identity, &checked, &feed)?;
        with(|state| state.prepared = Some(prepared));
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&work);
    result?;
    publish(
        json!({"state": "ready", "progress": 100, "preparedVersion": checked.version, "message": "更新已通过启动检查。"}),
    );
    Ok(())
}

/// Copies the selected release asset to disk while reporting download progress.
fn stream(
    input: &mut impl Read,
    mut output: std::fs::File,
    release: &Release,
    mut progress: impl FnMut(u64),
) -> Result<(), UpdateError> {
    let mut total = 0u64;
    let mut sha256 = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(|e| UpdateError::new("update.download_failed", e.to_string()))?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > release.size {
            return Err(UpdateError::new(
                "update.asset_size_mismatch",
                "Downloaded release exceeds its published size.",
            ));
        }
        output
            .write_all(&buffer[..read])
            .map_err(|e| UpdateError::new("update.download_failed", e.to_string()))?;
        sha256.update(&buffer[..read]);
        progress(total);
    }
    output
        .sync_all()
        .map_err(|e| UpdateError::new("update.download_failed", e.to_string()))?;
    if total != release.size {
        return Err(UpdateError::new(
            "update.asset_size_mismatch",
            "Downloaded release size does not match GitHub metadata.",
        ));
    }
    if format!("{:x}", sha256.finalize()) != release.digest {
        return Err(UpdateError::new(
            "update.digest_mismatch",
            "Downloaded release SHA-256 does not match GitHub metadata.",
        ));
    }
    Ok(())
}

fn download(feed: &Feed, release: &Release, destination: &Path) -> Result<(), UpdateError> {
    let mut response = feed
        .client(180)?
        .get(&release.download)
        .send()
        .map_err(|e| {
            UpdateError::new(
                "update.download_failed",
                format!("Release download failed: {e}"),
            )
        })?;
    if !response.status().is_success() || !feed.trusted(response.url()) {
        return Err(UpdateError::new(
            "update.download_failed",
            format!(
                "Release download failed (HTTP {}).",
                response.status().as_u16()
            ),
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length != release.size)
    {
        return Err(UpdateError::new(
            "update.asset_size_mismatch",
            "Downloaded release size does not match GitHub metadata.",
        ));
    }
    let output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(destination)
        .map_err(|e| UpdateError::new("update.download_failed", e.to_string()))?;
    let mut last = 0;
    stream(&mut response, output, release, |total| {
        let percent = total * 100 / release.size;
        if percent != last {
            last = percent;
            publish(json!({"progress": percent}));
        }
    })
}

fn prepare(
    setup: &Setup,
    bundle: &Path,
    source: &Path,
    current: &files::Identity,
    accepted: &files::Identity,
    release: &Release,
    feed: &Feed,
) -> Result<Prepared, UpdateError> {
    let fail = |message: String| UpdateError::new("update.install_prepare_failed", message);
    let id = uuid::Uuid::new_v4().to_string();
    let transaction = setup.root.join("updates").join(&id);
    files::private_dir(&transaction)?;
    let candidate = transaction.join("candidate.app");
    files::copy_bundle(source, &candidate)?;
    if files::bundle_identity(&candidate, files::host_arch())? != *accepted {
        return Err(fail("候选应用身份或版本与已接受的发布版本不一致。".into()));
    }
    let sha256 = files::bundle_hash(&candidate)?;
    if sha256 != files::bundle_hash(source)? {
        return Err(fail("候选应用复制结果与发布版本不一致。".into()));
    }
    let previous_sha256 = files::bundle_hash(bundle)?;
    let profile = transaction.join("acceptance");
    files::private_dir(&profile)?;
    let log_path = transaction.join("acceptance.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log_path)
        .map_err(|e| fail(e.to_string()))?;
    let mut command = crate::shell_env::command(candidate.join("Contents/MacOS").join(&accepted.executable));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("MUTEKI_") {
            command.env_remove(key);
        }
    }
    let mut child = command
        .args([
            "--muteki-channel=candidate".to_owned(),
            format!("--muteki-environment-root={}", profile.display()),
            "--muteki-self-check".to_owned(),
        ])
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(|e| fail(e.to_string()))?)
        .stderr(log)
        .process_group(0)
        .spawn()
        .map_err(|e| fail(format!("Cannot start the candidate: {e}")))?;
    let deadline = Instant::now() + Duration::from_secs(150);
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| fail(e.to_string()))? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let result = files::read_json(&profile.join("candidate-result.json")).ok();
    let passed = status.is_some_and(|s| s.success())
        && result.as_ref().is_some_and(|r| r["status"] == "passed");
    if !passed {
        let detail = result
            .as_ref()
            .and_then(|r| r["error"].as_str())
            .unwrap_or("no result");
        return Err(fail(format!(
            "候选版本验收失败（{detail}），请查看 {}",
            log_path.display()
        )));
    }
    if files::bundle_hash(&candidate)? != sha256 {
        return Err(fail("候选版本运行时修改了应用包，不能安装。".into()));
    }
    let helper = transaction.join("muteki-update-helper");
    let executable = std::env::current_exe().map_err(|e| fail(e.to_string()))?;
    std::fs::copy(&executable, &helper)
        .map_err(|e| fail(format!("Cannot copy the update helper: {e}")))?;
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| fail(e.to_string()))?;
    let test_fault = (feed.kind == "test"
        && std::env::var(TEST_FAULT).is_ok_and(|v| v == "validation"))
    .then_some("validation");
    let journal = json!({
        "version": 2, "id": id, "phase": "prepared", "mode": "official",
        "current": bundle, "candidate": candidate, "identity": current.to_json(), "candidateIdentity": accepted.to_json(),
        "sha256": sha256, "previousSha256": previous_sha256,
        "assetDigest": format!("sha256:{}", release.digest),
        "environmentRoot": setup.root, "acceptance": result,
        "integrity": {"kind": "github-release-asset", "repository": "FishCodeTech/muteki", "feed": feed.kind, "base": feed.base,
            "releaseUrl": release.page, "assetName": release.asset_name,
            "sha256": format!("sha256:{}", release.digest)},
        "testFault": test_fault,
    });
    let file = transaction.join("transaction.json");
    files::atomic_json(&file, &journal).map_err(|e| fail(e.to_string()))?;
    Ok(Prepared {
        file,
        version: accepted.version.clone(),
    })
}

struct DraftBarrier{token:String,expected:std::collections::HashMap<String,String>,acknowledged:std::collections::HashSet<String>,invalidated:Option<String>,sender:Option<mpsc::Sender<(bool,String)>>}
struct FinishedDraftBarrier{token:String,expected:std::collections::HashMap<String,String>}
static DOCUMENTS:Mutex<Option<std::collections::HashMap<String,String>>>=Mutex::new(None);
static FLUSH:Mutex<Option<DraftBarrier>>=Mutex::new(None);
static FINISHED_FLUSHES:Mutex<std::collections::VecDeque<FinishedDraftBarrier>>=Mutex::new(std::collections::VecDeque::new());
static BARRIER_ACTIVE:std::sync::atomic::AtomicBool=std::sync::atomic::AtomicBool::new(false);
pub fn barrier_active()->bool{BARRIER_ACTIVE.load(std::sync::atomic::Ordering::SeqCst)}
pub fn has_documents()->bool{DOCUMENTS.lock().unwrap_or_else(|error|error.into_inner()).as_ref().is_some_and(|documents|!documents.is_empty())}
fn invalidate(barrier:&mut DraftBarrier,message:String){
    barrier.invalidated.get_or_insert(message.clone());
    if let Some(sender)=barrier.sender.take(){let _=sender.send((false,message));}
}
pub fn document_ready(window:&str,document:&str){
    if document.is_empty(){return}
    let mut docs=DOCUMENTS.lock().unwrap_or_else(|error|error.into_inner());let docs=docs.get_or_insert_with(Default::default);let previous=docs.insert(window.into(),document.into());
    if previous.as_deref()==Some(document){return}
    if let Some(barrier)=FLUSH.lock().unwrap_or_else(|error|error.into_inner()).as_mut(){invalidate(barrier,"保存期间有窗口打开或刷新，请重新执行。".into());}
}
pub fn document_closed(window:&str){
    let mut docs=DOCUMENTS.lock().unwrap_or_else(|error|error.into_inner());
    if let Some(docs)=docs.as_mut(){docs.remove(window);}
    if let Some(barrier)=FLUSH.lock().unwrap_or_else(|error|error.into_inner()).as_mut(){if barrier.expected.contains_key(window){invalidate(barrier,"保存期间有窗口关闭，请重新执行。".into());}}
}
pub fn acknowledge(token:&str,window:&str,document:&str,persisted:bool,error:&str)->bool{
    let mut pending=FLUSH.lock().unwrap_or_else(|error|error.into_inner());let Some(barrier)=pending.as_mut().filter(|barrier|barrier.token==token)else{return FINISHED_FLUSHES.lock().unwrap_or_else(|error|error.into_inner()).iter().any(|barrier|barrier.token==token&&barrier.expected.get(window).map(String::as_str)==Some(document))};
    if barrier.expected.get(window).map(String::as_str)!=Some(document){return false}
    if barrier.invalidated.is_some(){return true}
    if !persisted{invalidate(barrier,format!("窗口 {window}：{error}"));return true}
    barrier.acknowledged.insert(window.into());
    if barrier.acknowledged.len()==barrier.expected.len(){if let Some(sender)=barrier.sender.take(){let _=sender.send((true,String::new()));}}true
}
fn check_barrier(documents:&Option<std::collections::HashMap<String,String>>,barrier:&Option<DraftBarrier>)->Result<(),String>{
    let Some(barrier)=barrier else{return if !native::BRIDGE_READY.load(std::sync::atomic::Ordering::SeqCst)&&documents.as_ref().is_none_or(|docs|docs.is_empty()){Ok(())}else{Err("全部窗口保存屏障已失效。".into())}};
    if let Some(error)=&barrier.invalidated{return Err(error.clone())}
    if documents.as_ref()!=Some(&barrier.expected)||barrier.acknowledged.len()!=barrier.expected.len(){return Err("窗口文档或草稿确认已改变，请重新执行。".into())}Ok(())
}
pub fn validate_barrier()->Result<(),UpdateError>{with_valid_barrier(||Ok(())).map_err(|message|UpdateError::new("update.drafts_unsaved",message))}
/// Holds the registered document generation stable through the irreversible admission step.
pub fn with_valid_barrier<T>(work:impl FnOnce()->Result<T,String>)->Result<T,String>{with_draft_documents(work)?}
pub fn with_draft_documents<T>(work:impl FnOnce()->T)->Result<T,String>{
    let documents=DOCUMENTS.lock().unwrap_or_else(|error|error.into_inner());
    let barrier=FLUSH.lock().unwrap_or_else(|error|error.into_inner());
    check_barrier(&documents,&barrier)?;Ok(work())
}

pub fn flush_drafts()->Result<(),UpdateError>{
    if BARRIER_ACTIVE.swap(true,std::sync::atomic::Ordering::SeqCst){return Err(UpdateError::new("update.drafts_busy","另一项操作正在保存全部窗口的草稿。"));}
    let token=uuid::Uuid::new_v4().to_string();let(sender,receiver)=mpsc::channel();
    let documents=DOCUMENTS.lock().unwrap_or_else(|error|error.into_inner());let expected=documents.clone().unwrap_or_default();
    if expected.is_empty(){BARRIER_ACTIVE.store(false,std::sync::atomic::Ordering::SeqCst);return if native::BRIDGE_READY.load(std::sync::atomic::Ordering::SeqCst){Err(UpdateError::new("update.drafts_unsaved","没有已登记的窗口文档，请等待页面就绪后重试。"))}else{Ok(())}}
    *FLUSH.lock().unwrap_or_else(|error|error.into_inner())=Some(DraftBarrier{token:token.clone(),expected,acknowledged:Default::default(),invalidated:None,sender:Some(sender)});drop(documents);
    native::notify(Notice::Event("before-close".into(),json!({"token":token,"reason":"persistence-barrier"})));
    let answer=receiver.recv_timeout(Duration::from_secs(10));
    match answer{Ok((true,_))=>{if let Err(error)=validate_barrier(){release_barrier();return Err(error)}Ok(())},Ok((false,error))=>{release_barrier();Err(UpdateError::new("update.drafts_unsaved",format!("草稿未保存，操作已暂停。{error}")))},Err(_)=>{release_barrier();Err(UpdateError::new("update.drafts_unsaved","全部窗口未在 10 秒内确认草稿保存，操作已暂停。"))}}
}
pub fn release_barrier(){
    let mut pending=FLUSH.lock().unwrap_or_else(|error|error.into_inner());
    let token=pending.take().map(|barrier|{
        let mut finished=FINISHED_FLUSHES.lock().unwrap_or_else(|error|error.into_inner());
        finished.push_back(FinishedDraftBarrier{token:barrier.token.clone(),expected:barrier.expected});
        if finished.len()>16{finished.pop_front();}
        barrier.token
    });
    BARRIER_ACTIVE.store(false,std::sync::atomic::Ordering::SeqCst);drop(pending);
    if let Some(token)=token{native::notify(Notice::Event("persistence-released".into(),json!({"token":token})));}
}

fn apply() -> Result<Value, UpdateError> {
    let file = with(|state| state.prepared.as_ref().map(|p| p.file.clone())).ok_or_else(|| {
        UpdateError::new("update.not_available", "No accepted candidate is prepared.")
    })?;
    publish(json!({"message": "正在保存草稿…"}));
    flush_drafts()?;
    publish(json!({"message": "正在等待当前任务结束…"}));
    if let Err(message)=backend::set_writes(false){release_barrier();return Err(UpdateError::new("update.service_busy",message));}
    let launched = with_valid_barrier(|| {
        update_runner::patch(
            &file,
            json!({"phase": "waiting-for-owner", "ownerPid": std::process::id(),
                "backendPort": backend::backend_port(), "uiOrigin": backend::ui_origin()?}),
        )?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(file.with_file_name("install.log"))
            .map_err(|e| e.to_string())?;
        let mut command = crate::shell_env::command(file.with_file_name("muteki-update-helper"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("MUTEKI_") {
                command.env_remove(key);
            }
        }
        command
            .arg(format!("--muteki-update-runner={}", file.display()))
            .stdin(Stdio::null())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log)
            .process_group(0)
            .spawn()
            .map(drop)
            .map_err(|e| format!("Cannot start the update helper: {e}"))
    });
    if let Err(message) = launched {
        release_barrier();
        let _ = backend::set_writes(true);
        let _ = update_runner::patch(&file, json!({"phase": "prepared", "ownerPid": null}));
        return Err(UpdateError::new("update.helper_failed", message));
    }
    let status =
        publish(json!({"state": "installing", "message": "正在退出并安装更新。", "error": null}));
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(300));
        native::on_main(native::quit_now);
    });
    Ok(status)
}

static AWAITING_UI: Mutex<Option<PathBuf>> = Mutex::new(None);

/// The updated version completes its transaction once its page is up.
pub fn complete_when_ready(file: PathBuf) {
    *AWAITING_UI.lock().unwrap_or_else(|p| p.into_inner()) = Some(file);
}

pub fn ui_ready() {
    if let Some(file) = AWAITING_UI.lock().unwrap_or_else(|p| p.into_inner()).take() {
        if let Err(error) = update_runner::patch(
            &file,
            json!({"phase": "complete", "completedAt": update_runner::now()}),
        ) {
            eprintln!("Muteki: cannot complete the update journal: {error}");
        }
    }
}

/// The newest finished transaction, as the status the settings page shows.
fn last_install() -> Option<Value> {
    let setup = SETUP.get()?;
    let mut newest: Option<(std::time::SystemTime, Value)> = None;
    for entry in std::fs::read_dir(setup.root.join("updates"))
        .ok()?
        .flatten()
    {
        let file = entry.path().join("transaction.json");
        let Ok(meta) = std::fs::symlink_metadata(&file) else {
            continue;
        };
        let Ok(journal) = files::read_json(&file) else {
            continue;
        };
        let terminal = [
            "complete",
            "rolled-back",
            "failed",
            "repair-required",
            "recovery-required",
        ]
        .contains(&journal["phase"].as_str().unwrap_or(""));
        let valid = meta.is_file()
            && matches!(journal["version"].as_u64(),Some(1|2))
            && journal["id"].as_str() == Some(&*entry.file_name().to_string_lossy())
            && journal["environmentRoot"].as_str() == Some(&*setup.root.to_string_lossy())
            && journal["candidateIdentity"]["id"] == files::BUNDLE_ID
            && journal["candidateIdentity"]["version"]
                .as_str()
                .is_some_and(files::semantic);
        let at = meta.modified().ok()?;
        if terminal && valid && newest.as_ref().is_none_or(|(when, _)| at > *when) {
            newest = Some((at, journal));
        }
    }
    let (at, journal) = newest?;
    let failed = journal["phase"] != "complete";
    let code = if ["repair-required", "recovery-required"]
        .contains(&journal["phase"].as_str().unwrap_or(""))
    {
        "update.recovery_required"
    } else {
        "update.last_install_failed"
    };
    let detail: String = journal["error"]
        .as_str()
        .unwrap_or("")
        .chars()
        .take(1000)
        .collect();
    let version = journal["candidateIdentity"]["version"]
        .as_str()
        .unwrap_or("");
    let message = if failed {
        format!(
            "上次更新到 {version} 未完成。{}",
            if detail.is_empty() {
                String::new()
            } else {
                format!(" {detail}")
            }
        )
    } else {
        format!("上次更新已完成，安装版本为 {version}。")
    };
    let when = update_runner::iso(at.duration_since(std::time::UNIX_EPOCH).unwrap_or_default());
    let mut status = json!({"state": if failed { "error" } else { "idle" }, "currentVersion": app_version(), "message": message,
        "lastInstall": {"state": if failed { "failed" } else { "complete" }, "version": version, "at": when, "message": message, "phase": journal["phase"]}});
    if failed {
        status["error"] = json!({"code": code, "message": message});
        status["lastInstall"]["code"] = json!(code);
    }
    Some(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, url: &str) -> Value {
        let version = tag.trim_start_matches('v');
        let name = format!("Muteki-{version}-{}.zip", files::host_arch());
        json!({"tag_name": tag, "draft": false, "prerelease": false, "html_url": format!("{GITHUB_BASE}/releases/tag/{tag}"),
            "assets": [
                {"name": name, "state": "uploaded", "size": 10,
                    "digest": format!("sha256:{}", "0".repeat(64)),
                    "browser_download_url": url.replace("{name}", &name)}]})
    }

    #[test]
    fn only_canonical_stable_release_assets_are_offered() {
        let feed = Feed {
            kind: "github",
            api: GITHUB_API.into(),
            base: GITHUB_BASE.into(),
        };
        let good = release(
            "v0.4.9",
            &format!("{GITHUB_BASE}/releases/download/v0.4.9/{{name}}"),
        );
        let checked = checked_release(&feed, &good).unwrap();
        assert_eq!((checked.version.as_str(), checked.size), ("0.4.9", 10));
        let mut prerelease = good.clone();
        prerelease["prerelease"] = json!(true);
        assert_eq!(
            checked_release(&feed, &prerelease).unwrap_err().code,
            "update.release_invalid"
        );
        let mut moved = good.clone();
        moved["html_url"] = json!("https://github.com/attacker/muteki/releases/tag/v0.4.9");
        assert_eq!(
            checked_release(&feed, &moved).unwrap_err().code,
            "update.release_invalid"
        );
        let elsewhere = release("v0.4.9", "https://example.com/{name}");
        assert_eq!(
            checked_release(&feed, &elsewhere).unwrap_err().code,
            "update.asset_unavailable"
        );
        let mut no_asset = good.clone();
        no_asset["assets"].as_array_mut().unwrap().pop();
        assert_eq!(
            checked_release(&feed, &no_asset).unwrap_err().code,
            "update.asset_unavailable"
        );
        assert_eq!(
            checked_release(&feed, &release("v0.4.9-beta", ""))
                .unwrap_err()
                .code,
            "update.release_invalid"
        );
    }

    #[test]
    fn test_feeds_and_downloads_stay_on_loopback() {
        assert!(loopback_base("http://127.0.0.1:9123/FishCodeTech/muteki"));
        assert!(!loopback_base(
            "http://192.168.1.2:9123/FishCodeTech/muteki"
        ));
        assert!(!loopback_base("https://127.0.0.1:9123/FishCodeTech/muteki"));
        assert!(!loopback_base("http://127.0.0.1:9123/Other/muteki"));
        let github = Feed {
            kind: "github",
            api: GITHUB_API.into(),
            base: GITHUB_BASE.into(),
        };
        let url = |u: &str| reqwest::Url::parse(u).unwrap();
        assert!(github.trusted(&url("https://release-assets.githubusercontent.com/x")));
        assert!(!github.trusted(&url("http://github.com/x")));
        assert!(!github.trusted(&url("https://github.com.evil.example/x")));
        let local = Feed {
            kind: "test",
            api: String::new(),
            base: "http://127.0.0.1:9123/FishCodeTech/muteki".into(),
        };
        assert!(local.trusted(&url(
            "http://127.0.0.1:9123/FishCodeTech/muteki/releases/download/v1.0.0/a.zip"
        )));
        assert!(!local.trusted(&url("http://127.0.0.1:9124/a.zip")));
    }

    #[test]
    fn streams_keep_only_the_published_bytes() {
        let dir =
            std::env::temp_dir().join(format!("muteki-stream-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let release = Release {
            version: "1.0.0".into(),
            asset_name: "a".into(),
            size: 5,
            digest: format!("{:x}", Sha256::digest(b"hello")),
            download: String::new(),
            page: String::new(),
        };
        let open = |name: &str| std::fs::File::create(dir.join(name)).unwrap();
        assert_eq!(
            stream(&mut &b"hello"[..], open("ok"), &release, |_| {}),
            Ok(())
        );
        assert_eq!(
            stream(&mut &b"hello!"[..], open("long"), &release, |_| {})
                .unwrap_err()
                .code,
            "update.asset_size_mismatch"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn loopback_release_download_checks_sha256_before_acceptance() {
        use std::io::{BufRead, BufReader};
        use std::net::TcpListener;
        let original = vec![b'x'; 1024 * 1024 + 7];
        let expected_digest =
            "sha256:168ed845d93ef3ae56e43467533f949ba8cf344bede498a2e2e98c4258586bd7";
        for scenario in ["correct", "same_size_tamper", "missing_digest"] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let base = format!("http://127.0.0.1:{port}/FishCodeTech/muteki");
            let api = format!("http://127.0.0.1:{port}/repos/FishCodeTech/muteki/releases/latest");
            let name = format!("Muteki-0.4.12-{}.zip", files::host_arch());
            let mut release = json!({"tag_name":"v0.4.12","draft":false,"prerelease":false,
                "html_url":format!("{base}/releases/tag/v0.4.12"),
                "assets":[{"name":name,"state":"uploaded","size":original.len(),
                    "digest":expected_digest,
                    "browser_download_url":format!("{base}/releases/download/v0.4.12/{name}")}]});
            if scenario == "missing_digest" {
                release["assets"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("digest");
            }
            let mut served = original.clone();
            if scenario == "same_size_tamper" {
                served[1024 * 1024] = b'y';
            }
            let requests = if scenario == "missing_digest" { 1 } else { 2 };
            let server = std::thread::spawn(move || {
                let mut routes = Vec::new();
                for _ in 0..requests {
                    let (socket, _) = listener.accept().unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    let mut reader = BufReader::new(socket);
                    let mut request = String::new();
                    reader.read_line(&mut request).unwrap();
                    loop {
                        let mut header = String::new();
                        reader.read_line(&mut header).unwrap();
                        if header == "\r\n" || header.is_empty() {
                            break;
                        }
                    }
                    let body = if request.starts_with("GET /repos/") {
                        release.to_string().into_bytes()
                    } else {
                        assert!(request.starts_with("GET /FishCodeTech/muteki/releases/download/"));
                        served.clone()
                    };
                    routes.push(request.trim().to_owned());
                    let mut socket = reader.into_inner();
                    write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                    socket.write_all(&body).unwrap();
                }
                routes
            });
            let feed = Feed {
                kind: "test",
                api,
                base,
            };
            let metadata = fetch_release(&feed).unwrap();
            let checked = checked_release(&feed, &metadata);
            let dir = std::env::temp_dir().join(format!(
                "muteki-update-digest-{}",
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("release.zip");
            match scenario {
                "correct" => {
                    download(&feed, &checked.unwrap(), &path).unwrap();
                    assert_eq!(std::fs::read(&path).unwrap(), original);
                    eprintln!(
                        "live loopback: correct SHA-256 accepted ({} downloaded bytes)",
                        original.len()
                    );
                }
                "same_size_tamper" => {
                    let error = download(&feed, &checked.unwrap(), &path).unwrap_err();
                    assert_eq!(error.code, "update.digest_mismatch");
                    assert_eq!(
                        std::fs::metadata(&path).unwrap().len(),
                        original.len() as u64
                    );
                    eprintln!("live loopback: same-size changed content rejected by SHA-256");
                }
                _ => {
                    assert_eq!(checked.unwrap_err().code, "update.asset_unavailable");
                    assert!(!path.exists());
                    eprintln!("live loopback: missing digest rejected before asset download");
                }
            }
            let routes = server.join().unwrap();
            assert_eq!(routes.len(), requests);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    /// Source `persistenceBarrier` must hear from every registered window.
    /// The product route is `flush_drafts` plus bridge `close.ack` (`acknowledge`).
    #[test]
    fn draft_barrier_keeps_the_update_until_every_window_confirms() {
        let documents = std::collections::HashMap::from([
            ("i131-a".to_owned(), "doc-a".to_owned()),
            ("i131-b".to_owned(), "doc-b".to_owned()),
        ]);
        let partial = DraftBarrier {
            token: "partial".into(),
            expected: documents.clone(),
            acknowledged: std::collections::HashSet::from(["i131-a".to_owned()]),
            invalidated: None,
            sender: None,
        };
        assert!(check_barrier(&Some(documents.clone()), &Some(partial)).is_err());
        let both = std::collections::HashSet::from(["i131-a".to_owned(), "i131-b".to_owned()]);
        assert!(check_barrier(
            &Some(documents.clone()),
            &Some(DraftBarrier {
                token: "complete".into(),
                expected: documents.clone(),
                acknowledged: both.clone(),
                invalidated: None,
                sender: None,
            })
        )
        .is_ok());
        assert!(check_barrier(
            &Some(documents.clone()),
            &Some(DraftBarrier {
                token: "closed".into(),
                expected: documents.clone(),
                acknowledged: both,
                invalidated: Some("保存期间有窗口关闭，请重新执行。".into()),
                sender: None,
            })
        )
        .is_err());
        let shifted = std::collections::HashMap::from([
            ("i131-a".to_owned(), "doc-a-next".to_owned()),
            ("i131-b".to_owned(), "doc-b".to_owned()),
        ]);
        let stale_set = DraftBarrier {
            token: "shifted".into(),
            expected: documents.clone(),
            acknowledged: std::collections::HashSet::from([
                "i131-a".to_owned(),
                "i131-b".to_owned(),
            ]),
            invalidated: None,
            sender: None,
        };
        assert!(check_barrier(&Some(shifted), &Some(stale_set)).is_err());

        struct Cleanup;
        impl Drop for Cleanup {
            fn drop(&mut self) {
                release_barrier();
                document_closed("i131-a");
                document_closed("i131-b");
            }
        }
        let _cleanup = Cleanup;
        release_barrier();
        document_closed("i131-a");
        document_closed("i131-b");
        document_ready("i131-a", "doc-a");
        document_ready("i131-b", "doc-b");
        let (sender, receiver) = mpsc::channel();
        *FLUSH.lock().unwrap_or_else(|error| error.into_inner()) = Some(DraftBarrier {
            token: "i131-live-token".into(),
            expected: documents.clone(),
            acknowledged: Default::default(),
            invalidated: None,
            sender: Some(sender),
        });
        BARRIER_ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(acknowledge("i131-live-token", "i131-a", "doc-a", true, ""));
        assert!(receiver.try_recv().is_err());
        assert_eq!(
            validate_barrier().unwrap_err().code,
            "update.drafts_unsaved"
        );
        assert!(acknowledge("i131-live-token", "i131-b", "doc-b", true, ""));
        assert_eq!(receiver.recv_timeout(Duration::from_secs(1)).unwrap().0, true);
        assert!(validate_barrier().is_ok());
        release_barrier();

        let (sender, receiver) = mpsc::channel();
        *FLUSH.lock().unwrap_or_else(|error| error.into_inner()) = Some(DraftBarrier {
            token: "i131-fail-token".into(),
            expected: documents,
            acknowledged: Default::default(),
            invalidated: None,
            sender: Some(sender),
        });
        BARRIER_ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(acknowledge(
            "i131-fail-token",
            "i131-b",
            "doc-b",
            false,
            "disk full"
        ));
        let (saved, message) = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(!saved);
        assert!(message.contains("disk full"));
        assert!(validate_barrier().is_err());
        release_barrier();
        document_ready("i131-a", "doc-a");
        let (sender, receiver) = mpsc::channel();
        *FLUSH.lock().unwrap_or_else(|error| error.into_inner()) = Some(DraftBarrier {
            token: "i131-refresh-token".into(),
            expected: std::collections::HashMap::from([
                ("i131-a".to_owned(), "doc-a".to_owned()),
                ("i131-b".to_owned(), "doc-b".to_owned()),
            ]),
            acknowledged: Default::default(),
            invalidated: None,
            sender: Some(sender),
        });
        BARRIER_ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
        document_ready("i131-a", "doc-a-refreshed");
        let (saved, message) = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(!saved);
        assert!(message.contains("刷新"));
        assert!(validate_barrier().unwrap_err().message.contains("刷新"));
        release_barrier();
        document_closed("i131-a");
        document_closed("i131-b");
        if !native::BRIDGE_READY.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(flush_drafts().is_ok());
        }
    }
}
