use std::io::{self, Read};
use std::path::{Path, PathBuf};

pub fn read_bounded(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let too_large = || {
        io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!("file exceeds the {limit} byte size limit"),
        )
    };
    if file.metadata()?.len() > limit {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    // Metadata can become stale while the file is read.
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(too_large());
    }
    Ok(bytes)
}

/// Return the directory used to persist attachments for a session.
pub fn attachment_dir(data_root: &Path, session_id: &str) -> PathBuf {
    attachments_root(data_root).join(session_id)
}

/// The directory every session's attachment directory lives under; a client
/// may only save attachments below it.
pub fn attachments_root(data_root: &Path) -> PathBuf {
    data_root.join("attachments")
}

#[derive(Clone, Debug)]
pub struct UserDirectories {
    pub home: Option<PathBuf>,
    pub documents: Option<PathBuf>,
}

impl Default for UserDirectories {
    fn default() -> Self {
        Self {
            home: dirs::home_dir(),
            documents: dirs::document_dir(),
        }
    }
}

impl UserDirectories {
    pub fn new_project_root(&self, name: &str) -> std::io::Result<PathBuf> {
        let name = name.trim();
        if name.is_empty()
            || name.contains(['/', '\\'])
            || !matches!(
                Path::new(name).components().collect::<Vec<_>>().as_slice(),
                [std::path::Component::Normal(_)]
            )
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Project name must be a non-empty single directory name without / or \\.",
            ));
        }
        let home = self.home.as_ref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Host home directory is unavailable.",
            )
        })?;
        let root = home.join("TcodeProjects").join(name);
        create_directory(&root)?;
        Ok(root)
    }

    pub fn scratch_root(&self) -> std::io::Result<PathBuf> {
        let documents = self
            .documents
            .clone()
            .or_else(|| self.home.as_ref().map(|home| home.join("Documents")))
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "Host documents directory is unavailable.",
                )
            })?;
        Ok(documents.join("Tcode"))
    }

    pub fn scratch_directory(&self) -> std::io::Result<(PathBuf, PathBuf)> {
        let root = self.scratch_root()?;
        let daily = root.join(chrono::Local::now().date_naive().to_string());
        create_directory(&daily)?;
        Ok((root, daily))
    }
}

fn create_directory(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path).map_err(|error| {
        std::io::Error::new(
            error.kind(),
            format!("Could not create {}: {error}", path.display()),
        )
    })
}
