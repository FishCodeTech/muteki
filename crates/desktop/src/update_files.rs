//! File-level pieces of the desktop update: release archive checks, bundle
//! identity and the transaction journal. Shared by the app and
//! its private update helper.
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

pub const BUNDLE_ID: &str = "tech.fishcode.muteki.desktop";
pub const MAX_ASSET: u64 = 1024 * 1024 * 1024;
const MAX_EXPANDED: u64 = 2 * 1024 * 1024 * 1024;

/// An error with the stable code the settings page shows.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateError {
    pub code: &'static str,
    pub message: String,
}

impl UpdateError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub type Result<T> = std::result::Result<T, UpdateError>;

pub fn semantic(version: &str) -> bool {
    let parts: Vec<_> = version.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Dotted numeric comparison, missing parts counting as zero.
pub fn compare_versions(left: &str, right: &str) -> std::cmp::Ordering {
    let parse = |v: &str| {
        v.split('.')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    let (a, b) = (parse(left), parse(right));
    for i in 0..a.len().max(b.len()) {
        let order = a
            .get(i)
            .copied()
            .unwrap_or(0)
            .cmp(&b.get(i).copied().unwrap_or(0));
        if order.is_ne() {
            return order;
        }
    }
    std::cmp::Ordering::Equal
}

/// SHA-256 of the physical application tree accepted by the original updater.
/// This checks copied bytes and modes; the bundle version remains the release tag.
pub fn bundle_hash(root: &Path) -> Result<String> {
    fn walk(
        root: &Path,
        root_real: &Path,
        dir: &Path,
        tree: &mut Sha256,
        buffer: &mut [u8],
    ) -> Result<()> {
        let mut entries = std::fs::read_dir(dir)
            .map_err(|e| UpdateError::new("update.candidate_invalid", e.to_string()))?
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|e| UpdateError::new("update.candidate_invalid", e.to_string()))?;
        entries.sort_by(|left, right| {
            left.file_name()
                .to_string_lossy()
                .encode_utf16()
                .cmp(right.file_name().to_string_lossy().encode_utf16())
        });
        for entry in entries {
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|e| UpdateError::new("update.candidate_invalid", e.to_string()))?
                .to_string_lossy();
            let meta = std::fs::symlink_metadata(&path)
                .map_err(|e| UpdateError::new("update.candidate_invalid", e.to_string()))?;
            tree.update(relative.as_bytes());
            tree.update([0]);
            tree.update(
                format!(
                    "{:o}",
                    std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o777
                )
                .as_bytes(),
            );
            tree.update([0]);
            if meta.file_type().is_symlink() {
                let resolved = path
                    .canonicalize()
                    .map_err(|e| UpdateError::new("update.candidate_invalid", e.to_string()))?;
                if !resolved.starts_with(root_real) || resolved == root_real {
                    return Err(UpdateError::new(
                        "update.candidate_invalid",
                        format!("Bundle symlink escapes its root: {relative}"),
                    ));
                }
                let target = std::fs::read_link(&path)
                    .map_err(|e| UpdateError::new("update.candidate_invalid", e.to_string()))?;
                tree.update(b"link\0");
                tree.update(target.to_string_lossy().as_bytes());
                tree.update([0]);
            } else if meta.is_dir() {
                walk(root, root_real, &path, tree, buffer)?;
            } else if meta.is_file() {
                let mut file = std::fs::File::open(&path)
                    .map_err(|e| UpdateError::new("update.candidate_invalid", e.to_string()))?;
                let mut content = Sha256::new();
                loop {
                    let read = file
                        .read(buffer)
                        .map_err(|e| UpdateError::new("update.candidate_invalid", e.to_string()))?;
                    if read == 0 {
                        break;
                    }
                    content.update(&buffer[..read]);
                }
                tree.update(content.finalize());
            } else {
                return Err(UpdateError::new(
                    "update.candidate_invalid",
                    format!("Unsupported bundle entry: {relative}"),
                ));
            }
        }
        Ok(())
    }
    let real = root
        .canonicalize()
        .map_err(|e| UpdateError::new("update.candidate_invalid", e.to_string()))?;
    let mut tree = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    walk(root, &real, root, &mut tree, &mut buffer)?;
    Ok(format!("{:x}", tree.finalize()))
}

fn le16(bytes: &[u8], at: usize) -> Option<u64> {
    bytes
        .get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]) as u64)
}

fn le32(bytes: &[u8], at: usize) -> Option<u64> {
    bytes
        .get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as u64)
}

/// Normalizes a relative POSIX path; `None` when it climbs above its start.
fn normalize(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

/// Rejects any release archive that is not one `Muteki.app` tree of plain
/// files, directories and bundle-internal symlinks, before it is extracted.
pub fn assert_zip_safe(bytes: &[u8]) -> Result<()> {
    let invalid = |message: &str| UpdateError::new("update.archive_invalid", message);
    let unsafe_ = |message: &str| UpdateError::new("update.archive_unsafe", message);
    let layout = || invalid("Release archive uses an unsupported ZIP layout.");
    if bytes.len() < 22 {
        return Err(layout());
    }
    let start = bytes.len().saturating_sub(65557);
    let eocd = (start..=bytes.len() - 22)
        .rev()
        .find(|&i| le32(bytes, i) == Some(0x0605_4b50))
        .ok_or_else(layout)?;
    let field = |at: usize| le16(bytes, eocd + at).ok_or_else(layout);
    if field(4)? != 0 || field(6)? != 0 || eocd + 22 + field(20)? as usize != bytes.len() {
        return Err(layout());
    }
    let count = field(10)?;
    let size = le32(bytes, eocd + 12).ok_or_else(layout)? as usize;
    let central_start = le32(bytes, eocd + 16).ok_or_else(layout)? as usize;
    if field(8)? != count {
        return Err(invalid("Release archive entry counts do not match."));
    }
    if central_start.checked_add(size) != Some(eocd) {
        return Err(invalid(
            "Release archive central directory bounds are invalid.",
        ));
    }
    let directory = || invalid("Release archive directory is invalid.");
    let mut offset = central_start;
    let mut expanded = 0u64;
    let mut seen = HashSet::new();
    let mut kinds: HashMap<String, u64> = HashMap::new();
    let mut links: Vec<(String, String)> = Vec::new();
    for _ in 0..count {
        let at = |delta: usize, width: u8| {
            if width == 2 {
                le16(bytes, offset + delta)
            } else {
                le32(bytes, offset + delta)
            }
        };
        if at(0, 4) != Some(0x0201_4b50) {
            return Err(directory());
        }
        let (flags, method) = (
            at(8, 2).ok_or_else(directory)?,
            at(10, 2).ok_or_else(directory)?,
        );
        let central_crc = at(16, 4).ok_or_else(directory)?;
        let compressed = at(20, 4).ok_or_else(directory)?;
        let uncompressed = at(24, 4).ok_or_else(directory)?;
        let name_len = at(28, 2).ok_or_else(directory)? as usize;
        let extra_len = at(30, 2).ok_or_else(directory)? as usize;
        let comment_len = at(32, 2).ok_or_else(directory)? as usize;
        let mode = at(38, 4).ok_or_else(directory)? >> 16;
        let local = at(42, 4).ok_or_else(directory)? as usize;
        let name_bytes = bytes
            .get(offset + 46..offset + 46 + name_len)
            .ok_or_else(directory)?;
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| unsafe_("Release archive contains an unsafe or unsupported path."))?;
        let kind = mode & 0o170000;
        let drive = name.len() >= 2
            && name.as_bytes()[0].is_ascii_alphabetic()
            && name.as_bytes()[1] == b':';
        if name.is_empty()
            || name.contains('\\')
            || name.contains('\0')
            || name.starts_with('/')
            || drive
            || name.split('/').any(|p| p == ".." || p == ".")
            || (kind != 0 && ![0o100000, 0o040000, 0o120000].contains(&kind))
            || ![0, 8].contains(&method)
            || flags & 1 != 0
            || uncompressed > MAX_ASSET
        {
            return Err(unsafe_(
                "Release archive contains an unsafe or unsupported path.",
            ));
        }
        let canonical = name.strip_suffix('/').unwrap_or(name).to_owned();
        let parts: Vec<_> = canonical.split('/').collect();
        if parts[0] != "Muteki.app" || parts.iter().any(|p| p.is_empty()) {
            return Err(unsafe_(
                "Release archive contains an unexpected top-level path.",
            ));
        }
        let entry = || invalid("Release archive entry path or bounds are invalid.");
        if local >= central_start || le32(bytes, local) != Some(0x0403_4b50) {
            return Err(invalid("Release archive local entry is invalid."));
        }
        let local_name_len = le16(bytes, local + 26).ok_or_else(entry)? as usize;
        let local_extra_len = le16(bytes, local + 28).ok_or_else(entry)? as usize;
        let local_name = bytes
            .get(local + 30..local + 30 + local_name_len)
            .ok_or_else(entry)?;
        let data = local + 30 + local_name_len + local_extra_len;
        let local_flags = le16(bytes, local + 6).ok_or_else(entry)?;
        let local_method = le16(bytes, local + 8).ok_or_else(entry)?;
        let local_crc = le32(bytes, local + 14).ok_or_else(entry)?;
        let local_compressed = le32(bytes, local + 18).ok_or_else(entry)?;
        let local_uncompressed = le32(bytes, local + 22).ok_or_else(entry)?;
        if local_name != name_bytes
            || local_flags != flags
            || local_method != method
            || data as u64 + compressed > central_start as u64
            || (flags & 8 == 0
                && (local_crc != central_crc
                    || local_compressed != compressed
                    || local_uncompressed != uncompressed))
        {
            return Err(entry());
        }
        expanded += uncompressed;
        if expanded > MAX_EXPANDED {
            return Err(unsafe_("Release archive expands beyond the allowed size."));
        }
        if kind == 0o120000 {
            if uncompressed > 4096 {
                return Err(unsafe_("Release archive symlink target is too long."));
            }
            let raw = &bytes[data..data + compressed as usize];
            let target = if method == 0 {
                raw.to_vec()
            } else {
                miniz_oxide::inflate::decompress_to_vec_with_limit(raw, 4096)
                    .map_err(|_| invalid("Release archive symlink target is invalid."))?
            };
            if target.len() as u64 != uncompressed {
                return Err(invalid("Release archive symlink target is invalid."));
            }
            links.push((
                canonical.clone(),
                String::from_utf8_lossy(&target).into_owned(),
            ));
        }
        if !seen.insert(canonical.clone()) {
            return Err(unsafe_("Release archive contains duplicate paths."));
        }
        let effective = if kind != 0 {
            kind
        } else if name.ends_with('/') {
            0o040000
        } else {
            0o100000
        };
        kinds.insert(canonical, effective);
        offset += 46 + name_len + extra_len + comment_len;
    }
    if offset != eocd {
        return Err(invalid("Release archive directory size is invalid."));
    }
    let symlinks: HashSet<&str> = links.iter().map(|(name, _)| name.as_str()).collect();
    for name in kinds.keys() {
        let segments: Vec<_> = name.split('/').collect();
        for i in 1..segments.len() {
            let ancestor = segments[..i].join("/");
            if symlinks.contains(ancestor.as_str()) {
                return Err(unsafe_("Release archive writes through a symlink path."));
            }
            if kinds.get(&ancestor).is_some_and(|kind| *kind != 0o040000) {
                return Err(unsafe_(
                    "Release archive contains a file used as a directory.",
                ));
            }
        }
    }
    for (name, target) in &links {
        let parent = name.rsplit_once('/').map_or("", |(parent, _)| parent);
        let resolved = normalize(&format!("{parent}/{target}"));
        if target.is_empty()
            || target.starts_with('/')
            || !resolved.is_some_and(|r| r.starts_with("Muteki.app/"))
        {
            return Err(unsafe_(
                "Release archive symlink escapes its application bundle.",
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub struct Identity {
    pub id: String,
    pub version: String,
    pub executable: String,
    pub team: Option<String>,
}

impl Identity {
    pub fn to_json(&self) -> Value {
        serde_json::json!({"id": self.id, "version": self.version, "executable": self.executable, "team": self.team})
    }

    pub fn from_json(value: &Value) -> Option<Identity> {
        Some(Identity {
            id: value["id"].as_str()?.to_owned(),
            version: value["version"].as_str()?.to_owned(),
            executable: value["executable"].as_str()?.to_owned(),
            team: value["team"].as_str().map(str::to_owned),
        })
    }
}

fn output(command: &mut Command) -> std::result::Result<std::process::Output, String> {
    command.output().map_err(|e| e.to_string())
}

/// Code signature, bundle id, version, executable and architecture of an
/// application bundle, as macOS reports them.
pub fn bundle_identity(root: &Path, expected_arch: &str) -> Result<Identity> {
    let fail = |message: String| UpdateError::new("update.identity_invalid", message);
    if root.extension().is_none_or(|e| e != "app") {
        return Err(fail(
            "An update candidate must be a macOS .app bundle.".into(),
        ));
    }
    let verify = output(
        crate::shell_env::command("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(format!("-R={}", include_str!("../../../assets/macos-release.requirement")))
            .arg(root),
    )
    .map_err(&fail)?;
    if !verify.status.success() {
        return Err(fail(format!(
            "Code signature is invalid: {}",
            String::from_utf8_lossy(&verify.stderr).trim()
        )));
    }
    let details = output(
        crate::shell_env::command("/usr/bin/codesign")
            .args(["-dv", "--verbose=4"])
            .arg(root),
    )
    .map_err(&fail)?;
    if !details.status.success() {
        return Err(fail(
            String::from_utf8_lossy(&details.stderr).trim().to_owned(),
        ));
    }
    let team = String::from_utf8_lossy(&details.stderr)
        .lines()
        .find_map(|line| line.strip_prefix("TeamIdentifier=").map(str::to_owned))
        .filter(|team| !team.is_empty() && team != "not set");
    if team.is_none() {
        return Err(fail(
            "Candidate must be signed with a stable signing identity.".into(),
        ));
    }
    let plist = root.join("Contents/Info.plist");
    let read = |key: &str| -> Result<String> {
        let out = output(
            crate::shell_env::command("/usr/libexec/PlistBuddy")
                .args(["-c", &format!("Print :{key}")])
                .arg(&plist),
        )
        .map_err(&fail)?;
        if !out.status.success() {
            return Err(fail(format!("Info.plist has no {key}.")));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    let executable = read("CFBundleExecutable")?;
    if executable.is_empty() || executable.contains('/') {
        return Err(fail("Invalid bundle executable.".into()));
    }
    let (id, version) = (
        read("CFBundleIdentifier")?,
        read("CFBundleShortVersionString")?,
    );
    let binary_arch = if expected_arch == "x64" {
        "x86_64"
    } else {
        expected_arch
    };
    let archs = output(
        crate::shell_env::command("/usr/bin/lipo")
            .arg("-archs")
            .arg(root.join("Contents/MacOS").join(&executable)),
    )
    .map_err(&fail)?;
    if !archs.status.success()
        || !String::from_utf8_lossy(&archs.stdout)
            .split_whitespace()
            .any(|a| a == binary_arch)
    {
        return Err(fail(format!(
            "Application executable does not contain {binary_arch}."
        )));
    }
    let manifest: Value = std::fs::read(root.join("Contents/Resources/runtime/manifest.json"))
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .ok_or_else(|| fail("Application runtime manifest is missing.".into()))?;
    if manifest["platform"] != "darwin" || manifest["arch"] != expected_arch {
        return Err(fail(format!(
            "Application runtime does not match darwin/{expected_arch}."
        )));
    }
    Ok(Identity {
        id,
        version,
        executable,
        team,
    })
}

pub fn host_arch() -> &'static str {
    if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    }
}

pub fn copy_bundle(source: &Path, destination: &Path) -> Result<()> {
    let out = output(
        crate::shell_env::command("/usr/bin/ditto")
            .args(["--rsrc", "--extattr"])
            .arg(source)
            .arg(destination),
    )
    .map_err(|e| UpdateError::new("update.copy_failed", e))?;
    if !out.status.success() {
        return Err(UpdateError::new(
            "update.copy_failed",
            format!(
                "Cannot copy the application: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    Ok(())
}

/// Replaces `file` with `value` so a reader sees the old or the new journal,
/// never a partial one.
pub fn atomic_json(file: &Path, value: &Value) -> std::io::Result<()> {
    let temporary = file.with_extension("json.tmp");
    let _ = std::fs::remove_file(&temporary);
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    out.write_all(
        serde_json::to_string_pretty(value)
            .unwrap_or_default()
            .as_bytes(),
    )?;
    out.write_all(b"\n")?;
    out.sync_all()?;
    std::fs::rename(&temporary, file)?;
    std::fs::File::open(file.parent().unwrap_or(Path::new(".")))?.sync_all()
}

pub fn read_json(file: &Path) -> std::io::Result<Value> {
    serde_json::from_slice(&std::fs::read(file)?)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

pub fn pid_alive(pid: i64) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    pid > 0
        && pid <= i32::MAX as i64
        && (unsafe { libc::kill(pid as i32, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

pub fn helper_running(journal: &Value) -> bool {
    journal["helperPid"].as_i64().is_some_and(pid_alive)
}

/// Creates (or tightens) a private directory that is not a symlink.
pub fn private_dir(path: &Path) -> Result<()> {
    let fail = |message: String| UpdateError::new("update.cache_unavailable", message);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| fail(format!("{}: {e}", path.display())))?;
    let meta = std::fs::symlink_metadata(path).map_err(|e| fail(e.to_string()))?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(fail(format!(
            "{} is not a private directory.",
            path.display()
        )));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| fail(e.to_string()))
}

/// The `.app` that contains `executable`, when it runs from a bundle.
pub fn bundle_of(executable: &Path) -> Option<PathBuf> {
    let contents = executable.parent()?.parent()?;
    (contents.file_name()? == "Contents")
        .then(|| contents.parent().map(Path::to_path_buf))
        .flatten()
}

pub fn copy_tree_except(
    source: &Path,
    destination: &Path,
    skip: &[&str],
) -> std::result::Result<(), String> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(destination)
        .map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(source).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        if skip.iter().any(|s| name == *s) {
            continue;
        }
        let out = crate::shell_env::command("/usr/bin/ditto")
            .arg(entry.path())
            .arg(destination.join(&name))
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!(
                "Cannot copy {}: {}",
                entry.path().display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }
    Ok(())
}

pub fn remove_tree_except(directory: &Path, skip: &[&str]) -> std::result::Result<(), String> {
    for entry in std::fs::read_dir(directory).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if skip.iter().any(|s| entry.file_name() == *s) {
            continue;
        }
        let path = entry.path();
        let result = if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        result.map_err(|e| format!("Cannot remove {}: {e}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stored (uncompressed) ZIP with the given (name, unix mode, content) entries.
    fn zip(entries: &[(&str, u32, &[u8])]) -> Vec<u8> {
        let (mut out, mut central) = (Vec::new(), Vec::new());
        for (name, mode, data) in entries {
            let offset = out.len() as u32;
            let crc = crc32(data);
            let mut local = vec![];
            local.extend(0x0403_4b50u32.to_le_bytes());
            local.extend([20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            local.extend(crc.to_le_bytes());
            local.extend((data.len() as u32).to_le_bytes());
            local.extend((data.len() as u32).to_le_bytes());
            local.extend((name.len() as u16).to_le_bytes());
            local.extend(0u16.to_le_bytes());
            local.extend(name.as_bytes());
            local.extend(*data);
            out.extend(local);
            central.extend(0x0201_4b50u32.to_le_bytes());
            central.extend([20, 3, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            central.extend(crc.to_le_bytes());
            central.extend((data.len() as u32).to_le_bytes());
            central.extend((data.len() as u32).to_le_bytes());
            central.extend((name.len() as u16).to_le_bytes());
            central.extend([0, 0, 0, 0, 0, 0, 0, 0]);
            central.extend((mode << 16).to_le_bytes());
            central.extend(offset.to_le_bytes());
            central.extend(name.as_bytes());
        }
        let start = out.len() as u32;
        out.extend(&central);
        out.extend(0x0605_4b50u32.to_le_bytes());
        out.extend([0, 0, 0, 0]);
        out.extend((entries.len() as u16).to_le_bytes());
        out.extend((entries.len() as u16).to_le_bytes());
        out.extend((central.len() as u32).to_le_bytes());
        out.extend(start.to_le_bytes());
        out.extend([0, 0]);
        out
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for byte in data {
            crc ^= *byte as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    #[test]
    fn archives_are_one_application_without_escapes() {
        let good = zip(&[
            ("Muteki.app/", 0o040755, b""),
            ("Muteki.app/Contents/Info.plist", 0o100644, b"x"),
            ("Muteki.app/Contents/link", 0o120755, b"Info.plist"),
        ]);
        assert_eq!(assert_zip_safe(&good), Ok(()));
        let code =
            |entries: &[(&str, u32, &[u8])]| assert_zip_safe(&zip(entries)).unwrap_err().code;
        assert_eq!(
            code(&[("Other.app/a", 0o100644, b"x")]),
            "update.archive_unsafe"
        );
        assert_eq!(
            code(&[("Muteki.app/../evil", 0o100644, b"x")]),
            "update.archive_unsafe"
        );
        assert_eq!(
            code(&[("/Muteki.app/a", 0o100644, b"x")]),
            "update.archive_unsafe"
        );
        assert_eq!(
            code(&[
                ("Muteki.app/a", 0o100644, b"x"),
                ("Muteki.app/a", 0o100644, b"y")
            ]),
            "update.archive_unsafe"
        );
        assert_eq!(
            code(&[("Muteki.app/l", 0o120755, b"../../etc")]),
            "update.archive_unsafe"
        );
        assert_eq!(
            code(&[("Muteki.app/l", 0o120755, b"/etc/passwd")]),
            "update.archive_unsafe"
        );
        assert_eq!(
            code(&[
                ("Muteki.app/l", 0o120755, b"Contents"),
                ("Muteki.app/l/x", 0o100644, b"x")
            ]),
            "update.archive_unsafe"
        );
        assert_eq!(
            code(&[
                ("Muteki.app/f", 0o100644, b"x"),
                ("Muteki.app/f/x", 0o100644, b"x")
            ]),
            "update.archive_unsafe"
        );
        assert_eq!(
            code(&[("Muteki.app/dev", 0o020644, b"")]),
            "update.archive_unsafe"
        );
        let mut truncated = good.clone();
        truncated.pop();
        assert_eq!(
            assert_zip_safe(&truncated).unwrap_err().code,
            "update.archive_invalid"
        );
        assert_eq!(
            assert_zip_safe(b"not a zip").unwrap_err().code,
            "update.archive_invalid"
        );
    }

    #[test]
    fn versions_and_journal() {
        assert!(compare_versions("0.4.10", "0.4.9").is_gt());
        assert!(compare_versions("0.4.8", "0.4.8").is_eq());
        assert!(semantic("0.4.8") && !semantic("v0.4.8") && !semantic("0.4"));
        let dir =
            std::env::temp_dir().join(format!("muteki-bundle-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let journal = dir.join("t.json");
        atomic_json(&journal, &serde_json::json!({"phase":"prepared"})).unwrap();
        assert_eq!(read_json(&journal).unwrap()["phase"], "prepared");
        assert_eq!(
            std::fs::metadata(&journal).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
