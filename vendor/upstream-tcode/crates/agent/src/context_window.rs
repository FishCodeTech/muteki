//! The `contextWindow` option every provider shares: the token grammar of its
//! values and the window a session runs at, read from the model's descriptor
//! and the session's selections. How a provider enforces the window on its
//! CLI is the provider's own business.

use serde_json::Value;

use crate::{OptionDescriptor, OptionSelection};

pub const OPTION_ID: &str = "contextWindow";

/// The smallest window a user can ask for; a provider's largest preset caps
/// the other end.
pub const MIN_TOKENS: u64 = 100_000;

/// Parse a context-window value (`200k`, `1m`, `500000`, `500`) into tokens.
pub fn parse_tokens(value: &Value) -> Option<u64> {
    let tokens = match value {
        Value::Number(number) => number.as_u64()?,
        Value::String(value) => {
            let value = value.trim().to_ascii_lowercase();
            if let Some(value) = value.strip_suffix('k') {
                value.parse::<u64>().ok()?.checked_mul(1_000)?
            } else if let Some(value) = value.strip_suffix('m') {
                value.parse::<u64>().ok()?.checked_mul(1_000_000)?
            } else {
                let value = value.parse::<u64>().ok()?;
                if value < 1_000 {
                    value.checked_mul(1_000)?
                } else {
                    value
                }
            }
        }
        _ => return None,
    };
    (tokens >= MIN_TOKENS).then_some(tokens)
}

/// Format a context-window token count for display.
pub fn format(tokens: u64) -> String {
    if tokens >= 1_000_000 && tokens.is_multiple_of(1_000_000) {
        format!("{}M", tokens / 1_000_000)
    } else {
        format!("{}k", tokens / 1_000)
    }
}

/// The `contextWindow` selector among a model's descriptors.
pub fn descriptor(descriptors: &[OptionDescriptor]) -> Option<&OptionDescriptor> {
    descriptors.iter().find(
        |descriptor| matches!(descriptor, OptionDescriptor::Select { id, .. } if id == OPTION_ID),
    )
}

/// The window a session runs at: its `contextWindow` selection, else the
/// model's default preset. `None` when the model has no selector or the
/// selector names no default.
pub fn selected(descriptors: &[OptionDescriptor], selections: &[OptionSelection]) -> Option<u64> {
    selections
        .iter()
        .find(|selection| selection.id == OPTION_ID)
        .and_then(|selection| parse_tokens(&selection.value))
        .or_else(|| match descriptor(descriptors)? {
            OptionDescriptor::Select { default_value, .. } => {
                parse_tokens(&Value::String(default_value.clone()?))
            }
            OptionDescriptor::Boolean { .. } => None,
        })
}

/// The largest preset of the model's selector: the window it can launch with,
/// which caps a custom size.
pub fn largest(descriptors: &[OptionDescriptor]) -> Option<u64> {
    match descriptor(descriptors)? {
        OptionDescriptor::Select { options, .. } => options
            .iter()
            .filter_map(|option| parse_tokens(&Value::String(option.value.clone())))
            .max(),
        OptionDescriptor::Boolean { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_and_formats_window_values() {
        assert_eq!(parse_tokens(&json!("200k")), Some(200_000));
        assert_eq!(parse_tokens(&json!("1m")), Some(1_000_000));
        assert_eq!(parse_tokens(&json!("1M")), Some(1_000_000));
        assert_eq!(parse_tokens(&json!("500k")), Some(500_000));
        assert_eq!(parse_tokens(&json!("500000")), Some(500_000));
        assert_eq!(parse_tokens(&json!("500")), Some(500_000));
        assert_eq!(parse_tokens(&json!(750_000)), Some(750_000));
        assert_eq!(parse_tokens(&json!("872k")), Some(872_000));
        assert_eq!(parse_tokens(&json!(99_999)), None);
        assert_eq!(parse_tokens(&json!("99k")), None);
        assert_eq!(parse_tokens(&json!("garbage")), None);
        assert_eq!(parse_tokens(&json!(-200_000)), None);
        assert_eq!(parse_tokens(&json!(null)), None);
        assert_eq!(format(200_000), "200k");
        assert_eq!(format(750_000), "750k");
        assert_eq!(format(872_000), "872k");
        assert_eq!(format(1_000_000), "1M");
        assert_eq!(format(2_000_000), "2M");
    }

    #[test]
    fn selection_wins_over_the_default_preset() {
        let descriptors = [OptionDescriptor::Select {
            id: OPTION_ID.into(),
            label: "Context Window".into(),
            options: vec![
                crate::SelectOption {
                    value: "272k".into(),
                    label: "272k".into(),
                    description: None,
                    unavailable: None,
                },
                crate::SelectOption {
                    value: "872k".into(),
                    label: "872k".into(),
                    description: None,
                    unavailable: None,
                },
            ],
            default_value: Some("272k".into()),
            role: Default::default(),
            apply: Default::default(),
            recommended: None,
            permissive: None,
        }];
        assert_eq!(selected(&descriptors, &[]), Some(272_000));
        let selection = [OptionSelection {
            id: OPTION_ID.into(),
            value: json!(500_000),
        }];
        assert_eq!(selected(&descriptors, &selection), Some(500_000));
        assert_eq!(largest(&descriptors), Some(872_000));
        assert_eq!(selected(&[], &selection), Some(500_000));
        assert_eq!(selected(&[], &[]), None);
        assert_eq!(largest(&[]), None);
    }
}
