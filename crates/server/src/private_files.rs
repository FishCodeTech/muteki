//! Descriptor-relative private projection writes beneath a trusted state root.
use std::path::{Component, Path};

#[cfg(unix)]
fn directory_mode(root: &Path, path: &Path, create: bool, mode:libc::mode_t) -> std::io::Result<std::fs::File> {
    use std::{
        ffi::CString,
        fs::{File, OpenOptions},
        io::{Error, ErrorKind},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{ffi::OsStrExt, fs::OpenOptionsExt},
        },
    };
    let relative = path.strip_prefix(root).map_err(|_| {
        Error::new(
            ErrorKind::PermissionDenied,
            "Private projection leaves state root",
        )
    })?;
    let parts = relative
        .components()
        .map(|part| match part {
            Component::Normal(name) => Ok(name),
            _ => Err(Error::new(ErrorKind::InvalidInput, "Invalid private path")),
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root.canonicalize()?)?;
    for segment in &parts {
        let name = CString::new(segment.as_bytes())
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "Invalid private path"))?;
        if create
            && unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), mode) } < 0
            && Error::last_os_error().kind() != ErrorKind::AlreadyExists
        {
            return Err(Error::last_os_error());
        }
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(Error::last_os_error());
        }
        directory = unsafe { File::from_raw_fd(fd) };
        if create && mode==0o755 && unsafe {libc::fchmod(directory.as_raw_fd(),mode)}<0{return Err(Error::last_os_error());}
    }
    if create && !parts.is_empty() && unsafe { libc::fchmod(directory.as_raw_fd(), mode) } < 0 {
        return Err(Error::last_os_error());
    }
    Ok(directory)
}

#[cfg(unix)]
fn directory(root:&Path,path:&Path,create:bool)->std::io::Result<std::fs::File>{directory_mode(root,path,create,0o700)}

#[cfg(unix)]
pub fn ensure_dir(root: &Path, path: &Path) -> Result<(), String> {
    directory(root, path, true)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(not(unix))]
pub fn ensure_dir(_: &Path, _: &Path) -> Result<(), String> {
    Err("Secure credential directories are not supported on this platform".into())
}

#[cfg(unix)]
pub fn write(root: &Path, path: &Path, content: &[u8]) -> Result<(), String> {write_mode(root,path,content,0o600)}
#[cfg(unix)]
pub fn write_mode(root: &Path, path: &Path, content: &[u8],mode:u32) -> Result<(), String> {
    use std::{
        ffi::CString,
        fs::File,
        io::Write,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
    };
    let directory = directory_mode(
        root,
        path.parent()
            .ok_or("Private projection needs a parent directory")?,
        true, if mode==0o600{0o700}else{0o755},
    )
    .map_err(|e| e.to_string())?;
    let name = CString::new(
        path.file_name()
            .ok_or("Private projection needs a file name")?
            .as_bytes(),
    )
    .map_err(|e| e.to_string())?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } == 0
    {
        let stat = unsafe { stat.assume_init() };
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG || stat.st_nlink != 1 {
            return Err("Private projection target is not an ordinary single-link file".into());
        }
    } else if std::io::Error::last_os_error().kind() != std::io::ErrorKind::NotFound {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let temporary = CString::new(format!(".muteki-private-{}", uuid::Uuid::new_v4())).unwrap();
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
    let result = (|| {
        let mut file = unsafe { File::from_raw_fd(fd) };
        if unsafe {libc::fchmod(file.as_raw_fd(),mode as libc::mode_t)}<0{return Err(std::io::Error::last_os_error().to_string());}
        file.write_all(content).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        if unsafe {
            libc::renameat(
                directory.as_raw_fd(),
                temporary.as_ptr(),
                directory.as_raw_fd(),
                name.as_ptr(),
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        directory.sync_all().map_err(|e| e.to_string())
    })();
    if result.is_err() {
        unsafe { libc::unlinkat(directory.as_raw_fd(), temporary.as_ptr(), 0) };
    }
    result
}
#[cfg(not(unix))]
pub fn write(_: &Path, _: &Path, _: &[u8]) -> Result<(), String> {
    Err("Secure credential projection is not supported on this platform".into())
}

#[cfg(unix)]
pub fn read(root: &Path, path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    use std::{
        ffi::CString,
        fs::File,
        io::Read,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{ffi::OsStrExt, fs::MetadataExt},
        },
    };
    let directory = directory(
        root,
        path.parent().ok_or("Private file parent missing")?,
        false,
    )
    .map_err(|e| e.to_string())?;
    let name = CString::new(
        path.file_name()
            .ok_or("Private file name missing")?
            .as_bytes(),
    )
    .map_err(|e| e.to_string())?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > limit as u64 {
        return Err("Private file type or size is invalid".into());
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > limit {
        return Err("Private file grew beyond its bound".into());
    }
    Ok(bytes)
}
#[cfg(not(unix))]
pub fn read(_: &Path, _: &Path, _: usize) -> Result<Vec<u8>, String> {
    Err("Secure private reads are not supported on this platform".into())
}

#[cfg(unix)]
pub fn remove_tree(root: &Path, path: &Path) -> Result<(), String> {
    use std::{
        ffi::{CStr, CString},
        fs::File,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
    };
    struct Entries(*mut libc::DIR);
    impl Drop for Entries {
        fn drop(&mut self) {
            unsafe { libc::closedir(self.0) };
        }
    }
    fn clear(file: &File, depth: usize, count: &mut usize) -> Result<(), String> {
        if depth > 32 {
            return Err("Credential cleanup directory depth exceeded".into());
        }
        let fd = unsafe { libc::dup(file.as_raw_fd()) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let raw = unsafe { libc::fdopendir(fd) };
        if raw.is_null() {
            unsafe { libc::close(fd) };
            return Err(std::io::Error::last_os_error().to_string());
        }
        let entries = Entries(raw);
        loop {
            let entry = unsafe { libc::readdir(entries.0) };
            if entry.is_null() {
                break;
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            *count += 1;
            if *count > 10000 {
                return Err(
                    "Credential cleanup entry budget exceeded; retry remaining files".into(),
                );
            }
            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe {
                libc::fstatat(
                    file.as_raw_fd(),
                    name.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } < 0
            {
                return Err(std::io::Error::last_os_error().to_string());
            }
            let directory = unsafe { stat.assume_init() }.st_mode & libc::S_IFMT == libc::S_IFDIR;
            if directory {
                let child = unsafe {
                    libc::openat(
                        file.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if child < 0 {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                clear(&unsafe { File::from_raw_fd(child) }, depth + 1, count)?;
            }
            if unsafe {
                libc::unlinkat(
                    file.as_raw_fd(),
                    name.as_ptr(),
                    if directory { libc::AT_REMOVEDIR } else { 0 },
                )
            } < 0
            {
                return Err(std::io::Error::last_os_error().to_string());
            }
        }
        Ok(())
    }
    let parent = match directory(
        root,
        path.parent().ok_or("Credential cleanup parent missing")?,
        false,
    ) {
        Ok(parent) => parent,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    let name = CString::new(
        path.file_name()
            .ok_or("Credential cleanup name missing")?
            .as_bytes(),
    )
    .map_err(|e| e.to_string())?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().to_string())
        };
    }
    clear(&unsafe { File::from_raw_fd(fd) }, 0, &mut 0)?;
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}
#[cfg(not(unix))]
pub fn remove_tree(_: &Path, _: &Path) -> Result<(), String> {
    Err("Secure credential cleanup is unsupported on this platform".into())
}

#[cfg(unix)]
pub fn lock(root: &Path, path: &Path) -> Result<std::fs::File, String> {
    use std::{
        ffi::CString,
        fs::File,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{ffi::OsStrExt, fs::MetadataExt},
        },
    };
    let parent = directory(
        root,
        path.parent().ok_or("Private lock parent missing")?,
        true,
    )
    .map_err(|e| e.to_string())?;
    let name = CString::new(
        path.file_name()
            .ok_or("Private lock name missing")?
            .as_bytes(),
    )
    .map_err(|e| e.to_string())?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err("Private lock is not an ordinary file".into());
    }
    fs2::FileExt::lock_exclusive(&file).map_err(|e| e.to_string())?;
    Ok(file)
}
#[cfg(not(unix))]
pub fn lock(_: &Path, _: &Path) -> Result<std::fs::File, String> {
    Err("Secure private locks are unsupported on this platform".into())
}

/// Hands a tree beneath `root` to the Worker user without following links the
/// Worker may have planted; every step is resolved relative to a held descriptor.
#[cfg(unix)]
pub fn chown_tree(root: &Path, path: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
    use std::{
        ffi::{CStr, CString},
        fs::File,
        io::{Error, ErrorKind},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
    };
    struct Entries(*mut libc::DIR);
    impl Drop for Entries {
        fn drop(&mut self) {
            unsafe { libc::closedir(self.0) };
        }
    }
    fn entry(parent: &File, name: &CStr, uid: u32, gid: u32) -> std::io::Result<()> {
        let child = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if child >= 0 {
            return walk(&unsafe { File::from_raw_fd(child) }, uid, gid);
        }
        if unsafe { libc::fchownat(parent.as_raw_fd(), name.as_ptr(), uid, gid, libc::AT_SYMLINK_NOFOLLOW) } < 0 {
            return Err(Error::last_os_error());
        }
        Ok(())
    }
    fn walk(directory: &File, uid: u32, gid: u32) -> std::io::Result<()> {
        if unsafe { libc::fchown(directory.as_raw_fd(), uid, gid) } < 0 {
            return Err(Error::last_os_error());
        }
        let fd = unsafe { libc::dup(directory.as_raw_fd()) };
        if fd < 0 {
            return Err(Error::last_os_error());
        }
        let raw = unsafe { libc::fdopendir(fd) };
        if raw.is_null() {
            unsafe { libc::close(fd) };
            return Err(Error::last_os_error());
        }
        let entries = Entries(raw);
        let mut result = Ok(());
        loop {
            let next = unsafe { libc::readdir(entries.0) };
            if next.is_null() {
                break;
            }
            let name = unsafe { CStr::from_ptr((*next).d_name.as_ptr()) };
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            let outcome = entry(directory, name, uid, gid);
            if result.is_ok() {
                result = outcome;
            }
        }
        result
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if path != root => {
            let parent = directory(root, parent, false)?;
            let name = CString::new(name.as_bytes())
                .map_err(|_| Error::new(ErrorKind::InvalidInput, "Invalid private path"))?;
            entry(&parent, &name, uid, gid)
        }
        _ => walk(&directory(root, path, false)?, uid, gid),
    }
}
#[cfg(not(unix))]
pub fn chown_tree(_: &Path, _: &Path, _: u32, _: u32) -> std::io::Result<()> {
    Ok(())
}

/// Hands the directories strictly between `root` and `path` to the Worker user,
/// refusing any link in between.
#[cfg(unix)]
pub fn chown_ancestors(root: &Path, path: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let Some(parent) = path.parent() else { return Ok(()) };
    for ancestor in parent.ancestors().take_while(|dir| *dir != root && dir.starts_with(root)) {
        let directory = directory(root, ancestor, false)?;
        if unsafe { libc::fchown(directory.as_raw_fd(), uid, gid) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}
#[cfg(not(unix))]
pub fn chown_ancestors(_: &Path, _: &Path, _: u32, _: u32) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn private_projection_rejects_links_without_touching_external_data() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("untouched");
        std::fs::write(&secret, b"original").unwrap();
        symlink(outside.path(), root.path().join("escaped")).unwrap();
        assert!(
            write(
                root.path(),
                &root.path().join("escaped/untouched"),
                b"secret"
            )
            .is_err()
        );
        symlink(&secret, root.path().join("link")).unwrap();
        assert!(write(root.path(), &root.path().join("link"), b"secret").is_err());
        std::fs::hard_link(&secret, root.path().join("hardlink")).unwrap();
        assert!(write(root.path(), &root.path().join("hardlink"), b"secret").is_err());
        assert!(write(root.path(), &root.path().join("../escape"), b"secret").is_err());
        assert_eq!(std::fs::read(&secret).unwrap(), b"original");
        let target = root.path().join("homes/worker/.codex/auth.json");
        write(root.path(), &target, b"initial").unwrap();
        write(root.path(), &target, b"refreshed").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"refreshed");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(target.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::read_dir(target.parent().unwrap()).unwrap().count(),
            1
        );
    }
    #[cfg(unix)]
    #[test]
    fn private_reads_and_cleanup_never_follow_worker_links() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let marker = outside.path().join("marker");
        std::fs::write(&marker, b"outside").unwrap();
        let home = root.path().join("homes/grant");
        write(root.path(), &home.join("token"), b"private").unwrap();
        assert_eq!(
            read(root.path(), &home.join("token"), 32).unwrap(),
            b"private"
        );
        assert!(read(root.path(), &home.join("token"), 2).is_err());
        symlink(&marker, home.join("linked-token")).unwrap();
        assert!(read(root.path(), &home.join("linked-token"), 32).is_err());
        symlink(outside.path(), home.join("outside-directory")).unwrap();
        remove_tree(root.path(), &home).unwrap();
        assert!(!home.exists());
        assert_eq!(std::fs::read(&marker).unwrap(), b"outside");
        symlink(outside.path(), root.path().join("escape")).unwrap();
        assert!(remove_tree(root.path(), &root.path().join("escape/child")).is_err());
        remove_tree(root.path(), &root.path().join("missing/child")).unwrap();
    }
}

#[cfg(not(unix))]
pub fn write_mode(_: &Path,_:&Path,_:&[u8],_:u32)->Result<(),String>{Err("Secure workspace writes require Unix".into())}
