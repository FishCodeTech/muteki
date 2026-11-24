//! Deterministic, zero-AI-fluff SRC frozen report rendering across Chinese,
//! HackerOne / Bugcrowd English, and Bilingual templates.
use super::*;

fn present(value: &Value) -> bool {
    !value.is_null() && value != "" && value != false && value != &json!([]) && value != &json!({})
}

fn prose(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn redact(value: &mut Value, needle: &str, replacement: &str) -> usize {
    match value {
        Value::String(text) => {
            let count = text.matches(needle).count();
            if count > 0 {
                *text = text.replace(needle, replacement);
            }
            count
        }
        Value::Array(items) => items
            .iter_mut()
            .map(|item| redact(item, needle, replacement))
            .sum(),
        Value::Object(items) => {
            let old = std::mem::take(items);
            let mut count = 0;
            for (key, mut value) in old {
                count += key.matches(needle).count();
                count += redact(&mut value, needle, replacement);
                items.insert(key.replace(needle, replacement), value);
            }
            count
        }
        _ => 0,
    }
}

fn label(mapping: &Value, field: &str) -> String {
    mapping[field].as_str().unwrap_or(field).to_owned()
}

pub(super) fn render(s: &AppState, task_id: &str, report: &Value, body: &Value) -> Api<Value> {
    let document = report["document"].clone();
    let claims = report["claims"].clone();
    let review = report["review"].clone();
    let cvss = report["cvss"].clone();
    let dedup = report["deduplication"].clone();
    let mut redactions = Vec::new();
    let mut rules = Vec::new();
    for (index, rule) in body["redactions"].as_array().into_iter().flatten().enumerate() {
        let value = require(rule, "value")?;
        let replacement = rule["replacement"]
            .as_str()
            .filter(|value| !value.is_empty())
            .unwrap_or("[REDACTED]");
        redactions.push(json!({"rule_index":index,"replacement":replacement,"occurrences":0}));
        rules.push((value.to_owned(), replacement.to_owned()));
    }
    let lang = body["language"]
        .as_str()
        .or_else(|| report["template"]["language"].as_str())
        .unwrap_or("zh");
    let is_en = lang == "en";
    let is_bi = lang == "zh-en";
    let mapping = &report["template"]["field_mapping"];
    let severity = &report["effective_severity"];
    let capped_note = if severity["capped"] == true {
        if is_en {
            format!(" — capped: {}", prose(&severity["cap_reason"]))
        } else {
            format!(" — 置信度封顶：{}", prose(&severity["cap_reason"]))
        }
    } else {
        String::new()
    };
    let cvss_vec = cvss["vector_string"]
        .as_str()
        .or_else(|| cvss["vector"].as_str())
        .unwrap_or("");

    let mut lines = vec![format!("# {}", prose(&document["title"])), String::new()];
    if is_en {
        lines.extend([
            format!("- **Report ID**: `{}` (v{})", report["report_id"].as_str().unwrap_or(""), report["report_seq"]),
            format!("- **Target**: {} ({})", prose(&document["target"]), prose(&document["target_version"])),
            format!("- **Authorized Scope**: {}", prose(&document["scope"])),
            format!("- **Attacker Preconditions**: {}", prose(&document["attacker"])),
            format!("- **Readiness**: {}", prose(&report["review"]["readiness"])),
            format!(
                "- **Effective Severity**: {} (Assessed Score: {}{}, Confidence: `{}`{})",
                prose(&severity["effective_severity"]),
                prose(&severity["assessed_score"]),
                if cvss_vec.is_empty() { String::new() } else { format!(" · `{cvss_vec}`") },
                prose(&severity["confidence"]),
                capped_note
            ),
            String::new(),
        ]);
    } else if is_bi {
        lines.extend([
            format!("- **报告编号 / Report ID**：`{}` (v{})", report["report_id"].as_str().unwrap_or(""), report["report_seq"]),
            format!("- **受影响目标 / Target**：{} ({})", prose(&document["target"]), prose(&document["target_version"])),
            format!("- **授权范围 / Scope**：{}", prose(&document["scope"])),
            format!("- **最低权限前提 / Preconditions**：{}", prose(&document["attacker"])),
            format!("- **交付准备度 / Readiness**：{}", prose(&report["review"]["readiness"])),
            format!(
                "- **生效严重度 / Effective Severity**：{}（评估分数 / Score {}{}，置信度 / Confidence `{}`{}）",
                prose(&severity["effective_severity"]),
                prose(&severity["assessed_score"]),
                if cvss_vec.is_empty() { String::new() } else { format!(" · `{cvss_vec}`") },
                prose(&severity["confidence"]),
                capped_note
            ),
            String::new(),
        ]);
    } else {
        lines.extend([
            format!("- 报告 ID：`{}`（版本 {}）", report["report_id"].as_str().unwrap_or(""), report["report_seq"]),
            format!("- 目标：{}（{}）", prose(&document["target"]), prose(&document["target_version"])),
            format!("- 计划范围：{}", prose(&document["scope"])),
            format!("- 攻击者最低角色与前提：{}", prose(&document["attacker"])),
            format!("- 准备度：{}", prose(&report["review"]["readiness"])),
            format!(
                "- 生效严重度：{}（评估分数 {}{}，置信度 {}{}）",
                prose(&severity["effective_severity"]),
                prose(&severity["assessed_score"]),
                if cvss_vec.is_empty() { String::new() } else { format!(" · `{cvss_vec}`") },
                prose(&severity["confidence"]),
                capped_note
            ),
            String::new(),
        ]);
    }

    let coverage = &report["coverage"];
    if present(coverage) {
        if coverage["status"] != "complete" {
            if is_en {
                lines.push(format!(
                    "> Assessment Coverage Status: `{}`. Zero or few findings do NOT prove target safety.",
                    prose(&coverage["status"])
                ));
            } else if is_bi {
                lines.push(format!(
                    "> 覆盖状态 / Coverage Status：`{}`。零发现或发现较少不代表目标安全 / Few or zero findings do not prove target safety.",
                    prose(&coverage["status"])
                ));
            } else {
                lines.push(format!(
                    "> 扫描未正常完成：{}。零发现或发现较少不代表目标安全。",
                    prose(&coverage["status"])
                ));
            }
            lines.push(String::new());
        }
        let cov_heading = if is_en {
            "## Coverage & Environment Blockers"
        } else if is_bi {
            "## 测试覆盖与阻断因素 / Coverage & Blockers"
        } else {
            "## 测试覆盖与阻断因素"
        };
        lines.extend([cov_heading.to_owned(), String::new()]);
        if is_en {
            lines.extend([
                format!("- Verified Hypotheses: {} / {}", coverage["verified_items"], coverage["applicable_items"]),
                format!(
                    "- Claimed Only: {}; Pending: {}; Failed: {}; Not Applicable: {}",
                    coverage["counts"]["claimed"],
                    coverage["counts"]["pending"],
                    coverage["counts"]["failed"],
                    coverage["counts"]["not_applicable"]
                ),
            ]);
            if coverage["low_denominator_warning"] == true {
                lines.push("- Low denominator notice: modeled hypothesis count is small; coverage is not a safety proof.".to_owned());
            }
            for blocker in coverage["open_blockers"].as_array().into_iter().flatten() {
                lines.push(format!(
                    "- Environment Blocker (excluded from findings): `{}` — {}",
                    prose(&blocker["category"]),
                    prose(&blocker["detail"])
                ));
            }
            for item in coverage["critical_unresolved_items"].as_array().into_iter().flatten() {
                lines.push(format!(
                    "- Unverified High-Priority Item: `{} {}` — {} (`{}`)",
                    prose(&item["method"]),
                    prose(&item["target"]),
                    prose(&item["hypothesis"]),
                    prose(&item["evidence_state"])
                ));
            }
        } else {
            lines.extend([
                format!("- 已验证测试：{} / {}", coverage["verified_items"], coverage["applicable_items"]),
                format!(
                    "- 仅声明：{}；未尝试：{}；失败：{}；不适用：{}",
                    coverage["counts"]["claimed"],
                    coverage["counts"]["pending"],
                    coverage["counts"]["failed"],
                    coverage["counts"]["not_applicable"]
                ),
            ]);
            if coverage["low_denominator_warning"] == true {
                lines.push("- 覆盖分母较小，百分比置信度低；覆盖不代表安全证明。".to_owned());
            }
            for blocker in coverage["open_blockers"].as_array().into_iter().flatten() {
                lines.push(format!(
                    "- 阻断（不计为漏洞）：{} — {}",
                    prose(&blocker["category"]),
                    prose(&blocker["detail"])
                ));
            }
            for item in coverage["critical_unresolved_items"].as_array().into_iter().flatten() {
                lines.push(format!(
                    "- 未验高优项：{} {} — {}（{}）",
                    prose(&item["method"]),
                    prose(&item["target"]),
                    prose(&item["hypothesis"]),
                    prose(&item["evidence_state"])
                ));
            }
        }
        lines.push(String::new());
    }

    for field in [
        "function",
        "mechanism",
        "normal_behavior",
        "actual_behavior",
        "confirmed_impact",
        "conditional_impact",
    ] {
        if present(&document[field]) {
            lines.extend([
                format!("## {}", label(mapping, field)),
                String::new(),
                prose(&document[field]),
                String::new(),
            ]);
        }
    }

    let steps_heading = if is_en || is_bi {
        format!("## {}", label(mapping, "steps"))
    } else {
        "## 逐步复现".to_owned()
    };
    lines.extend([steps_heading, String::new()]);
    for (index, step) in document["steps"].as_array().into_iter().flatten().enumerate() {
        lines.push(if is_en {
            format!("### Step {}", index + 1)
        } else if is_bi {
            format!("### 步骤 / Step {}", index + 1)
        } else {
            format!("### 步骤 {}", index + 1)
        });
        for field in [
            "action",
            "identity",
            "object_source",
            "input",
            "expected",
            "actual",
            "evidence",
        ] {
            if !step[field].is_null() {
                lines.push(format!("- {field}: {}", prose(&step[field])));
            }
        }
        lines.push(String::new());
    }

    if let Some(exchanges) = document["exchanges"].as_array().filter(|rows| !rows.is_empty()) {
        lines.extend([
            if is_en {
                "## Captured HTTP Wire Evidence".to_owned()
            } else if is_bi {
                "## 原始 HTTP 交换证据 / Captured HTTP Exchanges".to_owned()
            } else {
                "## 原始 HTTP 交换证据".to_owned()
            },
            String::new(),
        ]);
        for ex in exchanges {
            lines.push(format!(
                "- `{}` (`{}` · identity `{}`) `{} {}` -> HTTP `{}` (req `{}` / resp `{}`)",
                ex["exchange_id"].as_str().unwrap_or(""),
                ex["source_kind"].as_str().unwrap_or("http"),
                ex["context_id"].as_str().unwrap_or("default"),
                ex["method"].as_str().unwrap_or("GET"),
                ex["observed_url"].as_str().or_else(|| ex["url"].as_str()).unwrap_or(""),
                ex["status_code"],
                ex["request_raw_ref"]["artifact_id"].as_str().unwrap_or("-"),
                ex["response_raw_ref"]["artifact_id"].as_str().unwrap_or("-"),
            ));
        }
        lines.push(String::new());
    }

    for field in [
        "decisive_evidence",
        "cvss",
        "remediation",
        "counterevidence",
        "limitations",
        "restoration",
    ] {
        if present(&document[field]) {
            lines.extend([
                format!("## {}", label(mapping, field)),
                String::new(),
                prose(&document[field]),
                String::new(),
            ]);
        }
    }

    let poc = &document["poc"];
    let runnable = present(poc) && rules.is_empty();
    let mut issues = Vec::new();
    if present(poc) {
        lines.extend([
            if is_en {
                "## Reproducible PoC".to_owned()
            } else if is_bi {
                "## 可运行 PoC / Reproducible PoC".to_owned()
            } else {
                "## 可运行 PoC".to_owned()
            },
            String::new(),
        ]);
        for field in ["dependencies", "initialization", "success_assertion", "cleanup"] {
            if present(&poc[field]) {
                lines.push(format!("- {field}: {}", prose(&poc[field])));
            }
        }
        lines.push(format!(
            "- {}: {}",
            if is_en { "Verification" } else { "执行验证" },
            if present(&poc["verification"]) {
                prose(&poc["verification"])
            } else {
                "{\"status\":\"not_verified\"}".to_owned()
            }
        ));
        lines.push(String::new());
        if poc["content_ref"].is_object() {
            match verify_ref(s, task_id, &poc["content_ref"]) {
                Ok(content) => {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(content["data"].as_str().unwrap_or(""))
                        .map_err(super::db)?;
                    if let Ok(text) = String::from_utf8(bytes) {
                        let mut run = 0;
                        let mut longest = 0;
                        for character in text.chars() {
                            if character == '`' {
                                run += 1;
                                longest = longest.max(run);
                            } else {
                                run = 0;
                            }
                        }
                        let fence = "`".repeat(longest.max(2) + 1);
                        lines.extend([
                            format!("{fence}{}", poc["language"].as_str().unwrap_or("")),
                            text,
                            fence,
                            String::new(),
                        ]);
                    } else {
                        lines.push(if is_en {
                            "PoC is a binary artifact; refer to the exported bundle.".to_owned()
                        } else {
                            "PoC 为二进制文件，请使用包内附件，不在 Markdown 中转写。".to_owned()
                        });
                    }
                }
                Err(error) if body["allow_partial"] == true => {
                    issues.push(json!({"path":"poc/content","code":error.code,"reason":error.message}));
                }
                Err(error) => return Err(error),
            }
        }
        if present(&poc["command"]) {
            lines.extend([
                if is_en { "Command:".to_owned() } else { "执行命令：".to_owned() },
                String::new(),
                "```sh".to_owned(),
                prose(&poc["command"]),
                "```".to_owned(),
                String::new(),
            ]);
        }
    }

    lines.extend([
        if is_en {
            "## Evidence & Artifact Manifest".to_owned()
        } else if is_bi {
            "## 附件目录 / Artifact Manifest".to_owned()
        } else {
            "## 附件目录".to_owned()
        },
        String::new(),
    ]);
    for reference in report["evidence_refs"].as_array().into_iter().flatten() {
        lines.push(format!(
            "- `{}`: Artifact `{}`, {}, {} bytes",
            reference["name"]
                .as_str()
                .or_else(|| reference["artifact_id"].as_str())
                .unwrap_or(""),
            reference["artifact_id"].as_str().unwrap_or(""),
            reference["media_type"].as_str().unwrap_or(""),
            reference["size"]
        ));
    }
    for reference in document["attachments"].as_array().into_iter().flatten() {
        lines.push(format!("- {}", prose(reference)));
    }
    let missing = report["template_missing"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !missing.is_empty() {
        lines.extend([
            String::new(),
            if is_en {
                "## Missing Template Fields".to_owned()
            } else {
                "## 模板缺项".to_owned()
            },
            String::new(),
            missing.iter().map(prose).collect::<Vec<_>>().join(", "),
            String::new(),
        ]);
        issues.extend(missing.iter().map(|field| json!({"path":field,"code":"template_field_missing"})));
    }
    if !rules.is_empty() {
        lines.extend([
            String::new(),
            if is_en {
                "## Redaction Notice".to_owned()
            } else {
                "## 派生脱敏说明".to_owned()
            },
            String::new(),
            if is_en {
                "This derivative view applies explicit redactions; PoC commands may require credential substitution. Original frozen artifacts remain intact.".to_owned()
            } else {
                "本版本经过用户明确选择的脱敏；相关 PoC 可能不能直接运行。原始冻结证据未被覆盖。".to_owned()
            },
            String::new(),
        ]);
    }
    let mut markdown = lines.join("\n");
    for (needle, replacement) in &rules {
        markdown = markdown.replace(needle, replacement);
    }
    let mut canonical = json!({"schema":"muteki.src.submission.v1","report_id":report["report_id"],
        "report_seq":report["report_seq"],"language":lang,"document":document,"claims":claims,
        "review":review,"cvss":cvss,"deduplication":dedup,
        "effective_severity":severity,"coverage":coverage,
        "completion_status":report["completion_status"],"source_watermarks":report["source_watermarks"],
        "template":report["template"],"renderer_version":"src-submission-renderer.v2",
        "derived_from":report["report_id"],"redactions":[]});
    let mut auth_redact_count: u64 = 0;
    if body["redact_auth"] == true || !rules.is_empty() {
        let mut sensitive_values: Vec<String> = Vec::new();
        if let Some(exchanges) = canonical["document"]["exchanges"].as_array_mut() {
            for ex in exchanges {
                for hdr_key in ["request_headers", "response_headers"] {
                    if let Some(pairs) = ex[hdr_key].as_array_mut() {
                        for pair in pairs {
                            if let Some(arr) = pair.as_array_mut()
                                && arr.len() >= 2
                                && let Some(name) = arr[0].as_str()
                                && matches!(
                                    name.to_ascii_lowercase().as_str(),
                                    "authorization"
                                        | "proxy-authorization"
                                        | "cookie"
                                        | "cookie2"
                                        | "set-cookie"
                                        | "x-token"
                                        | "x-auth-token"
                                        | "x-api-key"
                                        | "satoken"
                                )
                            {
                                if let Some(val) = arr[1].as_str()
                                    && !val.is_empty()
                                    && val != "[REDACTED]"
                                    && val != "[已脱敏]"
                                {
                                    sensitive_values.push(val.to_owned());
                                }
                                arr[1] = json!("[REDACTED]");
                                auth_redact_count += 1;
                            }
                        }
                    }
                }
            }
        }
        for secret_val in sensitive_values {
            auth_redact_count += redact(&mut canonical, &secret_val, "[REDACTED]") as u64;
            markdown = markdown.replace(&secret_val, "[REDACTED]");
        }
    }
    for (index, (needle, replacement)) in rules.iter().enumerate() {
        let count = redact(&mut canonical, needle, replacement);
        redactions[index]["occurrences"] =
            json!(redactions[index]["occurrences"].as_u64().unwrap_or(0) + count as u64);
    }
    if auth_redact_count > 0 {
        redactions.push(json!({"rule_index": "auth_headers", "replacement": "[REDACTED]", "occurrences": auth_redact_count}));
    }
    canonical["redactions"] = json!(redactions);
    Ok(json!({"markdown":markdown,"canonical":canonical,"issues":issues,
        "runnable":runnable,"redactions":redactions}))
}
