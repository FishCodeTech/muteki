//! Managed Web/CLI release install and rollback.
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const REPOSITORY: &str = "FishCodeTech/muteki";
static RUNNING: AtomicBool = AtomicBool::new(false);
struct Running;
impl Running {
    fn acquire() -> Option<Self> {
        RUNNING.compare_exchange(false,true,Ordering::SeqCst,Ordering::SeqCst).ok().map(|_|Self)
    }
}
impl Drop for Running {
    fn drop(&mut self) { RUNNING.store(false,Ordering::SeqCst); }
}


#[derive(Clone)]
struct Release {
    version: String,
    git_ref: String,
    published_at: String,
    asset_url: String,
    asset_size: u64,
    asset_sha256: String,
    source_url: String,
    images: Value,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
fn release_sha256(value: &str) -> Option<String> {
    let value = value.strip_prefix("sha256:").unwrap_or(value);
    (value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| value.to_ascii_lowercase())
}
fn expand_root(path:PathBuf)->PathBuf{
    let path=if path==Path::new("~"){std::env::var_os("HOME").map(PathBuf::from).unwrap_or(path)}else if let Ok(suffix)=path.strip_prefix("~/"){std::env::var_os("HOME").map(|home|PathBuf::from(home).join(suffix)).unwrap_or(path)}else{path};
    std::path::absolute(&path).unwrap_or(path)
}
fn compose_file(root:&Path)->PathBuf{let file=std::env::var_os("MUTEKI_COMPOSE_FILE").map(PathBuf::from).unwrap_or_else(||root.join("docker-compose.release.yml"));std::path::absolute(&file).unwrap_or(file)}

fn installation_root() -> PathBuf {
    if let Some(root)=CLI_CONFIG.get().and_then(|config|config.root.clone()){return expand_root(root);}
    if let Ok(value) = std::env::var("MUTEKI_INSTALL_ROOT") {
        if !value.trim().is_empty() {
            return expand_root(PathBuf::from(value));
        }
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".local/share/muteki")
}
fn read_json(path: &Path) -> Value {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}
fn atomic_json(path: &Path, value: &Value) -> Result<(), String> {
    let parent = path.parent().ok_or("JSON file has no directory")?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        uuid::Uuid::new_v4().simple()
    ));
    let mut options=std::fs::OpenOptions::new();options.write(true).create_new(true);
    #[cfg(unix)]{use std::os::unix::fs::OpenOptionsExt;options.mode(0o600);}
    let result=(||->Result<(),String>{use std::io::Write;let mut file=options.open(&temporary).map_err(|e|e.to_string())?;file.write_all(&serde_json::to_vec_pretty(value).map_err(|e|e.to_string())?).and_then(|_|file.sync_all()).map_err(|e|e.to_string())?;std::fs::rename(&temporary,path).map_err(|e|e.to_string())?;std::fs::File::open(parent).and_then(|file|file.sync_all()).map_err(|e|e.to_string())})();
    if result.is_err(){let _=std::fs::remove_file(&temporary);}result
}
fn metadata(root: &Path) -> Value {
    read_json(&root.join("install.json"))
}
fn current_version(root: &Path) -> String {
    metadata(root)["current_version"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or(env!("CARGO_PKG_VERSION"))
        .trim_start_matches('v')
        .to_owned()
}
fn version(value: &str) -> Result<semver::Version, String> {
    semver::Version::parse(value.trim().trim_start_matches('v'))
        .map_err(|_| format!("发布版本格式无效：{value}"))
}
fn platform() -> Result<(&'static str, &'static str), String> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        other => return Err(format!("该系统暂无 Web 发布包：{other}")),
    };
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => return Err(format!("该架构暂无 Web 发布包：{other}")),
    };
    Ok((os, arch))
}
fn asset_name(version: &str) -> Result<String, String> {
    let (os, arch) = platform()?;
    Ok(format!("Muteki-Web-{version}-{os}-{arch}.tar.gz"))
}
fn github_token() -> Option<String> {
    ["MUTEKI_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"]
        .into_iter()
        .find_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
}
fn release_url(target: Option<&str>) -> Result<String, String> {
    if let Some(url)=CLI_CONFIG.get().and_then(|config|config.manifest_url.as_ref()){return Ok(url.replace("{version}",target.unwrap_or("latest").trim_start_matches('v')));}
    if let Ok(override_url) = std::env::var("MUTEKI_UPDATE_MANIFEST_URL") {
        if !override_url.trim().is_empty() {
            return Ok(override_url.replace(
                "{version}",
                target.unwrap_or("latest").trim_start_matches('v'),
            ));
        }
    }
    if let Ok(override_url) = std::env::var("MUTEKI_RELEASE_API_URL") {
        if !override_url.trim().is_empty() {
            return Ok(override_url.replace(
                "{version}",
                target.unwrap_or("latest").trim_start_matches('v'),
            ));
        }
    }
    let repository =
        std::env::var("MUTEKI_RELEASE_REPOSITORY").unwrap_or_else(|_| REPOSITORY.to_owned());
    Ok(if let Some(target) = target {
        format!(
            "https://api.github.com/repos/{repository}/releases/tags/v{}",
            target.trim_start_matches('v')
        )
    } else {
        format!("https://api.github.com/repos/{repository}/releases/latest")
    })
}
fn client() -> Result<reqwest::Client, String> {
    crate::network_proxy::http_client_builder()
        .connect_timeout(Duration::from_secs(45))
        .user_agent("muteki-updater/0.4")
        .build()
        .map_err(|error| error.to_string())
}
async fn fetch_release(target: Option<&str>) -> Result<Release, String> {fetch_release_mode(target,false).await}
async fn fetch_release_mode(target:Option<&str>,compose:bool)->Result<Release,String>{
    let source_url = release_url(target)?;
    let client = client()?;
    let mut request = client
        .get(&source_url)
        .header("Accept", "application/vnd.github+json, application/json")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if source_url.starts_with("https://api.github.com/") {
        if let Some(token) = github_token() {
            request = request.bearer_auth(token);
        }
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("无法读取发布清单：{error}"))?;
    if !response.status().is_success() {
        return Err(format!("无法读取发布清单：HTTP {}", response.status()));
    }
    let content = response.bytes().await.map_err(|error| error.to_string())?;
    if content.len() > 1024 * 1024 {
        return Err("发布清单超过 1 MiB".to_owned());
    }
    let payload: Value =
        serde_json::from_slice(&content).map_err(|_| "发布清单不是有效 JSON".to_owned())?;
    let body = payload.as_object().ok_or("发布清单必须是 JSON 对象")?;
    let explicit_manifest = body.contains_key("schema_version");
    let release = if explicit_manifest {
        if payload["schema_version"] != 1 {
            return Err("发布清单版本不受支持".to_owned());
        }
        let version_text = payload["version"]
            .as_str()
            .unwrap_or("")
            .trim_start_matches('v');
        version(version_text)?;
        let raw_url = payload["artifact"]["url"].as_str().unwrap_or("");
        let asset_size = payload["artifact"]["size"].as_u64().unwrap_or(0);
        let asset_sha256 = if compose{String::new()}else{release_sha256(payload["artifact"]["sha256"].as_str().unwrap_or("")).ok_or("发布清单缺少有效的应用包 SHA-256")?};
        if !compose&&(raw_url.is_empty() || asset_size == 0) {
            return Err("发布清单缺少应用包 URL 或大小".to_owned());
        }
        let resolved = reqwest::Url::parse(&source_url)
            .and_then(|base| base.join(raw_url))
            .map_err(|error| error.to_string())?;
        Release {
            version: version_text.to_owned(),
            git_ref: payload["git_ref"]
                .as_str()
                .or(payload["commit"].as_str())
                .unwrap_or("")
                .to_owned(),
            published_at: payload["published_at"].as_str().unwrap_or("").to_owned(),
            asset_url: resolved.to_string(),
            asset_size,
            asset_sha256,
            source_url: source_url.clone(),
            images: payload.get("images").cloned().unwrap_or_else(|| json!({})),
        }
    } else {
        let tag = payload["tag_name"].as_str().unwrap_or("");
        let version_text = tag.trim_start_matches('v');
        version(version_text)?;
        if payload["draft"] == true || payload["prerelease"] == true {
            return Err("GitHub Release 不是稳定发布版本".to_owned());
        }
        let expected = asset_name(version_text)?;
        let asset = payload["assets"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|asset| asset["name"] == expected);
        let empty=json!({});let asset=if compose{asset.unwrap_or(&empty)}else{asset.ok_or_else(||format!("该 GitHub Release 缺少 {expected}"))?};
        let asset_url = asset["url"]
            .as_str()
            .or(asset["browser_download_url"].as_str())
            .unwrap_or("");
        let asset_size = asset["size"].as_u64().unwrap_or(0);
        let asset_sha256=if compose{String::new()}else{release_sha256(asset["digest"].as_str().unwrap_or("")).ok_or("发布资产缺少有效的 SHA-256")?};
        if !compose&&(asset_url.is_empty() || asset_size == 0) {
            return Err("发布资产缺少 URL 或大小".to_owned());
        }
        Release {
            version: version_text.to_owned(),
            git_ref: payload["target_commitish"]
                .as_str()
                .unwrap_or("")
                .to_owned(),
            published_at: payload["published_at"].as_str().unwrap_or("").to_owned(),
            asset_url: asset_url.to_owned(),
            asset_size,
            asset_sha256,
            source_url: source_url.clone(),
            images: json!({}),
        }
    };
    if target.is_some_and(|target| target.trim_start_matches('v') != release.version) {
        return Err(format!(
            "发布清单版本不匹配：期望 {}，实际 {}",
            target.unwrap_or(""),
            release.version
        ));
    }
    Ok(release)
}

fn write_state(root: &Path, fields: Value) -> Result<Value, String> {
    let mut state = read_json(&root.join("update-state.json"));
    if !state.is_object() {
        state = json!({});
    }
    state["schema_version"] = json!(1);
    if state["status"].is_null() {
        state["status"] = json!("idle");
    }
    state["current_version"] = json!(current_version(root));
    state["updated_at"] = json!(now());
    for (key, value) in fields.as_object().into_iter().flatten() {
        state[key] = value.clone();
    }
    atomic_json(&root.join("update-state.json"), &state)?;
    Ok(state)
}
fn deployment(root: &Path) -> String {
    std::env::var("MUTEKI_DEPLOYMENT_MODE")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| metadata(root)["deployment"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "local".to_owned())
}
pub(super) fn status() -> Value {
    let root = installation_root();
    let state = read_json(&root.join("update-state.json"));
    let installed = root.join("install.json").is_file() && root.join("current").exists();
    let metadata = metadata(&root);
    let selected = current_version(&root);
    let active = env!("CARGO_PKG_VERSION");
    let deployment = deployment(&root);
    json!({"status":state["status"].as_str().unwrap_or("idle"),
        "running":RUNNING.load(Ordering::SeqCst),
        "current_version":selected,"active_version":active,
        "latest_version":state["latest_version"],"available":state["available"] == true,
        "progress":state["progress"],"message":state["message"],"error":state["error"],
        "checked_at":state["checked_at"],"updated_at":state["updated_at"],
        "restart_required":state["restart_required"] == true && active != selected,
        "install_kind":if deployment == "compose" {"compose"} else if installed {"managed"} else {"source"},
        "install_root":root.to_string_lossy(),"channel":metadata["channel"].as_str().unwrap_or("stable"),
        "previous_version":metadata["previous_version"],"deployment":deployment,"compose_operation":read_json(&root.join("compose-operation.json"))["phase"]})
}
pub(super) async fn check(target: Option<&str>) -> Result<Value, String> {
    if RUNNING.load(Ordering::SeqCst) {
        return Ok(status());
    }
    let root = installation_root();
    write_state(
        &root,
        json!({"status":"checking","message":"正在读取发布清单",
        "error":null}),
    )?;
    let result = fetch_release(target).await;
    match result {
        Ok(release) => {
            let current = current_version(&root);
            let installed = root.join("install.json").is_file() && root.join("current").exists();
            let available = if target.is_some() {
                release.version != current || !installed
            } else {
                version(&release.version)?
                    .cmp_precedence(&version(&current)?)
                    .is_gt()
                    || !installed
            };
            write_state(
                &root,
                json!({"status":if available {"available"} else {"current"},
                "current_version":current,"latest_version":release.version,
                "available":available,"checked_at":now(),
                "message":if available {format!("可升级到 {}",release.version)}
                    else {"当前已是最新版本".to_owned()},
                "error":null,"manifest":{"version":release.version,
                    "commit":release.git_ref,"published_at":release.published_at,
                    "images":release.images,"source_url":release.source_url}}),
            )?;
            Ok(status())
        }
        Err(error) => {
            let _ = write_state(
                &root,
                json!({"status":"error","available":false,
                "error":error,"message":"检查更新失败"}),
            );
            Err(error)
        }
    }
}
pub(super) async fn start(target: Option<String>, force: bool) -> Result<Value, String> {
    let root = installation_root();
    if deployment(&root) == "compose" {
        return Err("容器部署请在宿主机执行 muteki upgrade --compose".to_owned());
    }
    let Some(running) = Running::acquire() else { return Ok(status()); };
    tokio::spawn(async move {
        let _running = running;
        if let Err(error) = upgrade(&root, target.as_deref(), force).await {
            let _ = write_state(
                &root,
                json!({"status":"error","progress":null,
                "error":error,"message":"升级失败"}),
            );
        }
    });
    tokio::task::yield_now().await;
    Ok(status())
}
pub(super) async fn rollback() -> Result<Value, String> {
    let root = installation_root();
    if deployment(&root) == "compose" {
        return Err("容器部署请在宿主机执行 muteki rollback --compose".to_owned());
    }
    let Some(running) = Running::acquire() else { return Ok(status()); };
    tokio::task::spawn_blocking(move || { let _running = running; rollback_inner(&root) })
        .await
        .map_err(|error| error.to_string())??;
    Ok(status())
}

struct UpdateLock {
    path: PathBuf,
    file: std::fs::File,
    remove_on_drop: bool,
}

impl UpdateLock {
    #[cfg(unix)]
    fn matches_path(&self) -> std::io::Result<bool> {
        use std::os::unix::fs::MetadataExt;

        let held = self.file.metadata()?;
        let linked = std::fs::symlink_metadata(&self.path)?;
        Ok((held.dev(), held.ino()) == (linked.dev(), linked.ino()))
    }

    fn remove(&mut self) -> std::io::Result<()> {
        #[cfg(unix)]
        if !self.matches_path()? {
            return Err(std::io::Error::other("升级锁已被替换"));
        }
        std::fs::remove_file(&self.path)?;
        self.remove_on_drop = false;
        Ok(())
    }
}

impl Drop for UpdateLock {
    fn drop(&mut self) {
        if self.remove_on_drop && let Err(error) = self.remove() {
            tracing::error!(path = %self.path.display(), %error, "Failed to remove the legacy update lock");
        }
    }
}

fn uses_legacy_update_lock(version: &str) -> bool {
    version.trim_start_matches('v').starts_with("0.4.")
}

fn update_lock(root: &Path) -> Result<UpdateLock, String> {
    use std::io::ErrorKind;

    std::fs::create_dir_all(root).map_err(|error| error.to_string())?;
    let path = root.join(".update.lock");
    let busy = "已有升级任务正在执行";
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    let (file, created) = match options.open(&path) {
        Ok(file) => (file, true),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            if uses_legacy_update_lock(&current_version(root)) {
                return Err(busy.to_owned());
            }
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .map_err(|error| error.to_string())?;
            (file, false)
        }
        Err(error) => return Err(error.to_string()),
    };
    fs2::FileExt::try_lock_exclusive(&file).map_err(|error| match error.kind() {
        ErrorKind::WouldBlock => busy.to_owned(),
        _ => error.to_string(),
    })?;
    let legacy = uses_legacy_update_lock(&current_version(root));
    let lock = UpdateLock { path, file, remove_on_drop: legacy && created };
    if (legacy && !created) || lock.file.metadata().map_err(|error| error.to_string())?.len() != 0 {
        return Err(busy.to_owned());
    }
    #[cfg(unix)]
    if !lock.matches_path().map_err(|error| error.to_string())? {
        return Err(busy.to_owned());
    }
    Ok(lock)
}
async fn download(root: &Path, release: &Release, path: &Path) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let client = client()?;
    let mut request = client
        .get(&release.asset_url)
        .header("Accept", "application/octet-stream")
        .header("X-GitHub-Api-Version", "2022-11-28");
    if release.asset_url.starts_with("https://api.github.com/") {
        if let Some(token) = github_token() {
            request = request.bearer_auth(token);
        }
    }
    let mut response = request
        .send()
        .await
        .map_err(|error| format!("应用包下载失败：{error}"))?;
    if !response.status().is_success() {
        return Err(format!("应用包下载失败：HTTP {}", response.status()));
    }
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|error| format!("应用包下载失败：{error}"))?;
    let mut received = 0u64;
    let mut sha256 = Sha256::new();
    loop {
        let chunk = tokio::time::timeout(Duration::from_secs(45), response.chunk())
            .await
            .map_err(|_| "应用包下载超时".to_owned())?
            .map_err(|error| format!("应用包下载失败：{error}"))?;
        let Some(chunk) = chunk else {
            break;
        };
        received += chunk.len() as u64;
        if received > release.asset_size {
            return Err("下载内容超过发布清单声明的大小".to_owned());
        }
        file.write_all(&chunk)
            .await
            .map_err(|error| error.to_string())?;
        sha256.update(&chunk);
        let progress = (received.saturating_mul(70) / release.asset_size).clamp(1, 70);
        let _ = write_state(
            root,
            json!({"status":"downloading","progress":progress,
            "message":format!("正在下载 {}",release.version)}),
        );
    }
    file.flush().await.map_err(|error| error.to_string())?;
    if received != release.asset_size {
        return Err(format!(
            "应用包大小不匹配：期望 {}，实际 {received}",
            release.asset_size
        ));
    }
    if format!("{:x}", sha256.finalize()) != release.asset_sha256 {
        return Err("下载的应用包 SHA-256 与发布清单不匹配".to_owned());
    }
    Ok(())
}
fn extract(path: &Path, staging: &Path, release: &Release) -> Result<PathBuf, String> {
    std::fs::create_dir(staging).map_err(|error| format!("无法解压应用包：{error}"))?;
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let gzip = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(gzip);
    for entry in archive
        .entries()
        .map_err(|error| format!("无法解压应用包：{error}"))?
    {
        let mut entry = entry.map_err(|error| format!("无法解压应用包：{error}"))?;
        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() {
            return Err("应用包包含不允许的链接或设备文件".to_owned());
        }
        let member = entry.path().map_err(|error| error.to_string())?;
        if member.is_absolute()
            || member.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir | std::path::Component::RootDir
                )
            })
        {
            return Err(format!("应用包包含越界路径：{}", member.display()));
        }
        entry
            .unpack_in(staging)
            .map_err(|error| format!("无法解压应用包：{error}"))?;
    }
    let candidate = staging.join(format!("muteki-web-{}", release.version));
    let manifest = read_json(&candidate.join("release.json"));
    let (os, arch) = platform()?;
    if manifest["schema_version"] != 1
        || manifest["version"] != release.version
        || manifest["platform"] != os
        || manifest["arch"] != arch
        || !manifest["git_ref"].is_string()
    {
        return Err("应用包 release.json 与发布版本或平台不匹配".to_owned());
    }
    for path in [
        "bin/muteki-server",
        "bin/node",
        "frontend/server.js",
        "launcher.mjs",
    ] {
        if !candidate.join(path).is_file() {
            return Err(format!("应用包缺少 {path}"));
        }
    }
    for path in ["frontend/public", "frontend/.next/static"] {
        if !candidate.join(path).is_dir() {
            return Err(format!("应用包缺少 {path}"));
        }
    }
    Ok(candidate)
}
async fn candidate_starts(candidate: &Path) -> Result<(), String> {
    let mut command = crate::host_process::command(candidate.join("bin/muteki-server"));
    command
        .arg("--version")
        .current_dir(candidate)
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(60), command.output())
        .await
        .map_err(|_| "新版本启动检查超时".to_owned())?
        .map_err(|error| format!("新版本启动检查失败：{error}"))?;
    if !output.status.success() {
        return Err(format!(
            "新版本启动检查失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn install_launcher(root: &Path) -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let bin = std::env::var_os("MUTEKI_BIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/bin"));
    std::fs::create_dir_all(&bin).map_err(|error| error.to_string())?;
    let destination = bin.join("muteki");
    let temporary = bin.join(format!(".muteki.{}", uuid::Uuid::new_v4().simple()));
    let node = shell_quote(&root.join("current/bin/node").to_string_lossy());
    let launcher = shell_quote(&root.join("current/launcher.mjs").to_string_lossy());
    std::fs::write(
        &temporary,
        format!("#!/bin/sh\nexec {node} {launcher} \"$@\"\n"),
    )
    .map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| error.to_string())?;
    }
    std::fs::rename(&temporary, &destination).map_err(|error| error.to_string())?;
    Ok(destination)
}
fn set_link(root: &Path, name: &str, target: &Path) -> Result<(), String> {
    let relative = target
        .strip_prefix(root)
        .map_err(|error| error.to_string())?;
    let temporary = root.join(format!(".{name}.{}", uuid::Uuid::new_v4().simple()));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(relative, &temporary).map_err(|error| error.to_string())?;
    }
    #[cfg(not(unix))]
    {
        return Err("该平台不支持托管 Web 启动器".to_owned());
    }
    std::fs::rename(&temporary, root.join(name)).map_err(|error| error.to_string())
}
fn initial_metadata(root: &Path) -> Result<Value, String> {
    let mut value = metadata(root);
    if !value.is_object() {
        value = json!({});
    }
    let data = root.join("data");
    let state = value["state_root"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| data.join("state"));
    let sessions = value["sessions_root"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| data.join("sessions"));
    let control = value["control_root"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| state.join("control"));
    let env_file = value["env_file"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("config/.env"));
    for path in [&data, &state, &sessions, &control] {
        std::fs::create_dir_all(path).map_err(|error| error.to_string())?;
    }
    std::fs::create_dir_all(env_file.parent().ok_or("env file has no directory")?)
        .map_err(|error| error.to_string())?;
    value["schema_version"] = json!(1);
    if value["channel"].is_null() {
        value["channel"] = json!("stable");
    }
    value["deployment"] = json!(deployment(root));
    if value["data_root"].is_null() {
        value["data_root"] = json!(data);
    }
    value["sessions_root"] = json!(sessions);
    value["state_root"] = json!(state);
    value["control_root"] = json!(control);
    value["env_file"] = json!(env_file);
    if value["installed_at"].is_null() {
        value["installed_at"] = json!(now());
    }
    Ok(value)
}
fn switch_release(root: &Path, candidate: &Path, release: &Release) -> Result<PathBuf, String> {
    let releases = root.join("releases");
    let release_dir = releases.join(&release.version);
    let old_target = std::fs::canonicalize(root.join("current")).ok();
    let prior_previous = std::fs::canonicalize(root.join("previous")).ok();
    let release_meta = json!({"schema_version":1,"version":release.version,
        "git_ref":release.git_ref,
        "built_at":if release.published_at.is_empty(){now()}else{release.published_at.clone()},
        "manifest_url":release.source_url,"images":release.images});
    atomic_json(&candidate.join(".muteki-release.json"), &release_meta)?;
    let backup = if release_dir.exists() {
        let backup = releases.join(format!(
            ".{}.replaced-{}",
            release.version,
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::rename(&release_dir, &backup).map_err(|error| error.to_string())?;
        Some(backup)
    } else {
        None
    };
    if let Err(error) = std::fs::rename(candidate, &release_dir) {
        if let Some(backup) = backup.as_ref() {
            let _ = std::fs::rename(backup, &release_dir);
        }
        return Err(error.to_string());
    }
    let result = (|| -> Result<PathBuf, String> {
        let launcher = install_launcher(root)?;
        if let Some(old) = old_target.as_ref().filter(|old| *old != &release_dir) {
            set_link(root, "previous", old)?;
        }
        set_link(root, "current", &release_dir)?;
        let mut metadata = initial_metadata(root)?;
        metadata["current_version"] = json!(release.version);
        if old_target.as_ref().is_some_and(|old| old != &release_dir) {
            metadata["previous_version"] = json!(current_version_before(root, old_target.as_ref()));
        }
        metadata["updated_at"] = json!(now());
        metadata["release"] = release_meta;
        atomic_json(&root.join("install.json"), &metadata)?;
        Ok(launcher)
    })();
    if result.is_err() {
        match old_target.as_ref() {
            Some(old) => {
                let _ = set_link(root, "current", old);
            }
            None => {
                let _ = std::fs::remove_file(root.join("current"));
            }
        }
        match prior_previous.as_ref() {
            Some(previous) => {
                let _ = set_link(root, "previous", previous);
            }
            None => {
                let _ = std::fs::remove_file(root.join("previous"));
            }
        }
        if let Some(backup) = backup.as_ref() {
            let failed = releases.join(format!(
                ".{}.failed-{}",
                release.version,
                uuid::Uuid::new_v4().simple()
            ));
            let _ = std::fs::rename(&release_dir, failed);
            let _ = std::fs::rename(backup, &release_dir);
        } else {
            let _ = std::fs::remove_dir_all(&release_dir);
        }
    } else if let Some(backup) = backup {
        let _ = std::fs::remove_dir_all(backup);
    }
    result
}
fn current_version_before(root: &Path, old_target: Option<&PathBuf>) -> String {
    let existing = metadata(root);
    existing["current_version"]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            old_target
                .and_then(|path| path.file_name())
                .map(|name| name.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned())
}
async fn upgrade(root: &Path, target: Option<&str>, force: bool) -> Result<Value, String> {
    std::fs::create_dir_all(root).map_err(|error| error.to_string())?;
    let root = std::fs::canonicalize(root).map_err(|error| error.to_string())?;
    let _lock = update_lock(&root)?;
    let release = fetch_release(target).await?;
    let current = current_version(&root);
    let installed = root.join("install.json").is_file() && root.join("current").exists();
    if installed && current == release.version && !force {
        return write_state(
            &root,
            json!({"status":"current","latest_version":release.version,
            "available":false,"progress":100,"message":"当前已是指定版本","error":null}),
        );
    }
    write_state(
        &root,
        json!({"status":"downloading","latest_version":release.version,
        "available":true,"progress":1,
        "message":format!("准备升级到 {}",release.version),"error":null}),
    )?;
    let releases = root.join("releases");
    let cache = root.join("cache");
    std::fs::create_dir_all(&releases).map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&cache).map_err(|error| error.to_string())?;
    let nonce = uuid::Uuid::new_v4().simple();
    let archive = cache.join(format!("muteki-{nonce}.tar.gz"));
    let staging = releases.join(format!(".{}.staging-{nonce}", release.version));
    let result = async {
        download(&root, &release, &archive).await?;
        write_state(
            &root,
            json!({"status":"preparing","progress":78,
            "message":"正在安装运行依赖"}),
        )?;
        let archive_for_extract = archive.clone();
        let staging_for_extract = staging.clone();
        let release_for_extract = release.clone();
        let candidate = tokio::task::spawn_blocking(move || {
            extract(
                &archive_for_extract,
                &staging_for_extract,
                &release_for_extract,
            )
        })
        .await
        .map_err(|error| error.to_string())??;
        candidate_starts(&candidate).await?;
        write_state(
            &root,
            json!({"status":"switching","progress":92,
            "message":"正在切换应用版本"}),
        )?;
        let root_for_switch = root.clone();
        let release_for_switch = release.clone();
        let launcher = tokio::task::spawn_blocking(move || {
            switch_release(&root_for_switch, &candidate, &release_for_switch)
        })
        .await
        .map_err(|error| error.to_string())??;
        write_state(
            &root,
            json!({"status":"installed",
            "current_version":release.version,"latest_version":release.version,
            "available":false,"progress":100,"restart_required":true,
            "message":format!("{} 已安装，重启服务后生效",release.version),
            "error":null,"launcher":launcher,
            "manifest":{"version":release.version,"commit":release.git_ref,
                "published_at":release.published_at,"images":release.images,
                "source_url":release.source_url}}),
        )
    }
    .await;
    let _ = std::fs::remove_file(archive);
    let _ = std::fs::remove_dir_all(staging);
    result
}
fn rollback_inner(root: &Path) -> Result<(), String> {
    let root = std::fs::canonicalize(root).map_err(|error| error.to_string())?;
    let root = root.as_path();
    let mut lock = update_lock(root)?;
    let current_link = root.join("current");
    let previous_link = root.join("previous");
    let current =
        std::fs::canonicalize(&current_link).map_err(|_| "没有可回滚的上一版本".to_owned())?;
    let previous =
        std::fs::canonicalize(&previous_link).map_err(|_| "没有可回滚的上一版本".to_owned())?;
    if !previous.is_dir() {
        return Err("上一版本目录不存在".to_owned());
    }
    let current_version = current_version(root);
    let release = read_json(&previous.join(".muteki-release.json"));
    let previous_version = release["version"]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            previous
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .ok_or("上一版本目录缺少版本")?;
    lock.remove_on_drop |= uses_legacy_update_lock(&previous_version);
    set_link(root, "current", &previous)?;
    set_link(root, "previous", &current)?;
    let mut metadata = initial_metadata(root)?;
    metadata["current_version"] = json!(previous_version);
    metadata["previous_version"] = json!(current_version);
    metadata["updated_at"] = json!(now());
    metadata["release"] = release;
    atomic_json(&root.join("install.json"), &metadata)?;
    write_state(
        root,
        json!({"status":"rolled_back",
        "current_version":previous_version,"latest_version":previous_version,
        "available":false,"progress":100,"restart_required":true,"error":null,
        "message":format!("已回滚到 {previous_version}，重启服务后生效")}),
    )?;
    if lock.remove_on_drop {
        lock.remove().map_err(|error| format!("无法移除回滚后的升级锁：{error}"))?;
    }
    Ok(())
}


struct CliConfig{root:Option<PathBuf>,manifest_url:Option<String>}
static CLI_CONFIG:std::sync::OnceLock<CliConfig>=std::sync::OnceLock::new();
#[derive(clap::Parser)]
#[command(name="muteki",about="Muteki 安装与升级管理")]
struct ManagementCli{
    #[command(subcommand)]command:ManagementCommand,
    #[arg(long,global=true)]json:bool,
    #[arg(long,global=true)]root:Option<PathBuf>,
    #[arg(long,global=true)]manifest_url:Option<String>,
}
#[derive(clap::Subcommand)]enum ManagementCommand{
    Version,Status,
    Install{target:Option<String>,#[arg(long)]force:bool},
    Upgrade{target:Option<String>,#[arg(long)]check:bool,#[arg(long)]force:bool,#[arg(long)]compose:bool},
    Rollback{#[arg(long)]compose:bool},
}
pub async fn cli_entry()->Option<i32>{
    use clap::Parser;
    let action=std::env::args().nth(1)?;if !matches!(action.as_str(),"version"|"status"|"install"|"upgrade"|"rollback"){return None}
    let options=match ManagementCli::try_parse(){Ok(options)=>options,Err(error)=>{let code=error.exit_code();let _=error.print();return Some(code)}};
    let _=CLI_CONFIG.set(CliConfig{root:options.root,manifest_url:options.manifest_url});let root=installation_root();
    let result=async{match options.command{
        ManagementCommand::Version=>Ok(json!({"version":env!("CARGO_PKG_VERSION"),"install_kind":status()["install_kind"],"root":root})),
        ManagementCommand::Status=>Ok(status()),
        ManagementCommand::Install{target,force}=>{if deployment(&root)=="compose"{return Err("容器部署请在宿主机执行 muteki upgrade --compose".into())}upgrade(&root,target.as_deref(),force).await},
        ManagementCommand::Upgrade{target,check:only_check,force,compose}=>{
            if compose{if only_check{let release=fetch_release_mode(target.as_deref(),true).await?;Ok(json!({"current_version":current_version(&root),"latest_version":release.version,"available":release.version!=current_version(&root),"images":release.images}))}else{compose_change(&root,target.as_deref(),false).await}}
            else if only_check{check(target.as_deref()).await}else{if deployment(&root)=="compose"{return Err("容器部署请在宿主机执行 muteki upgrade --compose".into())}upgrade(&root,target.as_deref(),force).await}
        },
        ManagementCommand::Rollback{compose}=>if compose{compose_change(&root,None,true).await}else{if deployment(&root)=="compose"{return Err("容器部署请在宿主机执行 muteki rollback --compose".into())}rollback_inner(&root)?;Ok(status())},
    }}.await;
    match result{Ok(payload)=>{if options.json{println!("{}",serde_json::to_string_pretty(&payload).unwrap_or_else(|_|payload.to_string()));}else{println!("{}",payload["message"].as_str().map(str::to_owned).unwrap_or_else(||payload.to_string()));}Some(0)},Err(error)=>{let _=write_state(&root,json!({"status":"error","error":error,"message":"安装升级操作失败"}));if options.json{println!("{}",json!({"ok":false,"error":error}));}else{eprintln!("安装升级操作失败：{error}");}Some(1)}}
}
async fn compose_apply(root:&Path,selected_version:&str,images:&Value)->Result<(),String>{
    compose_cutover(root,true).await?;
    compose_steps(root,selected_version,images,&[&["pull"],&["up","-d","--remove-orphans","--wait"]]).await
}
async fn docker_text(args:&[&str])->Result<String,String>{
    let mut command=tokio::process::Command::from(agent::supervised_command("docker"));
    if args.first()==Some(&"compose"){if let Some(password)=std::env::var_os("MUTEKI_WEB_PASSWORD"){command.env("MUTEKI_WEB_PASSWORD",password);}}
    command.args(args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
    let output=tokio::time::timeout(Duration::from_secs(60),command.output()).await.map_err(|_|"Docker 查询超时".to_owned())?.map_err(|error|error.to_string())?;
    if !output.status.success(){return Err(format!("Docker 查询失败：{}",String::from_utf8_lossy(&output.stderr).trim()))}
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
/// A pre-split Web container embeds the only Runner for its state directory and holds the Docker Socket.
/// Its Workers reverse-connect to that Web, so they cannot move to the new Runner service. Refuse while
/// any is alive, then stop the old Web so the new Runner is the only controller of the state directory.
/// The refusal also runs before the recovery journal exists, because compensating a refused change would
/// otherwise tear down the stack that the refusal is meant to protect.
async fn compose_cutover(root:&Path,stop:bool)->Result<(),String>{
    let file=compose_file(root);let file=file.to_string_lossy().into_owned();
    let ids=docker_text(&["compose","-f",&file,"ps","-q","web"]).await?;
    for id in ids.lines().filter(|line|!line.trim().is_empty()){
        let mounts=docker_text(&["inspect",id,"--format","{{range .Mounts}}{{.Destination}}{{\"\\n\"}}{{end}}"]).await?;
        let embedded=mounts.lines().any(|mount|mount=="/var/run/docker.sock");
        let environment=docker_text(&["inspect",id,"--format","{{range .Config.Env}}{{println .}}{{end}}"]).await?;
        let state_dir=environment.lines().find_map(|line|line.strip_prefix("MUTEKI_STATE_DIR=")).filter(|value|!value.is_empty())
            .ok_or("旧版 Web 容器缺少 MUTEKI_STATE_DIR，无法确认排空状态")?;
        let workers=docker_text(&["ps","-q","--filter",&format!("label=muteki.state_dir={state_dir}")]).await?;
        if !workers.is_empty(){
            return Err(if embedded {
                format!("旧版 Web 内嵌 Runner 仍在管理 {} 个运行中的 Worker 容器；请先停止或等待这些 Run 结束后再迁移到独立 Runner。",workers.lines().count())
            } else {
                format!("Runner 仍在管理 {} 个运行中的 Worker 容器；请先排空这些 Run，再升级或回滚执行节点。",workers.lines().count())
            })
        }
        if embedded&&stop{docker_text(&["stop",id]).await?;}
    }
    Ok(())
}
async fn compose_steps(root:&Path,selected_version:&str,images:&Value,steps:&[&[&str]])->Result<(),String>{
    version(selected_version)?;
    let file=compose_file(root);
    if !file.is_file(){if std::env::var_os("MUTEKI_COMPOSE_FILE").is_some(){return Err(format!("版本化编排文件不存在：{}",file.display()));}std::fs::create_dir_all(root).map_err(|error|error.to_string())?;std::fs::write(&file,include_str!("../../../../docker-compose.release.yml")).map_err(|error|error.to_string())?;}
    let registry=std::env::var("MUTEKI_IMAGE_REGISTRY").unwrap_or_else(|_|"ghcr.io/fishcodetech".into());
    let image=|key:&str,name:&str|images[key].as_str().or_else(||images[name].as_str()).map(str::to_owned).unwrap_or_else(||format!("{registry}/{name}:v{}",selected_version.trim_start_matches('v')));
    let worker_image=image("worker","muteki-worker");
    for args in steps{
        let mut command=tokio::process::Command::from(agent::supervised_command("docker"));command.args(["compose","-f"]).arg(&file).args(*args).current_dir(file.parent().unwrap_or(root)).env("MUTEKI_WEB_IMAGE",image("web","muteki-web")).env("MUTEKI_WORKER_IMAGE",&worker_image).env("MUTEKI_RELEASE_VERSION",format!("v{}",selected_version.trim_start_matches('v'))).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
        if let Some(password)=std::env::var_os("MUTEKI_WEB_PASSWORD"){command.env("MUTEKI_WEB_PASSWORD",password);}
        let mut child=command.spawn().map_err(|error|format!("启动 Docker Compose 失败：{error}"))?;let stdout=child.stdout.take().ok_or("Compose stdout missing")?;let stderr=child.stderr.take().ok_or("Compose stderr missing")?;
        let result=tokio::time::timeout(Duration::from_secs(1800),async{tokio::try_join!(async{child.wait().await.map_err(|error|error.to_string())},compose_tail(stdout),compose_tail(stderr))}).await;
        match result{Ok(Ok((status,_,_)))if status.success()=>{},Ok(Ok((status,out,err)))=>return Err(format!("Docker Compose 失败（{status}）：{}\n{}",String::from_utf8_lossy(&out),String::from_utf8_lossy(&err))),Ok(Err(error))=>{let _=child.kill().await;let _=child.wait().await;return Err(error)},Err(_)=>{let _=child.kill().await;let _=child.wait().await;return Err("Docker Compose 操作超时，已停止所属客户端进程；请检查实际容器状态后重试。".into())}}
    }Ok(())
}
fn compose_configuration(root:&Path)->Result<String,String>{
    let file=compose_file(root);
    let bytes=if file.exists(){std::fs::read(&file).map_err(|e|e.to_string())?}else if std::env::var_os("MUTEKI_COMPOSE_FILE").is_none(){include_bytes!("../../../../docker-compose.release.yml").to_vec()}else{return Err("Compose 文件不存在".into())};
    let mut env=serde_json::Map::new();for name in ["MUTEKI_COMPOSE_FILE","MUTEKI_IMAGE_REGISTRY","COMPOSE_PROJECT_NAME","MUTEKI_COMPOSE_PROJECT","MUTEKI_HOST_DATA_ROOT","MUTEKI_WEB_PASSWORD","MUTEKI_WEB_PORT","MUTEKI_WORKER_NETWORK","DOCKER_HOST","DOCKER_CONTEXT","DOCKER_CONFIG"]{env.insert(name.into(),json!(std::env::var(name).ok()));}
    let dotenv=file.parent().unwrap_or(root).join(".env");let dotenv=if dotenv.exists(){std::fs::read(dotenv).map_err(|e|e.to_string())?}else{Vec::new()};
    Ok(format!("{:x}",Sha256::digest(json!([file,format!("{:x}",Sha256::digest(bytes)),format!("{:x}",Sha256::digest(dotenv)),env]).to_string().as_bytes())))
}
async fn compose_daemon()->Result<String,String>{
    let mut command=tokio::process::Command::from(agent::supervised_command("docker"));command.args(["info","--format","{{.ID}}"]);command.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
    let mut child=command.spawn().map_err(|e|e.to_string())?;let stdout=child.stdout.take().ok_or("Docker identity stdout missing")?;let stderr=child.stderr.take().ok_or("Docker identity stderr missing")?;
    let output=tokio::time::timeout(Duration::from_secs(15),async{tokio::try_join!(async{child.wait().await.map_err(|e|e.to_string())},compose_tail(stdout),compose_tail(stderr))}).await.map_err(|_|"Docker identity discovery timed out".to_owned())??;
    if !output.0.success(){return Err(format!("Docker identity discovery failed: {}",String::from_utf8_lossy(&output.2)))}let id=String::from_utf8(output.1).map_err(|e|e.to_string())?.trim().to_owned();if id.is_empty(){return Err("Docker daemon identity is empty".into())}Ok(id)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum ComposePhase { Prepared, Applied, Compensating, Completed, RolledBack }
fn compose_phase(journal: &Value) -> Result<ComposePhase,String> {
    serde_json::from_value(journal["phase"].clone()).map_err(|_|"Compose 恢复日志阶段无效，需检查现有事务".into())
}
fn compose_transition(root: &Path,journal: &mut Value,next: ComposePhase) -> Result<(),String> {
    use ComposePhase::*;
    let previous = compose_phase(journal)?;
    if previous != next && !matches!((previous,next),
        (Prepared,Applied|Compensating) | (Applied,Completed|Compensating) | (Compensating,RolledBack)) {
        return Err(format!("Invalid Compose update transition: {previous:?} -> {next:?}"));
    }
    let mut updated = journal.clone();
    updated["phase"] = serde_json::to_value(next).map_err(|error|error.to_string())?;
    atomic_json(&root.join("compose-operation.json"),&updated)?;
    *journal = updated;
    Ok(())
}
async fn compose_compensate(root:&Path,journal:&mut Value)->Result<(),String>{
    compose_transition(root,journal,ComposePhase::Compensating)?;
    let previous=&journal["before"];let old=previous["current_version"].as_str().unwrap_or("");
    if old.is_empty(){compose_steps(root,journal["selected"].as_str().ok_or("Compose recovery version missing")?,&journal["images"],&[&["down"]]).await?;}else{compose_apply(root,old,&previous["release"]["images"]).await?;}
    atomic_json(&root.join("install.json"),previous)?;
    compose_transition(root,journal,ComposePhase::RolledBack)
}
async fn recover_compose(root:&Path)->Result<(),String>{
    let path=root.join("compose-operation.json");if !path.exists(){return Ok(())}
    let mut journal:Value=serde_json::from_slice(&std::fs::read(&path).map_err(|error|error.to_string())?).map_err(|error|error.to_string())?;
    if journal["version"]!=1{return Err("Compose 恢复日志版本无效".into())}
    let phase=compose_phase(&journal)?;
    if matches!(phase,ComposePhase::Completed|ComposePhase::RolledBack){return Ok(())}
    if journal["configuration"]!=compose_configuration(root)?||journal["daemon_id"]!=compose_daemon().await?{return Err("Compose 恢复目标已改变；请恢复原编排配置和 Docker 服务后重试。".into())}
    if phase==ComposePhase::Compensating{return compose_compensate(root,&mut journal).await}
    if phase==ComposePhase::Prepared || metadata(root)!=journal["after"]{
        let selected=journal["selected"].as_str().ok_or("Compose recovery version missing")?;
        if let Err(error)=compose_apply(root,selected,&journal["images"]).await{journal["error"]=json!(error);compose_compensate(root,&mut journal).await?;return Err(error)}
        compose_transition(root,&mut journal,ComposePhase::Applied)?;
        if let Err(error)=atomic_json(&root.join("install.json"),&journal["after"]){journal["error"]=json!(error);compose_compensate(root,&mut journal).await?;return Err(error)}
    }
    compose_transition(root,&mut journal,ComposePhase::Completed)
}
async fn compose_change(root:&Path,target:Option<&str>,rollback:bool)->Result<Value,String>{
    std::fs::create_dir_all(root).map_err(|error|error.to_string())?;let _lock=update_lock(root)?;
    compose_cutover(root,false).await?;
    recover_compose(root).await?;
    let mut data=initial_metadata(root)?;let before=data.clone();let current=data["current_version"].as_str().filter(|value|!value.is_empty()).unwrap_or("").to_owned();
    let (selected,images,git_ref)=if rollback{let previous=data["previous_version"].as_str().filter(|value|!value.is_empty()).ok_or("没有可回滚的上一容器版本")?.to_owned();(previous,data["previous_release"]["images"].clone(),data["previous_release"]["commit"].clone())}else{let release=fetch_release_mode(target,true).await?;(release.version,release.images,json!(release.git_ref))};
    data["deployment"]=json!("compose");data["previous_version"]=json!(current);data["current_version"]=json!(selected);data["previous_release"]=data["release"].clone();data["release"]=json!({"version":selected,"images":images,"commit":git_ref});data["updated_at"]=json!(now());
    let journal=json!({"version":1,"id":uuid::Uuid::new_v4().to_string(),"phase":"prepared","before":before,"after":data,"selected":selected,"images":images,"configuration":compose_configuration(root)?,"daemon_id":compose_daemon().await?,"created_at":now()});
    atomic_json(&root.join("compose-operation.json"),&journal)?;
    write_state(root,json!({"status":"preparing","latest_version":selected,"message":"正在拉取并切换版本化容器","error":null}))?;
    if let Err(error)=recover_compose(root).await{write_state(root,json!({"status":"error","error":error,"message":"容器版本切换失败，恢复状态已保存"}))?;return Err(error)}
    write_state(root,json!({"status":if rollback{"rolled_back"}else{"installed"},"current_version":selected,"latest_version":selected,"available":false,"progress":100,"restart_required":false,"error":null,"message":format!("容器服务已切换到 {selected}")}))
}

async fn compose_tail(mut reader:impl tokio::io::AsyncRead+Unpin)->Result<Vec<u8>,String>{
    use tokio::io::AsyncReadExt;let mut tail=Vec::new();let mut chunk=[0u8;8192];loop{let count=reader.read(&mut chunk).await.map_err(|error|error.to_string())?;if count==0{break}tail.extend_from_slice(&chunk[..count]);if tail.len()>256*1024{tail.drain(..tail.len()-256*1024);}}Ok(tail)
}
