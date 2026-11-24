//! Orchestrate's bundled fleet: the shipped collaboration and execution
//! profiles, their guidance, and the effort lists used before discovery.

use std::sync::LazyLock;

use agent::ProviderKind;
use serde::Deserialize;

const BUNDLED: &str = include_str!("../../../../assets/orchestrate/fleet.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Role {
    Collaboration,
    Execution,
}

impl Role {
    pub(super) fn of(collaboration: bool) -> Self {
        if collaboration {
            Self::Collaboration
        } else {
            Self::Execution
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FleetFile {
    profiles: Vec<ProfileFile>,
    effort_fallbacks: Vec<EffortFallback>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileFile {
    id: String,
    role: Role,
    provider: ProviderKind,
    model: String,
    #[serde(default)]
    default: bool,
    /// Paragraphs, joined with a blank line.
    guidance: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EffortFallback {
    provider: ProviderKind,
    models: Vec<String>,
    efforts: Vec<String>,
}

pub(super) struct Profile {
    pub id: String,
    pub role: Role,
    pub provider: ProviderKind,
    pub model: String,
    pub default: bool,
    pub guidance: String,
}

pub(super) struct Fleet {
    profiles: Vec<Profile>,
    effort_fallbacks: Vec<EffortFallback>,
}

static BUNDLED_FLEET: LazyLock<Fleet> = LazyLock::new(|| {
    Fleet::from_json(BUNDLED).expect("bundled Orchestrate fleet manifest is valid")
});

pub(super) fn bundled() -> &'static Fleet {
    &BUNDLED_FLEET
}

impl Fleet {
    fn from_json(text: &str) -> Result<Self, String> {
        let file: FleetFile = serde_json::from_str(text).map_err(|error| error.to_string())?;
        let mut profiles: Vec<Profile> = Vec::with_capacity(file.profiles.len());
        for profile in file.profiles {
            if profile.guidance.is_empty()
                || profile
                    .guidance
                    .iter()
                    .any(|paragraph| paragraph.trim().is_empty())
            {
                return Err(format!("profile {} has an empty paragraph", profile.id));
            }
            // Rows resolve guidance by role, provider and model, and follow a profile by id.
            if profiles.iter().any(|existing| {
                existing.id == profile.id
                    || (existing.role == profile.role
                        && existing.provider == profile.provider
                        && existing.model == profile.model)
            }) {
                return Err(format!("profile {} is not unique", profile.id));
            }
            profiles.push(Profile {
                id: profile.id,
                role: profile.role,
                provider: profile.provider,
                model: profile.model,
                default: profile.default,
                guidance: profile.guidance.join("\n\n"),
            });
        }
        Ok(Self {
            profiles,
            effort_fallbacks: file.effort_fallbacks,
        })
    }

    pub(super) fn profile(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|profile| profile.id == id)
    }

    pub(super) fn profile_for(
        &self,
        role: Role,
        provider: ProviderKind,
        model: &str,
    ) -> Option<&Profile> {
        self.profiles.iter().find(|profile| {
            profile.role == role && profile.provider == provider && profile.model == model
        })
    }

    pub(super) fn defaults(&self, role: Role) -> impl Iterator<Item = &Profile> {
        self.profiles
            .iter()
            .filter(move |profile| profile.default && profile.role == role)
    }

    pub(super) fn effort_fallback(&self, provider: ProviderKind, model: &str) -> &[String] {
        self.effort_fallbacks
            .iter()
            .find(|fallback| {
                fallback.provider == provider && fallback.models.iter().any(|id| id == model)
            })
            .map(|fallback| fallback.efforts.as_slice())
            .unwrap_or_default()
    }
}
