//! CVSS 3.1 assessment used by the original SRC Finding review contract.
use axum::http::StatusCode;
use serde_json::{Map, Value, json};

use super::super::Error;

const BASE: [&str; 8] = ["AV", "AC", "PR", "UI", "S", "C", "I", "A"];
const TEMPORAL: [&str; 3] = ["E", "RL", "RC"];
const ENVIRONMENTAL: [&str; 11] = [
    "CR", "IR", "AR", "MAV", "MAC", "MPR", "MUI", "MS", "MC", "MI", "MA",
];

fn error(code: &'static str, message: impl Into<String>) -> Error {
    Error::new(StatusCode::BAD_REQUEST, code, message)
}

fn value(name: &str, selected: &str) -> Option<f64> {
    match (name, selected) {
        ("AV" | "MAV", "N") => Some(0.85),
        ("AV" | "MAV", "A") => Some(0.62),
        ("AV" | "MAV", "L") => Some(0.55),
        ("AV" | "MAV", "P") => Some(0.2),
        ("AC" | "MAC", "L") => Some(0.77),
        ("AC" | "MAC", "H") => Some(0.44),
        ("PR" | "MPR", "N") => Some(0.85),
        ("PR" | "MPR", "L") => Some(0.62),
        ("PR" | "MPR", "H") => Some(0.27),
        ("UI" | "MUI", "N") => Some(0.85),
        ("UI" | "MUI", "R") => Some(0.62),
        ("S" | "MS", "U" | "C") => Some(1.0),
        ("C" | "I" | "A" | "MC" | "MI" | "MA", "H") => Some(0.56),
        ("C" | "I" | "A" | "MC" | "MI" | "MA", "L") => Some(0.22),
        ("C" | "I" | "A" | "MC" | "MI" | "MA", "N") => Some(0.0),
        ("E", "U") => Some(0.91),
        ("E", "P") => Some(0.94),
        ("E", "F") => Some(0.97),
        ("E", "H") => Some(1.0),
        ("RL", "O") => Some(0.95),
        ("RL", "T") => Some(0.96),
        ("RL", "W") => Some(0.97),
        ("RL", "U") => Some(1.0),
        ("RC", "U") => Some(0.92),
        ("RC", "R") => Some(0.96),
        ("RC", "C") => Some(1.0),
        ("CR" | "IR" | "AR", "L") => Some(0.5),
        ("CR" | "IR" | "AR", "M") => Some(1.0),
        ("CR" | "IR" | "AR", "H") => Some(1.5),
        ("E" | "RL" | "RC" | "CR" | "IR" | "AR", "X") => Some(1.0),
        ("MAV", "X") => Some(0.85),
        ("MAC", "X") => Some(0.77),
        ("MPR", "X") => Some(0.85),
        ("MUI", "X") => Some(0.85),
        ("MS", "X") => Some(1.0),
        ("MC" | "MI" | "MA", "X") => Some(0.56),
        _ => None,
    }
}

fn selected<'a>(metrics: &'a Map<String, Value>, name: &str) -> &'a str {
    metrics.get(name).and_then(Value::as_str).unwrap_or("X")
}

fn weight(metrics: &Map<String, Value>, name: &str) -> f64 {
    value(name, selected(metrics, name)).expect("CVSS metric validated by parse_vector")
}

fn base_weight(metrics: &Map<String, Value>, modified: &str, base: &str) -> f64 {
    if selected(metrics, modified) == "X" {
        weight(metrics, base)
    } else {
        weight(metrics, modified)
    }
}

fn parse_vector(vector: &str) -> Result<Map<String, Value>, Error> {
    let Some(parts) = vector.trim().strip_prefix("CVSS:3.1/") else {
        return Err(error(
            "cvss_vector_invalid",
            "CVSS vector must start with CVSS:3.1/",
        ));
    };
    let mut metrics = Map::new();
    for part in parts.split('/') {
        let Some((name, selected)) = part.split_once(':') else {
            return Err(error(
                "cvss_vector_invalid",
                format!("invalid or duplicate CVSS metric: {part}"),
            ));
        };
        if !BASE.contains(&name) && !TEMPORAL.contains(&name) && !ENVIRONMENTAL.contains(&name)
            || selected.contains(':')
            || metrics.contains_key(name)
        {
            return Err(error(
                "cvss_vector_invalid",
                format!("invalid or duplicate CVSS metric: {part}"),
            ));
        }
        if value(name, selected).is_none() {
            return Err(error(
                "cvss_vector_invalid",
                format!("invalid value {selected} for {name}"),
            ));
        }
        metrics.insert(name.to_owned(), json!(selected));
    }
    let missing: Vec<_> = BASE
        .iter()
        .copied()
        .filter(|name| !metrics.contains_key(*name))
        .collect();
    if !missing.is_empty() {
        return Err(error(
            "cvss_vector_incomplete",
            format!("missing base metrics: {}", missing.join(", ")),
        ));
    }
    Ok(metrics)
}

fn canonical_vector(metrics: &Map<String, Value>) -> String {
    let mut parts = Vec::new();
    for name in BASE.into_iter().chain(TEMPORAL).chain(ENVIRONMENTAL) {
        if let Some(value) = metrics.get(name).and_then(Value::as_str) {
            parts.push(format!("{name}:{value}"));
        }
    }
    format!("CVSS:3.1/{}", parts.join("/"))
}

fn roundup(value: f64) -> f64 {
    let five_decimals = (value * 100_000.0).round() as i64;
    let tenth = if five_decimals % 10_000 == 0 {
        five_decimals / 10_000
    } else {
        five_decimals / 10_000 + 1
    };
    tenth as f64 / 10.0
}

fn changed_privileges(choice: &str, scope: &str) -> f64 {
    if scope == "C" {
        match choice {
            "N" => 0.85,
            "L" => 0.68,
            "H" => 0.5,
            _ => unreachable!("validated CVSS privileges value"),
        }
    } else {
        value("PR", choice).expect("validated CVSS privileges value")
    }
}

fn privileges(metrics: &Map<String, Value>, modified: bool) -> f64 {
    let scope = if modified && selected(metrics, "MS") != "X" {
        selected(metrics, "MS")
    } else {
        selected(metrics, "S")
    };
    if modified && selected(metrics, "MPR") != "X" {
        let choice = selected(metrics, "MPR");
        if scope == "C" {
            changed_privileges(choice, scope)
        } else {
            weight(metrics, "MPR")
        }
    } else {
        changed_privileges(selected(metrics, "PR"), scope)
    }
}

fn exploitability(metrics: &Map<String, Value>, modified: bool) -> f64 {
    if modified {
        8.22 * base_weight(metrics, "MAV", "AV")
            * base_weight(metrics, "MAC", "AC")
            * privileges(metrics, true)
            * base_weight(metrics, "MUI", "UI")
    } else {
        8.22 * weight(metrics, "AV")
            * weight(metrics, "AC")
            * privileges(metrics, false)
            * weight(metrics, "UI")
    }
}

fn temporal_factor(metrics: &Map<String, Value>) -> f64 {
    weight(metrics, "E") * weight(metrics, "RL") * weight(metrics, "RC")
}

fn base_score(metrics: &Map<String, Value>) -> f64 {
    let isc = 1.0
        - (1.0 - weight(metrics, "C"))
            * (1.0 - weight(metrics, "I"))
            * (1.0 - weight(metrics, "A"));
    let impact = if selected(metrics, "S") == "U" {
        6.42 * isc
    } else {
        7.52 * (isc - 0.029) - 3.25 * (isc - 0.02).powi(15)
    };
    if impact <= 0.0 {
        return 0.0;
    }
    let combined = impact + exploitability(metrics, false);
    if selected(metrics, "S") == "C" {
        roundup((1.08 * combined).min(10.0))
    } else {
        roundup(combined.min(10.0))
    }
}

fn environmental_score(metrics: &Map<String, Value>) -> f64 {
    let mc = base_weight(metrics, "MC", "C");
    let mi = base_weight(metrics, "MI", "I");
    let ma = base_weight(metrics, "MA", "A");
    let isc = (1.0
        - (1.0 - mc * weight(metrics, "CR"))
            * (1.0 - mi * weight(metrics, "IR"))
            * (1.0 - ma * weight(metrics, "AR")))
    .min(0.915);
    let changed = selected(metrics, "MS") == "C"
        || (selected(metrics, "MS") == "X" && selected(metrics, "S") == "C");
    let impact = if changed {
        7.52 * (isc - 0.029) - 3.25 * (isc * 0.9731 - 0.02).powi(13)
    } else {
        6.42 * isc
    };
    if impact <= 0.0 {
        return 0.0;
    }
    let exploitability = exploitability(metrics, true);
    let interim = if changed {
        roundup((1.08 * (impact + exploitability)).min(10.0))
    } else {
        roundup((impact + exploitability).min(10.0))
    };
    roundup(interim * temporal_factor(metrics))
}

fn severity(score: f64) -> &'static str {
    if score == 0.0 {
        "NONE"
    } else if score < 4.0 {
        "LOW"
    } else if score < 7.0 {
        "MEDIUM"
    } else if score < 9.0 {
        "HIGH"
    } else {
        "CRITICAL"
    }
}

/// Evaluate a source-shaped `CVSSAssessment` and compare its declared scores.
pub(super) fn evaluate(input: &Value) -> Result<Value, Error> {
    let fields = input.as_object().ok_or_else(|| {
        error(
            "cvss_assessment_invalid",
            "CVSS assessment must be an object",
        )
    })?;
    for key in fields.keys() {
        if ![
            "schema_version",
            "vector",
            "base_score",
            "temporal_score",
            "environmental_score",
            "base_severity",
            "temporal_severity",
            "environmental_severity",
            "calculator_version",
            "metric_reasons",
        ]
        .contains(&key.as_str())
        {
            return Err(error(
                "cvss_assessment_invalid",
                format!("unknown CVSS assessment field: {key}"),
            ));
        }
    }
    let vector = input["vector"].as_str().unwrap_or("");
    let metrics = parse_vector(vector)?;
    let raw_reasons = if input["metric_reasons"].is_null() {
        Map::new()
    } else {
        input["metric_reasons"]
            .as_object()
            .cloned()
            .ok_or_else(|| {
                error(
                    "cvss_reason_invalid",
                    "CVSS metric_reasons must be an object",
                )
            })?
    };
    let mut reasons = Map::new();
    for (name, raw) in &raw_reasons {
        if !BASE.contains(&name.as_str())
            && !TEMPORAL.contains(&name.as_str())
            && !ENVIRONMENTAL.contains(&name.as_str())
        {
            return Err(error(
                "cvss_reason_invalid",
                format!("reasons reference unknown metrics: {name}"),
            ));
        }
        let mut entry = if let Some(reason) = raw.as_str() {
            Map::from_iter([("reason".to_owned(), json!(reason))])
        } else {
            raw.as_object().cloned().ok_or_else(|| {
                error(
                    "cvss_reason_required",
                    format!("metric {name} needs an evidence-based reason"),
                )
            })?
        };
        let reason = entry
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if reason.is_empty() {
            return Err(error(
                "cvss_reason_required",
                format!("metric {name} needs an evidence-based reason"),
            ));
        }
        entry.insert("reason".to_owned(), json!(reason));
        reasons.insert(name.clone(), Value::Object(entry));
    }
    for name in BASE {
        if !reasons.contains_key(name) {
            return Err(error(
                "cvss_reason_required",
                format!("metric {name} needs an evidence-based reason"),
            ));
        }
    }
    let base = base_score(&metrics);
    let temporal = roundup(base * temporal_factor(&metrics));
    let environmental = environmental_score(&metrics);
    for (field, calculated) in [
        ("base_score", base),
        ("temporal_score", temporal),
        ("environmental_score", environmental),
    ] {
        let supplied = input[field]
            .as_f64()
            .or_else(|| input[field].as_str().and_then(|v| v.parse::<f64>().ok()));
        if supplied != Some(calculated) {
            return Err(error(
                "cvss_mismatch",
                "CVSS scores do not match the vector and metric reasons",
            ));
        }
    }
    for field in [
        "base_severity",
        "temporal_severity",
        "environmental_severity",
    ] {
        if input[field].as_str().is_none() {
            return Err(error(
                "cvss_assessment_invalid",
                format!("CVSS {field} is required"),
            ));
        }
    }
    let assessment = json!({
        "schema_version":1,
        "vector":canonical_vector(&metrics),
        "base_score":format!("{base:.1}"),
        "temporal_score":format!("{temporal:.1}"),
        "environmental_score":format!("{environmental:.1}"),
        "base_severity":severity(base),
        "temporal_severity":severity(temporal),
        "environmental_severity":severity(environmental),
        "calculator_version":"cvss-3.1-official-v1",
        "metric_reasons":reasons,
    });
    Ok(assessment)
}
