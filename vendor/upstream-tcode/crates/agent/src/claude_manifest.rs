//! Claude Code's model catalog, in the t3code model manifest format.
//!
//! `claude_model_manifest.json` follows the shape of
//! <https://raw.githubusercontent.com/pingdotgg/t3code/main/apps/server/src/provider/model-manifest.json>
//! (t3code, MIT licensed) and ships with every release as the offline
//! fallback. Two copies are maintained: t3code's and the one in this
//! repository. With the `process` feature, [`refresh`] fetches both and keeps
//! the last good copy on disk in the tcode data dir. Whichever manifest is
//! newest by `updatedAt` wins, across the bundle, the disk cache and every
//! remote source, so either side can ship a model first and a release can
//! correct model data before the next fetch. Invalid data never replaces a
//! usable catalog, and no failure here ever fails `list_models`.
//!
//! There is no local override table: an edit to this repository's copy is made
//! in the upstream shape with a bumped `updatedAt`, and stays in effect until
//! a newer manifest appears on either side. Bringing upstream changes in is a
//! verbatim copy over `claude_model_manifest.json`.
//!
//! The catalog is process-shared state, as upstream: sessions resolve their
//! launch flags and the UI resolves context windows against [`current`].

// Type-only builds (web and mobile clients) use this module solely to resolve
// context windows against the bundle; launch mapping, version gating and the
// refresh bookkeeping only have callers under `process`.
#![cfg_attr(not(feature = "process"), allow(dead_code))]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::Value;

use crate::{ModelSpec, OptionDescriptor, OptionSelection, SelectOption};

const BUNDLED: &str = include_str!("claude_model_manifest.json");

/// Only manifest version 1 is understood; `version` gates breaking changes.
const SUPPORTED_VERSION: u64 = 1;

/// The manifest calls the reasoning selector `effort`; everything in tcode
/// keys on `reasoningEffort`. Translated once here, at the boundary.
fn option_id(id: &str) -> &str {
    match id {
        "effort" => "reasoningEffort",
        other => other,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestFile {
    version: u64,
    updated_at: Option<String>,
    #[serde(default)]
    providers: ProvidersFile,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ProvidersFile {
    claude_agent: Option<CatalogFile>,
}

#[derive(Deserialize)]
struct CatalogFile {
    #[serde(default)]
    defaults: DefaultsFile,
    #[serde(default)]
    profiles: HashMap<String, ProfileFile>,
    models: Vec<ModelFile>,
}

#[derive(Deserialize, Default)]
struct DefaultsFile {
    chat: Option<String>,
}

#[derive(Deserialize, Default)]
struct ProfileFile {
    #[serde(default)]
    capabilities: CapabilitiesFile,
    #[serde(default)]
    adapter: ProfileAdapterFile,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct CapabilitiesFile {
    #[serde(default)]
    option_descriptors: Vec<OptionFile>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum OptionFile {
    Select {
        id: String,
        label: String,
        options: Vec<SelectOptionFile>,
        /// Values t3code sends as a prompt keyword rather than a launch
        /// setting. The keyword reaches Claude Code verbatim when the user
        /// types it, so tcode does not offer it as an option.
        #[serde(default, rename = "promptInjectedValues")]
        prompt_injected_values: Vec<String>,
    },
    Boolean {
        id: String,
        label: String,
    },
    /// Descriptor types tcode cannot render are dropped rather than
    /// rejecting the whole manifest.
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SelectOptionFile {
    id: String,
    label: String,
    description: Option<String>,
    #[serde(default)]
    is_default: bool,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ProfileAdapterFile {
    claude_code: Option<ClaudeCodeProfile>,
}

/// How a capability profile maps onto Claude Code launch flags.
#[derive(Deserialize, Default, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClaudeCodeProfile {
    /// Effort selection → `--effort` value; `None` means no flag at all.
    #[serde(default)]
    effort_map: HashMap<String, Option<String>>,
    /// Option id → option value → suffix appended to the model slug
    /// (`contextWindow` / `1m` / `[1m]`).
    #[serde(default)]
    model_suffixes: HashMap<String, HashMap<String, String>>,
    /// Context window option value → token count.
    #[serde(default)]
    context_window_tokens: HashMap<String, u64>,
    /// Token count for models without a context window selector.
    fixed_context_window_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct ModelFile {
    slug: String,
    name: String,
    #[serde(default)]
    aliases: Vec<String>,
    profile: Option<String>,
    #[serde(default)]
    adapter: ModelAdapterFile,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ModelAdapterFile {
    claude_code: Option<CompatFile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompatFile {
    min_version: Option<String>,
    max_version_exclusive: Option<String>,
}

type Version = (u32, u32, u32);

/// One catalog model with its tcode-shaped spec and Claude Code adapter data.
#[derive(Debug, Clone)]
pub(crate) struct CatalogModel {
    pub(crate) spec: ModelSpec,
    /// Alternate ids (`opus`, `claude-opus-5.0`) that resolve to this slug.
    aliases: Vec<String>,
    runtime: ClaudeCodeProfile,
    min_version: Option<Version>,
    max_version_exclusive: Option<Version>,
}

impl CatalogModel {
    /// Whether the installed CLI can run this model. Gated models need a
    /// known version; unknown versions hide them.
    fn available(&self, version: Option<Version>) -> bool {
        if self.min_version.is_none() && self.max_version_exclusive.is_none() {
            return true;
        }
        let Some(version) = version else {
            return false;
        };
        self.min_version.is_none_or(|min| version >= min)
            && self.max_version_exclusive.is_none_or(|max| version < max)
    }

    /// The `--effort` value for an accepted effort selection: the profile's
    /// `effortMap` entry when present (`None` = no flag), else passthrough.
    pub(crate) fn cli_effort(&self, effort: &str) -> Option<String> {
        match self.runtime.effort_map.get(effort) {
            Some(mapped) => mapped.clone(),
            None => Some(effort.to_owned()),
        }
    }

    fn default_select(&self, id: &str) -> Option<&str> {
        self.spec.options.iter().find_map(|option| match option {
            OptionDescriptor::Select {
                id: option_id,
                default_value,
                ..
            } if option_id == id => default_value.as_deref(),
            _ => None,
        })
    }

    /// Tokens of the default context window selection.
    fn default_context_window(&self) -> Option<u64> {
        self.runtime.fixed_context_window_tokens.or_else(|| {
            let default = self.default_select("contextWindow")?;
            self.runtime.context_window_tokens.get(default).copied()
        })
    }

    /// The largest window this model can run with.
    pub(crate) fn largest_context_window(&self) -> Option<u64> {
        self.runtime
            .fixed_context_window_tokens
            .or_else(|| self.runtime.context_window_tokens.values().copied().max())
    }

    /// Slug suffix that opens a window of at least `window` tokens: the
    /// smallest listed context window option that fits, e.g. `[1m]` for both
    /// the `1m` option and a custom 500k window on a 200k model. Windows the
    /// bare slug already covers (at or below the model's default) need no
    /// suffix, so a 1M-default model launches as its plain id.
    pub(crate) fn context_window_suffix(&self, window: u64) -> &str {
        if self
            .default_context_window()
            .is_some_and(|default| window <= default)
        {
            return "";
        }
        let Some(suffixes) = self.runtime.model_suffixes.get("contextWindow") else {
            return "";
        };
        self.runtime
            .context_window_tokens
            .iter()
            .filter(|(_, tokens)| **tokens >= window)
            .min_by_key(|(_, tokens)| **tokens)
            .and_then(|(id, _)| suffixes.get(id))
            .map_or("", String::as_str)
    }
}

/// The decoded, validated Claude catalog of one manifest.
#[derive(Debug, Clone)]
pub(crate) struct ClaudeCatalog {
    /// ISO-8601 UTC edit date. Same-format timestamps order lexicographically,
    /// and an undated manifest counts as older than any dated one.
    updated_at: Option<String>,
    pub(crate) models: Vec<CatalogModel>,
}

fn non_empty(value: &str, what: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(format!("empty {what}"));
    }
    Ok(trimmed.to_owned())
}

fn parse_version(value: &str) -> Result<Version, String> {
    crate::parse_semver(value).ok_or_else(|| format!("invalid version {value:?}"))
}

fn descriptor(option: &OptionFile) -> Result<Option<OptionDescriptor>, String> {
    Ok(Some(match option {
        OptionFile::Select {
            id,
            label,
            options,
            prompt_injected_values,
        } => {
            let id = non_empty(id, "option id")?;
            let mut default_value = None;
            let mut mapped = Vec::with_capacity(options.len());
            for option in options
                .iter()
                .filter(|option| !prompt_injected_values.contains(&option.id))
            {
                let value = non_empty(&option.id, "option value")?;
                if option.is_default && default_value.is_none() {
                    default_value = Some(value.clone());
                }
                mapped.push(SelectOption {
                    value,
                    label: non_empty(&option.label, "option label")?,
                    description: option.description.clone(),
                    unavailable: None,
                });
            }
            OptionDescriptor::Select {
                id: option_id(&id).to_owned(),
                label: non_empty(label, "option label")?,
                options: mapped,
                default_value,
                role: crate::OptionRole::Model,
                apply: crate::ApplyTiming::Live,
                recommended: None,
                permissive: None,
            }
        }
        OptionFile::Boolean { id, label } => OptionDescriptor::Boolean {
            id: option_id(&non_empty(id, "option id")?).to_owned(),
            label: non_empty(label, "option label")?,
            default_value: false,
            role: crate::OptionRole::Model,
            apply: crate::ApplyTiming::Live,
            recommended: None,
            permissive: None,
        },
        OptionFile::Unknown => return Ok(None),
    }))
}

/// A `contextWindow` selector whose only preset is the model's fixed window.
fn fixed_context_window_descriptor(tokens: u64) -> OptionDescriptor {
    let label = crate::context_window::format(tokens);
    let value = label.to_ascii_lowercase();
    OptionDescriptor::Select {
        id: "contextWindow".into(),
        label: "Context Window".into(),
        options: vec![SelectOption {
            value: value.clone(),
            label,
            description: None,
            unavailable: None,
        }],
        default_value: Some(value),
        role: crate::OptionRole::Model,
        apply: crate::ApplyTiming::Live,
        recommended: None,
        permissive: None,
    }
}

impl ClaudeCatalog {
    pub(crate) fn from_json(text: &str) -> Result<Self, String> {
        serde_json::from_str(text)
            .map_err(|error| error.to_string())
            .and_then(Self::from_value)
    }

    pub(crate) fn from_value(value: Value) -> Result<Self, String> {
        serde_json::from_value(value)
            .map_err(|error| error.to_string())
            .and_then(Self::from_file)
    }

    fn from_file(file: ManifestFile) -> Result<Self, String> {
        if file.version != SUPPORTED_VERSION {
            return Err(format!("unsupported manifest version {}", file.version));
        }
        let catalog = file
            .providers
            .claude_agent
            .ok_or("manifest has no claudeAgent catalog")?;
        let mut seen = HashSet::new();
        let mut models = Vec::with_capacity(catalog.models.len());
        let no_profile = ProfileFile::default();
        for model in &catalog.models {
            let slug = non_empty(&model.slug, "model slug")?;
            if !seen.insert(slug.clone()) {
                return Err(format!("duplicate model slug {slug:?}"));
            }
            let profile = match &model.profile {
                Some(name) => catalog
                    .profiles
                    .get(name)
                    .ok_or_else(|| format!("model {slug:?} references unknown profile {name:?}"))?,
                None => &no_profile,
            };
            let runtime = profile.adapter.claude_code.clone().unwrap_or_default();
            for (effort, mapped) in &runtime.effort_map {
                non_empty(effort, "effortMap key")?;
                if let Some(mapped) = mapped {
                    non_empty(mapped, "effortMap value")?;
                }
            }
            let compat = model.adapter.claude_code.as_ref();
            let min_version = compat
                .and_then(|compat| compat.min_version.as_deref())
                .map(parse_version)
                .transpose()?;
            let max_version_exclusive = compat
                .and_then(|compat| compat.max_version_exclusive.as_deref())
                .map(parse_version)
                .transpose()?;
            if let (Some(min), Some(max)) = (min_version, max_version_exclusive)
                && min >= max
            {
                return Err(format!(
                    "model {slug:?} minVersion is not below maxVersionExclusive"
                ));
            }
            let mut options = profile
                .capabilities
                .option_descriptors
                .iter()
                .filter_map(|option| descriptor(option).transpose())
                .collect::<Result<Vec<_>, _>>()?;
            // Upstream lists no selector for a fixed window because it only
            // chooses between slug suffixes. Claude Code's `autoCompactWindow`
            // runs any of these models at a smaller window, so the window is
            // still a choice here.
            if let Some(tokens) = runtime.fixed_context_window_tokens
                && !options.iter().any(|option| {
                    matches!(option, OptionDescriptor::Select { id, .. } if id == "contextWindow")
                })
            {
                options.push(fixed_context_window_descriptor(tokens));
            }
            let aliases = model
                .aliases
                .iter()
                .map(|alias| non_empty(alias, "model alias"))
                .collect::<Result<Vec<_>, _>>()?;
            models.push(CatalogModel {
                spec: ModelSpec {
                    is_default: catalog.defaults.chat.as_deref() == Some(slug.as_str()),
                    display_name: non_empty(&model.name, "model name")?,
                    id: slug,
                    options,
                },
                aliases,
                runtime,
                min_version,
                max_version_exclusive,
            });
        }
        if let Some(chat) = &catalog.defaults.chat
            && !seen.contains(chat)
        {
            return Err(format!("default model {chat:?} is not in the catalog"));
        }
        Ok(Self {
            updated_at: file.updated_at,
            models,
        })
    }

    /// Look up a model by slug, else by alias (case-insensitively, as
    /// upstream does); a context suffix such as `[1m]` is ignored.
    pub(crate) fn model(&self, id: &str) -> Option<&CatalogModel> {
        let id = id.trim().split('[').next().unwrap_or_default();
        self.models
            .iter()
            .find(|model| model.spec.id == id)
            .or_else(|| {
                self.models.iter().find(|model| {
                    model
                        .aliases
                        .iter()
                        .any(|alias| alias.eq_ignore_ascii_case(id))
                })
            })
    }

    /// Models the installed CLI version can run, in manifest order.
    pub(crate) fn models_for_version(&self, version: Option<Version>) -> Vec<ModelSpec> {
        self.models
            .iter()
            .filter(|model| model.available(version))
            .map(|model| model.spec.clone())
            .collect()
    }

    /// The selected context window in tokens, else the model's default (200k
    /// when the manifest says nothing about it).
    pub(crate) fn resolved_context_window(
        &self,
        model_id: &str,
        selections: &[OptionSelection],
    ) -> u64 {
        selections
            .iter()
            .find(|selection| selection.id == "contextWindow")
            .and_then(|selection| crate::context_window::parse_tokens(&selection.value))
            .or_else(|| self.model(model_id)?.default_context_window())
            .unwrap_or(200_000)
    }
}

fn bundled() -> ClaudeCatalog {
    ClaudeCatalog::from_json(BUNDLED).expect("bundled Claude model manifest is valid")
}

/// In-memory manifest plus the fetch bookkeeping that paces refreshes.
pub(crate) struct ManifestState {
    catalog: Arc<ClaudeCatalog>,
    /// Wall-clock millis of the fetch that produced `catalog`; `None` for the
    /// bundle. Persisted with the disk cache so a restart does not refetch.
    fetched_at_ms: Option<u64>,
    last_attempt_ms: Option<u64>,
    disk_loaded: bool,
    #[cfg(feature = "process")]
    source_responses: HashMap<String, (String, Vec<u8>)>,
}

static STATE: Mutex<Option<ManifestState>> = Mutex::new(None);

fn with_state<R>(f: impl FnOnce(&mut ManifestState) -> R) -> R {
    let mut guard = STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(guard.get_or_insert_with(|| ManifestState::new(bundled())))
}

/// The catalog in effect right now; never waits on the network.
pub(crate) fn current() -> Arc<ClaudeCatalog> {
    with_state(|state| state.catalog.clone())
}

impl ManifestState {
    pub(crate) fn new(catalog: ClaudeCatalog) -> Self {
        Self {
            catalog: Arc::new(catalog),
            fetched_at_ms: None,
            last_attempt_ms: None,
            disk_loaded: false,
            #[cfg(feature = "process")]
            source_responses: HashMap::new(),
        }
    }
}

#[cfg(feature = "process")]
pub(crate) use refresh::refresh;

#[cfg(feature = "process")]
mod refresh {
    use std::io::Read as _;
    use std::path::Path;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use serde::{Deserialize, Serialize};
    use serde_json::Value;

    use super::{ClaudeCatalog, ManifestState, with_state};

    /// Every maintained copy of the manifest; the newest by `updatedAt` wins.
    const MANIFEST_SOURCES: [(&str, &str); 2] = [
        (
            "t3code",
            "https://raw.githubusercontent.com/pingdotgg/t3code/main/apps/server/src/provider/model-manifest.json",
        ),
        (
            "tcode",
            "https://raw.githubusercontent.com/Tryanks/tcode/main/crates/agent/src/claude_model_manifest.json",
        ),
    ];
    pub(super) const CACHE_FILE: &str = "claude-model-manifest.json";
    /// How long a fetched manifest stays fresh.
    pub(super) const TTL_MS: u64 = 60 * 60 * 1000;
    /// Minimum gap between attempts after a failure, so an offline machine
    /// does not pay a network timeout on every catalog refresh.
    pub(super) const RETRY_MS: u64 = 5 * 60 * 1000;
    const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
    const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;

    /// On-disk shape of the last good remote manifest.
    #[derive(Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub(super) struct CacheFile {
        pub(super) fetched_at_ms: u64,
        pub(super) manifest: Value,
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as u64)
    }

    impl ManifestState {
        /// Adopt the disk cache once, unless the bundle is newer by `updatedAt`.
        pub(crate) fn load_cache(&mut self, path: &Path) {
            if std::mem::replace(&mut self.disk_loaded, true) {
                return;
            }
            let Some(cache) = std::fs::read_to_string(path)
                .ok()
                .and_then(|text| serde_json::from_str::<CacheFile>(&text).ok())
            else {
                return;
            };
            match ClaudeCatalog::from_value(cache.manifest) {
                Ok(catalog) if catalog.updated_at >= self.catalog.updated_at => {
                    self.catalog = catalog.into();
                    self.fetched_at_ms = Some(cache.fetched_at_ms);
                }
                Ok(_) => log::info!("bundled Claude model manifest is newer than the cache"),
                Err(error) => log::warn!("ignoring cached Claude model manifest: {error}"),
            }
        }

        /// Whether a fetch is due now; records the attempt when it is.
        pub(crate) fn begin_fetch(&mut self, now_ms: u64, network: bool) -> bool {
            // A timestamp in the future means the wall clock moved backwards;
            // treat it as expired so the refetch rewrites both timestamps.
            let within = |since: Option<u64>, window: u64| {
                since.is_some_and(|since| now_ms >= since && now_ms - since < window)
            };
            if !network
                || within(self.fetched_at_ms, TTL_MS)
                || within(self.last_attempt_ms, RETRY_MS)
            {
                return false;
            }
            self.last_attempt_ms = Some(now_ms);
            true
        }

        /// Adopt a fetched manifest when it is at least as new by `updatedAt`
        /// as the catalog in effect; returns the adopted manifest. An
        /// undecodable or invalid body leaves the catalog in place and is an
        /// error. A valid but older manifest is `Ok(None)`: not adopted, but
        /// it still counts as a fetch so the TTL paces the next attempt.
        pub(crate) fn install(
            &mut self,
            now_ms: u64,
            body: &[u8],
        ) -> Result<Option<Value>, String> {
            let value: Value = serde_json::from_slice(body).map_err(|error| error.to_string())?;
            let catalog = ClaudeCatalog::from_value(value.clone())?;
            self.fetched_at_ms = Some(now_ms);
            if catalog.updated_at < self.catalog.updated_at {
                return Ok(None);
            }
            self.catalog = catalog.into();
            Ok(Some(value))
        }
    }

    fn fetch(url: &str, now: u64) -> Result<Option<Value>, String> {
        let cached = with_state(|state| state.source_responses.get(url).cloned());
        let mut request = ureq::get(url)
            .set("User-Agent", "tcode")
            .timeout(FETCH_TIMEOUT);
        if let Some((etag, _)) = &cached {
            request = request.set("If-None-Match", etag);
        }
        let response = request.call().map_err(|error| error.to_string())?;
        // A 304 can retain Content-Encoding despite having no response body.
        // Do not pass it through the decompressor.
        if response.status() == 304 {
            let (_, body) = cached.ok_or("304 without a cached manifest")?;
            return with_state(|state| state.install(now, &body));
        }
        let etag = response.header("ETag").map(str::to_owned);
        let mut body = Vec::new();
        response
            .into_reader()
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut body)
            .map_err(|error| error.to_string())?;
        if body.len() as u64 > MAX_RESPONSE_BYTES {
            return Err("response exceeds 1 MiB".into());
        }
        with_state(|state| {
            let adopted = state.install(now, &body)?;
            if let Some(etag) = etag {
                state.source_responses.insert(url.to_owned(), (etag, body));
            } else {
                state.source_responses.remove(url);
            }
            Ok(adopted)
        })
    }

    /// Load the disk cache under `cache_dir` and, when the TTL allows and
    /// `network` is on, fetch every remote manifest and keep the newest.
    /// Blocking; never fails.
    pub(crate) fn refresh(cache_dir: Option<&Path>, network: bool) {
        let cache_path = cache_dir.map(|dir| dir.join(CACHE_FILE));
        let now = now_ms();
        let due = with_state(|state| {
            if let Some(path) = &cache_path {
                state.load_cache(path);
            }
            state.begin_fetch(now, network)
        });
        if !due {
            return;
        }
        // The lock is not held across a fetch: `current()` must stay
        // instant for the UI while a 10 s timeout plays out. Sources are
        // installed as they arrive; `install` only moves forward, so the
        // last adoption is the newest manifest of the round.
        let mut newest = None;
        for (name, url) in MANIFEST_SOURCES {
            match fetch(url, now) {
                Ok(Some(manifest)) => newest = Some(manifest),
                Ok(None) => {
                    log::info!("{name} Claude model manifest is older than the current one")
                }
                Err(error) => log::warn!("{name} Claude model manifest refresh failed: {error}"),
            }
        }
        let (Some(manifest), Some(path)) = (newest, cache_path) else {
            return;
        };
        let cache = CacheFile {
            fetched_at_ms: now,
            manifest,
        };
        if let Err(error) = serde_json::to_vec(&cache)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&path, bytes))
        {
            log::warn!("failed to cache Claude model manifest: {error}");
        }
    }
}

/// A synthetic manifest exercising every adapter feature tcode reads, so
/// catalog tests never depend on the bundled model list.
#[cfg(test)]
pub(crate) const TEST_MANIFEST: &str = r#"{
  "version": 1,
  "updatedAt": "2030-01-01T00:00:00Z",
  "providers": {
    "claudeAgent": {
      "defaults": { "chat": "test-wide" },
      "profiles": {
        "wide": {
          "capabilities": { "optionDescriptors": [
            { "id": "effort", "label": "Reasoning", "type": "select", "options": [
              { "id": "low", "label": "Low" },
              { "id": "medium", "label": "Medium", "isDefault": true },
              { "id": "max", "label": "Max" },
              { "id": "ultracode", "label": "Ultracode", "description": "xhigh plus orchestration" },
              { "id": "ultrathink", "label": "Ultrathink" }
            ], "promptInjectedValues": ["ultrathink"] },
            { "id": "contextWindow", "label": "Context Window", "type": "select", "options": [
              { "id": "200k", "label": "200k" },
              { "id": "1m", "label": "1M", "isDefault": true }
            ] },
            { "id": "tone", "label": "Tone", "type": "slider" }
          ] },
          "adapter": { "claudeCode": {
            "effortMap": { "ultracode": "xhigh", "ultrathink": null },
            "modelSuffixes": { "contextWindow": { "1m": "[1m]" } },
            "contextWindowTokens": { "200k": 200000, "1m": 1000000 }
          } }
        },
        "narrow": {
          "capabilities": { "optionDescriptors": [
            { "id": "effort", "label": "Reasoning", "type": "select", "options": [
              { "id": "high", "label": "High", "isDefault": true },
              { "id": "max", "label": "Max" }
            ] },
            { "id": "contextWindow", "label": "Context Window", "type": "select", "options": [
              { "id": "200k", "label": "200k", "isDefault": true },
              { "id": "1m", "label": "1M" }
            ] }
          ] },
          "adapter": { "claudeCode": {
            "effortMap": { "max": "high" },
            "modelSuffixes": { "contextWindow": { "1m": "[1m]" } },
            "contextWindowTokens": { "200k": 200000, "1m": 1000000 }
          } }
        },
        "fixed": {
          "capabilities": { "optionDescriptors": [
            { "id": "effort", "label": "Reasoning", "type": "select", "options": [
              { "id": "high", "label": "High", "isDefault": true },
              { "id": "xhigh", "label": "Extra High" }
            ] },
            { "id": "fastMode", "label": "Fast Mode", "type": "boolean" }
          ] },
          "adapter": { "claudeCode": { "effortMap": { "xhigh": "max" }, "fixedContextWindowTokens": 1000000 } }
        },
        "plain": {
          "capabilities": { "optionDescriptors": [
            { "id": "thinking", "label": "Thinking", "type": "boolean" }
          ] },
          "adapter": { "claudeCode": {} }
        }
      },
      "models": [
        { "slug": "test-wide", "name": "Test Wide", "aliases": ["wide", "test-wide.0"], "status": "current", "badge": "new", "profile": "wide",
          "adapter": { "claudeCode": { "minVersion": "2.1.257" } } },
        { "slug": "test-fixed", "name": "Test Fixed", "status": "legacy", "profile": "fixed",
          "adapter": { "claudeCode": { "minVersion": "2.1.111", "maxVersionExclusive": "3.0.0" } } },
        { "slug": "test-narrow", "name": "Test Narrow", "status": "legacy", "profile": "narrow" },
        { "slug": "test-plain", "name": "Test Plain", "status": "legacy", "profile": "plain" }
      ]
    }
  }
}"#;

#[cfg(test)]
pub(crate) fn test_catalog() -> ClaudeCatalog {
    ClaudeCatalog::from_json(TEST_MANIFEST).expect("test manifest is valid")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn bundled_manifest_decodes_into_a_usable_catalog() {
        let catalog = bundled();
        assert!(!catalog.models.is_empty());
        assert_eq!(
            catalog
                .models
                .iter()
                .filter(|model| model.spec.is_default)
                .count(),
            1
        );
        // The offline fallback must carry the two selectors the composer keys on.
        let default = catalog.models.iter().find(|m| m.spec.is_default).unwrap();
        let ids: Vec<&str> = default
            .spec
            .options
            .iter()
            .map(|option| match option {
                OptionDescriptor::Select { id, .. } | OptionDescriptor::Boolean { id, .. } => {
                    id.as_str()
                }
            })
            .collect();
        assert!(ids.contains(&"reasoningEffort"), "{ids:?}");
        assert!(ids.contains(&"contextWindow"), "{ids:?}");
        assert!(!ids.contains(&"effort"), "manifest id must be translated");
    }

    #[test]
    fn manifest_descriptors_map_onto_tcode_options() {
        let catalog = test_catalog();
        let wide = catalog.model("test-wide").unwrap();
        assert_eq!(wide.spec.display_name, "Test Wide");
        assert!(wide.spec.is_default);
        assert_eq!(
            wide.spec.options,
            vec![
                OptionDescriptor::Select {
                    id: "reasoningEffort".into(),
                    label: "Reasoning".into(),
                    options: vec![
                        SelectOption {
                            value: "low".into(),
                            label: "Low".into(),
                            description: None,
                            unavailable: None,
                        },
                        SelectOption {
                            value: "medium".into(),
                            label: "Medium".into(),
                            description: None,
                            unavailable: None,
                        },
                        SelectOption {
                            value: "max".into(),
                            label: "Max".into(),
                            description: None,
                            unavailable: None,
                        },
                        SelectOption {
                            value: "ultracode".into(),
                            label: "Ultracode".into(),
                            description: Some("xhigh plus orchestration".into()),
                            unavailable: None,
                        },
                    ],
                    default_value: Some("medium".into()),
                    role: crate::OptionRole::Model,
                    apply: crate::ApplyTiming::Live,
                    recommended: None,
                    permissive: None,
                },
                OptionDescriptor::Select {
                    id: "contextWindow".into(),
                    label: "Context Window".into(),
                    options: vec![
                        SelectOption {
                            value: "200k".into(),
                            label: "200k".into(),
                            description: None,
                            unavailable: None,
                        },
                        SelectOption {
                            value: "1m".into(),
                            label: "1M".into(),
                            description: None,
                            unavailable: None,
                        },
                    ],
                    default_value: Some("1m".into()),
                    role: crate::OptionRole::Model,
                    apply: crate::ApplyTiming::Live,
                    recommended: None,
                    permissive: None,
                },
            ],
            "unknown descriptor types and prompt-injected values are dropped"
        );
        assert!(!catalog.model("test-fixed").unwrap().spec.is_default);
        assert_eq!(
            catalog.model("test-fixed").unwrap().spec.options[1],
            OptionDescriptor::Boolean {
                id: "fastMode".into(),
                label: "Fast Mode".into(),
                default_value: false,
                role: crate::OptionRole::Model,
                apply: crate::ApplyTiming::Live,
                recommended: None,
                permissive: None,
            }
        );
        // A fixed window still gets a selector, defaulting to that window.
        assert_eq!(
            catalog.model("test-fixed").unwrap().spec.options[2],
            OptionDescriptor::Select {
                id: "contextWindow".into(),
                label: "Context Window".into(),
                options: vec![SelectOption {
                    value: "1m".into(),
                    label: "1M".into(),
                    description: None,
                    unavailable: None,
                }],
                default_value: Some("1m".into()),
                role: crate::OptionRole::Model,
                apply: crate::ApplyTiming::Live,
                recommended: None,
                permissive: None,
            }
        );
        // No window data at all offers no selector.
        assert_eq!(catalog.model("test-plain").unwrap().spec.options.len(), 1);
        // Lookups tolerate whitespace and a context suffix.
        assert!(catalog.model(" test-plain ").is_some());
        assert!(catalog.model("test-narrow[1m]").is_some());
        assert!(catalog.model("test-missing").is_none());
        // Aliases resolve case-insensitively to the canonical slug.
        assert_eq!(catalog.model("WIDE").unwrap().spec.id, "test-wide");
        assert_eq!(
            catalog.model("test-wide.0[1m]").unwrap().spec.id,
            "test-wide"
        );
        assert!(catalog.model("wid").is_none());
    }

    #[test]
    fn invalid_manifests_are_rejected() {
        let mutate = |edit: fn(&mut Value)| {
            let mut value: Value = serde_json::from_str(TEST_MANIFEST).unwrap();
            edit(&mut value);
            ClaudeCatalog::from_value(value).expect_err("must be rejected")
        };
        assert!(mutate(|v| v["version"] = json!(2)).contains("version"));
        assert!(mutate(|v| v["providers"] = json!({})).contains("claudeAgent"));
        assert!(
            mutate(|v| v["providers"]["claudeAgent"]["models"][1]["slug"] = json!("test-wide"))
                .contains("duplicate")
        );
        assert!(
            mutate(|v| v["providers"]["claudeAgent"]["models"][0]["profile"] = json!("nope"))
                .contains("unknown profile")
        );
        assert!(
            mutate(|v| v["providers"]["claudeAgent"]["defaults"]["chat"] = json!("nope"))
                .contains("default model")
        );
        assert!(
            mutate(|v| {
                v["providers"]["claudeAgent"]["models"][0]["adapter"]["claudeCode"]["minVersion"] =
                    json!("latest")
            })
            .contains("invalid version")
        );
        assert!(mutate(|v| {
            v["providers"]["claudeAgent"]["models"][1]["adapter"]["claudeCode"]
                ["maxVersionExclusive"] = json!("2.1.111")
        })
        .contains("maxVersionExclusive"));
        assert!(mutate(|v| {
            v["providers"]["claudeAgent"]["profiles"]["wide"]["capabilities"]
                ["optionDescriptors"][0]["id"] = json!(" ")
        })
        .contains("option id"));
        assert!(mutate(|v| {
            v["providers"]["claudeAgent"]["profiles"]["wide"]["capabilities"]
                ["optionDescriptors"][0]["options"][0]["id"] = json!("")
        })
        .contains("option value"));
        assert!(mutate(|v| {
            v["providers"]["claudeAgent"]["profiles"]["wide"]["adapter"]["claudeCode"]
                ["effortMap"][""] = json!("low")
        })
        .contains("effortMap"));
        assert!(ClaudeCatalog::from_json("not json").is_err());
    }

    #[cfg(feature = "process")]
    mod refresh {
        use super::super::refresh::{CACHE_FILE, CacheFile, RETRY_MS, TTL_MS};
        use super::*;

        fn fresh_state() -> ManifestState {
            ManifestState::new(test_catalog())
        }

        fn model_ids(state: &ManifestState) -> Vec<String> {
            state
                .catalog
                .models
                .iter()
                .map(|model| model.spec.id.clone())
                .collect()
        }

        fn manifest_with(updated_at: &str, slug: &str) -> Value {
            let mut value: Value = serde_json::from_str(TEST_MANIFEST).unwrap();
            value["updatedAt"] = json!(updated_at);
            value["providers"]["claudeAgent"]["defaults"]["chat"] = json!(slug);
            value["providers"]["claudeAgent"]["models"][0]["slug"] = json!(slug);
            value
        }

        #[test]
        fn invalid_fetch_keeps_the_previous_catalog() {
            let mut state = fresh_state();
            let before = model_ids(&state);
            assert!(state.install(1_000, b"{ not json").is_err());
            let mut broken = manifest_with("2031-01-01T00:00:00Z", "test-wide");
            broken["providers"]["claudeAgent"]["models"][0]["profile"] = json!("missing");
            assert!(state.install(1_000, broken.to_string().as_bytes()).is_err());
            assert_eq!(model_ids(&state), before);
            assert_eq!(state.fetched_at_ms, None);

            let fresh = manifest_with("2031-01-01T00:00:00Z", "test-remote");
            let adopted = state.install(2_000, fresh.to_string().as_bytes()).unwrap();
            assert_eq!(adopted, Some(fresh));
            assert_eq!(model_ids(&state)[0], "test-remote");
            assert_eq!(state.fetched_at_ms, Some(2_000));
        }

        #[test]
        fn the_newest_manifest_wins_whatever_its_source() {
            // The test catalog (the bundle) is dated 2030; a remote copy from
            // 2029 is a valid manifest that predates it.
            let mut state = fresh_state();
            let stale = manifest_with("2029-12-31T23:59:59Z", "test-stale");
            let adopted = state.install(3_000, stale.to_string().as_bytes()).unwrap();
            assert_eq!(adopted, None);
            assert_eq!(model_ids(&state)[0], "test-wide");
            assert_eq!(state.fetched_at_ms, Some(3_000), "still paced by the TTL");

            // Same date as the current catalog: adopted.
            let same = manifest_with("2030-01-01T00:00:00Z", "test-same");
            assert!(
                state
                    .install(4_000, same.to_string().as_bytes())
                    .unwrap()
                    .is_some()
            );
            assert_eq!(model_ids(&state)[0], "test-same");

            // Sources arrive in any order within a round: the newer one ends
            // up in effect and the older is left alone, regardless of which
            // repository served it.
            let newer = manifest_with("2032-01-01T00:00:00Z", "test-newer");
            let older = manifest_with("2031-01-01T00:00:00Z", "test-older");
            assert!(
                state
                    .install(5_000, newer.to_string().as_bytes())
                    .unwrap()
                    .is_some()
            );
            assert!(
                state
                    .install(5_000, older.to_string().as_bytes())
                    .unwrap()
                    .is_none()
            );
            assert_eq!(model_ids(&state)[0], "test-newer");

            let mut state = fresh_state();
            assert!(
                state
                    .install(6_000, older.to_string().as_bytes())
                    .unwrap()
                    .is_some()
            );
            assert!(
                state
                    .install(6_000, newer.to_string().as_bytes())
                    .unwrap()
                    .is_some()
            );
            assert_eq!(model_ids(&state)[0], "test-newer");
        }

        #[test]
        fn fetches_are_paced_by_ttl_retry_and_setting() {
            let mut state = fresh_state();
            assert!(!state.begin_fetch(1_000, false), "network disabled");
            assert!(state.begin_fetch(1_000, true));
            assert!(!state.begin_fetch(1_000 + RETRY_MS - 1, true), "retry gap");
            assert!(state.begin_fetch(1_000 + RETRY_MS, true));
            state.fetched_at_ms = Some(10_000);
            assert!(!state.begin_fetch(10_000 + TTL_MS - 1, true), "fresh");
            assert!(state.begin_fetch(10_000 + TTL_MS, true));
            // A future timestamp (clock moved backwards) counts as expired.
            state.fetched_at_ms = Some(u64::MAX);
            state.last_attempt_ms = Some(u64::MAX);
            assert!(state.begin_fetch(5, true));
        }

        #[test]
        fn disk_cache_is_adopted_unless_the_bundle_is_newer() {
            let dir = std::env::temp_dir().join(format!(
                "tcode-manifest-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(CACHE_FILE);
            let write = |updated_at: &str| {
                let cache = CacheFile {
                    fetched_at_ms: 42,
                    manifest: manifest_with(updated_at, "test-cached"),
                };
                std::fs::write(&path, serde_json::to_vec(&cache).unwrap()).unwrap();
            };

            // Older than the bundle (test catalog dated 2030): ignored.
            write("2029-12-31T23:59:59Z");
            let mut state = fresh_state();
            state.load_cache(&path);
            assert_eq!(model_ids(&state)[0], "test-wide");
            assert_eq!(state.fetched_at_ms, None);

            // Same or newer: adopted, together with its fetch time.
            write("2030-01-01T00:00:00Z");
            let mut state = fresh_state();
            state.load_cache(&path);
            assert_eq!(model_ids(&state)[0], "test-cached");
            assert_eq!(state.fetched_at_ms, Some(42));

            // Loaded once: a later cache write is not re-read.
            write("2035-01-01T00:00:00Z");
            state.load_cache(&path);
            assert_eq!(model_ids(&state)[0], "test-cached");

            // Corrupt cache: bundle stays.
            std::fs::write(&path, b"{}").unwrap();
            let mut state = fresh_state();
            state.load_cache(&path);
            assert_eq!(model_ids(&state)[0], "test-wide");
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }
}
