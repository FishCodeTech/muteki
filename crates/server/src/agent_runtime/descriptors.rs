//! One descriptor per Agent engine.
//!
//! `descriptors.json` contains the supported engines from the original backend's
//! `public_descriptor()` dump, preserving the shapes the settings and picker UI
//! were written against. [`overlay`] holds capability differences: places where
//! the runtime in this build cannot honor what the source declared. Everything
//! runtime code needs is parsed into the typed view below; the public JSON is
//! served as-is.
//!
//! Probe results remain the runtime truth for a concrete installation;
//! [`AdapterDescriptor::capabilities`] is only the declared baseline.

use agent::ProviderKind;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::OnceLock;

const SOURCE: &str = include_str!("descriptors.json");

pub const DESCRIPTOR_SCHEMA_VERSION: u32 = 1;

/// Conversation-facing autonomy contract. Adapters own the translation to their
/// engine's native permission and sandbox controls.
pub const ACCESS_SUPERVISED: &str = "supervised";
pub const ACCESS_AUTO_ACCEPT_EDITS: &str = "auto-accept-edits";
pub const ACCESS_AUTO: &str = "auto";
pub const ACCESS_FULL: &str = "full-access";

pub const SOURCE_STATIC: &str = "static";
pub const SOURCE_PROBE: &str = "probe";
pub const SOURCE_REPORTED: &str = "adapter_reported";

/// Boolean capability fields, in the order the source contract lists them.
pub const BOOL_CAPABILITY_FIELDS: &[&str] = &[
    "streaming",
    "resume",
    "steer",
    "interrupt",
    "approval",
    "user_input",
    "fork",
    "structured_output",
    "subagents",
    "skills",
    "mcp",
    "native_tool_binding",
    "acp_mcp_config",
    "agent_plugin",
    "structured_http_rpc",
    "tool_events",
    "usage_events",
    "session_persistence",
    "plan",
    "plan_mode",
    "image_input",
    "compaction",
    "context_usage",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CustomTransport {
    DroidRpc,
    KimiLocalServer,
    OmpRpcV2,
    OpenCodeV2,
}

/// How a live session for one adapter is actually run. `None` on an adapter
/// means this build has no transport for it: callers must refuse the session
/// with a typed `capability.unsupported` rather than silently use another one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeTransport {
    /// A tcode-maintained adapter (`agent::start_session`).
    Provider(ProviderKind),
    /// The Agent Client Protocol over the engine binary's own ACP subcommand.
    Acp {
        /// Registry-style agent id reported to the UI and persisted with the session.
        agent_id: &'static str,
        /// Arguments after the binary that put it into ACP stdio mode.
        args: &'static [&'static str],
    },
    /// Muteki-owned native transport driver (`droid.rpc`, `kimi.local_server`, `omp.rpc_v2`).
    Custom(CustomTransport),
}

/// Where the engine binary comes from, highest precedence first.
#[derive(Clone, Copy, Debug, Default)]
pub struct BinarySpec {
    /// On-disk basename. Cursor's headless CLI ships as `cursor-agent`; the
    /// bare `cursor` launcher is a different tool.
    pub basename: &'static str,
    /// Operator override variable, trusted as given.
    pub env_override: &'static str,
    /// First-party install locations, highest first. `~` is expanded at
    /// resolve time. A GUI process inherits a short PATH, so these are
    /// searched before the PATH scan.
    pub known_good: &'static [&'static str],
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct AgentCapabilities {
    pub streaming: bool,
    pub resume: bool,
    pub resume_continues_turn: bool,
    pub steer: bool,
    pub interrupt: bool,
    pub approval: bool,
    pub user_input: bool,
    pub fork: bool,
    pub structured_output: bool,
    pub subagents: bool,
    pub skills: bool,
    pub mcp: bool,
    pub native_tool_binding: bool,
    pub acp_mcp_config: bool,
    pub agent_plugin: bool,
    pub runtime_plugin_binding: bool,
    pub structured_http_rpc: bool,
    pub tool_events: bool,
    pub usage_events: bool,
    pub session_persistence: bool,
    pub plan: bool,
    pub plan_mode: bool,
    pub plan_mode_per_turn: bool,
    pub model_options_per_turn: bool,
    pub image_input: bool,
    pub supported_models: Vec<String>,
    pub supported_efforts: Vec<String>,
    pub access_modes: Vec<String>,
    pub permission_modes: Vec<String>,
    pub sandbox_modes: Vec<String>,
    pub compaction: bool,
    pub context_usage: bool,
    pub transport_kind: String,
    pub protocol_version: String,
    pub runtime_version: String,
    pub capability_source: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AdapterDescriptor {
    pub adapter_id: String,
    pub role: String,
    pub transport_kind: String,
    #[serde(default)]
    pub legacy_aliases: Vec<String>,
    pub capabilities: AgentCapabilities,
    #[serde(default)]
    pub native_rewind: bool,
    #[serde(default)]
    pub capability_gateway: bool,
    #[serde(default)]
    pub access_mode_notes: BTreeMap<String, String>,
    #[serde(default)]
    pub scoped_model_catalog_probe: bool,
    #[serde(default)]
    pub accepts_adapter_endpoint: bool,
    #[serde(default)]
    pub accepts_launch_args: bool,
    #[serde(default)]
    pub transport_settings: Vec<Value>,
    #[serde(default)]
    pub notes: String,
    /// The session transport this build runs the adapter on, if any.
    #[serde(skip)]
    pub native: Option<NativeTransport>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ProviderIdentity {
    pub engine: String,
    pub display_name: String,
    pub support_status: String,
    pub default_adapter_id: String,
    pub cli_adapter_id: String,
    pub transport_label: String,
    /// Listed and enabled for chat without the user adding it first.
    #[serde(default)]
    pub default_enabled: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct LoginSpec {
    pub guidance_command: String,
    pub guidance_note: String,
    pub login_argv: Vec<String>,
    pub status_probe: String,
    pub status_argv: Vec<String>,
    pub host_login_import: String,
    pub system_login_only: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct EngineDescriptor {
    pub identity: ProviderIdentity,
    pub adapters: Vec<AdapterDescriptor>,
    #[serde(default)]
    pub login: LoginSpec,
    #[serde(skip)]
    pub binary: BinarySpec,
    /// The public descriptor, overlays applied.
    #[serde(skip)]
    public: Value,
}

impl EngineDescriptor {
    pub fn engine(&self) -> &str {
        &self.identity.engine
    }

    /// An adapter by id or legacy alias; an empty id selects the default.
    pub fn adapter(&self, adapter_id: &str) -> Option<&AdapterDescriptor> {
        let wanted = if adapter_id.is_empty() {
            self.identity.default_adapter_id.as_str()
        } else {
            adapter_id
        };
        self.adapters.iter().find(|item| {
            item.adapter_id == wanted || item.legacy_aliases.iter().any(|alias| alias == wanted)
        })
    }

    pub fn default_adapter(&self) -> &AdapterDescriptor {
        self.adapter("").expect("validated when the table is loaded")
    }

    /// The descriptor exactly as the descriptor API publishes it.
    pub fn public(&self) -> &Value {
        &self.public
    }
}

fn overlay(_descriptor: &mut Value) {}

fn native(adapter_id: &str) -> Option<NativeTransport> {
    use NativeTransport::{Acp, Custom, Provider};
    Some(match adapter_id {
        "codex.app_server" => Provider(ProviderKind::Codex),
        "claude.sdk" => Provider(ProviderKind::ClaudeCode),
        "cursor.sdk" => Provider(ProviderKind::Cursor),
        "cursor.acp" => Acp { agent_id: "cursor", args: &["acp"] },
        "grok.acp" => Provider(ProviderKind::Grok),
        "opencode.server" => Custom(CustomTransport::OpenCodeV2),
        "pi.rpc" => Provider(ProviderKind::Pi),
        "kimi.acp" => Acp { agent_id: "kimi", args: &["acp"] },
        "kimi.local_server" => Custom(CustomTransport::KimiLocalServer),
        "omp.acp" => Acp { agent_id: "omp", args: &["acp"] },
        "omp.rpc_v2" => Custom(CustomTransport::OmpRpcV2),
        "agy.acp" => Acp { agent_id: "agy", args: &["acp", "--process-per-session"] },
        "devin.acp" => Acp { agent_id: "devin", args: &["acp"] },
        "droid.rpc" => Custom(CustomTransport::DroidRpc),
        "droid.acp" => Acp { agent_id: "droid", args: &["exec", "--output-format", "acp"] },
        _ => return None,
    })
}

fn binary(engine: &str) -> BinarySpec {
    let (basename, env_override, known_good): (&'static str, &'static str, &'static [&'static str]) =
        match engine {
            "codex" => ("codex", "MUTEKI_CODEX_BIN", &["~/.local/bin/codex", "/opt/homebrew/bin/codex", "/usr/local/bin/codex"]),
            "claude" => ("claude", "MUTEKI_CLAUDE_BIN", &["~/.local/bin/claude", "/opt/homebrew/bin/claude", "/usr/local/bin/claude"]),
            "cursor" => ("cursor-agent", "MUTEKI_CURSOR_BIN", &["~/.local/bin/cursor-agent", "/opt/homebrew/bin/cursor-agent", "/usr/local/bin/cursor-agent"]),
            "grok" => ("grok", "MUTEKI_GROK_BIN", &["~/.grok/bin/grok", "~/.local/bin/grok", "/opt/homebrew/bin/grok", "/usr/local/bin/grok"]),
            "opencode" => ("opencode", "MUTEKI_OPENCODE_BIN", &["~/.local/bin/opencode", "/opt/homebrew/bin/opencode", "/usr/local/bin/opencode"]),
            "pi" => ("pi", "MUTEKI_PI_BIN", &["~/.local/bin/pi", "/opt/homebrew/bin/pi", "/usr/local/bin/pi"]),
            "kimi" => ("kimi", "MUTEKI_KIMI_BIN", &["~/.kimi-code/bin/kimi", "~/.local/bin/kimi", "/opt/homebrew/bin/kimi", "/usr/local/bin/kimi"]),
            "omp" => ("omp", "MUTEKI_OMP_BIN", &["~/.bun/bin/omp", "~/.local/bin/omp", "/opt/homebrew/bin/omp", "/usr/local/bin/omp"]),
            "agy" => ("agy", "MUTEKI_AGY_BIN", &["~/.local/bin/agy", "/opt/homebrew/bin/agy", "/usr/local/bin/agy"]),
            "devin" => ("devin", "MUTEKI_DEVIN_BIN", &["~/.local/bin/devin", "/opt/homebrew/bin/devin", "/usr/local/bin/devin"]),
            "droid" => ("droid", "MUTEKI_DROID_BIN", &["~/.local/bin/droid", "/usr/local/bin/droid", "/opt/homebrew/bin/droid"]),
            other => panic!("engine {other} has no binary resolution"),
        };
    BinarySpec { basename, env_override, known_good }
}

fn load() -> Vec<EngineDescriptor> {
    let source: Vec<Value> = serde_json::from_str(SOURCE).expect("descriptors.json is valid JSON");
    let mut seen = std::collections::BTreeSet::new();
    source
        .into_iter()
        .map(|mut public| {
            overlay(&mut public);
            let mut descriptor: EngineDescriptor =
                serde_json::from_value(public.clone()).expect("descriptor matches the typed view");
            descriptor.binary = binary(descriptor.engine());
            for adapter in &mut descriptor.adapters {
                adapter.native = native(&adapter.adapter_id);
                for key in std::iter::once(&adapter.adapter_id).chain(adapter.legacy_aliases.iter()) {
                    assert!(seen.insert(key.clone()), "duplicate adapter id {key}");
                }
            }
            assert!(
                descriptor.adapter("").is_some(),
                "engine {} declares a missing default adapter",
                descriptor.engine()
            );
            descriptor.public = public;
            descriptor
        })
        .collect()
}

/// Every declared engine, in the order provider lists present them.
pub fn all() -> &'static [EngineDescriptor] {
    static TABLE: OnceLock<Vec<EngineDescriptor>> = OnceLock::new();
    TABLE.get_or_init(load)
}

pub fn engine_ids() -> Vec<&'static str> {
    all().iter().map(EngineDescriptor::engine).collect()
}

pub fn find(engine: &str) -> Option<&'static EngineDescriptor> {
    let engine = engine.trim().to_ascii_lowercase();
    all().iter().find(|item| item.identity.engine == engine)
}

/// Resolve an adapter id (or a bare engine id) to its engine and adapter.
pub fn for_adapter(adapter_id: &str) -> Option<(&'static EngineDescriptor, &'static AdapterDescriptor)> {
    let wanted = adapter_id.trim();
    if !wanted.is_empty() {
        for descriptor in all() {
            if let Some(adapter) = descriptor.adapter(wanted) {
                return Some((descriptor, adapter));
            }
        }
    }
    let descriptor = find(wanted)?;
    Some((descriptor, descriptor.default_adapter()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn differences(path: String, source: &Value, public: &Value, out: &mut Vec<String>) {
        match (source, public) {
            (Value::Object(a), Value::Object(b)) => {
                for key in a.keys().chain(b.keys().filter(|key| !a.contains_key(*key))) {
                    let next = format!("{path}.{key}");
                    match (a.get(key), b.get(key)) {
                        (Some(x), Some(y)) => differences(next, x, y, out),
                        _ => out.push(next),
                    }
                }
            }
            (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
                for (x, y) in a.iter().zip(b) {
                    let label = x["adapter_id"].as_str().map(str::to_owned).unwrap_or_default();
                    differences(format!("{path}[{label}]"), x, y, out);
                }
            }
            _ if source != public => out.push(path),
            _ => {}
        }
    }

    #[test]
    fn public_descriptors_are_the_source_except_for_declared_overlays() {
        let source: Vec<Value> = serde_json::from_str(SOURCE).unwrap();
        assert_eq!(source.len(), all().len());
        let mut found = Vec::new();
        for (raw, descriptor) in source.iter().zip(all()) {
            differences(descriptor.engine().to_owned(), raw, descriptor.public(), &mut found);
        }
        assert!(found.is_empty(), "unexpected descriptor overlays: {found:?}");
    }

    #[test]
    fn every_engine_resolves_its_default_adapter() {
        for descriptor in all() {
            let default = descriptor.default_adapter();
            assert_eq!(default.adapter_id, descriptor.identity.default_adapter_id);
            let expected = if default.adapter_id == descriptor.identity.cli_adapter_id { "cli" } else { "default" };
            assert_eq!(default.role, expected, "{}", descriptor.engine());
        }
        assert_eq!(find("droid").unwrap().default_adapter().adapter_id, "droid.rpc");
    }

    #[test]
    fn legacy_adapter_aliases_resolve() {
        let (engine, adapter) = for_adapter("claude.agent_sdk").expect("alias resolves");
        assert_eq!(engine.engine(), "claude");
        assert_eq!(adapter.adapter_id, "claude.sdk");
        let (engine, adapter) = for_adapter("omp.rpc").expect("alias resolves");
        assert_eq!(engine.engine(), "omp");
        assert_eq!(adapter.adapter_id, "omp.rpc_v2");
    }

    #[test]
    fn a_bare_engine_id_selects_the_default_adapter() {
        let (engine, adapter) = for_adapter("codex").expect("engine resolves");
        assert_eq!(engine.engine(), "codex");
        assert_eq!(adapter.adapter_id, "codex.app_server");
        assert!(for_adapter("").is_none());
    }

    #[test]
    fn conversation_engines_declare_a_transport() {
        for engine in ["codex", "claude", "cursor", "grok", "opencode", "pi", "kimi", "omp", "devin", "droid"] {
            let descriptor = find(engine).expect("declared engine");
            assert!(
                descriptor.default_adapter().native.is_some(),
                "{engine} must declare a session transport"
            );
        }
        assert!(find("zcode").is_none());
        assert!(for_adapter("cli.zcode").is_none());
    }

    #[test]
    fn cli_compatibility_variants_are_declared_but_not_native_sessions() {
        for adapter_id in ["cli.codex", "cli.droid"] {
            let (_, adapter) = for_adapter(adapter_id).expect("declared adapter");
            assert!(adapter.native.is_none(), "{adapter_id} must not claim a native session transport");
        }
        for adapter_id in ["kimi.local_server", "omp.rpc_v2", "droid.rpc", "droid.acp"] {
            let (_, adapter) = for_adapter(adapter_id).expect("declared adapter");
            assert!(adapter.native.is_some(), "{adapter_id} must have a native transport");
        }
    }

    #[test]
    fn every_runnable_adapter_is_declared() {
        let declared: Vec<_> = all()
            .iter()
            .flat_map(|descriptor| descriptor.adapters.iter())
            .filter(|adapter| adapter.native.is_some())
            .map(|adapter| adapter.adapter_id.as_str())
            .collect();
        assert_eq!(declared.len(), 14, "{declared:?}");
    }
}
