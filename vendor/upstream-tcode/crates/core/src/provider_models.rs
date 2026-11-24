//! Pure provider model resolution, ordering, and custom-slug validation.

use agent::{ModelSpec, OptionDescriptor, OptionSelection, SelectOption};
use serde_json::Value;

use crate::settings::ProviderSettings;

pub const MAX_SLUG_LEN: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelCapability {
    FastMode,
    Thinking,
    Reasoning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    pub id: String,
    pub name: String,
    pub favorite: bool,
    pub hidden: bool,
    pub custom: bool,
    pub capabilities: Vec<ModelCapability>,
}

fn capabilities(spec: &ModelSpec) -> Vec<ModelCapability> {
    let mut out = Vec::new();
    for option in &spec.options {
        let capability = match option {
            OptionDescriptor::Boolean { id, .. } if id == "fastMode" => ModelCapability::FastMode,
            OptionDescriptor::Boolean { id, .. } if id == "thinking" => ModelCapability::Thinking,
            OptionDescriptor::Select { id, .. } if id == "reasoningEffort" => {
                ModelCapability::Reasoning
            }
            _ => continue,
        };
        if !out.contains(&capability) {
            out.push(capability);
        }
    }
    out
}

/// A model's fast-mode switch: Claude's `fastMode` boolean or a Codex
/// `serviceTier` select offering a fast tier, and above it an ultrafast tier
/// when the select offers one. `None` when the model has neither. The
/// composer's bolt and the Orchestrate Fast switch both key on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FastMode {
    /// The option id the switch writes.
    pub option_id: String,
    off: Value,
    fast: Value,
    ultra: Option<Value>,
    default_level: FastLevel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FastLevel {
    Off,
    Fast,
    Ultra,
}

impl FastMode {
    pub fn of(spec: &ModelSpec) -> Option<Self> {
        spec.options.iter().find_map(|option| match option {
            OptionDescriptor::Boolean {
                id, default_value, ..
            } if id == "fastMode" => Some(Self {
                option_id: id.clone(),
                off: Value::Bool(false),
                fast: Value::Bool(true),
                ultra: None,
                default_level: if *default_value {
                    FastLevel::Fast
                } else {
                    FastLevel::Off
                },
            }),
            OptionDescriptor::Select {
                id,
                options,
                default_value,
                ..
            } if id == "serviceTier" => {
                let fast = options.iter().find(|choice| Self::is_fast_tier(choice))?;
                let ultra = options
                    .iter()
                    .find(|choice| Self::is_ultrafast_tier(choice));
                let default = default_value.as_deref();
                let default_level = if default == Some(fast.value.as_str()) {
                    FastLevel::Fast
                } else if ultra.is_some_and(|ultra| default == Some(ultra.value.as_str())) {
                    FastLevel::Ultra
                } else {
                    FastLevel::Off
                };
                let off = match (default_level, default) {
                    (FastLevel::Off, Some(tier)) => tier.to_owned(),
                    _ => "default".into(),
                };
                Some(Self {
                    option_id: id.clone(),
                    off: Value::String(off),
                    fast: Value::String(fast.value.clone()),
                    ultra: ultra.map(|ultra| Value::String(ultra.value.clone())),
                    default_level,
                })
            }
            _ => None,
        })
    }

    /// Whether a service-tier choice is the fast one: Codex's speed tiers name
    /// it `fast`; its app-server model list reports `priority` labelled "Fast".
    pub fn is_fast_tier(choice: &SelectOption) -> bool {
        choice.value == "fast" || choice.label.eq_ignore_ascii_case("fast")
    }

    /// Whether a service-tier choice is Codex's `ultrafast` tier.
    pub fn is_ultrafast_tier(choice: &SelectOption) -> bool {
        choice.value == "ultrafast" || choice.label.eq_ignore_ascii_case("ultrafast")
    }

    pub fn level(&self, selections: &[OptionSelection]) -> FastLevel {
        let Some(selection) = selections
            .iter()
            .find(|selection| selection.id == self.option_id)
        else {
            return self.default_level;
        };
        if selection.value == self.fast {
            FastLevel::Fast
        } else if self.ultra.as_ref() == Some(&selection.value) {
            FastLevel::Ultra
        } else {
            FastLevel::Off
        }
    }

    /// The level a press of the switch moves to: off, fast, then ultrafast
    /// when the model offers it, then off again.
    pub fn next(&self, level: FastLevel) -> FastLevel {
        match level {
            FastLevel::Off => FastLevel::Fast,
            FastLevel::Fast if self.ultra.is_some() => FastLevel::Ultra,
            FastLevel::Fast | FastLevel::Ultra => FastLevel::Off,
        }
    }

    /// The option value that selects a level.
    pub fn value(&self, level: FastLevel) -> Value {
        match level {
            FastLevel::Off => self.off.clone(),
            FastLevel::Fast => self.fast.clone(),
            FastLevel::Ultra => self.ultra.clone().unwrap_or_else(|| self.fast.clone()),
        }
    }
}

pub fn resolve_models(
    catalog: &[ModelSpec],
    settings: &ProviderSettings,
    favorites: &[String],
) -> Vec<ResolvedModel> {
    let is_favorite = |id: &str| favorites.iter().any(|f| f == id);
    let is_hidden = |id: &str| settings.hidden_models.iter().any(|h| h == id);
    let mut rows: Vec<_> = catalog
        .iter()
        .map(|spec| ResolvedModel {
            id: spec.id.clone(),
            name: spec.display_name.clone(),
            favorite: is_favorite(&spec.id),
            hidden: is_hidden(&spec.id),
            custom: false,
            capabilities: capabilities(spec),
        })
        .collect();
    for slug in &settings.custom_models {
        if rows.iter().any(|row| &row.id == slug) {
            continue;
        }
        rows.push(ResolvedModel {
            id: slug.clone(),
            name: slug.clone(),
            favorite: is_favorite(slug),
            hidden: is_hidden(slug),
            custom: true,
            capabilities: Vec::new(),
        });
    }
    // Favorites float to the top; everything else keeps its catalog order.
    rows.sort_by_key(|row| !row.favorite);
    rows
}

pub fn picker_models(
    catalog: &[ModelSpec],
    settings: &ProviderSettings,
    favorites: &[String],
) -> Vec<ResolvedModel> {
    resolve_models(catalog, settings, favorites)
        .into_iter()
        .filter(|row| !row.hidden)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlugError {
    Empty,
    AlreadyBuiltIn,
    TooLong,
    AlreadySaved,
}

pub fn validate_slug(
    raw: &str,
    catalog: &[ModelSpec],
    settings: &ProviderSettings,
) -> Result<String, SlugError> {
    let slug = raw.trim();
    if slug.is_empty() {
        return Err(SlugError::Empty);
    }
    if slug.chars().count() > MAX_SLUG_LEN {
        return Err(SlugError::TooLong);
    }
    if catalog.iter().any(|spec| spec.id == slug) {
        return Err(SlugError::AlreadyBuiltIn);
    }
    if settings.custom_models.iter().any(|model| model == slug) {
        return Err(SlugError::AlreadySaved);
    }
    Ok(slug.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::{OptionDescriptor, SelectOption};

    fn spec(id: &str, name: &str) -> ModelSpec {
        ModelSpec {
            id: id.into(),
            display_name: name.into(),
            is_default: false,
            options: Vec::new(),
        }
    }

    fn catalog() -> Vec<ModelSpec> {
        vec![
            spec("opus", "Opus"),
            spec("sonnet", "Sonnet"),
            spec("haiku", "Haiku"),
        ]
    }

    #[test]
    fn model_preferences_keep_catalog_order_and_hidden_rows_out_of_the_picker() {
        let mut models = catalog();
        models[1].options = vec![
            OptionDescriptor::Boolean {
                role: Default::default(),
                apply: Default::default(),
                recommended: None,
                permissive: None,
                id: "thinking".into(),
                label: "Thinking".into(),
                default_value: false,
            },
            OptionDescriptor::Select {
                role: Default::default(),
                apply: Default::default(),
                recommended: None,
                permissive: None,
                id: "reasoningEffort".into(),
                label: "Reasoning".into(),
                options: vec![SelectOption {
                    unavailable: None,
                    value: "high".into(),
                    label: "High".into(),
                    description: None,
                }],
                default_value: None,
            },
        ];
        let settings = ProviderSettings {
            custom_models: vec!["custom".into(), "hidden-custom".into()],
            hidden_models: vec!["opus".into(), "hidden-custom".into()],
            ..Default::default()
        };
        for (favorites, expected) in [
            (vec![], vec!["sonnet", "haiku", "custom"]),
            (
                vec!["haiku".into(), "sonnet".into()],
                vec!["sonnet", "haiku", "custom"],
            ),
            (
                vec![
                    "custom".into(),
                    "haiku".into(),
                    "opus".into(),
                    "missing".into(),
                ],
                vec!["haiku", "custom", "sonnet"],
            ),
        ] {
            let all = resolve_models(&models, &settings, &favorites);
            assert_eq!(all.len(), 5);
            for row in &all {
                assert_eq!(row.hidden, settings.hidden_models.contains(&row.id));
                assert_eq!(row.favorite, favorites.contains(&row.id));
                assert_eq!(
                    row.custom,
                    matches!(row.id.as_str(), "custom" | "hidden-custom")
                );
                if row.custom {
                    assert_eq!(row.name, row.id);
                }
            }
            let picker = picker_models(&models, &settings, &favorites);
            assert_eq!(
                picker.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
                expected
            );
            for rows in [&all, &picker] {
                assert_eq!(
                    rows.iter()
                        .find(|row| row.id == "sonnet")
                        .unwrap()
                        .capabilities,
                    [ModelCapability::Thinking, ModelCapability::Reasoning]
                );
            }
        }
    }

    #[test]
    fn validates_custom_slugs() {
        let settings = ProviderSettings {
            custom_models: vec!["already".into()],
            ..Default::default()
        };
        assert_eq!(
            validate_slug("  ", &catalog(), &settings),
            Err(SlugError::Empty)
        );
        assert_eq!(
            validate_slug("opus", &catalog(), &settings),
            Err(SlugError::AlreadyBuiltIn)
        );
        assert_eq!(
            validate_slug("already", &catalog(), &settings),
            Err(SlugError::AlreadySaved)
        );
        assert_eq!(
            validate_slug(&"x".repeat(MAX_SLUG_LEN + 1), &catalog(), &settings),
            Err(SlugError::TooLong)
        );
        assert_eq!(
            validate_slug("  claude-sonnet-5 ", &catalog(), &settings),
            Ok("claude-sonnet-5".into())
        );
    }

    #[test]
    fn fast_mode_keys_on_claude_boolean_and_codex_fast_tiers() {
        let claude = ModelSpec {
            options: vec![OptionDescriptor::Boolean {
                role: Default::default(),
                apply: Default::default(),
                recommended: None,
                permissive: None,
                id: "fastMode".into(),
                label: "Fast Mode".into(),
                default_value: false,
            }],
            ..spec("opus", "Opus")
        };
        let fast = FastMode::of(&claude).unwrap();
        assert_eq!(fast.level(&[]), FastLevel::Off);
        let on = OptionSelection {
            id: "fastMode".into(),
            value: fast.value(fast.next(FastLevel::Off)),
        };
        assert_eq!(on.value, serde_json::json!(true));
        assert_eq!(fast.level(&[on]), FastLevel::Fast);
        // No ultrafast tier: a second press turns fast mode off.
        assert_eq!(fast.next(FastLevel::Fast), FastLevel::Off);
        assert_eq!(fast.value(FastLevel::Off), serde_json::json!(false));

        // The app-server model list names the fast tier `priority` / "Fast".
        let codex = ModelSpec {
            options: vec![OptionDescriptor::Select {
                role: Default::default(),
                apply: Default::default(),
                recommended: None,
                permissive: None,
                id: "serviceTier".into(),
                label: "Service Tier".into(),
                options: vec![
                    SelectOption {
                        unavailable: None,
                        value: "default".into(),
                        label: "Standard".into(),
                        description: None,
                    },
                    SelectOption {
                        unavailable: None,
                        value: "priority".into(),
                        label: "Fast".into(),
                        description: None,
                    },
                    SelectOption {
                        unavailable: None,
                        value: "ultrafast".into(),
                        label: "Ultrafast".into(),
                        description: None,
                    },
                ],
                default_value: Some("default".into()),
            }],
            ..spec("gpt-6-astra", "GPT-6 Astra")
        };
        let fast = FastMode::of(&codex).unwrap();
        let select = |value: &str| {
            [OptionSelection {
                id: "serviceTier".into(),
                value: serde_json::json!(value),
            }]
        };
        assert_eq!(fast.level(&[]), FastLevel::Off);
        assert_eq!(fast.value(FastLevel::Fast), serde_json::json!("priority"));
        assert_eq!(fast.level(&select("priority")), FastLevel::Fast);
        // With an ultrafast tier the presses run off → fast → ultrafast → off.
        assert_eq!(fast.next(FastLevel::Fast), FastLevel::Ultra);
        assert_eq!(fast.value(FastLevel::Ultra), serde_json::json!("ultrafast"));
        assert_eq!(fast.level(&select("ultrafast")), FastLevel::Ultra);
        assert_eq!(fast.next(FastLevel::Ultra), FastLevel::Off);
        assert_eq!(fast.value(FastLevel::Off), serde_json::json!("default"));

        // Neither descriptor shape: no fast mode (Fable / Sonnet catalogs).
        let plain = ModelSpec {
            options: vec![OptionDescriptor::Select {
                role: Default::default(),
                apply: Default::default(),
                recommended: None,
                permissive: None,
                id: "reasoningEffort".into(),
                label: "Reasoning".into(),
                options: vec![SelectOption {
                    unavailable: None,
                    value: "high".into(),
                    label: "High".into(),
                    description: None,
                }],
                default_value: None,
            }],
            ..spec("fable", "Fable")
        };
        assert_eq!(FastMode::of(&plain), None);
    }
}
