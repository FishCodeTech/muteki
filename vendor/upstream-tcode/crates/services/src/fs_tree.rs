//! Moving and copying directory trees without following links.
//!
//! A move is a rename where the filesystem allows one. Across filesystems the
//! tree is copied, every file read back and compared byte for byte with its
//! source, and only then is the source deleted; a failed or mismatched copy is
//! discarded and the source kept.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;

pub const CHUNK_BYTES: usize = 8 << 20;

/// Called with the bytes written after each chunk; an error stops the copy.
pub type OnBytes<'a> = dyn FnMut(u64) -> io::Result<()> + 'a;

/// Move `source` to `destination`, which must not exist and whose parent must.
pub fn move_tree(source: &Path, destination: &Path) -> io::Result<()> {
    if fs::symlink_metadata(destination).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists", destination.display()),
        ));
    }
    match fs::rename(source, destination) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::CrossesDevices => {}
        Err(error) => return Err(error),
    }
    log::info!(
        "{} and {} are on different filesystems; copying",
        source.display(),
        destination.display()
    );
    if let Err(error) = copy_tree(source, destination, &mut |_| Ok(())) {
        if let Err(cleanup) = remove_if_present(destination) {
            log::warn!("could not discard {}: {cleanup}", destination.display());
        }
        return Err(error);
    }
    remove_if_present(source)
}

/// Copy `source` to `destination`, which must not exist, verifying every file.
pub fn copy_tree(source: &Path, destination: &Path, on_bytes: &mut OnBytes) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    let kind = metadata.file_type();
    if kind.is_symlink() {
        let target = fs::read_link(source)?;
        symlink(source, &target, destination)?;
        if fs::read_link(destination)? != target {
            return Err(mismatch(source, destination));
        }
    } else if kind.is_dir() {
        fs::create_dir(destination)?;
        let mut names = fs::read_dir(source)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<io::Result<Vec<_>>>()?;
        names.sort();
        for name in names {
            copy_tree(&source.join(&name), &destination.join(&name), on_bytes)?;
        }
        fs::set_permissions(destination, metadata.permissions())?;
        sync_dir(destination)?;
    } else if kind.is_file() {
        copy_file(source, destination, on_bytes)?;
        fs::set_permissions(destination, metadata.permissions())?;
    } else {
        log::warn!(
            "not moving {}: not a file, directory or link",
            source.display()
        );
    }
    Ok(())
}

fn copy_file(source: &Path, destination: &Path, on_bytes: &mut OnBytes) -> io::Result<()> {
    let mut reader = File::open(source)?;
    let mut writer = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let mut buffer = vec![0; CHUNK_BYTES];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        writer.write_all(&buffer[..read])?;
        on_bytes(read as u64)?;
    }
    writer.sync_all()?;
    drop(writer);
    verify_file(source, destination)
}

/// Read the copy back and compare it with its source, length and bytes.
fn verify_file(source: &Path, destination: &Path) -> io::Result<()> {
    if fs::metadata(source)?.len() != fs::metadata(destination)?.len() {
        return Err(mismatch(source, destination));
    }
    let (mut original, mut copy) = (File::open(source)?, File::open(destination)?);
    let (mut expected, mut actual) = (vec![0; CHUNK_BYTES], vec![0; CHUNK_BYTES]);
    loop {
        let read = original.read(&mut expected)?;
        if read == 0 {
            return if copy.read(&mut actual[..1])? == 0 {
                Ok(())
            } else {
                Err(mismatch(source, destination))
            };
        }
        copy.read_exact(&mut actual[..read])?;
        if expected[..read] != actual[..read] {
            return Err(mismatch(source, destination));
        }
    }
}

fn mismatch(source: &Path, destination: &Path) -> io::Error {
    io::Error::other(format!(
        "the copy {} differs from {}; the source is kept",
        destination.display(),
        source.display()
    ))
}

/// Bytes of every file under `path`, not following links.
pub fn tree_size(path: &Path) -> io::Result<u64> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() {
        return Ok(if metadata.is_file() {
            metadata.len()
        } else {
            0
        });
    }
    let mut total = 0;
    for entry in fs::read_dir(path)? {
        total += tree_size(&entry?.path())?;
    }
    Ok(total)
}

/// Delete a file, link or directory tree, without following links.
pub fn remove_if_present(path: &Path) -> io::Result<()> {
    let result = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => {
            make_removable(path).and_then(|()| fs::remove_dir_all(path))
        }
        Ok(_) => fs::remove_file(path),
        Err(error) => Err(error),
    };
    match result {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Flush a directory's entries to disk, where the platform allows opening one.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Unix removes an entry only from a writable directory, and a profile
/// home's Go module cache keeps its directories read-only.
#[cfg(unix)]
fn make_removable(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut permissions = fs::symlink_metadata(dir)?.permissions();
    if permissions.mode() & 0o700 != 0o700 {
        permissions.set_mode(permissions.mode() | 0o700);
        fs::set_permissions(dir, permissions)?;
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            make_removable(&entry.path())?;
        }
    }
    Ok(())
}

/// std's Windows `remove_dir_all` deletes with POSIX semantics, which ignore
/// the read-only attribute on NTFS.
#[cfg(not(unix))]
fn make_removable(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn symlink(_source: &Path, target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

/// Windows distinguishes links to directories from links to files.
#[cfg(windows)]
fn symlink(source: &Path, target: &Path, link: &Path) -> io::Result<()> {
    if fs::metadata(source).is_ok_and(|metadata| metadata.is_dir()) {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    }
}
