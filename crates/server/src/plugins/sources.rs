//! Package sources: a local directory, an archive, or a pinned Git revision,
//! fetched into a work directory and normalized into a link-free copy whose
//! bytes are the ones that get inspected and installed.

use super::inspect::{PluginError, Result};
use std::{
    io::Read,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const MAX_FILES: usize = 12_000;
const MAX_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TAR_BYTES: u64 = MAX_BYTES + MAX_FILES as u64 * 1024 + 1024 * 1024;
const IGNORED: [&str; 2] = [".git", "__pycache__"];

fn inaccessible() -> PluginError {
    PluginError::new("chat_plugin.source_inaccessible", "无法读取来源目录")
}

/// Copy a package without following links into credentials, other packages
/// or the operator home. Limits apply to the whole source, ignored caches
/// included, as they are what the copy has to walk.
pub fn copy_package(source: &Path, target: &Path) -> Result<()> {
    let mut size = 0u64;
    let mut count = 0usize;
    let mut entries = Vec::new();
    for entry in walkdir::WalkDir::new(source).min_depth(1).follow_links(false) {
        let entry = entry.map_err(|_| inaccessible())?;
        count += 1;
        if count > MAX_FILES {
            return Err(PluginError::invalid("插件文件数量超过 12000"));
        }
        let kind = entry.file_type();
        if kind.is_symlink() {
            return Err(PluginError::invalid("插件包不能包含符号链接，请导入完整文件副本"));
        }
        if kind.is_file() {
            size += entry.metadata().map_err(|_| inaccessible())?.len();
        }
        entries.push(entry);
    }
    if size > MAX_BYTES {
        return Err(PluginError::invalid("插件包超过 64 MiB"));
    }
    std::fs::create_dir_all(target).map_err(|_| inaccessible())?;
    for entry in entries {
        let relative = entry.path().strip_prefix(source).unwrap_or(entry.path());
        if relative.components().any(|c| IGNORED.iter().any(|name| c.as_os_str() == *name)) {
            continue;
        }
        let destination = target.join(relative);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&destination).map_err(|_| inaccessible())?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = destination.parent() {
                std::fs::create_dir_all(parent).map_err(|_| inaccessible())?;
            }
            std::fs::copy(entry.path(), &destination).map_err(|_| inaccessible())?;
        }
    }
    Ok(())
}

/// A local directory chosen by the user, resolved strictly.
pub fn resolve_local_dir(raw: &str) -> Result<PathBuf> {
    let path = raw.trim();
    if path.is_empty() {
        return Err(PluginError::new("chat_plugin.source_missing", "请填写来源目录路径"));
    }
    let expanded = match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(rest),
        None if path == "~" => std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default(),
        None => PathBuf::from(path),
    };
    let origin = std::fs::canonicalize(&expanded).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => PluginError::new("chat_plugin.source_not_found", "来源目录不存在"),
        std::io::ErrorKind::NotADirectory => PluginError::new("chat_plugin.source_not_directory", "来源路径不是目录"),
        _ => PluginError::new("chat_plugin.source_inaccessible", "无法访问来源目录"),
    })?;
    if !origin.is_dir() {
        return Err(PluginError::new("chat_plugin.source_not_directory", "来源路径不是目录"));
    }
    Ok(origin)
}

fn source_not_found() -> PluginError {
    PluginError::new("chat_plugin.source_not_found", "来源文件或目录不存在")
}

fn install_failed() -> PluginError {
    PluginError::new("chat_plugin.install_failed", "插件导入失败，请检查来源")
}

fn safe_member(name: &str) -> bool {
    let path = Path::new(name);
    !name.starts_with('/') && !path.components().any(|c| matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
}

fn write_member(target: &Path, name: &str, reader: &mut dyn Read, executable: bool, remaining: &mut u64) -> Result<()> {
    let destination = target.join(name);
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|_| install_failed())?;
    }
    let mut file = std::fs::File::create(&destination).map_err(|_| install_failed())?;
    let copied = std::io::copy(&mut reader.take(*remaining + 1), &mut file).map_err(|_| install_failed())?;
    if copied > *remaining {
        return Err(PluginError::invalid("插件包超过 64 MiB"));
    }
    *remaining -= copied;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if executable { 0o755 } else { 0o644 };
        let _ = std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    let _ = executable;
    Ok(())
}

fn unpack_zip(archive: &Path, target: &Path) -> Result<bool> {
    let file = std::fs::File::open(archive).map_err(|_| source_not_found())?;
    let Ok(mut zip) = zip::ZipArchive::new(file) else { return Ok(false) };
    if zip.len() > MAX_FILES {
        return Err(PluginError::invalid("插件文件数量超过 12000"));
    }
    let mut remaining = MAX_BYTES;
    for index in 0..zip.len() {
        let entry = zip.by_index_raw(index).map_err(|_| install_failed())?;
        remaining = remaining.checked_sub(entry.size()).ok_or_else(|| PluginError::invalid("插件包超过 64 MiB"))?;
        if !safe_member(entry.name()) || entry.unix_mode().is_some_and(|mode| mode & 0o170000 == 0o120000) {
            return Err(install_failed());
        }
    }
    let mut remaining = MAX_BYTES;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|_| install_failed())?;
        let name = entry.name().to_owned();
        if entry.is_dir() {
            std::fs::create_dir_all(target.join(&name)).map_err(|_| install_failed())?;
            continue;
        }
        let executable = entry.unix_mode().is_some_and(|mode| mode & 0o111 != 0);
        write_member(target, &name, &mut entry, executable, &mut remaining)?;
    }
    Ok(true)
}

fn unpack_tar(archive: &Path, target: &Path) -> Result<bool> {
    let mut head = [0u8; 512];
    let read = std::fs::File::open(archive).and_then(|mut f| f.read(&mut head)).map_err(|_| source_not_found())?;
    let gzip = read >= 2 && head[..2] == [0x1f, 0x8b];
    let lower = archive.to_string_lossy().to_lowercase();
    let plain = read > 262 && &head[257..262] == b"ustar";
    if !gzip && !plain && ![".tar.gz", ".tgz", ".tar"].iter().any(|s| lower.ends_with(s)) {
        return Ok(false);
    }
    let open = || -> Result<std::io::Take<Box<dyn Read>>> {
        let file = std::fs::File::open(archive).map_err(|_| source_not_found())?;
        let input: Box<dyn Read> = if gzip { Box::new(flate2::read::GzDecoder::new(file)) } else { Box::new(file) };
        Ok(input.take(MAX_TAR_BYTES))
    };
    let mut checked = tar::Archive::new(open()?);
    let mut remaining = MAX_BYTES;
    for (index, entry) in checked.entries().map_err(|_| install_failed())?.enumerate() {
        let entry = entry.map_err(|_| install_failed())?;
        if index >= MAX_FILES {
            return Err(PluginError::invalid("插件文件数量超过 12000"));
        }
        remaining = remaining.checked_sub(entry.size()).ok_or_else(|| PluginError::invalid("插件包超过 64 MiB"))?;
        let name = entry.path().map_err(|_| install_failed())?.to_string_lossy().into_owned();
        let kind = entry.header().entry_type();
        if !safe_member(&name) || kind.is_symlink() || kind.is_hard_link() {
            return Err(install_failed());
        }
    }
    if checked.into_inner().limit() == 0 { return Err(install_failed()); }
    let mut archive = tar::Archive::new(open()?);
    let mut remaining = MAX_BYTES;
    for entry in archive.entries().map_err(|_| install_failed())? {
        let mut entry = entry.map_err(|_| install_failed())?;
        let name = entry.path().map_err(|_| install_failed())?.to_string_lossy().into_owned();
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            std::fs::create_dir_all(target.join(&name)).map_err(|_| install_failed())?;
        } else if kind.is_file() {
            let executable = entry.header().mode().is_ok_and(|mode| mode & 0o111 != 0);
            write_member(target, &name, &mut entry, executable, &mut remaining)?;
        } else if !matches!(kind, tar::EntryType::XGlobalHeader | tar::EntryType::XHeader) {
            return Err(install_failed());
        }
    }
    Ok(true)
}

/// Unpack a zip, tar or tar.gz archive, refusing absolute or escaping paths
/// and link members. A single top-level directory without a root manifest
/// becomes the package root.
pub fn unpack(archive: &Path, target: &Path) -> Result<PathBuf> {
    if !archive.is_file() {
        return Err(source_not_found());
    }
    std::fs::create_dir_all(target).map_err(|_| install_failed())?;
    if !unpack_zip(archive, target)? && !unpack_tar(archive, target)? {
        return Err(install_failed());
    }
    let entries: Vec<PathBuf> = std::fs::read_dir(target)
        .map_err(|_| install_failed())?
        .flatten()
        .map(|e| e.path())
        .collect();
    if entries.len() == 1 && entries[0].is_dir() && !target.join("plugin.json").is_file() {
        return Ok(entries[0].clone());
    }
    Ok(target.to_path_buf())
}

fn run(command: &mut Command, timeout: Duration) -> std::io::Result<std::process::Output> {
    let mut child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output();
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "git timed out"));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Clone a Git URL at a pinned commit or tag; branches are refused.
pub fn fetch_git(url: &str, reference: &str, workdir: &Path) -> Result<PathBuf> {
    if url.trim().is_empty() {
        return Err(PluginError::new("chat_plugin.invalid_source", "插件来源无效"));
    }
    let reference = reference.trim();
    let unpinned = || PluginError::new("chat_plugin.unpinned_git_ref", "请提供固定的 Git commit 或 tag，不支持裸分支");
    if reference.is_empty() {
        return Err(unpinned());
    }
    let fetch_failed = || PluginError::new("chat_plugin.git_fetch_failed", "Git 拉取失败，请检查地址与固定版本");
    let checkout = workdir.join("pkg");
    let git = |args: &[&str], timeout: u64| {
        run(crate::host_process::std_command("git").args(args).env("GIT_TERMINAL_PROMPT", "0"), Duration::from_secs(timeout))
    };
    let path = checkout.to_string_lossy().into_owned();
    let cloned = git(&["clone", "--quiet", "--no-checkout", url.trim(), &path], 120).map_err(|_| fetch_failed())?;
    if !cloned.status.success() {
        return Err(fetch_failed());
    }
    let checked_out = git(&["-C", &path, "checkout", "--quiet", reference], 60).map_err(|_| fetch_failed())?;
    if !checked_out.status.success() {
        return Err(fetch_failed());
    }
    let commit = reference.len() >= 7 && reference.len() <= 40 && reference.chars().all(|c| c.is_ascii_hexdigit());
    let tag = git(&["-C", &path, "show-ref", "--verify", "--quiet", &format!("refs/tags/{reference}")], 30)
        .is_ok_and(|output| output.status.success());
    if !commit && !tag {
        return Err(unpinned());
    }
    let _ = std::fs::remove_dir_all(checkout.join(".git"));
    Ok(checkout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_skip_caches_and_refuse_links() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("src");
        std::fs::create_dir_all(source.join(".git")).unwrap();
        std::fs::create_dir_all(source.join("__pycache__")).unwrap();
        std::fs::write(source.join(".git/HEAD"), "ref").unwrap();
        std::fs::write(source.join("__pycache__/x.pyc"), "c").unwrap();
        std::fs::write(source.join("plugin.json"), "{}").unwrap();
        copy_package(&source, &dir.path().join("out")).unwrap();
        assert!(dir.path().join("out/plugin.json").is_file());
        assert!(!dir.path().join("out/.git").exists());
        assert!(!dir.path().join("out/__pycache__").exists());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/hosts", source.join("link")).unwrap();
            let error = copy_package(&source, &dir.path().join("out2")).unwrap_err();
            assert!(error.message.contains("符号链接"));
        }
    }

    #[test]
    fn archives_unpack_to_a_single_root_and_refuse_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("p.tar.gz");
        {
            let file = std::fs::File::create(&archive).unwrap();
            let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(file, flate2::Compression::fast()));
            let mut header = tar::Header::new_gnu();
            header.set_size(2);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, "pack/plugin.json", &b"{}"[..]).unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }
        let root = unpack(&archive, &dir.path().join("out")).unwrap();
        assert!(root.ends_with("pack"));
        assert!(root.join("plugin.json").is_file());

        let zip_path = dir.path().join("bad.zip");
        {
            let mut writer = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
            writer.start_file("../escape.txt", zip::write::SimpleFileOptions::default()).unwrap();
            std::io::Write::write_all(&mut writer, b"x").unwrap();
            writer.finish().unwrap();
        }
        assert_eq!(unpack(&zip_path, &dir.path().join("out2")).unwrap_err().code, "chat_plugin.install_failed");
        assert!(!dir.path().join("escape.txt").exists());
    }

    #[test]
    fn local_sources_are_resolved_strictly() {
        assert_eq!(resolve_local_dir("  ").unwrap_err().code, "chat_plugin.source_missing");
        assert_eq!(resolve_local_dir("/definitely/not/here").unwrap_err().code, "chat_plugin.source_not_found");
        let file = tempfile::NamedTempFile::new().unwrap();
        assert_eq!(resolve_local_dir(&file.path().to_string_lossy()).unwrap_err().code, "chat_plugin.source_not_directory");
    }
}
