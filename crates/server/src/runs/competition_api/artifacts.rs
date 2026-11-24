use super::*;
use sha2::{Digest,Sha256};

const ARTIFACTS: &str = "competition_artifacts_live";
const MAX_ARTIFACT: usize = 64 * 1024 * 1024;
fn verified_bytes(path:&std::path::Path,item:&Value)->ApiResult<Vec<u8>>{
    use std::io::Read;
    let mut options=OpenOptions::new();options.read(true);
    #[cfg(unix)]{use std::os::unix::fs::OpenOptionsExt;options.custom_flags(libc::O_NOFOLLOW);}
    let file=options.open(path).map_err(ApiError::internal)?;let metadata=file.metadata().map_err(ApiError::internal)?;
    if !metadata.is_file()||metadata.len()>MAX_ARTIFACT as u64||item["size"].as_u64().is_some_and(|size|size!=metadata.len()){return Err(ApiError::invalid("Attachment size or file type differs from its reference"));}
    let mut bytes=Vec::new();file.take(MAX_ARTIFACT as u64+1).read_to_end(&mut bytes).map_err(ApiError::internal)?;if bytes.len()>MAX_ARTIFACT||bytes.len() as u64!=metadata.len(){return Err(ApiError::invalid("Attachment changed while being read"));}
    if let Some(digest)=item["sha256"].as_str().filter(|digest|!digest.is_empty()){check_digest(digest,&bytes)?;}Ok(bytes)
}

fn seal_reference(state:&AppState,item:&Value)->ApiResult<Value>{
    let bytes=verified_bytes(&path_for_item(state,item)?,item)?;let mut reference=item.clone();reference["sha256"]=json!(format!("{:x}",Sha256::digest(&bytes)));reference["size"]=json!(bytes.len());reference["integrity_version"]=json!(1);
    let id=reference["artifact_id"].as_str().ok_or_else(||ApiError::invalid("Attachment identity missing"))?;
    let old=state.store.get(ARTIFACTS,id).map_err(ApiError::internal)?;
    if let Some(old)=old.as_ref(){if let Some(digest)=old["sha256"].as_str().filter(|v|!v.is_empty()){check_digest(digest,&bytes)?;}else{let mut sealed=old.clone();for key in ["sha256","size","integrity_version"]{sealed[key]=reference[key].clone();}if !state.store.put_many_if(&[(ARTIFACTS,id,Some(old))],&[(ARTIFACTS,id,&sealed)]).map_err(ApiError::internal)?{return Err(ApiError::new(StatusCode::CONFLICT,"competition.artifact.concurrent_update","Attachment metadata changed while recording integrity"));}}}
    Ok(reference)
}

fn root(state: &AppState) -> ApiResult<PathBuf> {
    let path = native_chat::state_dir(state).join("control/competition/artifacts/items");
    secure_dir(&path)?;
    Ok(path)
}
fn path_for_item(state: &AppState, item: &Value) -> ApiResult<PathBuf> {
    let uuid = item["artifact_id"].as_str()
        .and_then(|value|value.strip_prefix("cart-"))
        .and_then(|value|uuid::Uuid::parse_str(value).ok())
        .ok_or_else(||ApiError::invalid("Invalid attachment ID"))?;
    Ok(root(state)?.join(format!("cart-{}",uuid.simple())))
}
pub(super) fn local_paths(state: &AppState, revision: &Value) -> ApiResult<Vec<String>> {
    let mut paths = Vec::new();
    for item in revision["artifacts"].as_array().into_iter().flatten() {
        let path = path_for_item(state, item)?;
        verified_bytes(&path,item)?;
        paths.push(path.to_string_lossy().into_owned());
    }
    Ok(paths)
}
pub(super) fn stage_for_run(
    state: &AppState,
    revision: &Value,
    run_id: &str,
) -> ApiResult<Vec<String>> {
    validated_segment(run_id)?;
    let cas_paths = local_paths(state, revision)?;
    if cas_paths.is_empty() {
        return Ok(Vec::new());
    }
    let run_root = native_chat::state_dir(state).join("control/competition/artifacts/runs");
    secure_dir(&run_root)?;
    let dir = run_root.join(run_id);
    secure_dir(&dir)?;
    let mut names = std::collections::HashSet::new();
    let mut staged = Vec::new();
    for (item, source) in revision["artifacts"]
        .as_array()
        .into_iter()
        .flatten()
        .zip(cas_paths)
    {
        let name = item["name"]
            .as_str()
            .and_then(clean_name)
            .ok_or_else(|| ApiError::invalid("Attachment name is invalid"))?;
        if !names.insert(name.clone()) {
            return Err(ApiError::invalid("Duplicate attachment name in revision"));
        }
        let path = dir.join(&name);
        if !path.exists() {
            let temporary = dir.join(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary).map_err(ApiError::internal)?;
            let bytes = verified_bytes(std::path::Path::new(&source),item)?;
            if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
                let _ = fs::remove_file(&temporary);
                return Err(ApiError::internal(error));
            }
            fs::rename(&temporary, &path).map_err(ApiError::internal)?;
        }
        verified_bytes(&path,item)?;
        staged.push(path.to_string_lossy().into_owned());
    }
    Ok(staged)
}
fn clean_name(name: &str) -> Option<String> {
    let name = name.split('?').next()?.rsplit('/').next()?.trim();
    (!name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= 255
        && !name.contains(['\0', '\\']))
    .then(|| name.to_owned())
}
fn urls(remote: &Value) -> Vec<(String, String)> {
    let raw = &remote["raw"];
    let mut found = Vec::new();
    let details = raw["detail"]
        .as_object()
        .map(|_| &raw["detail"])
        .unwrap_or(raw);
    for key in ["files", "attachments"] {
        for entry in details[key].as_array().into_iter().flatten() {
            if let Some(url) = entry.as_str() {
                if let Some(name) = clean_name(url) {
                    found.push((name, url.to_owned()));
                }
            } else if let Some(url) = entry["url"].as_str().or_else(|| entry["location"].as_str()) {
                if let Some(name) = entry["name"]
                    .as_str()
                    .and_then(clean_name)
                    .or_else(|| clean_name(url))
                {
                    found.push((name, url.to_owned()));
                }
            }
        }
    }
    for key in ["attachment", "context"] {
        if let Some(url) = details[key]["url"].as_str() {
            if let Some(name) = details[key]["name"]
                .as_str()
                .and_then(clean_name)
                .or_else(|| clean_name(url))
            {
                found.push((name, url.to_owned()));
            }
        }
    }
    if remote["raw"].is_object() {
        collect_nested_urls(raw, &mut found);
    }
    found
}

fn collect_nested_urls(node: &Value, found: &mut Vec<(String,String)>) {
    match node {
        Value::Object(fields) => {
            if let Some(url) = fields.get("url").or_else(||fields.get("location")).and_then(Value::as_str) {
                let name = fields.get("name").and_then(Value::as_str).and_then(clean_name).or_else(||clean_name(url));
                if let Some(name) = name { found.push((name,url.to_owned())); }
            }
            for value in fields.values() { collect_nested_urls(value,found); }
        }
        Value::Array(items) => for item in items { collect_nested_urls(item,found); },
        Value::String(url) if url.contains('/') => {
            if let Some(name) = clean_name(url) { found.push((name,url.clone())); }
        }
        _ => {}
    }
}
fn artifact_url(connection: &Value, raw: &str) -> ApiResult<(reqwest::Url, bool)> {
    let base =
        reqwest::Url::parse(field(connection, "canonical_base_url")).map_err(ApiError::internal)?;
    let url = base
        .join(raw)
        .map_err(|_| ApiError::invalid("Invalid attachment URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(ApiError::invalid(
            "Attachment URL must be HTTP(S) without embedded credentials",
        ));
    }
    let same_origin = url.origin() == base.origin();
    Ok((url, same_origin))
}

async fn download(
    state: &AppState,
    connection: &Value,
    raw_url: &str,
    name: &str,
    expected_sha256:Option<&str>,
) -> ApiResult<Value> {
    let (url, same_origin) = artifact_url(connection, raw_url)?;
    let client = crate::network_proxy::http_client_builder()
        .timeout(Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(ApiError::internal)?;
    let mut request = client.get(url.as_str());
    if same_origin {
        match field(connection, "platform_kind") {
            "ctfd" | "rctf" => {
                let token = read_secret(state, field(connection, "credential_ref"))?;
                if token.contains(['\r', '\n']) {
                    return Err(ApiError::invalid("Invalid credential characters"));
                }
                request = request.header(
                    reqwest::header::AUTHORIZATION,
                    format!(
                        "{} {token}",
                        if connection["platform_kind"] == "ctfd" {
                            "Token"
                        } else {
                            "Bearer"
                        }
                    ),
                );
            }
            "gzctf" => {
                let cookie = gzctf_session_cookie(state, connection).await?;
                request = request.header(reqwest::header::COOKIE, format!("GZCTF_Token={cookie}"));
            }
            "generic_browser" => {
                request = request.header(
                    reqwest::header::COOKIE,
                    browser_cookie_header(state, connection, url.as_str())?,
                );
            }
            _ => {}
        }
    }
    let mut response = request.send().await.map_err(|_| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "competition.artifact_transport",
            "Attachment download failed",
        )
    })?;
    if !response.status().is_success() {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "competition.artifact_transport",
            format!("Attachment returned HTTP {}", response.status().as_u16()),
        ));
    }
    let media_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .chars()
        .take(128)
        .collect::<String>();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "competition.artifact_transport",
            "Attachment stream failed",
        )
    })? {
        if bytes.len().saturating_add(chunk.len()) > MAX_ARTIFACT {
            return Err(ApiError::invalid("Attachment exceeds 64 MiB"));
        }
        bytes.extend_from_slice(&chunk);
    }
    if let Some(expected)=expected_sha256{check_digest(expected,&bytes)?;}
    store_bytes(state, &bytes, name, &media_type)
}
pub(super) fn store_bytes(state: &AppState, bytes: &[u8], name: &str, media_type: &str) -> ApiResult<Value> {
    if bytes.len() > MAX_ARTIFACT { return Err(ApiError::invalid("Attachment exceeds 64 MiB")); }
    let artifact_id = id("cart");
    let path = root(state)?.join(&artifact_id);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path).map_err(ApiError::internal)?;
    file.write_all(bytes).and_then(|_| file.sync_all()).map_err(ApiError::internal)?;
    let metadata = json!({"artifact_id":artifact_id,"name":name,"size":bytes.len(),"media_type":media_type,"sha256":format!("{:x}",Sha256::digest(bytes)),"integrity_version":1});
    state
        .store
        .put(ARTIFACTS, &artifact_id, &metadata)
        .map_err(ApiError::internal)?;
    Ok(metadata)
}
pub(super) async fn materialize(
    state: &Arc<AppState>,
    connection: &Value,
    remote: &Value,
    prior_revision: Option<&Value>,
) -> ApiResult<Vec<Value>> {
    let available = urls(remote);
    let mut results = Vec::new();
    for artifact in remote["artifacts"].as_array().into_iter().flatten() {
        let name = artifact["name"]
            .as_str()
            .and_then(clean_name)
            .ok_or_else(|| ApiError::invalid("Attachment name is invalid"))?;
        let size = artifact["size"].as_u64().unwrap_or(0);
        let expected=artifact["sha256"].as_str().filter(|v|!v.is_empty());
        if expected.is_some_and(|digest|digest.len()!=64 || !digest.bytes().all(|byte|byte.is_ascii_hexdigit())){return Err(ApiError::invalid("Remote attachment SHA-256 is invalid"));}
        if let Some(digest)=expected{if let Some(mut cached)=state.store.list(ARTIFACTS).map_err(ApiError::internal)?.into_iter().find(|old|old["sha256"].as_str().is_some_and(|saved|saved.eq_ignore_ascii_case(digest)) && local_paths(state,&json!({"artifacts":[old]})).is_ok()){cached["name"]=json!(name);results.push(cached);continue;}}
        if let Some(previous) = prior_revision
            .into_iter()
            .flat_map(|revision| revision["artifacts"].as_array().into_iter().flatten())
            .find(|old| if let Some(digest)=expected{old["sha256"].as_str().is_some_and(|saved|saved.eq_ignore_ascii_case(digest))}else{old["name"] == name && (size == 0 || old["size"] == size)})
        {
            if local_paths(state, &json!({"artifacts":[previous]})).is_ok() {
                results.push(seal_reference(state,previous)?);
                continue;
            }
        }
        let url = artifact["url"].as_str().or_else(||available
            .iter()
            .find(|(candidate, _)| candidate == &name)
            .map(|(_, url)| url.as_str()))
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "competition.artifact_url_missing",
                    "Attachment URL is missing from the current challenge detail",
                )
            })?;
        results.push(if matches!(field(connection,"platform_kind"),"ctfd" | "rctf" | "gzctf" | "generic_browser") {
            download(state, connection, url, &name,expected).await?
        } else {
            let (bytes, media_type) = extension_adapter::fetch_artifact(state, connection,
                field(remote,"external_challenge_id"), url,expected).await?;
            store_bytes(state, &bytes, &name, &media_type)?
        });
    }
    Ok(results)
}

fn check_digest(expected:&str,bytes:&[u8])->ApiResult<()>{
    let actual=format!("{:x}",Sha256::digest(bytes));if expected.len()!=64 || !expected.eq_ignore_ascii_case(&actual){Err(ApiError::new(StatusCode::BAD_GATEWAY,"competition.artifact_integrity","Downloaded attachment differs from the platform SHA-256"))}else{Ok(())}
}
