//! Orchestrate rows written by versions that stored a copy of the bundled
//! guidance, or a fixed effort per row.
//!
//! The fingerprints below cover every guidance text those versions persisted
//! as a bundled default. Settings no longer write bundled text, so the set is
//! closed: a later bundle change needs no entry here.

use agent::ProviderKind;
use serde::Deserialize;

use super::OrchestrateChildModel;
use super::orchestrate_fleet::{self, Fleet, Role};

const SOL_5_6_EXECUTION: u64 = 0xb46da64639e46cc4;
const SOL_6_EXECUTION: u64 = 0xdff8cdeaebad644b;
const SOL_6_1_EXECUTION: u64 = 0x9dd4b82b0e2c1246;
const ASTRA_LOW_EXECUTION: u64 = 0x29119a347989d9ff;
const ASTRA_EXECUTION: u64 = 0x5d3eb02435fb0d37;
const OPUS_5_EXECUTION: u64 = 0xee89eee2a5e7c8e6;
const OPUS_ACROSS_PROVIDERS_EXECUTION: u64 = 0x381bc425522e6438;
const OPUS_5_5_EXECUTION: u64 = 0xfd27aee110ced32e;
const ASTRA_COLLABORATION: u64 = 0x7dafa492a5d0a69c;
const FABLE_5_1_COLLABORATION: u64 = 0xf051ca6ccad8f678;

const HISTORICAL_DEFAULTS: &[(Role, ProviderKind, &str, &[u64])] = &[
    (
        Role::Execution,
        ProviderKind::Codex,
        "gpt-5.6-sol",
        &[SOL_5_6_EXECUTION],
    ),
    (
        Role::Execution,
        ProviderKind::Codex,
        "gpt-6-sol",
        &[SOL_6_EXECUTION],
    ),
    (
        Role::Execution,
        ProviderKind::Codex,
        "gpt-6.1-sol",
        &[SOL_6_1_EXECUTION],
    ),
    (
        Role::Execution,
        ProviderKind::Codex,
        "gpt-6-astra",
        &[ASTRA_LOW_EXECUTION, ASTRA_EXECUTION],
    ),
    (
        Role::Execution,
        ProviderKind::ClaudeCode,
        "claude-opus-5",
        &[
            OPUS_5_EXECUTION,
            OPUS_ACROSS_PROVIDERS_EXECUTION,
            OPUS_5_5_EXECUTION,
        ],
    ),
    (
        Role::Execution,
        ProviderKind::ClaudeCode,
        "claude-opus-5-5",
        &[OPUS_ACROSS_PROVIDERS_EXECUTION, OPUS_5_5_EXECUTION],
    ),
    (
        Role::Collaboration,
        ProviderKind::Codex,
        "gpt-6-astra",
        &[ASTRA_COLLABORATION],
    ),
    (
        Role::Collaboration,
        ProviderKind::ClaudeCode,
        "claude-fable-5-1",
        &[FABLE_5_1_COLLABORATION],
    ),
];

/// Bundled guidance of the fixed-effort tiers, before rows were per model.
const EFFORT_TIER_DEFAULTS: &[u64] = &[
    0x3947eed54a8f994d,
    0xb85f3d7c2df53336,
    0x143922adfdca3f99,
    0xa2dc3a22e1187d46,
    0x0a6dcb139997f7c2,
    0xcf136f8ff8285d4b,
    0xe89d390cac9b873e,
];
const SONNET_EFFORT_TIER: u64 = 0x147cdba2adc4b858;

/// The bundled profile that took over an untouched row of this role and model.
fn successor(role: Role, provider: ProviderKind, model: &str) -> Option<&'static str> {
    match (role, provider, model) {
        (
            Role::Execution,
            ProviderKind::Codex,
            "gpt-5.6-sol" | "gpt-6-sol" | "gpt-6.1-sol" | "gpt-6-astra",
        ) => Some("sol"),
        (
            Role::Execution,
            ProviderKind::ClaudeCode,
            "claude-opus-4-8" | "claude-opus-5" | "claude-opus-5-5",
        ) => Some("opus"),
        (Role::Collaboration, ProviderKind::Codex, "gpt-6-astra") => Some("astra"),
        (Role::Collaboration, ProviderKind::ClaudeCode, "claude-fable-5" | "claude-fable-5-1") => {
            Some("fable")
        }
        _ => None,
    }
}

/// FNV-1a of the trimmed text; old files kept `include_str!` guidance with
/// its trailing newline. std's hasher is not stable across releases.
fn fingerprint(text: &str) -> u64 {
    text.trim().bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}

#[derive(Deserialize)]
pub(super) struct LegacyOrchestrateModel {
    #[serde(flatten)]
    entry: OrchestrateChildModel,
    #[serde(default, alias = "default_effort")]
    effort: Option<String>,
}

/// Rows of each role as the current format stores them. `decisions` is
/// `None` for files from before collaboration and execution were split.
pub(super) fn migrate(
    decisions: Option<Vec<LegacyOrchestrateModel>>,
    children: Vec<LegacyOrchestrateModel>,
    default_decisions: Vec<OrchestrateChildModel>,
) -> (Vec<OrchestrateChildModel>, Vec<OrchestrateChildModel>) {
    let fleet = orchestrate_fleet::bundled();
    let Some(decisions) = decisions else {
        let (legacy_decisions, children): (Vec<_>, Vec<_>) =
            children.into_iter().partition(|row| {
                matches!(
                    (row.entry.provider, row.entry.model.as_str()),
                    (ProviderKind::Codex, "gpt-6-astra")
                        | (
                            ProviderKind::ClaudeCode,
                            "claude-fable-5" | "claude-fable-5-1"
                        )
                )
            });
        let mut decisions = default_decisions;
        let mut migrated = migrate_role(fleet, Role::Collaboration, legacy_decisions);
        for builtin in &mut decisions {
            if let Some(index) = migrated.iter().position(|entry| {
                builtin.provider == entry.provider && builtin.model == entry.model
            }) {
                *builtin = migrated.remove(index);
            }
        }
        decisions.extend(migrated);
        return (decisions, migrate_role(fleet, Role::Execution, children));
    };
    (
        migrate_role(fleet, Role::Collaboration, decisions),
        migrate_role(fleet, Role::Execution, children),
    )
}

fn migrate_role(
    fleet: &Fleet,
    role: Role,
    rows: Vec<LegacyOrchestrateModel>,
) -> Vec<OrchestrateChildModel> {
    let listed: Vec<_> = rows
        .iter()
        .map(|row| (row.entry.provider, row.entry.model.clone()))
        .collect();
    rows.into_iter()
        .filter_map(|row| {
            let model = row.entry.model.clone();
            let entry = migrate_row(fleet, role, row)?;
            // An untouched row whose successor the user already listed merges into it.
            let superseded = entry.model != model
                && entry.bundled.is_some()
                && listed.contains(&(entry.provider, entry.model.clone()));
            (!superseded).then_some(entry)
        })
        .collect()
}

fn migrate_row(
    fleet: &Fleet,
    role: Role,
    row: LegacyOrchestrateModel,
) -> Option<OrchestrateChildModel> {
    let mut entry = row.entry;
    let text = fingerprint(&entry.description);
    let untouched = if let Some(effort) = row.effort {
        if entry.provider == ProviderKind::ClaudeCode {
            match entry.model.as_str() {
                "claude-sonnet-5" if text == SONNET_EFFORT_TIER => return None,
                "claude-opus-4-8" | "claude-opus-5" => entry.model = "claude-opus-5-5".into(),
                "claude-fable-5" => entry.model = "claude-fable-5-1".into(),
                _ => {}
            }
        }
        let untouched = EFFORT_TIER_DEFAULTS.contains(&text);
        if !untouched && !entry.description.trim().is_empty() {
            // Old custom tier guidance remains meaningful after merging rows.
            entry.description = format!(
                "Guidance previously used at {effort} effort: {}",
                entry.description
            );
        }
        untouched
    } else {
        HISTORICAL_DEFAULTS
            .iter()
            .any(|(row_role, provider, model, texts)| {
                *row_role == role
                    && *provider == entry.provider
                    && *model == entry.model
                    && texts.contains(&text)
            })
    };
    if !untouched {
        return Some(entry);
    }
    let successor = successor(role, entry.provider, &entry.model).and_then(|id| fleet.profile(id));
    if let Some(profile) = successor.filter(|_| entry.profile_id.is_none()) {
        entry.provider = profile.provider;
        entry.model = profile.model.clone();
        entry.description.clear();
        entry.bundled = Some(profile.id.clone());
    } else if fleet
        .profile_for(role, entry.provider, &entry.model)
        .is_some()
    {
        // An endpoint profile keeps its model; the bundled guidance still applies.
        entry.description.clear();
    }
    Some(entry)
}
