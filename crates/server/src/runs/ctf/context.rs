//! CTF model wire text, matching Python graph_render.to_ctf_graph_yaml.
//! This is a projection only; rendering never changes the shared graph.
use serde_json::Value;

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}
fn visible(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if (ch < ' ' && !matches!(ch, '\n' | '\t' | '\r')) || ch == '\u{7f}' {
                format!("\\u{:04x}", ch as u32)
            } else {
                ch.to_string()
            }
        })
        .collect()
}
fn scalar(value: &Value) -> String {
    serde_json::to_string(text(value)).unwrap()
}
pub(super) fn timestamp(value: &Value) -> String {
    value
        .as_f64()
        .and_then(|ts| chrono::DateTime::from_timestamp_millis((ts * 1000.) as i64))
        .map(|ts| ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

pub(super) fn render(source: &Value) -> String {
    let mut lines = vec![
        if source["facts"].as_array().is_some_and(|v| !v.is_empty()) {
            "facts:"
        } else {
            "facts: []"
        }
        .to_owned(),
    ];
    for fact in source["facts"].as_array().into_iter().flatten() {
        lines.push(format!("  - id: {}", text(&fact["id"])));
        lines.push(format!(
            "    title: {}",
            scalar(&Value::String(visible(text(&fact["title"]))))
        ));
        if !text(&fact["producedBy"]).is_empty() {
            lines.push(format!("    producedBy: {}", text(&fact["producedBy"])));
        }
        if !text(&fact["createdAt"]).is_empty() {
            lines.push(format!("    createdAt: {}", text(&fact["createdAt"])));
        }
        let content = visible(text(&fact["content"]));
        let first = content
            .split('\n')
            .find(|line| !line.is_empty())
            .unwrap_or("");
        if content.contains('\n')
            && !content.contains(['\r', '\u{2028}', '\u{2029}', '\u{85}'])
            && !content.ends_with('\n')
            && !first.is_empty()
            && !first.starts_with([' ', '\t'])
        {
            lines.push("    content: |-".into());
            lines.extend(content.split('\n').map(|line| {
                if line.is_empty() {
                    String::new()
                } else {
                    format!("      {line}")
                }
            }));
        } else {
            lines.push(format!("    content: {}", scalar(&Value::String(content))));
        }
    }
    lines.push("goals:".into());
    for goal in source["goals"].as_array().into_iter().flatten() {
        lines.push(format!("  - id: {}", text(&goal["id"])));
        lines.push(format!("    kind: {}", text(&goal["kind"])));
        lines.push(format!(
            "    criterion: {}",
            if goal["id"] == "goal_final" {
                text(&goal["criterion"]).to_owned()
            } else {
                scalar(&goal["criterion"])
            }
        ));
        if goal["state"] == "satisfied" {
            if let Some(facts) = goal["satisfiedBy"].as_array().filter(|v| !v.is_empty()) {
                lines.push("    satisfiedBy:".into());
                lines.extend(facts.iter().map(|fact| format!("      - {}", text(fact))));
            }
            for key in ["satisfiedReason", "satisfiedAt"] {
                if !text(&goal[key]).is_empty() {
                    lines.push(format!(
                        "    {key}: {}",
                        if key == "satisfiedReason" {
                            scalar(&goal[key])
                        } else {
                            text(&goal[key]).to_owned()
                        }
                    ));
                }
            }
        }
        if !text(&goal["createdAt"]).is_empty() {
            lines.push(format!("    createdAt: {}", text(&goal["createdAt"])));
        }
    }
    lines.push(
        if source["steps"].as_array().is_some_and(|v| !v.is_empty()) {
            "steps:"
        } else {
            "steps: []"
        }
        .into(),
    );
    for step in source["steps"].as_array().into_iter().flatten() {
        lines.extend([
            format!("  - id: {}", text(&step["id"])),
            format!("    action: {}", scalar(&step["action"])),
            "    from:".into(),
        ]);
        let from = step["from"].as_array().unwrap();
        if from.is_empty() {
            lines.push("      []".into());
        } else {
            lines.extend(from.iter().map(|fact| format!("      - {}", text(fact))));
        }
        lines.extend([
            "    actor: agent".into(),
            format!("    priority: {}", text(&step["priority"])),
            format!("    state: {}", text(&step["state"])),
        ]);
        for key in ["requiresCapabilities", "requires"] {
            if let Some(items) = step[key].as_array().filter(|v| !v.is_empty()) {
                lines.push(format!("    {key}:"));
                lines.extend(items.iter().map(|item| format!("      - {}", scalar(item))));
            }
        }
        if let Some(to) = step["to"].as_array().filter(|v| !v.is_empty()) {
            if to.len() == 1 {
                lines.push(format!("    to: {}", text(&to[0])));
            } else {
                lines.push("    to:".into());
                lines.extend(to.iter().map(|fact| format!("      - {}", text(fact))));
            }
        }
        if step["state"] == "dropped" && !text(&step["reason"]).is_empty() {
            lines.push(format!("    reason: {}", scalar(&step["reason"])));
        }
        if !text(&step["createdAt"]).is_empty() {
            lines.push(format!("    createdAt: {}", text(&step["createdAt"])));
        }
    }
    if let Some(notes) = source["operatorNotes"].as_array().filter(|v| !v.is_empty()) {
        lines.push("operatorNotes:".into());
        lines.extend(notes.iter().map(|note| format!("  - {}", note)));
    }
    lines.join("\n")
}

pub(super) fn live_origin(run: &Value) -> String {
    let raw = text(&run["task_contract"]["raw_instruction"]).trim();
    let description = text(&run["description"]).trim();
    let target = text(&run["target"]).trim();
    let mut endpoints: Vec<String> = text(&run["scope"])
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    if !target.is_empty() && !endpoints.iter().any(|s| s == target) {
        endpoints.insert(0, target.to_owned());
    }
    let mut instruction = if !endpoints.is_empty()
        && !description.is_empty()
        && endpoints.iter().all(|ep| description.contains(ep))
        && !endpoints.iter().all(|ep| raw.contains(ep))
    {
        description
    } else if !raw.is_empty() {
        raw
    } else {
        description
    }
    .to_owned();
    if !endpoints.is_empty() && instruction.contains("\"container_addr\"") {
        let replacement = format!(
            "\"container_addr\": [\n{}\n  ]",
            endpoints
                .iter()
                .map(|ep| format!("    {}", serde_json::to_string(ep).unwrap()))
                .collect::<Vec<_>>()
                .join(",\n")
        );
        let regex = regex::Regex::new(r#""container_addr"\s*:\s*\[[\s\S]*?\](\s*[,}\n])"#).unwrap();
        instruction = regex
            .replacen(&instruction, 1, |caps: &regex::Captures| {
                format!("{replacement}{}", &caps[1])
            })
            .into_owned();
        instruction = regex::Regex::new(r#"\n\s*"container_status"\s*:\s*"stopped"\s*,"#)
            .unwrap()
            .replacen(&instruction, 1, "")
            .into_owned();
    }
    if !endpoints.is_empty() && instruction.contains("Target endpoints:\n") {
        let block = format!(
            "Target endpoints:\n{}",
            endpoints
                .iter()
                .map(|ep| format!("- {ep}\n"))
                .collect::<String>()
        );
        if !instruction.contains(block.trim()) {
            instruction = regex::Regex::new(r"Target endpoints:\n(?:- [^\n]+\n?)+")
                .unwrap()
                .replacen(&instruction, 1, regex::NoExpand(&block))
                .into_owned();
        }
    }
    if instruction.contains("The target address may have changed.")
        && !instruction.contains("interact ONLY with the current Target endpoints")
    {
        instruction=instruction.replace("The target address may have changed.","The target address may have changed; interact ONLY with the current Target endpoints listed above and do not probe recycled IPs from earlier epochs.");
    }
    let mut attachments: Vec<String> = run["task_contract"]["attachments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a["name"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    if attachments.is_empty() {
        attachments = run["attachments"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| a.as_str().or_else(|| a["path"].as_str()))
            .filter_map(|p| std::path::Path::new(p).file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .collect();
    }
    let mut lines = vec!["题目信息:".to_owned()];
    if !instruction.is_empty() {
        lines.push(instruction);
    }
    if !endpoints.is_empty() && !endpoints.iter().all(|ep| lines.join("\n").contains(ep)) {
        lines.extend([String::new(), format!("目标地址: {}", endpoints.join(", "))]);
    }
    if !attachments.is_empty() {
        let line = format!("附件: {}", attachments.join(", "));
        if !lines.join("\n").contains(&line) {
            lines.push(line);
        }
    }
    lines.join("\n").trim().to_owned()
}
