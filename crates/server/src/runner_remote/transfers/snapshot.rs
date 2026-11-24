//! Immutable artifact staging from a node-owned workspace; never from caller host paths.
use super::{CHUNK_BYTES, Manifest, Scope, Transfers, digest, secure};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

#[cfg(unix)]
fn source(root: &Path, path: &str) -> Result<File, String> {
    use std::{
        ffi::CString,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::fs::{MetadataExt, OpenOptionsExt},
        },
    };
    let parts = secure::parts(path)?;
    let mut directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)
        .map_err(|e| e.to_string())?;
    for (index, part) in parts.iter().enumerate() {
        let name = CString::new(*part).map_err(|e| e.to_string())?;
        let last = index + 1 == parts.len();
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | if last {
                libc::O_NONBLOCK
            } else {
                libc::O_DIRECTORY
            };
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    let metadata = directory.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err("Artifact must be an ordinary file without hard links".into());
    }
    Ok(directory)
}
#[cfg(not(unix))]
fn source(_: &Path, _: &str) -> Result<File, String> {
    Err("Secure artifact snapshots are not supported on this platform".into())
}

impl Transfers {
    pub fn snapshot<F>(
        &self,
        id: &str,
        scope: &Scope,
        workspace: &Path,
        path: &str,
        max_bytes: u64,
        authorize: F,
    ) -> Result<Manifest, String>
    where
        F: Fn() -> Result<(), String>,
    {
        authorize()?;
        if max_bytes == 0 || max_bytes > self.quota {
            return Err("Artifact bound exceeds staging quota".into());
        }
        let mut file = source(workspace, path)?;
        let size = file.metadata().map_err(|e| e.to_string())?.len();
        if size > max_bytes {
            return Err("Artifact exceeds its authorized size bound".into());
        }
        let mut hash = Sha256::new();
        let mut block = vec![0; CHUNK_BYTES];
        let mut measured = 0u64;
        loop {
            authorize()?;
            let count = file.read(&mut block).map_err(|e| e.to_string())?;
            if count == 0 {
                break;
            }
            measured = measured
                .checked_add(count as u64)
                .ok_or("Artifact size overflow")?;
            if measured > size {
                return Err("Artifact changed while hashing".into());
            }
            hash.update(&block[..count]);
        }
        if measured != size {
            return Err("Artifact changed while hashing".into());
        }
        let manifest = Manifest {
            id: id.into(),
            scope: scope.clone(),
            path: path.into(),
            size,
            sha256: format!("{:x}", hash.finalize()),
            mode: 0o600,
        };
        authorize()?;
        let row = self.begin(&manifest)?;
        let mut offset = row["received"].as_u64().ok_or("Snapshot cursor missing")?;
        if offset > size {
            return Err("Snapshot cursor exceeds file size".into());
        }
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| e.to_string())?;
        while offset < size {
            authorize()?;
            let length = (size - offset).min(CHUNK_BYTES as u64) as usize;
            file.read_exact(&mut block[..length])
                .map_err(|e| e.to_string())?;
            self.chunk(
                id,
                scope,
                offset,
                &block[..length],
                &digest(&block[..length]),
            )?;
            offset += length as u64;
        }
        authorize()?;
        self.seal(id, scope)?;
        authorize()?;
        Ok(manifest)
    }
}
