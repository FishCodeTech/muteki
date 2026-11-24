use super::{ExtError, Result, manifest};
use base64::Engine;
use ring::signature::{ED25519, UnparsedPublicKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};

const MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;
const MAX_PACKAGE_FILES: usize = 12_000;
const MAX_PACKAGE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TAR_BYTES: u64 = MAX_PACKAGE_BYTES + MAX_PACKAGE_FILES as u64 * 1024 + 1024 * 1024;

fn source_err(code: &str, message: impl Into<String>) -> ExtError {
    ExtError::validation(code, message)
}

pub fn copy_tree(source: &Path, target: &Path) -> Result<()> {
    if !source.is_dir() {
        return Err(source_err(
            "extension.source_not_found",
            "source directory does not exist",
        ));
    }
    let mut entries = Vec::new();
    for entry in walkdir::WalkDir::new(source)
        .min_depth(1)
        .follow_links(false)
    {
        let entry =
            entry.map_err(|e| source_err("extension.source_inaccessible", e.to_string()))?;
        if !entry.file_type().is_file() && !entry.file_type().is_dir() {
            return Err(source_err("extension.unsafe_source", "extension sources cannot contain links or special files"));
        }
        entries.push(entry);
        if entries.len() > MAX_PACKAGE_FILES {
            return Err(source_err("extension.package_too_large", "extension exceeds 12000 entries"));
        }
    }
    fs::create_dir_all(target)
        .map_err(|e| source_err("extension.install_failed", e.to_string()))?;
    let mut remaining = MAX_PACKAGE_BYTES;
    for entry in entries {
        let rel = entry
            .path()
            .strip_prefix(source)
            .map_err(|e| source_err("extension.install_failed", e.to_string()))?;
        let dst = target.join(rel);
        if entry.file_type().is_dir() {
            fs::create_dir_all(dst)
                .map_err(|e| source_err("extension.install_failed", e.to_string()))?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| source_err("extension.install_failed", e.to_string()))?;
            }
            // Open every path component without following links, including a
            // link swapped in after the directory walk.
            let bytes = crate::private_files::read(source, entry.path(), remaining as usize)
                .map_err(|e| source_err("extension.unsafe_source", e))?;
            remaining -= bytes.len() as u64;
            fs::write(&dst, bytes)
                .map_err(|e| source_err("extension.install_failed", e.to_string()))?;
            fs::set_permissions(&dst, entry.metadata()
                .map_err(|e| source_err("extension.source_inaccessible", e.to_string()))?.permissions())
                .map_err(|e| source_err("extension.install_failed", e.to_string()))?;
        }
    }
    Ok(())
}

fn tree_digest<D: Digest + Default>(root: &Path) -> Result<String> {
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(root).min_depth(1).follow_links(false) {
        let entry = entry.map_err(|e| source_err("extension.snapshot_invalid", e.to_string()))?;
        if entry.file_type().is_symlink() {
            return Err(source_err(
                "extension.snapshot_invalid",
                "snapshot contains a symbolic link",
            ));
        }
        if entry.file_type().is_file() {
            files.push(entry.into_path());
        }
    }
    files.sort();
    let mut digest = D::default();
    for path in files {
        let rel = path
            .strip_prefix(root)
            .map_err(|e| source_err("extension.snapshot_invalid", e.to_string()))?;
        digest.update(rel.to_string_lossy().replace('\\', "/").as_bytes());
        digest.update([0]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path)
                .map_err(|e| source_err("extension.snapshot_invalid", e.to_string()))?
                .permissions()
                .mode()
                & 0o777;
            digest.update(mode.to_le_bytes());
            digest.update([0]);
        }
        let mut input = fs::File::open(&path)
            .map_err(|e| source_err("extension.snapshot_invalid", e.to_string()))?;
        let mut buf = [0u8; 65536];
        loop {
            let n = input
                .read(&mut buf)
                .map_err(|e| source_err("extension.snapshot_invalid", e.to_string()))?;
            if n == 0 {
                break;
            }
            digest.update(&buf[..n]);
        }
        digest.update([0]);
    }
    Ok(digest.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn sha256_tree(root: &Path) -> Result<String> { tree_digest::<Sha256>(root) }

pub fn matches_snapshot(root: &Path, verification: &Value) -> Result<bool> {
    if let Some(expected) = verification["content_sha256"].as_str().filter(|s| !s.is_empty()) {
        return Ok(sha256_tree(root)? == expected);
    }
    Ok(false)
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut digest = Sha256::new();
    let mut file = fs::File::open(path)
        .map_err(|e| source_err("extension.source_not_found", e.to_string()))?;
    let mut buf = [0u8; 65536];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| source_err("extension.source_inaccessible", e.to_string()))?;
        if n == 0 {
            break;
        }
        digest.update(&buf[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn safe_member(name: &str) -> bool {
    let path = Path::new(name);
    !name.starts_with('/')
        && !path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

fn write_member(
    target: &Path,
    name: &str,
    input: &mut dyn Read,
    executable: bool,
    remaining: &mut u64,
) -> Result<()> {
    let dst = target.join(name);
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| source_err("extension.install_failed", e.to_string()))?;
    }
    let mut file = fs::File::create(&dst)
        .map_err(|e| source_err("extension.install_failed", e.to_string()))?;
    let copied = std::io::copy(&mut input.take(*remaining + 1), &mut file)
        .map_err(|e| source_err("extension.install_failed", e.to_string()))?;
    if copied > *remaining {
        return Err(source_err("extension.package_too_large", "expanded extension exceeds 256 MiB"));
    }
    *remaining -= copied;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            &dst,
            fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
        )
        .map_err(|e| source_err("extension.install_failed", e.to_string()))?;
    }
    Ok(())
}

fn unpack(archive: &Path, target: &Path) -> Result<PathBuf> {
    fs::create_dir_all(target)
        .map_err(|e| source_err("extension.install_failed", e.to_string()))?;
    let mut handled = false;
    if let Ok(mut zip) = zip::ZipArchive::new(
        fs::File::open(archive)
            .map_err(|e| source_err("extension.source_not_found", e.to_string()))?,
    ) {
        if zip.len() > MAX_PACKAGE_FILES {
            return Err(source_err("extension.package_too_large", "archive exceeds 12000 entries"));
        }
        let mut remaining = MAX_PACKAGE_BYTES;
        for i in 0..zip.len() {
            let entry = zip
                .by_index_raw(i)
                .map_err(|e| source_err("extension.invalid_archive", e.to_string()))?;
            let mode = entry.unix_mode().unwrap_or(0);
            remaining = remaining.checked_sub(entry.size()).ok_or_else(||
                source_err("extension.package_too_large", "expanded extension exceeds 256 MiB"))?;
            if !safe_member(entry.name()) || mode & 0o170000 == 0o120000 {
                return Err(source_err(
                    "extension.unsafe_archive",
                    "archive contains unsafe path or link",
                ));
            }
        }
        let mut remaining = MAX_PACKAGE_BYTES;
        for i in 0..zip.len() {
            let mut entry = zip
                .by_index(i)
                .map_err(|e| source_err("extension.invalid_archive", e.to_string()))?;
            let name = entry.name().to_owned();
            if entry.is_dir() {
                fs::create_dir_all(target.join(name))
                    .map_err(|e| source_err("extension.install_failed", e.to_string()))?;
            } else {
                let executable = entry.unix_mode().is_some_and(|mode| mode & 0o111 != 0);
                write_member(target, &name, &mut entry, executable, &mut remaining)?;
            }
        }
        handled = true;
    }
    if !handled {
        let mut header = [0u8; 2];
        let n = fs::File::open(archive)
            .and_then(|mut file| file.read(&mut header))
            .map_err(|e| source_err("extension.source_not_found", e.to_string()))?;
        let gz = n == 2 && header == [0x1f, 0x8b];
        let open = || -> Result<std::io::Take<Box<dyn Read>>> {
            let file = fs::File::open(archive)
                .map_err(|e| source_err("extension.source_not_found", e.to_string()))?;
            let input: Box<dyn Read> = if gz {
                Box::new(flate2::read::GzDecoder::new(file))
            } else {
                Box::new(file)
            };
            Ok(input.take(MAX_TAR_BYTES))
        };
        let mut checked = tar::Archive::new(open()?);
        let mut remaining = MAX_PACKAGE_BYTES;
        for (index, item) in checked
            .entries()
            .map_err(|e| source_err("extension.invalid_archive", e.to_string()))?
            .enumerate()
        {
            let item = item.map_err(|e| source_err("extension.invalid_archive", e.to_string()))?;
            if index >= MAX_PACKAGE_FILES {
                return Err(source_err("extension.package_too_large", "archive exceeds 12000 entries"));
            }
            remaining = remaining.checked_sub(item.size()).ok_or_else(||
                source_err("extension.package_too_large", "expanded extension exceeds 256 MiB"))?;
            let path = item
                .path()
                .map_err(|e| source_err("extension.invalid_archive", e.to_string()))?
                .to_string_lossy()
                .into_owned();
            let kind = item.header().entry_type();
            if !safe_member(&path) || kind.is_symlink() || kind.is_hard_link() {
                return Err(source_err(
                    "extension.unsafe_archive",
                    "archive contains unsafe path or link",
                ));
            }
        }
        if checked.into_inner().limit() == 0 {
            return Err(source_err("extension.package_too_large", "archive metadata exceeds its bound"));
        }
        let mut archive = tar::Archive::new(open()?);
        let mut remaining = MAX_PACKAGE_BYTES;
        for item in archive
            .entries()
            .map_err(|e| source_err("extension.invalid_archive", e.to_string()))?
        {
            let mut item =
                item.map_err(|e| source_err("extension.invalid_archive", e.to_string()))?;
            let name = item
                .path()
                .map_err(|e| source_err("extension.invalid_archive", e.to_string()))?
                .to_string_lossy()
                .into_owned();
            let kind = item.header().entry_type();
            if kind.is_dir() {
                fs::create_dir_all(target.join(name))
                    .map_err(|e| source_err("extension.install_failed", e.to_string()))?;
            } else if kind.is_file() {
                let executable = item.header().mode().is_ok_and(|mode| mode & 0o111 != 0);
                write_member(target, &name, &mut item, executable, &mut remaining)?;
            } else if !matches!(
                kind,
                tar::EntryType::XGlobalHeader | tar::EntryType::XHeader
            ) {
                return Err(source_err(
                    "extension.unsafe_archive",
                    "archive contains special file",
                ));
            }
        }
    }
    let entries: Vec<PathBuf> = fs::read_dir(target)
        .map_err(|e| source_err("extension.install_failed", e.to_string()))?
        .flatten()
        .map(|e| e.path())
        .collect();
    let package =
        if entries.len() == 1 && entries[0].is_dir() && !target.join("plugin.json").is_file() {
            entries[0].clone()
        } else {
            target.to_path_buf()
        };
    Ok(package)
}

fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let ordered: BTreeMap<_, _> = map.iter().collect();
            format!(
                "{{{}}}",
                ordered
                    .iter()
                    .map(|(k, v)| format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap(),
                        canonical_json(v)
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        _ => serde_json::to_string(value).unwrap_or_default(),
    }
}

fn decode_signature(text: &str) -> Result<Vec<u8>> {
    if text.len() == 128 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
        (0..64)
            .map(|i| {
                u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)
                    .map_err(|e| source_err("extension.catalog_signature_invalid", e.to_string()))
            })
            .collect()
    } else {
        base64::engine::general_purpose::STANDARD
            .decode(text)
            .map_err(|e| source_err("extension.catalog_signature_invalid", e.to_string()))
    }
}

fn decode_key(text: &str) -> Result<Vec<u8>> {
    if text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
        (0..32)
            .map(|i| {
                u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)
                    .map_err(|e| source_err("extension.catalog_signature_invalid", e.to_string()))
            })
            .collect()
    } else {
        base64::engine::general_purpose::STANDARD
            .decode(text)
            .map_err(|e| source_err("extension.catalog_signature_invalid", e.to_string()))
    }
}

pub fn catalog_entries(root: &Path) -> Result<Vec<Value>> {
    let paths: Vec<PathBuf> = if root.is_file() {
        vec![root.to_path_buf()]
    } else if root.is_dir() {
        let mut list = fs::read_dir(root)
            .map_err(|e| source_err("extension.catalog_unavailable", e.to_string()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "yaml" || e == "yml"))
            .collect::<Vec<_>>();
        list.sort();
        list
    } else {
        vec![]
    };
    if paths.is_empty() {
        return Err(source_err(
            "extension.catalog_unavailable",
            "catalog not found",
        ));
    }
    let mut entries = Vec::new();
    let mut publishers = BTreeMap::<String, Value>::new();
    let mut revoked = Vec::new();
    for path in paths {
        let raw = fs::read_to_string(&path)
            .map_err(|e| source_err("extension.catalog_unavailable", e.to_string()))?;
        let yaml: serde_yaml::Value = serde_yaml::from_str(&raw)
            .map_err(|e| source_err("extension.catalog_unavailable", e.to_string()))?;
        let doc = serde_json::to_value(yaml)
            .map_err(|e| source_err("extension.catalog_unavailable", e.to_string()))?;
        for item in doc["publishers"].as_array().into_iter().flatten() {
            if let Some(id) = item["id"].as_str() {
                if publishers.get(id).is_some_and(|prior| prior != item) {
                    return Err(source_err(
                        "extension.catalog_unavailable",
                        "conflicting publisher trust records",
                    ));
                }
                publishers.insert(id.to_owned(), item.clone());
            }
        }
        revoked.extend(doc["revocations"].as_array().into_iter().flatten().cloned());
        let values = if doc.is_array() {
            doc.as_array()
        } else {
            doc["extensions"].as_array()
        }
        .ok_or_else(|| {
            source_err(
                "extension.catalog_unavailable",
                "catalog requires extensions array",
            )
        })?;
        entries.extend(values.iter().cloned());
    }
    let mut seen = HashSet::new();
    for entry in &mut entries {
        let id = entry["id"].as_str().unwrap_or("");
        let version = entry["version"].as_str().unwrap_or("");
        if id.is_empty() || version.is_empty() || !seen.insert((id.to_owned(), version.to_owned()))
        {
            return Err(source_err(
                "extension.catalog_unavailable",
                "invalid or duplicate catalog version",
            ));
        }
        let source = &entry["source"];
        if !["local-dir", "archive", "git", "http"].contains(&source["kind"].as_str().unwrap_or(""))
        {
            return Err(source_err(
                "extension.catalog_unavailable",
                "unknown catalog source kind",
            ));
        }
        let matching_revocation = revoked
            .iter()
            .filter(|r| {
                r["id"] == id
                    && r["version"]
                        .as_str()
                        .is_none_or(|v| v.is_empty() || v == version)
                    && r["sha256"].as_str().is_none_or(|s| {
                        s.is_empty()
                            || s.eq_ignore_ascii_case(source["sha256"].as_str().unwrap_or(""))
                    })
            })
            .next_back();
        if let Some(rev) = matching_revocation {
            entry["revoked"] = json!(true);
            entry["revoke_reason"] = json!(
                rev["reason"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("catalog revocation")
            );
            entry["trust_status"] = json!("revoked");
            continue;
        }
        let signature = entry["signature"].as_str().unwrap_or("");
        if signature.is_empty() {
            entry["trust_status"] = json!("unsigned");
            continue;
        }
        let publisher_id = entry["publisher"].as_str().unwrap_or("");
        let publisher = publishers.get(publisher_id).ok_or_else(|| {
            source_err("extension.catalog_signature_invalid", "unknown publisher")
        })?;
        if publisher["revoked"] == true {
            entry["revoked"] = json!(true);
            entry["revoke_reason"] = json!("publisher trust revoked");
            entry["trust_status"] = json!("revoked");
            continue;
        }
        let pub_alg = publisher["algorithm"].as_str().unwrap_or("ed25519");
        let entry_alg = entry["signature_algorithm"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(pub_alg);
        if pub_alg != "ed25519" || entry_alg != "ed25519" {
            return Err(source_err(
                "extension.catalog_signature_invalid",
                "unsupported signature algorithm",
            ));
        }
        let signed_source = json!({"kind":source["kind"],"path":source["path"].as_str().unwrap_or(""),
            "url":source["url"].as_str().unwrap_or(""),"ref":source["ref"].as_str().unwrap_or(""),
            "sha256":source["sha256"].as_str().unwrap_or("")});
        let signed = json!({"id":id,"version":version,"publisher":publisher_id,
            "published_at":entry["published_at"].as_str().unwrap_or(""),
            "signature_algorithm":entry["signature_algorithm"].as_str().unwrap_or(""),
            "source":signed_source});
        let message = canonical_json(&signed);
        let key = publisher["public_key"].as_str().ok_or_else(|| {
            source_err("extension.catalog_signature_invalid", "public key missing")
        })?;
        let key = decode_key(key)?;
        let signature = decode_signature(signature)?;
        UnparsedPublicKey::new(&ED25519, key)
            .verify(message.as_bytes(), &signature)
            .map_err(|_| {
                source_err(
                    "extension.catalog_signature_invalid",
                    "Ed25519 signature invalid",
                )
            })?;
        entry["signature_algorithm"] = json!("ed25519");
        entry["trust_status"] = json!("verified");
    }
    Ok(entries)
}

pub fn resolve_catalog(source: &Value) -> Result<Value> {
    let root = source["catalog_root"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| source_err("extension.invalid_source", "catalog_root missing"))?;
    let id = source["extension_id"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| source_err("extension.invalid_source", "catalog extension_id missing"))?;
    let version = source["version"].as_str().unwrap_or("");
    let entries = catalog_entries(Path::new(root))?;
    let chosen = entries
        .into_iter()
        .filter(|v| v["id"] == id && (version.is_empty() || v["version"] == version))
        .max_by(|a, b| {
            manifest::version_cmp(
                a["version"].as_str().unwrap_or(""),
                b["version"].as_str().unwrap_or(""),
            )
        })
        .ok_or_else(|| source_err("extension.catalog_unavailable", "catalog entry not found"))?;
    if chosen["revoked"] == true {
        return Err(source_err(
            "extension.source_revoked",
            "catalog entry is revoked",
        ));
    }
    let mut resolved = chosen["source"].clone();
    resolved["extension_id"] = chosen["id"].clone();
    resolved["version"] = chosen["version"].clone();
    resolved["publisher"] = chosen["publisher"].clone();
    resolved["trust_status"] = chosen["trust_status"].clone();
    Ok(resolved)
}

async fn run_git(args: &[&str], cwd: &Path, timeout_seconds: Option<u64>) -> Result<String> {
    let mut command = crate::host_process::command("git");
    command.args(args).current_dir(cwd).stdin(Stdio::null());
    let output = if let Some(seconds) = timeout_seconds {
        tokio::time::timeout(Duration::from_secs(seconds), command.output())
            .await
            .map_err(|_| source_err("extension.git_timeout", "git operation timed out"))?
    } else {
        command.output().await
    }
    .map_err(|e| source_err("extension.git_failed", e.to_string()))?;
    if !output.status.success() {
        return Err(source_err(
            "extension.git_failed",
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .take(500)
                .collect::<String>(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub async fn materialize(requested: &Value, workdir: &Path) -> Result<(PathBuf, Value)> {
    let from_catalog = requested["kind"] == "catalog";
    let source = if from_catalog {
        resolve_catalog(requested)?
    } else {
        requested.clone()
    };
    let kind = source["kind"].as_str().unwrap_or("");
    let package_dir = workdir.join("package");
    let mut source_sha256 = String::new();
    let mut commit = String::new();
    let root = match kind {
        "local-dir" => {
            let path = source["path"]
                .as_str()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    source_err("extension.invalid_source", "local source path is missing")
                })?;
            let path = Path::new(path)
                .canonicalize()
                .map_err(|e| source_err("extension.source_not_found", e.to_string()))?;
            copy_tree(&path, &package_dir)?;
            package_dir.clone()
        }
        "archive" => {
            let path = source["path"]
                .as_str()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| source_err("extension.invalid_source", "archive path is missing"))?;
            let path = Path::new(path)
                .canonicalize()
                .map_err(|e| source_err("extension.source_not_found", e.to_string()))?;
            source_sha256 = sha256_file(&path)?;
            if let Some(expected_256) = source["sha256"].as_str().filter(|v| !v.is_empty()) {
                let actual_256 = &source_sha256;
                if !expected_256.eq_ignore_ascii_case(&actual_256) {
                    return Err(source_err(
                        "extension.digest_mismatch",
                        "archive SHA-256 mismatch",
                    ));
                }
            }
            let extracted = unpack(&path, &workdir.join("unpacked"))?;
            copy_tree(&extracted, &package_dir)?;
            package_dir.clone()
        }
        "git" => {
            let url = source["url"]
                .as_str()
                .filter(|v| !v.trim().is_empty())
                .ok_or_else(|| source_err("extension.invalid_source", "git source requires url"))?;
            let reference = source["ref"]
                .as_str()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    source_err("extension.invalid_source", "git source requires pinned ref")
                })?;
            let package_path = package_dir.to_string_lossy().into_owned();
            run_git(&["clone", "--quiet", "--no-checkout", url, &package_path],
                workdir, Some(120)).await?;
            run_git(&["checkout", "--quiet", reference], &package_dir, Some(60)).await?;
            let is_commit = (7..=40).contains(&reference.len())
                && reference.bytes().all(|byte| byte.is_ascii_hexdigit());
            if !is_commit {
                let tag_ref = format!("refs/tags/{reference}");
                if run_git(&["show-ref", "--verify", "--quiet", &tag_ref],
                    &package_dir, None).await.is_err() {
                    return Err(source_err("extension.unpinned_git_ref",
                        "git ref must be an immutable commit hash or tag"));
                }
            }
            commit = run_git(&["rev-parse", "HEAD"], &package_dir, None).await?;
            let _ = fs::remove_dir_all(package_dir.join(".git"));
            let clean = workdir.join("clean");
            copy_tree(&package_dir, &clean)?;
            clean
        }
        "http" => {
            let url = source["url"]
                .as_str()
                .filter(|v| v.starts_with("https://") || v.starts_with("http://"))
                .ok_or_else(|| source_err("extension.invalid_source", "HTTP source URL invalid"))?;
            let has_256 = source["sha256"].as_str().is_some_and(|v| !v.is_empty());
            if !has_256 {
                return Err(source_err(
                    "extension.digest_required",
                    "HTTP source requires SHA-256",
                ));
            }
            let client = crate::network_proxy::http_client_builder()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::limited(3))
                .build()
                .map_err(|e| source_err("extension.download_failed", e.to_string()))?;
            let mut response = client
                .get(url)
                .send()
                .await
                .map_err(|e| source_err("extension.download_failed", e.to_string()))?
                .error_for_status()
                .map_err(|e| source_err("extension.download_failed", e.to_string()))?;
            let archive = workdir.join("download.archive");
            let mut out = fs::File::create(&archive)
                .map_err(|e| source_err("extension.download_failed", e.to_string()))?;
            let mut size = 0u64;
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| source_err("extension.download_failed", e.to_string()))?
            {
                size += chunk.len() as u64;
                if size > MAX_DOWNLOAD_BYTES {
                    return Err(source_err(
                        "extension.download_too_large",
                        "download exceeds limit",
                    ));
                }
                out.write_all(&chunk)
                    .map_err(|e| source_err("extension.download_failed", e.to_string()))?;
            }
            source_sha256 = sha256_file(&archive)?;
            if let Some(expected_256) = source["sha256"].as_str().filter(|v| !v.is_empty()) {
                let actual_256 = &source_sha256;
                if !expected_256.eq_ignore_ascii_case(&actual_256) {
                    return Err(source_err(
                        "extension.digest_mismatch",
                        "download SHA-256 mismatch",
                    ));
                }
            }
            let extracted = unpack(&archive, &workdir.join("unpacked"))?;
            copy_tree(&extracted, &package_dir)?;
            package_dir.clone()
        }
        _ => {
            return Err(source_err(
                "extension.invalid_source",
                format!("unknown source kind: {kind}"),
            ));
        }
    };
    let content_sha256 = sha256_tree(&root)?;
    let manifest = manifest::load(&root)?;
    if source["extension_id"]
        .as_str()
        .is_some_and(|v| !v.is_empty() && manifest["id"] != v)
    {
        return Err(source_err(
            "extension.catalog_identity_mismatch",
            "source extension id differs from plugin.json",
        ));
    }
    if source["version"]
        .as_str()
        .is_some_and(|v| !v.is_empty() && manifest["plugin_version"] != v)
    {
        return Err(source_err(
            "extension.catalog_version_mismatch",
            "source version differs from plugin.json",
        ));
    }
    let verified = from_catalog && source["trust_status"] == "verified";
    let has_pinned_digest = !source["sha256"].as_str().unwrap_or("").is_empty();
    let verification = json!({"snapshot_id":format!("snapshot-{}",uuid::Uuid::new_v4().simple()),
        "sha256":source_sha256,"content_sha256":content_sha256,
        "signature":if verified{"verified:ed25519"}else{"unsigned"},
        "trust_status":if verified{"verified"}else{"unsigned"},
        "publisher":if from_catalog{source["publisher"].as_str().unwrap_or("unknown")}else{"unknown"},
        "immutable":kind=="git"&&!commit.is_empty() || ["archive","http"].contains(&kind)&&has_pinned_digest,
        "commit":commit,"source":source,
        "phases":["downloaded","verified","unpacked","plugin_manifest_validated","dependencies_checked","pending_activation"],
        "agent_plugin":{"schema":manifest::PLUGIN_SCHEMA,"manifest":"plugin.json","name":manifest["id"],"client_namespace":manifest::NAMESPACE},
    });
    Ok((root, verification))
}
