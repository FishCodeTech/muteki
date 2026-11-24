//! Native model catalogs: the models an engine CLI offers under one launch
//! environment.
//!
//! Engines with a tcode adapter answer through it. ACP engines advertise their
//! models only inside a live session, so their catalogs come from the CLI
//! command each descriptor declares, parsed with that engine's own format.

use super::{
    RuntimeError,
    descriptors::{EngineDescriptor, NativeTransport},
};
use agent::{CatalogRefresh, LaunchEnv, ModelSpec, OptionDescriptor, OptionSelection, ProviderKind};
use regex::Regex;
use serde_json::{Map, Value, json};
use std::{path::Path, sync::LazyLock, time::Duration};

const CATALOG_TIMEOUT: Duration = Duration::from_secs(60);

/// The request id the Droid descriptor's catalog stdin uses.
const DROID_MODELS_REQUEST_ID: &str = "muteki-model-catalog";

/// Catalog stdin a descriptor declares; the CLI exits after answering it at EOF.
pub(crate) fn catalog_stdin(spec: &Value) -> Option<String> {
    spec["stdin"].as_str().filter(|text| !text.is_empty()).map(|text| format!("{}\n", text.trim_end()))
}

/// The selection id prefix for per-model ACP configuration.
const CONFIG_OPTION_PREFIX: &str = "acp:cfg:";

/// The grammar of catalog display labels, never of model id suffixes.
static VARIANT_SUFFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^(?:(?P<effort>No Thinking|None|Minimal|Low|Medium|High|XHigh|Max|Thinking)(?: Thinking)?(?: |$))?",
        r"(?:(?P<context>\d+(?:\.\d+)?[KM])(?: |$))?",
        r"(?P<speed>Ultra Fast|Fast Mode|Fast|Lightning)?$",
    ))
    .expect("variant suffix grammar")
});

fn variant(family_id: &str, family_label: &str, parameters: Map<String, Value>) -> Value {
    json!({"family_id": family_id, "family_label": family_label, "parameters": parameters})
}

/// Only fully understood suffixes; any other label stays a plain model.
fn label_parameters(suffix: &str) -> Option<Map<String, Value>> {
    let parts = VARIANT_SUFFIX.captures(suffix.trim())?;
    let mut parameters = Map::new();
    for key in ["effort", "context", "speed"] {
        if let Some(value) = parts.name(key) {
            let value = if value.as_str() == "Fast Mode" { "Fast" } else { value.as_str() };
            parameters.insert(key.to_owned(), json!(value));
        }
    }
    Some(parameters)
}

/// Group label suffixes only under a base label the catalog also lists.
fn label_family_variants(models: &mut [Value], speed_only: bool) {
    let labels: Vec<String> = models
        .iter()
        .map(|row| row["label"].as_str().unwrap_or("").trim_end_matches(" (default)").to_owned())
        .collect();
    let mut bases: Vec<usize> = (0..models.len()).filter(|&index| !labels[index].is_empty()).collect();
    bases.sort_by_key(|&index| labels[index].len());
    let mut groups: Vec<(usize, Vec<(usize, Map<String, Value>)>)> = Vec::new();
    for (index, label) in labels.iter().enumerate() {
        for &base in &bases {
            let Some(suffix) = label.strip_prefix(&format!("{} ", labels[base])) else { continue };
            let Some(parameters) = label_parameters(suffix) else { continue };
            if parameters.is_empty() || (speed_only && (parameters.len() != 1 || !parameters.contains_key("speed"))) {
                continue;
            }
            match groups.iter_mut().find(|(known, _)| *known == base) {
                Some((_, members)) => members.push((index, parameters)),
                None => groups.push((base, vec![(index, parameters)])),
            }
            break;
        }
    }
    for (base, members) in groups {
        let declares = |key: &str| members.iter().any(|(_, parameters)| parameters.contains_key(key));
        let (speed, effort, context) = (declares("speed"), declares("effort"), declares("context"));
        let family_id = models[base]["id"].as_str().unwrap_or("").to_owned();
        for (index, mut parameters) in std::iter::once((base, Map::new())).chain(members) {
            if speed { parameters.entry("speed").or_insert(json!("Standard")); }
            if effort { parameters.entry("effort").or_insert(json!("Default")); }
            if context { parameters.entry("context").or_insert(json!("Standard")); }
            models[index]["variant"] = variant(&family_id, &labels[base], parameters);
        }
    }
}

fn level_name(value: &Value) -> Option<String> {
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Object(row) => row
            .get("effort")
            .or_else(|| row.get("reasoningEffort"))
            .or_else(|| row.get("value"))
            .or_else(|| row.get("id"))?
            .as_str()?
            .to_owned(),
        _ => return None,
    };
    let text = text.trim().to_owned();
    (!text.is_empty() && text != "default").then_some(text)
}

/// The reasoning shape the conversation picker reads.
pub fn reasoning(levels: &Value, default: &Value, source: &str) -> Value {
    let mut out: Vec<String> = Vec::new();
    for level in levels.as_array().into_iter().flatten().filter_map(level_name) {
        if !out.contains(&level) {
            out.push(level);
        }
    }
    let default = default.as_str().map(str::trim).filter(|d| out.iter().any(|l| l == d)).unwrap_or("");
    json!({"supported": !out.is_empty(), "levels": out, "default": default, "kind": "effort", "source": source})
}

fn from_spec(spec: &ModelSpec, source: &str) -> Value {
    let effort = spec.options.iter().find_map(|option| match option {
        OptionDescriptor::Select { id, options, default_value, .. }
            if { let id = id.to_ascii_lowercase(); id.contains("effort") || id.contains("reasoning") } =>
        {
            Some((
                json!(options.iter().map(|o| o.value.clone()).collect::<Vec<_>>()),
                json!(default_value),
            ))
        }
        _ => None,
    });
    let (levels, default) = effort.unwrap_or((json!([]), Value::Null));
    let mut row = json!({
        "id": spec.id,
        "label": if spec.display_name.trim().is_empty() { spec.id.clone() } else { spec.display_name.clone() },
        "is_default": spec.is_default,
        "reasoning": reasoning(&levels, &default, source),
    });
    if let Some((options, default)) = spec.options.iter().find_map(|option| match option {
        OptionDescriptor::Select { id, options, default_value, .. } if id == "serviceTier" => {
            Some((options, default_value))
        }
        _ => None,
    }) {
        let tiers: Vec<Value> = options.iter()
            .filter(|option| option.value != "default" && option.unavailable.is_none())
            .map(|option| json!({"id":option.value,"name":option.label,"description":option.description}))
            .collect();
        row["default_service_tier"] = json!(default.as_deref()
            .filter(|value| tiers.iter().any(|tier| tier["id"] == *value)).unwrap_or(""));
        row["service_tiers"] = json!(tiers);
    }
    row
}

fn selection_key(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

fn selection_value(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// The picker parameter a Cursor or ACP config option fills.
fn cursor_parameter(config: &str) -> &str {
    match config {
        "thinking" | "context" => config,
        "context_window" | "contextWindow" => "context",
        "fast" => "speed",
        id if id.contains("effort") || id.contains("reasoning") => "effort",
        id => id,
    }
}

fn cursor_value_label(parameter: &str, value: &str, label: &str) -> String {
    let label = label
        .chars()
        .filter(|c| !matches!(c, '\u{200b}'..='\u{200d}' | '\u{feff}'))
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    match (parameter, value) {
        ("thinking", "true") => "Thinking".into(),
        ("thinking", "false") => "No Thinking".into(),
        ("speed", "true") => "Fast".into(),
        ("speed", "false") => "Standard".into(),
        _ if label.is_empty() => value.into(),
        ("context", _) => label.to_uppercase(),
        _ => label,
    }
}

/// Parameter combinations use `model[option=value,...]`, decoded at launch.
/// Grok's existing plain model ids retain their default context selection.
fn configured_rows(spec: &ModelSpec, source: &str, keep_default_id: bool) -> Vec<Value> {
    let name = if spec.display_name.trim().is_empty() { spec.id.clone() } else { spec.display_name.trim().to_owned() };
    let mut parameters: Vec<(String, String, Vec<(String, String)>, usize)> = Vec::new();
    for option in &spec.options {
        let OptionDescriptor::Select { id, options, default_value, .. } = option else { continue };
        let Some(config) = id.strip_prefix(CONFIG_OPTION_PREFIX).filter(|config| selection_key(config)) else { continue };
        let mut parameter = cursor_parameter(config).to_owned();
        if parameters.iter().any(|(_, known, _, _)| *known == parameter) {
            parameter = config.to_owned();
        }
        let values: Vec<(String, String)> = options
            .iter()
            .filter(|choice| choice.unavailable.is_none() && selection_value(&choice.value))
            .map(|choice| (choice.value.clone(), cursor_value_label(&parameter, &choice.value, &choice.label)))
            .collect();
        if values.is_empty() {
            continue;
        }
        let default = default_value.as_deref().and_then(|d| values.iter().position(|(value, _)| value == d)).unwrap_or(0);
        parameters.push((config.to_owned(), parameter, values, default));
    }
    if parameters.is_empty() {
        return vec![from_spec(spec, source)];
    }
    let mut combinations: Vec<Vec<usize>> = vec![Vec::new()];
    for (_, _, values, _) in &parameters {
        combinations = combinations
            .into_iter()
            .flat_map(|picked| (0..values.len()).map(move |index| [picked.as_slice(), &[index]].concat()))
            .collect();
    }
    let current: Vec<usize> = parameters.iter().map(|(_, _, _, default)| *default).collect();
    combinations
        .into_iter()
        .map(|picked| {
            let mut shown = Map::new();
            let mut pairs = Vec::new();
            for ((config, parameter, values, _), index) in parameters.iter().zip(&picked) {
                let (value, label) = &values[*index];
                pairs.push(format!("{config}={value}"));
                shown.insert(parameter.clone(), json!(label));
            }
            let summary = shown.values().filter_map(Value::as_str).collect::<Vec<_>>().join(" · ");
            let mut family = variant(&spec.id, &name, shown);
            if picked == current {
                family["default"] = json!(true);
            }
            let mut row = from_spec(spec, source);
            row["id"] = json!(if keep_default_id && picked == current { spec.id.clone() }
                else { format!("{}[{}]", spec.id, pairs.join(",")) });
            row["label"] = json!(format!("{name} · {summary}"));
            row["is_default"] = json!(spec.is_default && picked == current);
            row["variant"] = family;
            row
        })
        .collect()
}

/// A catalog id as the native model and selections a session start restores.
pub fn configured_model_selection(model: &str) -> Result<(String, Vec<OptionSelection>), RuntimeError> {
    if !model.contains(['[', ']']) {
        return Ok((model.to_owned(), Vec::new()));
    }
    let invalid = || RuntimeError::new("model.invalid_configuration", format!("无法识别的模型参数：{model}"));
    let (base, rest) = model.split_once('[').ok_or_else(invalid)?;
    let pairs = rest.strip_suffix(']').ok_or_else(invalid)?;
    if base.is_empty() || base.chars().any(char::is_whitespace) || pairs.contains(['[', ']']) {
        return Err(invalid());
    }
    let mut selections: Vec<OptionSelection> = Vec::new();
    for pair in pairs.split(',') {
        let (key, value) = pair.split_once('=').ok_or_else(invalid)?;
        let id = format!("{CONFIG_OPTION_PREFIX}{key}");
        if !selection_key(key) || !selection_value(value) || selections.iter().any(|known| known.id == id) {
            return Err(invalid());
        }
        selections.push(OptionSelection { id, value: json!(value) });
    }
    Ok((base.to_owned(), selections))
}

fn parse_kimi(text: &str) -> Result<Vec<Value>, String> {
    let data: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    Ok(data["models"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(alias, _)| !alias.trim().is_empty())
        .map(|(alias, meta)| {
            json!({
                "id": alias.trim(),
                "label": meta["displayName"].as_str().unwrap_or(alias.trim()),
                "provider": meta["provider"].as_str().or_else(|| meta["provider_id"].as_str()).unwrap_or(""),
                "reasoning": reasoning(&meta["supportEfforts"], &meta["defaultEffort"], "kimi_model_metadata"),
            })
        })
        .collect())
}

fn parse_omp(text: &str) -> Result<Vec<Value>, String> {
    let data: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    Ok(data["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = item["selector"].as_str().or_else(|| item["id"].as_str())?.trim();
            (!id.is_empty()).then(|| {
                let provider = item["provider"]
                    .as_str()
                    .or_else(|| item["provider_id"].as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| id.split_once('/').map(|(p, _)| p.to_owned()).unwrap_or_default());
                let mut row = json!({
                    "id": id, "label": item["name"].as_str().unwrap_or(id), "provider": provider,
                    "reasoning": reasoning(&item["thinking"], &Value::Null, "omp_model_metadata"),
                });
                if item["input"].is_array() {
                    row["input"] = item["input"].clone();
                }
                row
            })
        })
        .collect())
}

/// A paired family labels each variant as `(lead + sidekick)`, both being
/// variant labels the catalog lists on their own.
static PAIRED_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\((?P<lead>.+?) \+ (?P<sidekick>.+)\)$").expect("paired suffix grammar"));

/// Only pairs whose lead and sidekick are both catalog variants; the lead's
/// own label parameters stay as the pair's effort and speed.
fn paired_parameters(suffix: &str, families: &std::collections::HashMap<&str, &str>) -> Option<Map<String, Value>> {
    let parts = PAIRED_SUFFIX.captures(suffix.trim())?;
    let (lead, sidekick) = (&parts["lead"], &parts["sidekick"]);
    let lead_family = *families.get(lead)?;
    families.get(sidekick)?;
    let mut parameters = match lead.strip_prefix(&format!("{lead_family} ")) {
        Some(lead_suffix) => label_parameters(lead_suffix)?,
        None if lead == lead_family => Map::new(),
        None => return None,
    };
    parameters.insert("lead".to_owned(), json!(lead_family));
    parameters.insert("sidekick".to_owned(), json!(sidekick));
    Some(parameters)
}

/// Devin's families, each variant labelled as the family label plus its
/// parameters.
fn devin_family_variants(
    family: &Value,
    families: &std::collections::HashMap<&str, &str>,
) -> std::collections::HashMap<String, Value> {
    let family_id = family["family_uid"].as_str().or_else(|| family["slug"].as_str()).unwrap_or("").trim();
    let family_label = family["family_label"].as_str().unwrap_or("").trim();
    if family_id.is_empty() || family_label.is_empty() {
        return Default::default();
    }
    let parsed: Vec<(&Value, Map<String, Value>)> = family["variants"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| row.is_object())
        .map(|row| {
            let label = row["label"].as_str().or_else(|| row["model_uid"].as_str()).unwrap_or("").trim();
            let suffix = (label == family_label).then_some("").or_else(|| label.strip_prefix(&format!("{family_label} ")));
            let parameters = suffix.and_then(|suffix| label_parameters(suffix).or_else(|| paired_parameters(suffix, families))).unwrap_or_else(|| {
                let configuration = suffix.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(label);
                Map::from_iter([("configuration".to_owned(), json!(configuration))])
            });
            (row, parameters)
        })
        .collect();
    let speed = parsed.iter().any(|(_, parameters)| parameters.contains_key("speed"));
    let effort = parsed.iter().any(|(_, parameters)| parameters.contains_key("effort"));
    let thinking = parsed.iter().any(|(_, parameters)| parameters.get("effort") == Some(&json!("Thinking")));
    let context = parsed.iter().any(|(_, parameters)| parameters.contains_key("context"));
    let mut out = std::collections::HashMap::new();
    for (row, mut parameters) in parsed {
        if !parameters.contains_key("configuration") {
            if speed { parameters.entry("speed").or_insert(json!("Standard")); }
            if effort { parameters.entry("effort").or_insert(json!(if thinking { "No Thinking" } else { "Default" })); }
            if context && !parameters.contains_key("context") {
                let shown = match row["max_context_tokens"].as_u64().filter(|tokens| *tokens > 0) {
                    Some(tokens) if tokens % 1000 == 0 => format!("{}K", tokens / 1000),
                    Some(tokens) => tokens.to_string(),
                    None => "Standard".to_owned(),
                };
                parameters.insert("context".to_owned(), json!(shown));
            }
        }
        if let Some(id) = row["model_uid"].as_str().map(str::trim).filter(|id| !id.is_empty()) {
            out.insert(id.to_owned(), variant(family_id, family_label, parameters));
        }
    }
    out
}

fn parse_devin(text: &str) -> Result<Vec<Value>, String> {
    let data: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let mut rows = Vec::new();
    let families: std::collections::HashMap<&str, &str> = data["families"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|family| Some((family["family_label"].as_str()?.trim(), family["variants"].as_array()?)))
        .flat_map(|(family_label, variants)| {
            variants.iter().filter_map(move |row| Some((row["label"].as_str()?.trim(), family_label)))
        })
        .collect();
    for family in data["families"].as_array().into_iter().flatten() {
        let variants = devin_family_variants(family, &families);
        for item in family["variants"].as_array().into_iter().flatten() {
            let Some(id) = item["model_uid"].as_str().map(str::trim).filter(|id| !id.is_empty()) else { continue };
            let mut row = json!({"id": id, "label": item["label"].as_str().unwrap_or(id),
                "reasoning": {"supported": false, "levels": [], "default": ""}});
            if let Some(variant) = variants.get(id) {
                row["variant"] = variant.clone();
            }
            rows.push(row);
        }
    }
    Ok(rows)
}

fn parse_grok(text: &str) -> Result<Vec<Value>, String> {
    if text.trim_start().starts_with('{') {
        let data: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        return Ok(data["models"]["availableModels"].as_array().into_iter().flatten()
            .filter_map(|item| {
                let id=item["modelId"].as_str()?.trim();
                if id.is_empty() { return None; }
                let meta=&item["_meta"];
                let choices:Vec<Value>=meta["reasoningEfforts"].as_array().into_iter().flatten()
                    .filter_map(|row|row["value"].as_str().or_else(||row["id"].as_str()).map(|v|json!(v))).collect();
                let default=meta["reasoningEfforts"].as_array().into_iter().flatten()
                    .find(|row|row["default"]==true || row["isDefault"]==true)
                    .and_then(|row|row["value"].as_str().or_else(||row["id"].as_str())).unwrap_or("");
                let mut effort=reasoning(&json!(choices),&json!(default),"grok_acp_model_metadata");
                if let Some(supported)=meta["supportsReasoningEffort"].as_bool() { effort["supported"]=json!(supported); }
                Some(json!({"id":id,"label":item["name"].as_str().unwrap_or(id),"reasoning":effort}))
            }).collect());
    }
    let ansi=Regex::new(r"\x1b\[[0-9;]*m").map_err(|e|e.to_string())?;
    let clean=ansi.replace_all(text,"");
    Ok(clean.lines().filter_map(|line| {
        let trimmed=line.trim_start();
        let rest=trimmed.strip_prefix('-').or_else(||trimmed.strip_prefix('*'))?.trim_start();
        let id=rest.split(|c:char|c.is_whitespace() || c=='(').next()?.trim();
        (!id.is_empty()).then(||json!({"id":id,"label":id,
            "reasoning":reasoning(&Value::Null,&Value::Null,"grok_names_only")}))
    }).collect())
}

fn parse_opencode(text: &str) -> Result<Vec<Value>, String> {
    let mut rows=Vec::new();
    let mut offset=0;
    while let Some(start)=text[offset..].find('{') {
        let segment=&text[offset+start..];
        let mut stream=serde_json::Deserializer::from_str(segment).into_iter::<Value>();
        let Some(Ok(item))=stream.next() else { break; };
        offset+=start+stream.byte_offset();
        let (Some(id),Some(provider))=(item["id"].as_str(),item["providerID"].as_str()) else {continue};
        let variants:Vec<Value>=item["variants"].as_object().into_iter().flat_map(|map|map.keys().map(|key|json!(key))).collect();
        let mut effort=reasoning(&json!(variants),&Value::Null,"opencode_model_variants");
        effort["kind"]=json!("variant");
        rows.push(json!({"id":format!("{provider}/{id}"),"label":item["name"].as_str().unwrap_or(id),
            "provider":provider,"reasoning":effort}));
    }
    if !rows.is_empty() {return Ok(rows);}
    let valid=Regex::new(r"^[\w.-]+/[\w./:-]+$").map_err(|e|e.to_string())?;
    Ok(text.lines().map(str::trim).filter(|line|valid.is_match(line))
        .map(|id|json!({"id":id,"label":id,"reasoning":reasoning(&Value::Null,&Value::Null,"opencode_names_only")})).collect())
}

fn parse_openai_models(text: &str) -> Result<Vec<Value>, String> {
    let data: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let models = data["models"].as_array().or_else(|| data["data"].as_array());
    Ok(models.into_iter().flatten().filter_map(|item| {
        let id = item["slug"].as_str().or_else(|| item["id"].as_str())?.trim();
        if id.is_empty() { return None; }
        let label = item["display_name"].as_str().or_else(|| item["displayName"].as_str()).unwrap_or(id);
        let levels = item.get("supported_reasoning_levels")
            .or_else(|| item.get("supportedReasoningEfforts")).unwrap_or(&Value::Null);
        let default = item.get("default_reasoning_level")
            .or_else(|| item.get("defaultReasoningEffort")).unwrap_or(&Value::Null);
        let mut row = json!({"id":id,"label":label,"reasoning":reasoning(levels,default,"model_metadata")});
        if item["input"].is_array() { row["input"] = item["input"].clone(); }
        Some(row)
    }).collect())
}

fn parse_cursor(text: &str) -> Result<Vec<Value>, String> {
    let clean=Regex::new(r"[\u{200b}-\u{200d}\u{feff}]").map_err(|e|e.to_string())?;
    let suffix=Regex::new(r"\s+\((?:current, default|default|current|NO ZDR)\)$").map_err(|e|e.to_string())?;
    let grammar=Regex::new(r"^(.+?)((?: (?:\d+(?:\.\d+)?[KM]|Extra High|XHigh|Minimal|Low|Medium|High|Max|None|No Thinking|Thinking|Ultra Fast|Fast))+)$").map_err(|e|e.to_string())?;
    let tokens=Regex::new(r"\d+(?:\.\d+)?[KM]|Extra High|XHigh|Minimal|Low|Medium|High|Max|None|No Thinking|Thinking|Ultra Fast|Fast").map_err(|e|e.to_string())?;
    let context=Regex::new(r"^\d+(?:\.\d+)?[KM]$").map_err(|e|e.to_string())?;
    let mut rows:Vec<(Value,String,serde_json::Map<String,Value>)>=Vec::new();
    for line in text.lines() {
        let Some((id,label))=line.split_once(" - ") else {continue};
        if line.to_ascii_lowercase().starts_with("available models") {continue;}
        let id=id.trim();
        if id.is_empty() {continue;}
        let label=clean.replace_all(label,"");
        let label=suffix.replace(label.trim(),"");
        let label=label.split_whitespace().collect::<Vec<_>>().join(" ");
        let label=if label.is_empty(){id.to_owned()}else{label};
        let mut base=label.clone();
        let mut params=serde_json::Map::new();
        if let Some(parts)=grammar.captures(&label) {
            base=parts[1].to_owned();
            let mut repeated=false;
            for part in tokens.find_iter(&parts[2]) {
                let value=part.as_str();
                let key=if context.is_match(value){"context"}else if matches!(value,"Fast"|"Ultra Fast"){"speed"}
                    else if matches!(value,"Thinking"|"No Thinking"){"thinking"}else{"effort"};
                if params.insert(key.to_owned(),json!(value)).is_some() {repeated=true;break;}
            }
            if repeated {params=serde_json::Map::from_iter([("configuration".to_owned(),json!(parts[2].trim()))]);}
        }
        rows.push((json!({"id":id,"label":label,
            "reasoning":reasoning(&Value::Null,&Value::Null,"cursor_model_variant")}),base,params));
    }
    let mut groups:std::collections::HashMap<String,Vec<usize>>=Default::default();
    for (index,(_,base,_)) in rows.iter().enumerate() {groups.entry(base.clone()).or_default().push(index);}
    for (base,members) in groups {
        if members.len()<2 {continue;}
        let keys:std::collections::HashSet<String>=members.iter().flat_map(|index|rows[*index].2.keys().cloned()).collect();
        for index in members {
            for key in &keys {
                rows[index].2.entry(key.clone()).or_insert_with(||json!(match key.as_str() {
                    "speed" => "Standard", "thinking" => "No Thinking", _ => "Default",
                }));
            }
            rows[index].0["variant"]=json!({"family_id":base,"family_label":base,"parameters":rows[index].2});
        }
    }
    Ok(rows.into_iter().map(|(row,_,_)|row).collect())
}

fn valid_level(value: &str) -> bool {
    let mut chars = value.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && value.len() <= 64
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// The `droid.list_models` response from `droid exec` stream-jsonrpc.
///
/// `droid exec --help` resolves feature-flagged models with their default
/// values, so it omits models the account has enabled; this RPC applies the
/// account's live flags.
fn parse_droid_rpc(text: &str) -> Result<Vec<Value>, String> {
    let response = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
        .find(|frame| frame["id"] == DROID_MODELS_REQUEST_ID)
        .ok_or("没有 droid.list_models 响应")?;
    if let Some(error) = response.get("error").filter(|e| !e.is_null()) {
        return Err(error["message"].as_str().map(str::to_owned).unwrap_or_else(|| error.to_string()));
    }
    let models = response["result"]["models"].as_array().ok_or("响应缺少 result.models")?;
    let mut rows: Vec<Value> = models
        .iter()
        .filter(|model| model["disabled"] != true)
        .filter_map(|model| {
            let id = model["id"].as_str().filter(|id| !id.is_empty())?;
            let label = model["displayName"].as_str().filter(|l| !l.is_empty()).unwrap_or(id);
            let offered: Vec<&str> = model["supportedReasoningEfforts"].as_array().into_iter().flatten()
                .filter_map(Value::as_str).collect();
            let reasons = offered.iter().any(|level| *level != "none");
            let mut levels: Vec<String> = Vec::new();
            for item in offered {
                if item != "default" && (item != "none" || reasons) && valid_level(item) && !levels.iter().any(|l| l == item) {
                    levels.push(item.to_owned());
                }
            }
            let default = model["defaultReasoningEffort"].as_str().filter(|d| levels.iter().any(|l| l == d)).unwrap_or("");
            Some(json!({"id": id, "label": label, "reasoning": {
                "supported": !levels.is_empty(), "levels": levels, "default": default,
                "kind": "effort", "source": "droid_list_models"}}))
        })
        .collect();
    label_family_variants(&mut rows, true);
    Ok(rows)
}

async fn cli_catalog(
    engine: &EngineDescriptor,
    binary: &Path,
    env: &LaunchEnv,
) -> Result<Vec<Value>, RuntimeError> {
    let spec = &engine.public()["models"];
    let parser = spec["parser"].as_str().unwrap_or("");
    let argv: Vec<String> = spec["argv"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    if argv.is_empty() {
        return Err(RuntimeError::unsupported(format!("{} 没有可执行的模型目录命令", engine.identity.display_name)));
    }
    let input = catalog_stdin(spec);
    let mut command = tokio::process::Command::from(agent::supervised_command(binary));
    command
        .args(&argv)
        .envs(env.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .env("NO_COLOR", "1")
        .stdin(if input.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if engine.engine() == "kimi" && let Some(home) = &env.home {
        command.env("HOME", home);
    }
    let joined = argv.join(" ");
    let run = async {
        let mut child = command.spawn()?;
        if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(text.as_bytes()).await?;
            drop(stdin);
        }
        child.wait_with_output().await
    };
    let output = tokio::time::timeout(CATALOG_TIMEOUT, run)
        .await
        .map_err(|_| RuntimeError::retryable("model_catalog.timeout", format!("{joined} 超过 {} 秒", CATALOG_TIMEOUT.as_secs())))?
        .map_err(|e| RuntimeError::new("model_catalog.start_failed", format!("无法运行 {joined}：{e}")))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(RuntimeError::retryable(
            "model_catalog.command_failed",
            format!("{joined} 以 {} 退出：{}", output.status, if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() }),
        ));
    }
    parse_cli_output(parser, &stdout, &joined)
}

pub(crate) fn parse_cli_output(parser: &str, stdout: &str, command: &str) -> Result<Vec<Value>, RuntimeError> {
    let parsed = match parser {
        "kimi_models" => parse_kimi(&stdout),
        "omp_models" => parse_omp(&stdout),
        "devin_models" => parse_devin(&stdout),
        "droid_rpc_models" => parse_droid_rpc(&stdout),
        "grok_models" => parse_grok(&stdout),
        "opencode_models" => parse_opencode(&stdout),
        "cursor_models" => parse_cursor(&stdout),
        "openai_models" => parse_openai_models(&stdout),
        other => {
            return Err(RuntimeError::unsupported(format!("模型目录解析器 {other} 尚未接入")));
        }
    };
    parsed.map_err(|e| RuntimeError::new("model_catalog.invalid_response", format!("{command} 返回了无法解析的模型目录：{e}")))
}

fn acp_model_options(
    descriptors: &[OptionDescriptor],
    selections: &[OptionSelection],
) -> Vec<OptionDescriptor> {
    descriptors
        .iter()
        .filter_map(|descriptor| match descriptor {
            OptionDescriptor::Select {
                id,
                label,
                options,
                default_value,
                role,
                apply,
                recommended,
                permissive,
            } if id == "reasoningEffort" || id.starts_with(CONFIG_OPTION_PREFIX) => {
                let current = selections
                    .iter()
                    .find(|selection| selection.id == *id)
                    .and_then(|selection| selection.value.as_str())
                    .map(str::to_owned)
                    .or_else(|| default_value.clone());
                Some(OptionDescriptor::Select {
                    id: id.clone(),
                    label: label.clone(),
                    options: options.clone(),
                    default_value: current,
                    role: *role,
                    apply: *apply,
                    recommended: recommended.clone(),
                    permissive: permissive.clone(),
                })
            }
            _ => None,
        })
        .collect()
}

/// ACP catalogs are session-scoped: establish without sending a model prompt,
/// then switch `acp:model` across advertised models to read each model's live
/// config options (such as `thought_level` or `context_window`).
async fn acp_catalog(engine: &EngineDescriptor, binary: &Path, mut env: LaunchEnv) -> Result<Vec<Value>, RuntimeError> {
    let NativeTransport::Acp { agent_id, args } = engine.default_adapter().native
        .ok_or_else(|| RuntimeError::unsupported("ACP transport missing"))? else {
        return Err(RuntimeError::unsupported("ACP transport required"));
    };
    if !env.env.iter().any(|(key, _)| key == "PATH") {
        env.env.push(("PATH".into(), super::binaries::launch_path(binary)?));
    }
    let cwd = tempfile::tempdir().map_err(|e| RuntimeError::new("model_catalog.workspace_failed", e.to_string()))?;
    let session = agent::start_session(ProviderKind::Acp, agent::SessionOptions {
        cwd: cwd.path().to_path_buf(),
        acp: Some(agent::AcpAgent { id: agent_id.into(), name: engine.identity.display_name.clone(),
            launch: agent::AcpLaunch::Custom { command: binary.to_string_lossy().into_owned(),
                args: args.iter().map(|s| (*s).into()).collect(), env: Vec::new() } }),
        launch_env: env,
        model: None, resume: None, fork: false, binary_path: None,
        option_selections: Vec::new(), mcp_servers: Vec::new(), extra_args: Vec::new(), instructions: None,
    }).await.map_err(|e| RuntimeError::new("model_catalog.request_failed", e.to_string()))?;
    let result = tokio::time::timeout(CATALOG_TIMEOUT, async {
        let mut initial_models: Option<(Vec<agent::SelectOption>, Option<String>, Vec<OptionDescriptor>)> = None;
        while let Ok(event) = session.events.recv().await {
            match event {
                agent::AgentEvent::ProviderOptions { descriptors, selections } => {
                    if let Some(options) = descriptors.iter().find_map(|descriptor| match descriptor {
                        OptionDescriptor::Select { id, options, .. } if id == "acp:model" => Some(options.clone()),
                        _ => None,
                    }) {
                        let selected = selections
                            .iter()
                            .find(|s| s.id == "acp:model")
                            .and_then(|s| s.value.as_str())
                            .map(str::to_owned);
                        let initial_options = acp_model_options(&descriptors, &selections);
                        initial_models = Some((options, selected, initial_options));
                        break;
                    }
                }
                agent::AgentEvent::Error { message, fatal: true } => {
                    return Err(RuntimeError::new("model_catalog.request_failed", message));
                }
                agent::AgentEvent::SessionClosed { reason } => {
                    return Err(RuntimeError::new(
                        "model_catalog.session_closed",
                        reason.unwrap_or_else(|| "ACP session closed before reporting model catalog".into()),
                    ));
                }
                _ => {}
            }
        }
        let Some((options, default_model, initial_options)) = initial_models else {
            return Err(RuntimeError::new("model_catalog.empty", "ACP session closed without a model catalog"));
        };
        let mut options_by_model = std::collections::HashMap::new();
        if let Some(selected) = &default_model {
            options_by_model.insert(selected.clone(), initial_options);
        }
        for option in &options {
            if options_by_model.contains_key(&option.value) {
                continue;
            }
            session
                .commands
                .send(agent::SessionCommand::SetOption {
                    id: "acp:model".into(),
                    value: json!(option.value),
                })
                .await
                .map_err(|_| RuntimeError::new("model_catalog.session_closed", "ACP catalog session closed while probing model options"))?;
            loop {
                match session.events.recv().await {
                    Ok(agent::AgentEvent::ProviderOptions { descriptors, selections })
                        if selections.iter().any(|s| s.id == "acp:model" && s.value.as_str() == Some(option.value.as_str())) =>
                    {
                        options_by_model.insert(option.value.clone(), acp_model_options(&descriptors, &selections));
                        break;
                    }
                    Ok(agent::AgentEvent::Warning { message }) if message.contains("acp:model") => {
                        return Err(RuntimeError::new("model_catalog.option_probe_failed", message));
                    }
                    Ok(agent::AgentEvent::Error { message, fatal: true }) => {
                        return Err(RuntimeError::new("model_catalog.request_failed", message));
                    }
                    Ok(agent::AgentEvent::SessionClosed { reason }) => {
                        return Err(RuntimeError::new(
                            "model_catalog.session_closed",
                            reason.unwrap_or_else(|| "ACP session closed while probing model options".into()),
                        ));
                    }
                    Ok(_) => {}
                    Err(_) => {
                        return Err(RuntimeError::new("model_catalog.session_closed", "ACP event channel closed while probing model options"));
                    }
                }
            }
        }
        Ok(options
            .into_iter()
            .flat_map(|option| {
                let is_default = default_model.as_deref() == Some(option.value.as_str());
                let model_options = options_by_model.remove(&option.value).unwrap_or_default();
                configured_rows(
                    &ModelSpec {
                        id: option.value,
                        display_name: option.label,
                        is_default,
                        options: model_options,
                    },
                    "acp_model_options",
                    true,
                )
            })
            .collect())
    }).await;
    let _ = session.commands.send(agent::SessionCommand::Shutdown).await;
    result.map_err(|_| RuntimeError::retryable("model_catalog.timeout", "ACP model catalog timed out"))?
}

/// Every model `engine` offers under `env`, in the engine's own order.
pub async fn list(
    engine: &EngineDescriptor,
    native: NativeTransport,
    binary: &Path,
    mut env: LaunchEnv,
    cache_dir: &Path,
) -> Result<Vec<Value>, RuntimeError> {
    if !env.env.iter().any(|(key, _)| key == "PATH") {
        env.env.push(("PATH".into(), super::binaries::launch_path(binary)?));
    }
    if engine.public()["models"]["method"] == "acp" {
        return acp_catalog(engine, binary, env).await;
    }
    if native == NativeTransport::Custom(super::descriptors::CustomTransport::OpenCodeV2) {
        return super::native_drivers::list_opencode_v2(binary, &env).await;
    }
    let provider = match native {
        NativeTransport::Provider(provider) => Some(provider),
        NativeTransport::Acp { agent_id: "cursor", .. } => Some(ProviderKind::Cursor),
        NativeTransport::Acp { .. } | NativeTransport::Custom(_) => None,
    };
    let models = match provider {
        Some(provider) => {
            let source = format!("{}.model_list", engine.engine());
            let refresh = CatalogRefresh { cache_dir: Some(cache_dir.to_path_buf()), network: true };
            let listed = tokio::time::timeout(
                CATALOG_TIMEOUT,
                agent::list_models(provider, Some(binary.to_path_buf()), env, refresh),
            )
            .await
            .map_err(|_| RuntimeError::retryable("model_catalog.timeout", format!("模型目录查询超过 {} 秒", CATALOG_TIMEOUT.as_secs())))?
            .map_err(|e| RuntimeError::retryable("model_catalog.request_failed", e.to_string()))?;
            if matches!(provider, ProviderKind::Cursor | ProviderKind::Grok) {
                listed.iter().flat_map(|spec| configured_rows(spec, &source, provider == ProviderKind::Grok)).collect()
            } else {
                listed.iter().map(|spec| from_spec(spec, &source)).collect()
            }
        }
        None => cli_catalog(engine, binary, &env).await?,
    };
    if models.is_empty() {
        return Err(RuntimeError::new(
            "model_catalog.empty",
            format!("{} 没有返回任何模型", engine.identity.display_name),
        ));
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_catalog_formats_are_parsed_with_their_effort_metadata() {
        let kimi = parse_kimi(r#"{"models":{"k2":{"displayName":"K2","supportEfforts":["low","high"],"defaultEffort":"high"}}}"#).unwrap();
        assert_eq!(kimi[0]["id"], "k2");
        assert_eq!(kimi[0]["reasoning"]["default"], "high");
        let omp = parse_omp(r#"{"models":[{"selector":"anthropic/sonnet","name":"Sonnet","thinking":["low"]}]}"#).unwrap();
        assert_eq!(omp[0]["provider"], "anthropic");
        assert_eq!(omp[0]["reasoning"]["levels"], json!(["low"]));
        let devin = parse_devin(r#"{"families":[{"variants":[{"model_uid":"swe-1","label":"SWE 1"}]}]}"#).unwrap();
        assert_eq!(devin[0]["id"], "swe-1");
        let rpc = r#"{"type":"response","id":"muteki-model-catalog","result":{"models":[{"id":"auto","displayName":"Auto Model","supportedReasoningEfforts":["none"],"defaultReasoningEffort":"none"},{"id":"gpt-5","displayName":"GPT 5","supportedReasoningEfforts":["none","low","high"],"defaultReasoningEffort":"low"},{"id":"gpt-5-fast","displayName":"GPT 5 Fast Mode","supportedReasoningEfforts":["low"],"defaultReasoningEffort":"low"},{"id":"off","displayName":"Off","disabled":true}]}}"#;
        let droid = parse_droid_rpc(&format!("{{\"type\":\"notification\"}}\n{rpc}\n")).unwrap();
        assert_eq!(droid.len(), 3);
        assert_eq!(droid[0]["reasoning"]["supported"], false);
        assert_eq!(droid[1]["reasoning"]["levels"], json!(["none", "low", "high"]));
        assert_eq!(droid[1]["reasoning"]["default"], "low");
        assert_eq!(droid[2]["variant"]["family_id"], "gpt-5");
        assert!(parse_droid_rpc(r#"{"id":"muteki-model-catalog","error":{"message":"denied"}}"#).is_err());
    }

    #[test]
    fn grok_opencode_and_cursor_catalog_formats_keep_real_model_ids() {
        let grok = parse_grok("Default model: grok-4.6\nAvailable models:\n  * grok-4.6 (default)\n  - grok-4.5\n").unwrap();
        assert_eq!(grok.iter().map(|row|row["id"].as_str().unwrap()).collect::<Vec<_>>(),["grok-4.6","grok-4.5"]);
        let open = parse_opencode("opencode/beam-free\nopencode/glm-5.3-flash-free\n").unwrap();
        assert_eq!(open[0]["id"], "opencode/beam-free");
        assert_eq!(open[0]["reasoning"]["source"], "opencode_names_only");
        let verbose = parse_opencode("openai/demo\n{\"id\":\"demo\",\"providerID\":\"openai\",\"variants\":{\"fast\":{}}}\n").unwrap();
        assert_eq!(verbose[0]["id"], "openai/demo");
        assert_eq!(verbose[0]["reasoning"]["kind"], "variant");
        let cursor = parse_cursor("a - Claude Sonnet High (default)\nb - Claude Sonnet Low\n").unwrap();
        assert_eq!(cursor[0]["id"], "a");
        assert_eq!(cursor[1]["id"], "b");
        assert_eq!(cursor[0]["variant"]["parameters"]["effort"], "High");
        assert_eq!(cursor[1]["variant"]["parameters"]["effort"], "Low");
    }

    #[test]
    fn codex_bundled_catalog_keeps_metadata_without_a_model_turn() {
        let models = parse_openai_models(r#"{"models":[{"slug":"test-bundled","display_name":"Bundled","supported_reasoning_levels":[{"effort":"low"},{"effort":"high"}],"default_reasoning_level":"high","input":["text"]}]}"#).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["id"], "test-bundled");
        assert_eq!(models[0]["reasoning"]["levels"], json!(["low", "high"]));
        assert_eq!(models[0]["reasoning"]["default"], "high");
        assert_eq!(models[0]["input"], json!(["text"]));
    }

    #[test]
    fn reasoning_levels_drop_default_and_duplicates() {
        let value = reasoning(&json!(["default", "low", {"effort":"low"}, {"value":"high"}]), &json!("high"), "x");
        assert_eq!(value["levels"], json!(["low", "high"]));
        assert_eq!(value["default"], "high");
        assert_eq!(reasoning(&json!(null), &json!("x"), "x")["supported"], false);
    }
}
