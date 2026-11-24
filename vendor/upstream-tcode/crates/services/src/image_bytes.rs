use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::io;
use tcode_protocol::QueryResponse;

/// An owned encoded selection that can be decoded after releasing the session log.
pub struct ImageRead {
    encoded: Vec<u8>,
    kind: ImageReadKind,
}

enum ImageReadKind {
    Complete,
    Preview {
        width: u32,
        height: u32,
        data_dir: std::path::PathBuf,
    },
}

pub fn decoded_len(encoded: &str) -> io::Result<u64> {
    let padding = encoded
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'=')
        .count();
    if !encoded.len().is_multiple_of(4) || padding > 2 {
        return Err(io::Error::other("Invalid image encoding"));
    }
    Ok((encoded.len() / 4 * 3 - padding) as u64)
}

impl ImageRead {
    pub fn complete(encoded: &str, max_bytes: Option<u64>) -> io::Result<Self> {
        let total = decoded_len(encoded)?;
        if max_bytes.is_some_and(|limit| total > limit) {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "Image exceeds the reply budget",
            ));
        }
        Ok(Self {
            encoded: encoded.as_bytes().to_vec(),
            kind: ImageReadKind::Complete,
        })
    }

    pub fn preview(
        encoded: &str,
        width: u32,
        height: u32,
        data_dir: &std::path::Path,
    ) -> io::Result<Self> {
        if !(1..=1024).contains(&width) || !(1..=1024).contains(&height) {
            return Err(io::Error::other(
                "Preview size is outside the supported range",
            ));
        }
        let mut read = Self::complete(encoded, Some(64 * 1024 * 1024))?;
        read.kind = ImageReadKind::Preview {
            width,
            height,
            data_dir: data_dir.to_owned(),
        };
        Ok(read)
    }

    pub fn read(self) -> io::Result<QueryResponse> {
        let bytes = STANDARD.decode(&self.encoded).map_err(io::Error::other)?;
        let bytes = match self.kind {
            ImageReadKind::Complete => bytes,
            ImageReadKind::Preview {
                width,
                height,
                data_dir,
            } => crate::image_previews::read_bytes(&bytes, width, height, &data_dir)?,
        };
        Ok(QueryResponse::FileBytes(bytes))
    }
}
