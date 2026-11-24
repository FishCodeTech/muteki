//! Project-root configuration. See docs/project-config.md for the file contract.
use serde::Deserialize;
use std::{
    fs::File,
    io::{self, Read},
    path::Path,
};

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectConfig {
    pub icon_path: Option<String>,
}

/// Read the primary file, consulting the legacy file only if the primary is absent.
/// Unknown fields are ignored; syntax and known-field type errors are reported.
pub fn read(root: &Path) -> io::Result<ProjectConfig> {
    let file = match File::open(root.join("tcode.json")) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match File::open(root.join("t3.json")) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(ProjectConfig::default());
                }
                result => result?,
            }
        }
        result => result?,
    };
    const MAX_BYTES: u64 = 1024 * 1024;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(io::Error::other(
            "project config exceeds the 1 MiB size limit",
        ));
    }
    // Struct deserialization also accepts positional arrays; configuration is map-only.
    let fields = serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    serde_json::from_value(serde_json::Value::Object(fields))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn primary_config_is_authoritative_even_when_empty_or_invalid() {
        let root = std::env::temp_dir().join(format!("tcode-config-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        assert!(read(&root).unwrap().icon_path.is_none());
        fs::write(root.join("t3.json"), r#"{"iconPath":"legacy.png"}"#).unwrap();
        assert_eq!(
            read(&root).unwrap().icon_path.as_deref(),
            Some("legacy.png")
        );
        for (json, expected) in [
            (
                r#"{"iconPath":"primary.png","scripts":[{"command":"ignored"}],"future":{}}"#,
                Some("primary.png"),
            ),
            ("{}", None),
            (r#"{"iconPath":null}"#, None),
        ] {
            fs::write(root.join("tcode.json"), json).unwrap();
            assert_eq!(read(&root).unwrap().icon_path.as_deref(), expected);
        }
        for json in [
            "broken json",
            "null",
            "[]",
            r#"["logo.png"]"#,
            "[null]",
            r#"{"iconPath":42}"#,
            r#"{"iconPath":false}"#,
            r#"{"iconPath":"a",}"#,
        ] {
            fs::write(root.join("tcode.json"), json).unwrap();
            assert_eq!(
                read(&root).unwrap_err().kind(),
                io::ErrorKind::InvalidData,
                "{json}"
            );
        }
        fs::File::create(root.join("tcode.json"))
            .unwrap()
            .set_len(1024 * 1024 + 1)
            .unwrap();
        assert!(read(&root).unwrap_err().to_string().contains("1 MiB"));
        fs::remove_file(root.join("tcode.json")).unwrap();
        fs::create_dir(root.join("tcode.json")).unwrap();
        assert!(
            read(&root).is_err(),
            "an unreadable primary must not use legacy config"
        );
        fs::remove_dir(root.join("tcode.json")).unwrap();
        assert_eq!(
            read(&root).unwrap().icon_path.as_deref(),
            Some("legacy.png")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
