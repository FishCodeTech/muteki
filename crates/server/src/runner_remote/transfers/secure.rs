use super::{Manifest, file_digest};
use std::{
    fs::File,
    io::{Seek, SeekFrom},
    path::Path,
};

pub(super) fn parts(path: &str) -> Result<Vec<&str>, String> {
    if path.is_empty()
        || path.len() > 4096
        || (path.contains(['\\', '\0', ':']) || path.chars().any(char::is_control))
    {
        return Err("Invalid workspace-relative transfer path".into());
    }
    let parts: Vec<_> = path.split('/').collect();
    if parts
        .iter()
        .any(|part| part.is_empty() || matches!(*part, "." | "..") || part.len() > 255)
    {
        return Err("Transfer path must contain only ordinary relative segments".into());
    }
    if parts.first() == Some(&"homes")
        || parts.iter().any(|part| {
            matches!(
                *part,
                "_secrets" | ".muteki-secrets" | "account-projections" | ".muteki-browser-profile"
            )
        })
    {
        return Err(
            "Private credentials and browser profiles require their scoped transfer channel".into(),
        );
    }
    if parts
        .iter()
        .any(|part| part.starts_with(".muteki-transfer-") || part.starts_with(".muteki-private-"))
    {
        return Err("Transfer temporary names are reserved".into());
    }
    Ok(parts)
}

#[cfg(test)]
pub(super) fn install(root: &Path, manifest: &Manifest, source: &mut File) -> Result<(), String> {
    install_guarded(root, manifest, source, || Ok(()))
}
#[cfg(unix)]
pub(super) fn install_guarded<F, G>(
    root: &Path,
    manifest: &Manifest,
    source: &mut File,
    authorize: F,
) -> Result<(), String>
where
    F: Fn() -> Result<G, String>,
{
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::fs::{OpenOptionsExt, PermissionsExt},
        },
    };
    manifest.validate()?;
    let preparing = authorize()?;
    let parts = parts(&manifest.path)?;
    let mut directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(root)
        .map_err(|e| e.to_string())?;
    for segment in &parts[..parts.len() - 1] {
        let name = CString::new(*segment).map_err(|e| e.to_string())?;
        if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o755) } < 0
            && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    let name = CString::new(*parts.last().unwrap()).map_err(|e| e.to_string())?;
    let existing = || -> Result<bool, String> {
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            return if error.kind() == std::io::ErrorKind::NotFound {
                Ok(false)
            } else {
                Err(error.to_string())
            };
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_file()
            || metadata.len() != manifest.size
            || file_digest(&mut file)? != manifest.sha256
            || metadata.permissions().mode() & 0o777 != manifest.mode
        {
            return Err(
                "Destination conflicts with the transfer; existing content was preserved".into(),
            );
        }
        Ok(true)
    };
    let temporary =
        CString::new(format!(".muteki-transfer-{}", manifest.id)).map_err(|e| e.to_string())?;
    // A prior interrupted copy has a deterministic, reserved name. Unlinking it
    // never follows a symlink and cannot remove the already-published final name.
    if unsafe { libc::unlinkat(directory.as_raw_fd(), temporary.as_ptr(), 0) } < 0
        && std::io::Error::last_os_error().kind() != std::io::ErrorKind::NotFound
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    drop(preparing);
    if existing()? {
        let _publication = authorize()?;
        return Ok(());
    }
    let preparing = authorize()?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    drop(preparing);
    let result = (|| -> Result<(), String> {
        let mut target = unsafe { File::from_raw_fd(fd) };
        source.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        use sha2::{Digest, Sha256};
        use std::io::{Read, Write};
        let mut copied = 0u64;
        let mut hash = Sha256::new();
        let mut block = [0u8; 64 * 1024];
        loop {
            let count = source.read(&mut block).map_err(|e| e.to_string())?;
            if count == 0 {
                break;
            }
            copied = copied
                .checked_add(count as u64)
                .filter(|size| *size <= manifest.size)
                .ok_or("Source grew during transfer installation")?;
            hash.update(&block[..count]);
            target
                .write_all(&block[..count])
                .map_err(|e| e.to_string())?;
        }
        if copied != manifest.size || format!("{:x}", hash.finalize()) != manifest.sha256 {
            return Err("Source changed during transfer installation".into());
        }
        target
            .set_permissions(std::fs::Permissions::from_mode(manifest.mode))
            .map_err(|e| e.to_string())?;
        target.sync_all().map_err(|e| e.to_string())?;
        let _publication = authorize()?;
        // The lifecycle guard covers publication only, never the large copy.
        // linkat creates the final name atomically and refuses an existing name.
        if unsafe {
            libc::linkat(
                directory.as_raw_fd(),
                temporary.as_ptr(),
                directory.as_raw_fd(),
                name.as_ptr(),
                0,
            )
        } < 0
        {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists || !existing()? {
                return Err(error.to_string());
            }
        }
        directory.sync_all().map_err(|e| e.to_string())?;
        Ok(())
    })();
    unsafe { libc::unlinkat(directory.as_raw_fd(), temporary.as_ptr(), 0) };
    result
}
#[cfg(not(unix))]
pub(super) fn install_guarded<F, G>(
    _: &Path,
    _: &Manifest,
    _: &mut File,
    _: F,
) -> Result<(), String>
where
    F: Fn() -> Result<G, String>,
{
    Err("Secure remote file installation is not available on this platform".into())
}
