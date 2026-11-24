//! Read build-time image metadata without executing an image entrypoint.
use serde_json::Value;
use std::time::Duration;

async fn docker(args: &[&str]) -> Result<std::process::Output, String> {
    tokio::time::timeout(Duration::from_secs(20),
        crate::host_process::command("docker").args(args).kill_on_drop(true).output())
        .await.map_err(|_| "Docker image metadata operation timed out".to_owned())?
        .map_err(|e| e.to_string())
}

static CACHE: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, Option<Value>>>> =
    std::sync::LazyLock::new(Default::default);
/// Build-time manifest of an immutable image ID, cached for the process lifetime.
pub(crate) async fn cached(image_id: &str) -> Result<Option<Value>, String> {
    if let Some(value) = CACHE.lock().unwrap().get(image_id) { return Ok(value.clone()); }
    let value = manifest(image_id).await?;
    if image_id.starts_with("sha256:") { CACHE.lock().unwrap().insert(image_id.to_owned(), value.clone()); }
    Ok(value)
}

async fn manifest(image: &str) -> Result<Option<Value>, String> {
    let inspect = docker(&["image", "inspect", "--format", "{{json .Config.Labels}}", image]).await?;
    if !inspect.status.success() { return Err("Cannot inspect image capabilities".into()); }
    let labels: Value = serde_json::from_slice(&inspect.stdout).map_err(|e| e.to_string())?;
    let Some(version) = labels["io.muteki.capabilities.schema"].as_str() else { return Ok(None); };
    if version != "1" { return Err(format!("Unsupported image capabilities schema: {version}")); }
    let name = format!("muteki-image-metadata-{}", uuid::Uuid::new_v4());
    let created = docker(&["create", "--name", &name, "--network", "none", "--entrypoint", "/bin/true", image]).await;
    let result = async {
        if !created?.status.success() { return Err("Cannot create image metadata reader".into()); }
        let source = format!("{name}:/opt/muteki/IMAGE_CAPABILITIES.json");
        let output = docker(&["cp", &source, "-"]).await?;
        if !output.status.success() { return Err("Image declares capabilities but manifest is unreadable".into()); }
        parse_archive(&output.stdout).map(Some)
    }.await;
    let cleanup = docker(&["rm", "-f", &name]).await?;
    if !cleanup.status.success() && !super::container_exec::docker_reports_absent(&cleanup.stderr) {
        return Err("Image metadata reader cleanup was not confirmed".into());
    }
    result
}

fn parse_archive(bytes: &[u8]) -> Result<Value, String> {
    use std::io::Read;
    let mut archive = tar::Archive::new(bytes);
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        if entry.path().map_err(|e| e.to_string())?.as_ref() != std::path::Path::new("IMAGE_CAPABILITIES.json") { continue; }
        if entry.size() > 65536 || !entry.header().entry_type().is_file() { return Err("Invalid image capabilities manifest".into()); }
        let mut data = String::new();
        entry.read_to_string(&mut data).map_err(|e| e.to_string())?;
        let manifest: Value = serde_json::from_str(&data).map_err(|e| e.to_string())?;
        if manifest["schema_version"] != 1 { return Err("Unsupported image capabilities manifest".into()); }
        return Ok(manifest);
    }
    Err("Image capabilities manifest missing from archive".into())
}
