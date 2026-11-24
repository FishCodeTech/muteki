//! Native and largest context windows of Codex models. `model/list` does not
//! carry them; Codex keeps them in the model catalog it caches under its home
//! (`models_cache.json`), refreshed whenever the CLI runs.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ModelWindows {
    /// The window the model runs at unless `model_context_window` says otherwise.
    pub native: u64,
    /// The window Codex clamps `model_context_window` to.
    pub largest: u64,
}

#[derive(Deserialize)]
struct Cache {
    models: Vec<CachedModel>,
}

#[derive(Deserialize)]
struct CachedModel {
    slug: String,
    context_window: Option<u64>,
    max_context_window: Option<u64>,
}

/// Windows by model slug; empty when the cache is missing or unreadable.
pub(super) fn read(home: &Path) -> HashMap<String, ModelWindows> {
    let path = home.join("models_cache.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    match serde_json::from_str::<Cache>(&text) {
        Ok(cache) => parse(cache),
        Err(err) => {
            log::warn!("codex: could not parse {}: {err}", path.display());
            HashMap::new()
        }
    }
}

fn parse(cache: Cache) -> HashMap<String, ModelWindows> {
    cache
        .models
        .into_iter()
        .filter_map(|model| {
            let native = model.context_window?;
            let largest = model
                .max_context_window
                .map_or(native, |max| max.max(native));
            Some((model.slug, ModelWindows { native, largest }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_windows_from_the_cached_catalog() {
        let dir = std::env::temp_dir().join(format!("tcode-codex-windows-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("models_cache.json"),
            r#"{"fetched_at":"2026-10-18T00:00:00Z","models":[
                {"slug":"gpt-6-astra","context_window":272000,"max_context_window":872000,"visibility":"list"},
                {"slug":"gpt-fixed","context_window":128000},
                {"slug":"gpt-unknown"}
            ]}"#,
        )
        .unwrap();
        let windows = read(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            windows.get("gpt-6-astra"),
            Some(&ModelWindows {
                native: 272_000,
                largest: 872_000
            })
        );
        assert_eq!(
            windows.get("gpt-fixed"),
            Some(&ModelWindows {
                native: 128_000,
                largest: 128_000
            })
        );
        assert!(!windows.contains_key("gpt-unknown"));
        assert!(read(&dir.join("missing")).is_empty());
    }
}
