//! Composer capability discovery and explicit reference resolution.
//!
//! The composer shows one engine-scoped view: `/` lists Muteki commands,
//! client commands, built-in capabilities, what the live session published, and
//! skills (matching `$` by default); `@` lists other conversations and
//! workspace files; `$` lists the engine's skills. The
//! browser only ever sends back opaque ids, and every selected reference is
//! resolved again against the current engine and workspace before a turn is
//! admitted.

use crate::{
    AppState, capabilities, chat, plugins,
    agent_runtime::{access, descriptors, host_login, pool},
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

const MAX_SKILL_FILE_BYTES: u64 = 256_000;
const MAX_CONTEXT_CHARS: usize = 180_000;
pub const MAX_SELECTED_REFS: usize = 12;
const MAX_THREAD_CONTEXT_CHARS: usize = 24_000;
const MAX_THREAD_CONTEXT_MESSAGES: usize = 24;
const MAX_SNAPSHOT_CHARS: usize = 4_000;
const MAX_SKILL_MANIFESTS: usize = 800;
const FILE_LIMIT: usize = 48;
const THREAD_LIMIT: usize = 12;
const FILE_SCAN_BUDGET: Duration = Duration::from_secs(3);
const GIT_LIST_TIMEOUT: Duration = Duration::from_secs(10);
const GIT_LIST_CACHE: Duration = Duration::from_secs(5);
const CONTEXT_SCHEMA_V2: u64 = 2;

const IGNORED_DIRS: &[&str] = &[
    ".git", ".hg", ".svn", ".next", ".turbo", ".cache", ".pytest_cache", ".mypy_cache",
    ".ruff_cache", "node_modules", "dist", "build", "coverage", "__pycache__", ".venv", "venv",
    "sessions",
];

/// Muteki's own `/` commands; a runtime command of the same name never shadows them.
const MUTEKI_COMMANDS: &[(&str, &str, &str)] = &[
    ("new", "新建对话", "new"),
    ("clear", "开始空白聊天，保留历史记录", "new"),
    ("clear-input", "清空输入内容与引用", "clear"),
    ("later", "定时发送消息：/later 5:10 … 或 /later 5min …", "later"),
];

/// Chips that keep catalog identity even when sent as schema-v2 nodes.
const STRIP_CATALOG_KINDS: &[&str] = &[
    "skill", "mcp", "plugin", "command", "thread", "run", "competition", "component", "council", "roster",
];

/// Bundled modular domain skills & capability rows.
const CONTROL_SOURCE: &str = "Muteki 内置能力";
const SECOPS_SKILL_NAME: &str = "muteki-secops";
const WORKBENCH_SKILL_NAME: &str = "muteki-workbench";
const SRC_SKILL_NAME: &str = "muteki-src";
const CONTROL_UNSUPPORTED_REASON: &str = "当前 Runtime 未接入 Muteki 能力 Gateway";
const CONTROL_UNSUPPORTED_ALTERNATIVE: &str =
    "请改用已接入 Gateway 的 Agent（如 Claude / Codex / Pi / Droid / OMP），或使用工作区内本地工具";
const SECOPS_SKILL_FILES: &[(&str, &str)] = &[
    ("SKILL.md", include_str!("../assets/skills/muteki-secops/SKILL.md")),
];
const WORKBENCH_SKILL_FILES: &[(&str, &str)] = &[
    ("SKILL.md", include_str!("../assets/skills/muteki-workbench/SKILL.md")),
];
const SRC_SKILL_FILES: &[(&str, &str)] = &[
    ("SKILL.md", include_str!("../assets/skills/muteki-src/SKILL.md")),
];
const BUNDLED_PLATFORM_SKILLS: &[(&str, &[(&str, &str)])] = &[
    (SECOPS_SKILL_NAME, SECOPS_SKILL_FILES),
    (WORKBENCH_SKILL_NAME, WORKBENCH_SKILL_FILES),
    (SRC_SKILL_NAME, SRC_SKILL_FILES),
];

fn receives_gateway(engine: &str) -> bool {
    descriptors::find(engine)
        .and_then(|descriptor| descriptor.adapter(""))
        .is_some_and(|adapter| adapter.capability_gateway)
}

/// Materialize a bundled progressive-disclosure skill under the state dir and
/// return its manifest path.
fn bundled_skill_manifest(s: &AppState, name: &str, files: &[(&str, &str)]) -> Option<PathBuf> {
    let root = PathBuf::from(s.store.get("system", "state_dir").ok().flatten()?.as_str()?.to_owned());
    let dir = root.join("skills").join(name);
    for (relative, content) in files {
        let path = dir.join(relative);
        if std::fs::read_to_string(&path).unwrap_or_default() != *content {
            std::fs::create_dir_all(path.parent()?).ok()?;
            std::fs::write(&path, content).ok()?;
        }
    }
    Some(dir.join("SKILL.md"))
}

/// All bundled platform progressive-disclosure skill rows for this engine.
fn bundled_platform_skills(s: &AppState, engine: &str, errors: &mut Vec<Value>) -> Vec<Skill> {
    BUNDLED_PLATFORM_SKILLS
        .iter()
        .filter_map(|(name, files)| {
            skill_item(&bundled_skill_manifest(s, name, files)?, engine, CONTROL_SOURCE, "thread", 600, errors)
        })
        .collect()
}

fn is_bundled_platform_skill(name: &str) -> bool {
    BUNDLED_PLATFORM_SKILLS
        .iter()
        .any(|(bundled, _)| name.eq_ignore_ascii_case(bundled))
}

/// Materialize bundled platform progressive-disclosure skills and return their
/// lightweight session skill-catalog pointers when `muteki-platform` is active.
pub(crate) fn platform_skill_catalog_entries(s: &AppState, engine: &str) -> Vec<Value> {
    if !control_enabled(s) || !receives_gateway(engine) {
        return Vec::new();
    }
    bundled_platform_skills(s, engine, &mut Vec::new())
        .into_iter()
        .map(|skill| {
            let description: String = text(&skill.row, "description").chars().take(600).collect();
            json!({
                "name": skill.row["name"],
                "description": description,
                "path": skill.path.to_string_lossy(),
            })
        })
        .collect()
}

fn modular_slash_row(
    id: &str,
    name: &str,
    description: &str,
    source: &str,
    engine: &str,
    available: bool,
    disabled_reason: &str,
    disabled_alternative: &str,
) -> Value {
    if available {
        json!({
            "id": id, "kind": "mcp", "name": name, "description": description,
            "source": source, "scope": "turn", "engine": engine,
            "channel": "session_config", "delivery": "guaranteed", "verification": "verified",
            "status": "available", "support_level": "supported", "reason": "", "alternative": "",
            "invocation": {"wire_text": format!("/{name}")},
            "invocable": true, "action": "select-capability",
        })
    } else {
        json!({
            "id": id, "kind": "mcp", "name": name, "description": description,
            "source": source, "scope": "turn", "engine": engine,
            "channel": "session_config", "delivery": "local", "verification": "verified",
            "status": "unavailable", "support_level": "unsupported",
            "reason": disabled_reason, "alternative": disabled_alternative,
            "invocation": {"wire_text": format!("/{name}")},
            "invocable": false, "action": "inspect-runtime-capability",
        })
    }
}

fn modular_slash_rows(s: &AppState, engine: &str, gateway: bool) -> Vec<Value> {
    let browser_on = capabilities::browser_enabled(s);
    let computer_on = capabilities::computer_enabled(s);
    let control_on = control_enabled(s);
    vec![
        modular_slash_row(
            "mcp:muteki-browser",
            "browser",
            "使用内置预览或无头浏览器打开、检查与截图网页 (muteki-browser)",
            "Muteki 内置能力",
            engine,
            gateway && browser_on,
            if !gateway { CONTROL_UNSUPPORTED_REASON } else { "内置浏览器能力已在设置中关闭" },
            if !gateway { CONTROL_UNSUPPORTED_ALTERNATIVE } else { "请在「设置 → 内置能力」中启用内置浏览器" },
        ),
        modular_slash_row(
            "mcp:muteki-computer",
            "computer",
            "操控本机原生桌面应用与界面截图验证 (muteki-computer / Computer Use)",
            "Muteki 内置能力",
            engine,
            gateway && computer_on,
            if !gateway { CONTROL_UNSUPPORTED_REASON } else { "本机电脑操控能力已在设置中关闭" },
            if !gateway { CONTROL_UNSUPPORTED_ALTERNATIVE } else { "请在「设置 → 内置能力」中启用本机电脑操控" },
        ),
        modular_slash_row(
            "mcp:muteki-platform:ctf",
            "ctf",
            "按需唤醒安全平台渐进式披露桥接，查询或调度 CTF 题目与 Run (muteki-platform)",
            "Muteki 安全平台",
            engine,
            gateway && control_on,
            if !gateway { CONTROL_UNSUPPORTED_REASON } else { "平台控制扩展已停用" },
            if !gateway { CONTROL_UNSUPPORTED_ALTERNATIVE } else { "请在「设置 → 内置能力」检查安全平台桥接状态" },
        ),
        modular_slash_row(
            "mcp:muteki-platform:pentest",
            "pentest",
            "按需唤醒安全平台渐进式披露桥接，登记或跟进授权渗透目标 (muteki-platform)",
            "Muteki 安全平台",
            engine,
            gateway && control_on,
            if !gateway { CONTROL_UNSUPPORTED_REASON } else { "平台控制扩展已停用" },
            if !gateway { CONTROL_UNSUPPORTED_ALTERNATIVE } else { "请在「设置 → 内置能力」检查安全平台桥接状态" },
        ),
        modular_slash_row(
            "mcp:muteki-platform:workbench",
            "workbench",
            "按需唤醒工作台渐进式披露桥接，控制模型/端点/Ollama刷新/配额/Runtime/插件/工作区与运维 (muteki-platform)",
            "Muteki 工作台",
            engine,
            gateway && control_on,
            if !gateway { CONTROL_UNSUPPORTED_REASON } else { "平台控制扩展已停用" },
            if !gateway { CONTROL_UNSUPPORTED_ALTERNATIVE } else { "请在「设置 → 内置能力」检查安全平台与工作台桥接状态" },
        ),
    ]
}

pub(crate) struct RunReference {
    pub text: String,
    pub selected: Value,
}

/// The platform's description of a Run for a model, with how to reach it through tools.
pub(crate) fn run_reference(s: &AppState, run_id: &str) -> Option<RunReference> {
    let raw_run = (!run_id.is_empty())
        .then(|| s.store.get("runs", run_id).ok().flatten())
        .flatten()?;
    let run = crate::runs::project_run_summary(s, raw_run);
    let title = run_title_of(s, &run);
    let category = Some(text(&run, "category")).filter(|c| !c.is_empty()).unwrap_or("misc");
    let mode = Some(text(&run, "mode")).filter(|m| !m.is_empty()).unwrap_or("ctf");
    let status = if run["solved"] == true {
        "solved"
    } else if run["running"] == true {
        "running"
    } else {
        Some(text(&run, "status")).filter(|st| !st.is_empty()).unwrap_or("finished")
    };
    let task_id = bound_task_id_of(s, &run);
    let flag_count = run["flags"].as_array().map(Vec::len).unwrap_or(0);
    let redacted_title = crate::control_content::redact(&title).0;
    Some(RunReference {
        text: format!(
            "[显式引用的平台运行实例: {redacted_title} (run_id={run_id})]\n\
            模式: {mode} | 分类: {category} | 状态: {status} | 已捕获候选 Flag 数: {flag_count}{}\n\
            如需读取该 Run 的实时进度快照、事件流、SharedGraph 或发送 Operator 指令，请通过 `muteki-platform` 先调用 `muteki_platform_discover`（`tools: [\"muteki_get_run_snapshot\", \"muteki_read_shared_graph\", \"muteki_read_run_events\"]`），再调用 `muteki_platform_call`。",
            if task_id.is_empty() { String::new() } else { format!(" | 关联 task_id: {task_id}") }
        ),
        selected: json!({
            "id": format!("run:{run_id}"),
            "kind": "run",
            "name": redacted_title,
            "description": format!("Run · {category} · {status} ({run_id})"),
            "source": "Muteki 安全平台",
            "scope": "platform",
            "run_id": run_id,
        }),
    })
}

fn bound_task_id_of(s: &AppState, run: &Value) -> String {
    let direct = text(run, "task_id");
    if !direct.is_empty() {
        return direct.to_owned();
    }
    let run_id = text(run, "run_id");
    if run_id.is_empty() {
        return String::new();
    }
    let Some(owner) = s.store.get("bound_run_ids", run_id).ok().flatten() else {
        return String::new();
    };
    let binding_key = text(&owner, "binding_key");
    if binding_key.is_empty() {
        return String::new();
    }
    if let Some(bound) = s.store.get("bound_runs", binding_key).ok().flatten() {
        let tid = text(&bound, "task_id");
        if !tid.is_empty() {
            return tid.to_owned();
        }
    }
    if s.store.get("platform_tasks", binding_key).ok().flatten().is_some() {
        return binding_key.to_owned();
    }
    String::new()
}

fn run_title_of(s: &AppState, run: &Value) -> String {
    let run_id = text(run, "run_id");
    for key in ["challenge_name", "title", "custom_name", "operator_label", "name"] {
        if let Some(val) = run[key]
            .as_str()
            .map(str::trim)
            .filter(|v| !v.is_empty() && *v != run_id)
        {
            return val.to_owned();
        }
    }
    if let Some(val) = run["challenge"]["title"]
        .as_str()
        .or_else(|| run["challenge"]["name"].as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != run_id)
    {
        return val.to_owned();
    }
    let task_id = bound_task_id_of(s, run);
    if !task_id.is_empty()
        && let Some(task) = s.store.get("platform_tasks", &task_id).ok().flatten()
        && let Some(val) = task["title"]
            .as_str()
            .or_else(|| task["input"]["title"].as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    {
        return val.to_owned();
    }
    run_id.to_owned()
}

fn run_mention_rows(s: &AppState, limit: usize) -> Vec<Value> {
    let Ok(runs) = crate::runs::list_runs(s, false) else {
        return Vec::new();
    };
    runs.into_iter()
        .take(limit)
        .filter_map(|run| {
            let run_id = text(&run, "run_id");
            if run_id.is_empty() {
                return None;
            }
            let title = run_title_of(s, &run);
            let category = Some(text(&run, "category")).filter(|c| !c.is_empty()).unwrap_or("misc");
            let status = if run["solved"] == true {
                "已解出"
            } else if run["running"] == true {
                "运行中"
            } else {
                Some(text(&run, "status")).filter(|st| !st.is_empty()).unwrap_or("已结束")
            };
            Some(augment_row(json!({
                "id": format!("run:{run_id}"),
                "kind": "run",
                "name": title,
                "description": format!("Run · {category} · {status} ({run_id})"),
                "source": "Muteki 安全平台",
                "scope": "platform",
                "run_id": run_id,
            })))
        })
        .collect()
}

fn competition_mention_rows(s: &AppState, limit: usize) -> Vec<Value> {
    let Ok(records) = s.store.list("competitions") else {
        return Vec::new();
    };
    records
        .into_iter()
        .filter(|row| row["archived"] != true)
        .take(limit)
        .filter_map(|row| {
            let comp_id = Some(text(&row, "competition_id"))
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| text(&row, "id"));
            if comp_id.is_empty() {
                return None;
            }
            let name = Some(text(&row, "name"))
                .filter(|v| !v.is_empty())
                .or_else(|| Some(text(&row, "title")).filter(|v| !v.is_empty()))
                .unwrap_or(comp_id);
            let status = Some(text(&row, "status")).filter(|v| !v.is_empty()).unwrap_or("已登记");
            Some(augment_row(json!({
                "id": format!("competition:{comp_id}"),
                "kind": "competition",
                "name": name,
                "description": format!("比赛 · {status} ({comp_id})"),
                "source": "Muteki 安全平台",
                "scope": "platform",
                "competition_id": comp_id,
            })))
        })
        .collect()
}

/// One @-menu row per chat package enabled for the engine.
fn managed_plugin_rows(s: &AppState, engine: &str) -> Vec<Value> {
    plugins::registry(s)
        .and_then(|registry| registry.enabled(engine, "chat"))
        .map(|records| {
            records
                .iter()
                .map(|record| {
                    json!({
                        "id": format!("managed-plugin:{engine}:{}", record["id"].as_str().unwrap_or("")),
                        "kind": "plugin", "name": record["name"], "description": record["description"],
                        "source": plugins::MANAGED_SOURCE, "scope": "chat", "action": "inspect-runtime-status",
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The invocable managed skills of the packages enabled for `engine`.
fn managed_skills(s: &AppState, engine: &str) -> Vec<plugins::ManagedSkill> {
    plugins::registry(s).and_then(|registry| registry.skill_rows(engine, false)).unwrap_or_default()
}

/// The chat plugin page's own switch for the control tools; defaults on.
fn control_enabled(s: &AppState) -> bool {
    plugins::registry(s).map(|registry| registry.control_enabled()).unwrap_or(true)
}

/// `$ARGUMENTS`, `$ARGUMENTS[i]` and `$i` in a managed skill's body.
fn substitute_arguments(body: &str, arguments: &str) -> String {
    if !body.contains('$') {
        return body.to_owned();
    }
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| regex::Regex::new(r"\$ARGUMENTS\[(\d+)\]|\$(\d+)\b|\$ARGUMENTS\b").unwrap());
    let positional = split_arguments(arguments);
    pattern
        .replace_all(body, |caps: &regex::Captures| {
            if &caps[0] == "$ARGUMENTS" {
                return arguments.to_owned();
            }
            let index: usize = caps
                .get(1)
                .or_else(|| caps.get(2))
                .and_then(|group| group.as_str().parse().ok())
                .unwrap_or(usize::MAX);
            positional.get(index).cloned().unwrap_or_else(|| caps[0].to_owned())
        })
        .into_owned()
}

/// shlex-style splitting: quotes group words, backslash escapes outside them.
fn split_arguments(arguments: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut active = false;
    let mut chars = arguments.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, '"' | '\'') => {
                quote = Some(c);
                active = true;
            }
            (Some(open), c) if c == open => quote = None,
            (None, c) if c.is_whitespace() => {
                if active {
                    out.push(std::mem::take(&mut current));
                    active = false;
                }
            }
            (None, '\\') => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
                active = true;
            }
            (_, c) => {
                current.push(c);
                active = true;
            }
        }
    }
    if active {
        out.push(current);
    }
    out
}

fn client_description(action: &str) -> &'static str {
    match action {
        "skills" => "查看当前引擎的 Skills",
        "plugins" => "管理 Muteki 聊天扩展",
        "mcp" => "查看当前引擎的 MCP",
        "help" => "查看当前引擎的全部聊天命令",
        "model" => "选择当前聊天模型",
        "effort" => "调整当前聊天的思考强度",
        "permissions" => "调整当前聊天的权限模式",
        "rename" => "重命名当前聊天",
        "sessions" => "查找并切换 Muteki 中的聊天",
        "fork" => "从当前聊天分叉，保留原记录",
        "side" => "在侧边聊天中提问，继承当前上下文且不打扰主对话",
        "export" => "导出当前聊天",
        "status" => "查看当前聊天状态与用量",
        "copy" => "复制最后一条助手回复",
        "rewind" => "选择需要回退的轮次",
        "diff" => "查看当前聊天的文件变更",
        "details" => "查看当前聊天的执行详情",
        "new" => "开始空白聊天，保留历史记录",
        _ => "Muteki 聊天命令",
    }
}

fn project_skill_roots(engine: &str) -> &'static [&'static str] {
    match engine {
        "claude" => &[".claude/skills", ".agents/skills"],
        "codex" => &[".agents/skills", ".codex/skills"],
        "cursor" => &[".cursor/skills", ".agents/skills"],
        "pi" => &[".pi/skills", ".agents/skills"],
        "omp" => &[".omp/skills", ".agents/skills"],
        "kimi" => &[".kimi/skills", ".agents/skills"],
        "grok" => &[".grok/skills", ".agents/skills"],
        "opencode" => &[".opencode/skills", ".agents/skills"],
        "agy" => &[".gemini/config/skills", ".agent/skills", ".agents/skills"],
        _ => &[".agents/skills"],
    }
}

fn host_discovery() -> bool {
    std::env::var("MUTEKI_HOST_DISCOVERY").map_or(true, |value| value != "0")
}

pub fn opaque_id(kind: &str, identity: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(identity.as_bytes()));
    format!("{kind}:{}", &digest[..24])
}

fn chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn cap_snapshot(text: &str) -> String {
    if text.chars().count() <= MAX_SNAPSHOT_CHARS {
        text.to_owned()
    } else {
        format!("{}\n…", chars(text, MAX_SNAPSHOT_CHARS))
    }
}

fn section_failure(errors: &mut Vec<Value>, section: &str, message: impl std::fmt::Display, kind: &str) {
    errors.push(json!({"section":section,"code":"conversation.composer.section_failed","message":message.to_string(),"exception_type":kind}));
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().map(str::trim).unwrap_or("")
}


fn unquote(raw: &str) -> Value {
    let value = raw.trim();
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        return serde_json::from_str::<String>(value)
            .map(Value::String)
            .unwrap_or_else(|_| json!(value[1..value.len() - 1]));
    }
    if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        return json!(value[1..value.len() - 1].replace("''", "'"));
    }
    match value {
        "true" | "True" | "yes" => json!(true),
        "false" | "False" | "no" => json!(false),
        "" | "~" | "null" => Value::Null,
        other => json!(other),
    }
}

/// The scalar top-level keys of a SKILL.md frontmatter block, and the body.
/// Nested maps and lists are not needed by the catalog and are skipped.
fn frontmatter(raw: &str) -> (Map<String, Value>, String) {
    let lines: Vec<&str> = raw.lines().collect();
    if !raw.starts_with("---") {
        return (Map::new(), raw.to_owned());
    }
    let Some(end) = (1..lines.len()).find(|&i| lines[i].trim() == "---") else {
        return (Map::new(), raw.to_owned());
    };
    let mut meta = Map::new();
    let mut i = 1;
    while i < end {
        let line = lines[i];
        i += 1;
        if line.starts_with([' ', '\t', '#', '-']) || line.trim().is_empty() {
            continue;
        }
        let Some((key, rest)) = line.split_once(':') else { continue };
        let key = key.trim().to_owned();
        let rest = rest.trim();
        if let Some(style) = rest.chars().next().filter(|c| *c == '|' || *c == '>') {
            let mut block = Vec::new();
            while i < end && (lines[i].starts_with([' ', '\t']) || lines[i].trim().is_empty()) {
                block.push(lines[i].trim());
                i += 1;
            }
            let joined = if style == '|' { block.join("\n") } else { block.join(" ") };
            meta.insert(key, json!(joined.trim()));
        } else {
            meta.insert(key, unquote(rest));
        }
    }
    (meta, lines[end + 1..].join("\n"))
}

#[derive(Clone)]
struct Skill {
    row: Value,
    path: PathBuf,
    priority: i32,
}

fn skill_item(path: &Path, engine: &str, source: &str, scope: &str, priority: i32, errors: &mut Vec<Value>) -> Option<Skill> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_SKILL_FILE_BYTES {
        return None;
    }
    let resolved = std::fs::canonicalize(path).ok()?;
    let raw = match std::fs::read_to_string(&resolved) {
        Ok(raw) => raw,
        Err(error) => {
            section_failure(errors, "skills", error, "OSError");
            return None;
        }
    };
    let (meta, body) = frontmatter(&raw);
    for key in ["enabled", "user-invocable", "user_invocable"] {
        if meta.get(key) == Some(&json!(false)) {
            return None;
        }
    }
    let name = meta
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .or_else(|| resolved.parent()?.file_name().map(|n| n.to_string_lossy().into_owned()))?;
    let mut description = meta
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_owned();
    if description.is_empty() {
        let first = body
            .lines()
            .map(|line| line.trim_matches([' ', '#', '\t']))
            .find(|line| !line.is_empty())
            .unwrap_or("");
        description = if first.is_empty() { "Agent Skill".into() } else { chars(first, 180) };
    }
    Some(Skill {
        row: json!({
            "id": opaque_id("skill", &format!("{engine}\0{}", resolved.display())),
            "kind": "skill", "name": name, "description": chars(&description, 240),
            "source": source, "scope": scope, "engine": engine,
        }),
        path: resolved,
        priority,
    })
}

fn skill_roots(engine: &str, workspace_root: &str) -> Vec<(PathBuf, String, &'static str, i32)> {
    let mut roots = Vec::new();
    if !workspace_root.is_empty() {
        let workspace = PathBuf::from(workspace_root);
        for relative in project_skill_roots(engine) {
            let common = *relative == ".agents/skills";
            roots.push((workspace.join(relative), "当前项目".to_owned(), "project", if common { 350 } else { 400 }));
        }
    }
    if host_discovery()
        && let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && let Some(descriptor) = descriptors::find(engine)
    {
        let environment = &descriptor.public()["environment"];
        let home_relative = text(environment, "home_relative");
        for relative in environment["user_skill_roots"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            let common = relative == ".agents/skills";
            let path = match (relative.strip_prefix(home_relative).and_then(|r| r.strip_prefix('/')), host_login::login_root(engine)) {
                (Some(rest), Some(root)) if !home_relative.is_empty() => root.join(rest),
                _ => home.join(relative),
            };
            let source = if common { "通用 Agent".to_owned() } else { engine.to_owned() };
            roots.push((path, source, "personal", if common { 250 } else { 300 }));
        }
    }
    roots
}

fn manifests(root: &Path, limit: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries.into_iter().rev() {
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                stack.push(entry.path());
            } else if entry.file_name() == "SKILL.md" {
                found.push(entry.path());
            }
        }
    }
    found.sort();
    found.truncate(limit);
    found
}

fn discover_skills(engine: &str, workspace_root: &str, errors: &mut Vec<Value>) -> Vec<Skill> {
    if descriptors::find(engine).is_none() {
        return Vec::new();
    }
    let mut deduped: HashMap<String, Skill> = HashMap::new();
    let mut visited = HashSet::new();
    for (root, source, scope, priority) in skill_roots(engine, workspace_root) {
        let Ok(resolved) = std::fs::canonicalize(&root) else { continue };
        if !resolved.is_dir() || !visited.insert(resolved.clone()) {
            continue;
        }
        for manifest in manifests(&resolved, MAX_SKILL_MANIFESTS) {
            let Some(item) = skill_item(&manifest, engine, &source, scope, priority, errors) else { continue };
            let key = item.row["name"].as_str().unwrap_or("").to_lowercase();
            if deduped.get(&key).is_none_or(|previous| item.priority > previous.priority) {
                deduped.insert(key, item);
            }
        }
    }
    let mut skills: Vec<Skill> = deduped.into_values().collect();
    skills.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| text(&a.row, "name").to_lowercase().cmp(&text(&b.row, "name").to_lowercase()))
    });
    skills
}

/// The engine's own skills outside any workspace, as public catalog rows.
pub fn native_skill_rows(engine: &str) -> Vec<Value> {
    discover_skills(engine, "", &mut Vec::new()).into_iter().map(|skill| skill.row).collect()
}

/// Names and manifest paths of the skills `engine` discovers for a workspace.
pub fn native_skill_paths(engine: &str, workspace_root: &str) -> impl Iterator<Item = (String, PathBuf)> {
    discover_skills(engine, workspace_root, &mut Vec::new())
        .into_iter()
        .map(|skill| (text(&skill.row, "name").to_owned(), skill.path))
}


type GitListing = (Instant, Option<Vec<String>>);

fn git_cache() -> &'static Mutex<HashMap<String, GitListing>> {
    static CACHE: OnceLock<Mutex<HashMap<String, GitListing>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Tracked and untracked-not-ignored paths, or `None` outside a Git work tree.
fn git_listed_files(root: &Path) -> Result<Option<Vec<String>>, String> {
    let key = root.to_string_lossy().into_owned();
    if let Some((at, listed)) = git_cache().lock().unwrap().get(&key)
        && at.elapsed() < GIT_LIST_CACHE
    {
        return Ok(listed.clone());
    }
    let mut child = match crate::host_process::std_command("git")
        .args(["-C", &key, "ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        std::io::Read::read_to_end(&mut stdout, &mut buffer).map(|_| buffer)
    });
    let deadline = Instant::now() + GIT_LIST_TIMEOUT;
    let status = loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) => break status,
            None if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("git ls-files 超时".into());
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let output = reader.join().map_err(|_| "git ls-files reader panicked".to_string())?.map_err(|e| e.to_string())?;
    let listed = status.success().then(|| {
        let mut rows: Vec<String> = output
            .split(|byte| *byte == 0)
            .filter(|item| !item.is_empty())
            .map(|item| String::from_utf8_lossy(item).into_owned())
            .collect();
        rows.sort();
        rows.dedup();
        rows
    });
    git_cache().lock().unwrap().insert(key, (Instant::now(), listed.clone()));
    Ok(listed)
}

fn file_id(s: &AppState, root: &Path, relative: &str) -> Result<String, String> {
    let key = format!("{}\0{relative}", root.display());
    let existing = s.store.get("composer_file_ids", &key).map_err(|error| error.to_string())?;
    if let Some(id) = existing.as_ref().and_then(|row| row["id"].as_str()) {
        return Ok(id.to_owned());
    }
    let id = format!("file:{}", uuid::Uuid::new_v4());
    let value = json!({"id":id.clone()});
    if s.store.put_many_if(&[("composer_file_ids", &key, existing.as_ref())], &[("composer_file_ids", &key, &value)])
        .map_err(|error| error.to_string())?
    {
        return Ok(id);
    }
    s.store.get("composer_file_ids", &key)
        .map_err(|error| error.to_string())?
        .and_then(|row| row["id"].as_str().map(str::to_owned))
        .ok_or_else(|| "文件引用 ID 不可用".to_owned())
}

fn file_row(s: &AppState, root: &Path, relative: &str) -> Result<Value, String> {
    Ok(json!({
        "id": file_id(s, root, relative)?,
        "kind": "file", "name": relative.rsplit('/').next().unwrap_or(relative),
        "description": relative, "source": "当前项目", "scope": "project",
    }))
}

fn plain_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file())
}

fn discover_files(s: &AppState, workspace_root: &str, query: &str, limit: usize, errors: &mut Vec<Value>) -> Vec<Value> {
    if workspace_root.is_empty() {
        return Vec::new();
    }
    let root = match std::fs::canonicalize(workspace_root) {
        Ok(root) if root.is_dir() => root,
        Ok(_) => return Vec::new(),
        Err(error) => {
            section_failure(errors, "files", error, "OSError");
            return Vec::new();
        }
    };
    let needle = query.trim().to_lowercase();
    let mut rows = Vec::new();
    let listed = git_listed_files(&root).unwrap_or_else(|error| {
        section_failure(errors, "files", error, "SubprocessError");
        None
    });
    if let Some(listed) = listed {
        for relative in listed {
            if !needle.is_empty() && !relative.to_lowercase().contains(&needle) {
                continue;
            }
            if !plain_file(&root.join(&relative)) {
                continue;
            }
            match file_row(s, &root, &relative) {
                Ok(row) => rows.push(row),
                Err(error) => section_failure(errors, "files", error, "StoreError"),
            }
            if rows.len() >= limit {
                break;
            }
        }
        return rows;
    }
    let deadline = Instant::now() + FILE_SCAN_BUDGET;
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        if Instant::now() > deadline {
            errors.push(json!({"section":"files","code":"conversation.composer.files_scan_incomplete","message":format!("目录较大，文件搜索在 {} 秒内未扫描完，结果不完整；请输入更具体的文件名，或将该目录初始化为 Git 仓库。", FILE_SCAN_BUDGET.as_secs()),"exception_type":"TimeoutError"}));
            return rows;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) => {
                section_failure(errors, "files", error, "OSError");
                continue;
            }
        };
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            if kind.is_dir() && !IGNORED_DIRS.contains(&name.as_str()) {
                dirs.push(entry.path());
            } else if kind.is_file() {
                files.push(entry.path());
            }
        }
        files.sort();
        for path in files {
            let Ok(relative) = path.strip_prefix(&root) else { continue };
            let relative = relative.to_string_lossy().replace('\\', "/");
            if !needle.is_empty() && !relative.to_lowercase().contains(&needle) {
                continue;
            }
            match file_row(s, &root, &relative) {
                Ok(row) => rows.push(row),
                Err(error) => section_failure(errors, "files", error, "StoreError"),
            }
            if rows.len() >= limit {
                return rows;
            }
        }
        dirs.sort();
        stack.extend(dirs.into_iter().rev());
    }
    rows
}


struct ThreadRef {
    row: Value,
    thread_id: String,
}

fn thread_items(threads: &[Value], current: &str, limit: Option<usize>) -> Vec<ThreadRef> {
    let mut ordered: Vec<&Value> = threads.iter().map(|doc| &doc["thread"]).collect();
    ordered.sort_by(|a, b| text(b, "updated_at").cmp(text(a, "updated_at")));
    let mut rows = Vec::new();
    for thread in ordered {
        let thread_id = text(thread, "thread_id");
        if thread_id.is_empty() || thread_id == current {
            continue;
        }
        let title = Some(text(thread, "title")).filter(|t| !t.is_empty()).unwrap_or("新对话");
        let summary = text(thread, "summary");
        rows.push(ThreadRef {
            row: json!({
                "id": opaque_id("thread", thread_id), "kind": "thread", "name": title,
                "description": if summary.is_empty() { "引用这段对话的上下文".to_owned() } else { chars(summary, 240) },
                "source": "对话", "scope": "conversation",
            }),
            thread_id: thread_id.to_owned(),
        });
        if limit.is_some_and(|limit| rows.len() >= limit) {
            break;
        }
    }
    rows
}

fn thread_summaries(s: &AppState, project_id: &str, errors: &mut Vec<Value>) -> Vec<Value> {
    match s.store.conversation_summaries() {
        Ok(rows) => rows
            .into_iter()
            .filter(|doc| {
                !crate::chat::is_side_chat(&doc["thread"])
                    && !crate::chat::is_ctf_thread(&doc["thread"])
            })
            .filter(|doc| doc["thread"]["archived"] != true)
            .filter(|doc| project_id.is_empty() || doc["thread"]["project_id"] == project_id)
            .collect(),
        Err(error) => {
            section_failure(errors, "threads", error, "DatabaseError");
            Vec::new()
        }
    }
}


/// What the live engine session published for this thread, if it is the
/// runtime the composer asked about.
pub struct RuntimeSnapshot<'a> {
    pub adapter_id: String,
    pub commands: &'a [Value],
    pub native_capabilities: &'a Value,
    pub stale: bool,
    pub revision: u64,
}

fn runtime_rows(engine: &str, snapshot: &RuntimeSnapshot<'_>, local_skills: &HashMap<String, Skill>) -> Vec<Value> {
    let mut rows = Vec::new();
    for command in snapshot.commands {
        let name = text(command, "name").trim_start_matches(['/', '$']);
        if name.is_empty() {
            continue;
        }
        let reported_skill = command["kind"] == "skill";
        let local = local_skills.get(&name.to_lowercase());
        let kind = if reported_skill || local.is_some() { "skill" } else { "command" };
        let wire = if reported_skill { format!("${name}") } else { format!("/{name}") };
        if wire.starts_with('/') && MUTEKI_COMMANDS.iter().any(|(own, _, _)| own.eq_ignore_ascii_case(name)) {
            continue;
        }
        let display = match local {
            Some(skill) if kind == "skill" => text(&skill.row, "name").to_owned(),
            _ => name.to_owned(),
        };
        let description = Some(text(command, "description"))
            .filter(|d| !d.is_empty())
            .map(str::to_owned)
            .or_else(|| local.map(|skill| text(&skill.row, "description").to_owned()))
            .unwrap_or_else(|| "Runtime 当前公布的能力".into());
        let (support, invocable, reason, alternative) = if snapshot.stale {
            ("expired", false, "会话能力目录已过期，正在刷新", "请等待刷新完成后再调用该命令")
        } else {
            ("supported", true, "", "")
        };
        rows.push(json!({
            "id": format!("runtime:{}:{}:{name}", snapshot.adapter_id, if reported_skill { "skill" } else { "command" }),
            "kind": kind, "name": display, "description": description,
            "source": snapshot.adapter_id, "scope": "session", "engine": engine,
            "action": if invocable { "insert-runtime-invocation" } else { "inspect-runtime-capability" },
            "channel": "provider_native", "origin": "runtime_reported", "delivery": "guaranteed",
            "verification": "verified", "status": if snapshot.stale { "unavailable" } else { "available" },
            "argument_hint": "", "invocation": {"wire_text": wire},
            "support_level": support, "reason": reason, "alternative": alternative,
            "invocable": invocable, "revision": snapshot.revision,
        }));
    }
    for name in crate::native_chat::operation_names(&snapshot.adapter_id){
        if snapshot.adapter_id=="codex.app_server" && !snapshot.native_capabilities["operations"].as_array().into_iter().flatten().any(|v|v==name){continue;}
        if rows.iter().any(|row|row["name"]==*name){continue;}
        let mut row=compact_operation(engine,&snapshot.adapter_id);row["id"]=json!(format!("runtime:{}:operation:{name}",snapshot.adapter_id));row["name"]=json!(name);row["description"]=json!(format!("原生 /{name} 操作"));row["invocation"]=json!({"wire_text":format!("/{name}")});row["revision"]=json!(snapshot.revision);row["invocable"]=json!(!snapshot.stale);rows.push(row);
    }
    rows
}

fn client_rows(engine: &str, native_names: &HashSet<String>) -> Vec<Value> {
    let Some(descriptor) = descriptors::find(engine) else { return Vec::new() };
    let aliases = &descriptor.public()["commands"]["client_aliases"];
    let mut rows = Vec::new();
    for (name, action) in aliases.as_object().into_iter().flatten() {
        let action = action.as_str().unwrap_or("");
        if native_names.contains(name) && matches!(action, "help" | "status" | "skills" | "plugins" | "mcp") {
            continue;
        }
        let hint = match action {
            "model" => "[模型]",
            "rename" => "[名称]",
            "effort" => "[强度]",
            _ => "",
        };
        rows.push(json!({
            "id": format!("client:{engine}:{name}"), "kind": "command", "name": name,
            "description": client_description(action), "source": "Muteki 聊天",
            "scope": "conversation", "engine": engine, "channel": "local_handler",
            "action": format!("ui:{action}"), "invocable": true, "support_level": "supported",
            "argument_hint": hint,
        }));
    }
    for name in ["side", "btw"] {
        if native_names.contains(name) || rows.iter().any(|row| row["name"] == name) {
            continue;
        }
        rows.push(json!({
            "id": format!("client:{engine}:{name}"), "kind": "command", "name": name,
            "description": client_description("side"), "source": "Muteki 聊天",
            "scope": "conversation", "engine": engine, "channel": "local_handler",
            "action": "ui:side", "invocable": true, "support_level": "supported",
            "argument_hint": "[问题]",
        }));
    }
    rows
}

fn unavailable_rows(engine: &str, names: &HashSet<String>) -> Vec<Value> {
    let Some(descriptor) = descriptors::find(engine) else { return Vec::new() };
    descriptor.public()["commands"]["terminal_only"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|name| !names.contains(*name))
        .map(|name| json!({
            "id": format!("unavailable:{engine}:{name}"), "kind": "command", "name": name,
            "description": "当前聊天协议未提供此操作", "source": engine,
            "scope": "engine", "engine": engine, "action": "inspect-runtime-capability",
            "channel": "local_handler", "invocable": false, "support_level": "unsupported",
            "reason": "仅终端或原客户端可用，当前协议未公布可执行接口",
            "alternative": "请在原客户端使用；不会把该命令当作普通消息发送给模型",
        }))
        .collect()
}

fn compact_operation(engine: &str, adapter_id: &str) -> Value {
    json!({
        "id": format!("runtime:{adapter_id}:operation:compact"), "kind": "command", "name": "compact",
        "description": "压缩当前会话上下文", "source": adapter_id, "scope": "session", "engine": engine,
        "action": "insert-runtime-invocation", "channel": "provider_native", "origin": "runtime_operation",
        "delivery": "guaranteed", "verification": "verified", "status": "available",
        "argument_hint": "", "invocation": {"wire_text": "/compact"},
        "support_level": "supported", "reason": "", "alternative": "", "invocable": true,
    })
}

fn unavailable_compact(engine: &str, adapter_id: &str) -> Value {
    json!({
        "id": format!("unavailable:{engine}:compact"), "kind": "command", "name": "compact",
        "description": "压缩当前会话上下文", "source": adapter_id,
        "scope": "engine", "engine": engine, "action": "inspect-runtime-capability",
        "channel": "local_handler", "invocable": false, "support_level": "unsupported",
        "reason": format!("{adapter_id} 当前会话没有公布压缩命令或接口，平台无法触发上下文压缩"),
        "alternative": "用 /new 开始新聊天，或切换到支持压缩的接入；不会把 /compact 当作普通消息发送给模型",
    })
}

/// The fields that mark a catalog row as a selectable, verified capability.
fn augment_row(mut row: Value) -> Value {
    for (key, value) in [
        ("action", "select-capability"), ("channel", "session_config"), ("origin", "verified_static"),
        ("delivery", "guaranteed"), ("verification", "verified"), ("status", "available"),
        ("support_level", "supported"), ("reason", ""), ("alternative", ""),
    ] {
        row[key] = json!(value);
    }
    row["invocable"] = json!(true);
    row
}

fn local_skill_row(skill: &Skill) -> Value {
    augment_row(skill.row.clone())
}

fn matches(item: &Value, query: &str) -> bool {
    let needle = query.trim().to_lowercase();
    needle.is_empty()
        || ["name", "description", "source", "scope"]
            .iter()
            .map(|key| text(item, key))
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
            .contains(&needle)
}

/// One row per typed invocation; the first source wins unless only a later one can run.
fn dedupe_invocations(rows: Vec<Value>) -> Vec<Value> {
    let mut kept: HashMap<(String, String), usize> = HashMap::new();
    let mut out: Vec<Value> = Vec::new();
    for row in rows {
        let wire = row["invocation"]["wire_text"].as_str().unwrap_or("");
        let label = if wire.is_empty() { text(&row, "name") } else { wire.trim() }
            .trim_start_matches(['/', '$'])
            .to_lowercase();
        if label.is_empty() {
            out.push(row);
            continue;
        }
        let key = (text(&row, "kind").to_owned(), label);
        match kept.get(&key) {
            None => {
                kept.insert(key, out.len());
                out.push(row);
            }
            Some(&index) if out[index]["invocable"] == false && row["invocable"] != false => out[index] = row,
            Some(_) => {}
        }
    }
    out
}

pub struct CatalogRequest<'a> {
    pub engine: &'a str,
    pub workspace_root: &'a str,
    pub trigger: &'a str,
    pub query: &'a str,
    pub project_id: &'a str,
    pub current_thread_id: &'a str,
    pub snapshot: Option<RuntimeSnapshot<'a>>,
}

/// The engine-scoped menu for one trigger. Filesystem walks run here, so
/// callers keep this off the async executor.
pub fn catalog(s: &AppState, request: &CatalogRequest<'_>, errors: &mut Vec<Value>) -> Vec<Value> {
    let engine = request.engine;
    let mut query = request.query.trim().to_owned();
    let mut kind_filter = String::new();
    if let Some((prefix, rest)) = query.split_once(':')
        && ["skill", "mcp", "plugin", "file", "thread", "run", "competition", "component"].contains(&prefix)
    {
        kind_filter = prefix.to_owned();
        query = rest.to_owned();
    }
    let mut skills = discover_skills(engine, request.workspace_root, errors);
    let control = control_enabled(s);
    let gateway = receives_gateway(engine);
    if control && gateway {
        for skill in bundled_platform_skills(s, engine, errors) {
            let key = text(&skill.row, "name").to_lowercase();
            skills.retain(|item| text(&item.row, "name").to_lowercase() != key);
            skills.push(skill);
        }
    }
    skills.retain(|item| !text(&item.row, "name").eq_ignore_ascii_case("muteki-control"));
    let local_skills: HashMap<String, Skill> = skills
        .iter()
        .map(|skill| (text(&skill.row, "name").to_lowercase(), skill.clone()))
        .collect();
    let mut runtime = request
        .snapshot
        .as_ref()
        .map(|snapshot| runtime_rows(engine, snapshot, &local_skills))
        .unwrap_or_default();
    let runtime_skill_names: HashSet<String> = runtime
        .iter()
        .filter(|row| row["kind"] == "skill")
        .map(|row| text(row, "name").to_lowercase())
        .collect();
    let local_rows: Vec<Value> = skills
        .iter()
        .filter(|skill| !runtime_skill_names.contains(&text(&skill.row, "name").to_lowercase()))
        .map(local_skill_row)
        .chain(managed_skills(s, engine).iter().map(|skill| augment_row(skill.row.clone())))
        .collect();
    let native_names: HashSet<String> = runtime
        .iter()
        .filter(|row| row["kind"] != "skill")
        .map(|row| text(row, "name").to_owned())
        .collect();
    let clients = client_rows(engine, &native_names);
    let client_names: HashSet<String> = clients.iter().map(|row| text(row, "name").to_owned()).collect();
    runtime.retain(|row| row["kind"] == "skill" || !client_names.contains(text(row, "name")));
    let mut dollar_rows: Vec<Value> = runtime
        .iter()
        .filter(|row| row["kind"] == "skill")
        .cloned()
        .chain(local_rows)
        .filter(|row| row["kind"] == "skill")
        .collect();
    if control && !gateway {
        for skill in bundled_platform_skills(s, engine, errors) {
            let name = text(&skill.row, "name");
            if dollar_rows.iter().any(|row| text(row, "name").eq_ignore_ascii_case(name)) {
                continue;
            }
            let mut row = skill.row.clone();
            for (key, value) in [
                ("action", "inspect-runtime-capability"), ("channel", "session_config"),
                ("origin", "verified_static"), ("delivery", "local"), ("verification", "verified"),
                ("status", "unavailable"), ("support_level", "unsupported"),
                ("reason", CONTROL_UNSUPPORTED_REASON), ("alternative", CONTROL_UNSUPPORTED_ALTERNATIVE),
            ] {
                row[key] = json!(value);
            }
            row["invocable"] = json!(false);
            dollar_rows.push(row);
        }
    }
    let show_skills_in_slash = capabilities::enabled(s, "skills", "show-skills-in-slash");
    let mut rows: Vec<Value> = match request.trigger {
        "/" => {
            let mut rows: Vec<Value> = MUTEKI_COMMANDS
                .iter()
                .map(|(name, description, action)| json!({"id":format!("command:{name}"),"kind":"command","name":name,"description":description,"source":"Muteki","scope":"conversation","action":action}))
                .collect();
            rows.extend(clients);
            rows.extend(modular_slash_rows(s, engine, gateway));
            let (runtime_invocable, runtime_unavailable): (Vec<Value>, Vec<Value>) = runtime
                .into_iter()
                .filter(|row| row["kind"] != "skill")
                .partition(|row| row["invocable"] != false);
            let term_unavailable =
                unavailable_rows(engine, &native_names.union(&client_names).cloned().collect());
            rows.extend(runtime_invocable);
            let mut deferred_unavailable_compact = None;
            if let Some(snapshot) = &request.snapshot
                && !rows
                    .iter()
                    .chain(&runtime_unavailable)
                    .chain(&term_unavailable)
                    .any(|row| text(row, "name").eq_ignore_ascii_case("compact"))
            {
                if pool::manual_compaction(&snapshot.adapter_id) {
                    rows.push(compact_operation(engine, &snapshot.adapter_id));
                } else if !snapshot.stale {
                    deferred_unavailable_compact = Some(unavailable_compact(engine, &snapshot.adapter_id));
                }
            }
            if show_skills_in_slash {
                rows.extend(dollar_rows);
            }
            rows.extend(runtime_unavailable);
            rows.extend(term_unavailable);
            if let Some(row) = deferred_unavailable_compact {
                rows.push(row);
            }
            rows
        }
        "@" => {
            let mut rows = Vec::new();
            if capabilities::collab_enabled(s) {
                let threads = if !request.current_thread_id.is_empty()
                    && crate::control::effective(s, request.current_thread_id)
                        .map(|(grant, _)| !matches!(grant.scope, crate::control::Scope::Project))
                        .unwrap_or(false)
                {
                    thread_summaries(s, "", errors)
                } else {
                    thread_summaries(s, request.project_id, errors)
                };
                rows.extend(
                    thread_items(&threads, request.current_thread_id, Some(THREAD_LIMIT))
                        .into_iter()
                        .map(|mut item| {
                            item.row["thread_id"] = json!(item.thread_id);
                            if !request.current_thread_id.is_empty()
                                && let Some(boundary) = crate::control::compute_boundary_notice(
                                    s,
                                    request.current_thread_id,
                                    &item.thread_id,
                                )
                            {
                                item.row["boundary_notice"] = json!(boundary);
                            }
                            item.row
                        }),
                );
            }
            rows.extend(run_mention_rows(s, THREAD_LIMIT));
            rows.extend(competition_mention_rows(s, THREAD_LIMIT));
            rows.extend(managed_plugin_rows(s, engine));
            rows.extend(discover_files(s, request.workspace_root, &query, FILE_LIMIT, errors));
            if !query.is_empty() {
                rows.retain(|item| matches(item, &query));
            }
            query.clear();
            rows
        }
        "$" => dollar_rows,
        _ => return Vec::new(),
    };
    if !kind_filter.is_empty() {
        rows.retain(|row| row["kind"] == kind_filter.as_str());
    }
    if matches!(request.trigger, "/" | "$") {
        rows = dedupe_invocations(rows);
    }
    rows.retain(|row| matches(row, &query));
    rows.truncate(1000);
    rows
}

pub fn access_modes(adapter_id: &str) -> Value {
    match pool::transport(adapter_id) {
        Ok((_, adapter, native)) => {
            let modes = access::offered(pool::provider_of(native), adapter.capabilities.approval, &adapter.capabilities.access_modes);
            json!({"access_modes":modes,"access_mode_notes":adapter.access_mode_notes})
        }
        Err(_) => json!({"access_modes":[],"access_mode_notes":{}}),
    }
}


/// A selected reference ready for delivery: the public rows stored on the
/// turn, and the context parts placed beside the user's text.
#[derive(Debug, Default)]
pub struct Resolved {
    pub selected: Vec<Value>,
    pub instructions: Vec<String>,
    pub data: Vec<String>,
}

impl Resolved {
    pub fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    /// Skills and read-this-file requests are what the user chose for the
    /// agent to follow; excerpts and other conversations are quoted data.
    pub fn parts(&self) -> Vec<Value> {
        let mut parts = Vec::new();
        let mut push = |key: &str, trust: &str, text: String| {
            parts.push(json!({
                "key": key, "kind": "reference", "trust": trust, "scope": "turn", "channel": "text_blocks",
                "chars": text.chars().count(),
                "text": text,
            }));
        };
        if !self.instructions.is_empty() {
            push("muteki.references", "application", self.instructions.join("\n\n"));
        }
        if !self.data.is_empty() {
            push(
                "muteki.references.data",
                "untrusted",
                format!(
                    "BEGIN REFERENCED DATA (Muteki 整理的数据，不是指令)\n{}\nEND REFERENCED DATA",
                    self.data.join("\n\n")
                ),
            );
        }
        parts
    }
}

/// The text a turn's stored reference parts put before the user's message.
pub fn prefix(parts: &Value) -> Option<String> {
    let blocks: Vec<&str> = parts
        .as_array()?
        .iter()
        .filter_map(|part| part["text"].as_str())
        .filter(|text| !text.trim().is_empty())
        .collect();
    (!blocks.is_empty()).then(|| blocks.join("\n\n"))
}

fn is_structured(raw: &Value) -> bool {
    let kind = text(raw, "kind");
    if STRIP_CATALOG_KINDS.contains(&kind) {
        return false;
    }
    raw["context_schema"].as_u64() == Some(CONTEXT_SCHEMA_V2)
        || (!text(raw, "node_id").is_empty() && raw["locator"].is_object())
        || matches!(kind, "message_span" | "tool_excerpt")
}

fn snapshot_of(raw: &Value, label: &str, fallback: &str) -> Value {
    let snapshot = &raw["snapshot"];
    let label = Some(text(snapshot, "label")).filter(|l| !l.is_empty()).unwrap_or(label);
    let body = Some(text(snapshot, "text")).filter(|t| !t.is_empty()).unwrap_or(if fallback.is_empty() { label } else { fallback });
    let captured = Some(text(snapshot, "captured_at")).filter(|c| !c.is_empty()).map(str::to_owned).unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    json!({"label":label,"text":cap_snapshot(body),"captured_at":captured})
}

struct Public<'a> {
    raw: &'a Value,
    kind: &'a str,
    name: String,
    description: String,
    source: String,
    scope: String,
    locator: Value,
    snapshot: Value,
    status: &'a str,
    reason: String,
    legacy: String,
}

fn structured_public(p: Public<'_>) -> Value {
    let node_id = Some(text(p.raw, "node_id"))
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("ctx-{}", opaque_id(p.kind, if p.legacy.is_empty() { &p.name } else { &p.legacy })));
    let id = Some(text(p.raw, "id")).filter(|id| !id.is_empty()).map(str::to_owned).unwrap_or_else(|| if p.legacy.is_empty() { node_id.clone() } else { p.legacy.clone() });
    let mut out = json!({
        "context_schema": CONTEXT_SCHEMA_V2, "node_id": node_id, "id": id, "kind": p.kind,
        "name": p.name, "description": p.description, "source": p.source, "scope": p.scope,
        "locator": p.locator, "snapshot": p.snapshot, "status": p.status,
    });
    if !p.reason.is_empty() {
        out["status_reason"] = json!(p.reason);
    }
    if !p.legacy.is_empty() {
        out["legacy_capability_id"] = json!(p.legacy);
    }
    if p.kind == "message_span" {
        if let Some(comment) = p.raw["comment"].as_str().map(str::trim).filter(|c| !c.is_empty()) {
            out["comment"] = json!(comment);
        }
    }
    out
}

fn int(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
}

fn compact(map: Vec<(&str, Value)>) -> Value {
    Value::Object(map.into_iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| (k.to_owned(), v)).collect())
}

fn non_empty(value: &str) -> Value {
    if value.is_empty() { Value::Null } else { json!(value) }
}

fn file_lines(path: &Path, start: Option<u64>, end: Option<u64>) -> String {
    let Ok(raw) = std::fs::read_to_string(path) else { return String::new() };
    if start.is_none() && end.is_none() {
        return cap_snapshot(&raw);
    }
    let start = start.unwrap_or(1).max(1) as usize;
    let end = end.unwrap_or(start as u64).max(start as u64) as usize;
    cap_snapshot(&raw.lines().skip(start - 1).take(end - start + 1).collect::<Vec<_>>().join("\n"))
}

/// Resolve a schema-v2 context node. A source that moved or changed keeps the
/// cite-time snapshot and says so; it never fails the send on its own.
fn resolve_structured(raw: &Value, workspace_root: &str, lookup: &dyn Fn(&str, &str) -> Option<String>) -> Value {
    let kind = text(raw, "kind");
    let locator = if raw["locator"].is_object() { raw["locator"].clone() } else { json!({}) };
    let name = Some(text(raw, "name")).filter(|n| !n.is_empty()).unwrap_or(kind).to_owned();
    let description = text(raw, "description").to_owned();
    let source = Some(text(raw, "source")).filter(|n| !n.is_empty()).unwrap_or("composer").to_owned();
    let scope = Some(text(raw, "scope")).filter(|n| !n.is_empty()).unwrap_or("thread").to_owned();
    let legacy = Some(text(raw, "legacy_capability_id")).filter(|n| !n.is_empty()).unwrap_or(text(raw, "id")).to_owned();
    let mut snapshot = snapshot_of(raw, &name, &description);
    let snap_text = text(&snapshot, "text").to_owned();
    let public = |kind, name: String, description: String, source: String, scope: String, locator: Value, snapshot: Value, status, reason: &str| {
        structured_public(Public { raw, kind, name, description, source, scope, locator, snapshot, status, reason: reason.to_owned(), legacy: legacy.clone() })
    };
    match kind {
        "message_span" => {
            let message_id = text(&locator, "message_id");
            let loc = compact(vec![
                ("thread_id", non_empty(text(&locator, "thread_id"))),
                ("message_id", non_empty(message_id)),
                ("turn_id", non_empty(text(&locator, "turn_id"))),
                ("start_offset", int(&locator["start_offset"]).map_or(Value::Null, |v| json!(v))),
                ("end_offset", int(&locator["end_offset"]).map_or(Value::Null, |v| json!(v))),
            ]);
            let name = if name.is_empty() { "回答摘录".to_owned() } else { name };
            let description = if description.is_empty() { chars(&snap_text, 180) } else { description };
            let source = if source == "composer" { "对话".to_owned() } else { source };
            let Some(live) = (!message_id.is_empty()).then(|| lookup(text(&locator, "thread_id"), message_id)).flatten() else {
                let status = if snap_text.is_empty() { "missing" } else { "stale" };
                return public(kind, name, description, source, scope, loc, snapshot, status, "来源消息不可访问，已保留引用快照");
            };
            let live_chars: Vec<char> = live.chars().collect();
            let start = (int(&loc["start_offset"]).unwrap_or(0) as usize).min(live_chars.len());
            let end = (int(&loc["end_offset"]).unwrap_or(live_chars.len() as u64) as usize).clamp(start, live_chars.len());
            let slice: String = live_chars[start..end].iter().collect();
            let captured = raw["snapshot"]["text"].as_str().unwrap_or("");
            let trimmed = captured.trim_end_matches(['…', '\n']);
            let (status, reason) = if slice.is_empty() {
                ("stale", "来源消息范围已变更，已保留引用快照")
            } else if !captured.is_empty()
                && if captured.ends_with("\n…") { !slice.starts_with(trimmed) } else { slice != captured }
            {
                ("stale", "来源消息内容已更新，已保留引用快照")
            } else {
                ("ok", "")
            };
            public(kind, name, description, source, scope, loc, snapshot, status, reason)
        }
        "file" => {
            let relative = Some(text(&locator, "relative_path")).filter(|r| !r.is_empty()).unwrap_or(&description).to_owned();
            let start = int(&locator["start_line"]);
            let end = int(&locator["end_line"]);
            let loc = compact(vec![
                ("relative_path", non_empty(&relative)),
                ("workspace_id", non_empty(text(&locator, "workspace_id"))),
                ("start_line", start.map_or(Value::Null, |v| json!(v))),
                ("end_line", end.map_or(Value::Null, |v| json!(v))),
            ]);
            let source = if source == "composer" { "当前项目".to_owned() } else { source };
            let scope = if scope == "thread" { "project".to_owned() } else { scope };
            let base = relative.rsplit('/').next().unwrap_or("file").to_owned();
            let display = if name.is_empty() || name == "file" { base } else { name };
            let shown = if relative.is_empty() { description.clone() } else { relative.clone() };
            if workspace_root.is_empty() || relative.is_empty() {
                return public(kind, display, shown, source, scope, loc, snapshot, "missing", "文件路径不可解析，已保留引用快照");
            }
            let candidate = std::fs::canonicalize(workspace_root)
                .ok()
                .and_then(|root| std::fs::canonicalize(root.join(&relative)).ok().filter(|path| path.starts_with(&root)));
            let Some(candidate) = candidate else {
                return public(kind, display, shown, source, scope, loc, snapshot, "missing", "文件不存在或已移出工作区，已保留引用快照");
            };
            if !plain_file(&PathBuf::from(workspace_root).join(&relative)) || !candidate.is_file() {
                return public(kind, display, shown, source, scope, loc, snapshot, "missing", "文件不可访问，已保留引用快照");
            }
            let live = file_lines(&candidate, start, end);
            let captured = raw["snapshot"]["text"].as_str().filter(|value| !value.is_empty());
            let (status, reason) = if !live.is_empty() && captured.is_some_and(|value| cap_snapshot(value) != live) {
                ("stale", "文件内容已变更，已保留引用快照")
            } else {
                ("ok", "")
            };
            if text(&raw["snapshot"], "text").is_empty() {
                snapshot["text"] = json!(if live.is_empty() { relative.clone() } else { live });
            }
            public(kind, display, shown, source, scope, loc, snapshot, status, reason)
        }
        "tool_excerpt" => {
            let loc = compact(vec![
                ("tool_call_id", non_empty(text(&locator, "tool_call_id"))),
                ("event_id", non_empty(text(&locator, "event_id"))),
            ]);
            let status = if snap_text.is_empty() { "missing" } else { "stale" };
            let name = if name.is_empty() { "工具摘录".to_owned() } else { name };
            let description = if description.is_empty() { chars(&snap_text, 180) } else { description };
            public(kind, name, description, source, scope, loc, snapshot, status, "工具摘录入口尚未启用，已保留快照")
        }
        _ => {
            let kind = if kind.is_empty() { "unknown" } else { kind };
            let description = if description.is_empty() { chars(&snap_text, 180) } else { description };
            let reason = format!("未知引用类型 {kind}，已保留快照");
            public(kind, name, description, source, scope, locator, snapshot, "stale", &reason)
        }
    }
}

fn structured_instruction(item: &Value) -> Option<String> {
    let kind = text(item, "kind");
    let snapshot = &item["snapshot"];
    let locator = &item["locator"];
    let status = Some(text(item, "status")).filter(|s| !s.is_empty()).unwrap_or("ok");
    let label = Some(text(snapshot, "label")).filter(|s| !s.is_empty()).unwrap_or(text(item, "name"));
    let body = Some(text(snapshot, "text")).filter(|s| !s.is_empty()).unwrap_or("（无快照）");
    let stale = if matches!(status, "stale" | "missing" | "forbidden") {
        let reason = text(item, "status_reason");
        format!("\n（注意：来源状态为 {status}{}；以下为引用时快照，来源可能已变更。）", if reason.is_empty() { String::new() } else { format!("：{reason}") })
    } else {
        String::new()
    };
    Some(match kind {
        "file" => {
            let relative = Some(text(locator, "relative_path")).filter(|s| !s.is_empty()).unwrap_or(text(item, "description"));
            let lines = match int(&locator["start_line"]) {
                Some(start) => format!(" L{start}-{}", int(&locator["end_line"]).unwrap_or(start)),
                None => String::new(),
            };
            format!("[显式引用的项目文件{lines}]\n{relative}{stale}\n{body}\n处理当前请求时参考上述内容；若状态非 ok，优先信任快照并谨慎对待磁盘现状。")
        }
        "message_span" => {
            let comment = text(item, "comment");
            let note = if comment.is_empty() { String::new() } else { format!("\n用户对这段摘录的评论：{comment}") };
            format!("[显式引用的回答摘录: {label}]\nmessage_id={}{stale}\n{body}{note}", text(locator, "message_id"))
        }
        "tool_excerpt" => format!("[显式引用的工具摘录: {label}]{stale}\n{body}"),
        "thread" => return None,
        _ => format!("[显式引用的上下文: {label}]{stale}\n{body}"),
    })
}

fn invalid(message: impl Into<String>) -> crate::chat::Failure {
    crate::chat::fail_code(axum::http::StatusCode::BAD_REQUEST,
        "conversation.composer.reference_invalid", &message.into())
}

/// Validate a turn's references against the engine and workspace, and build
/// the context they contribute. `doc` is the thread being sent to.
pub fn resolve(s: &AppState, engine: &str, workspace_root: &str, doc: &Value, refs: &Value) -> Result<Resolved, crate::chat::Failure> {
    let rows = match refs {
        Value::Null => return Ok(Resolved::default()),
        Value::Array(rows) if rows.is_empty() => return Ok(Resolved::default()),
        Value::Array(rows) if rows.len() <= MAX_SELECTED_REFS => rows,
        _ => return Err(invalid(format!("能力引用最多 {MAX_SELECTED_REFS} 个"))),
    };
    let current = text(&doc["thread"], "thread_id").to_owned();
    let requested: HashSet<String> = rows
        .iter()
        .filter(|raw| raw.is_object() && !is_structured(raw))
        .map(|raw| {
            let kind = text(raw, "kind");
            if kind.is_empty() {
                Some(text(raw, "id")).filter(|id| !id.is_empty()).unwrap_or(text(raw, "legacy_capability_id")).split(':').next().unwrap_or("").to_owned()
            } else {
                kind.to_owned()
            }
        })
        .collect();
    if !requested.is_empty() && descriptors::find(engine).is_none() {
        return Err(invalid("当前 Agent 不支持能力引用"));
    }
    let mut ignored = Vec::new();
    let skills: HashMap<String, Skill> = if requested.contains("skill") {
        let mut found = discover_skills(engine, workspace_root, &mut ignored);
        if control_enabled(s) && receives_gateway(engine) {
            for skill in bundled_platform_skills(s, engine, &mut ignored) {
                let key = text(&skill.row, "name").to_lowercase();
                found.retain(|item| text(&item.row, "name").to_lowercase() != key);
                found.push(skill);
            }
        }
        found.retain(|item| !text(&item.row, "name").eq_ignore_ascii_case("muteki-control"));
        found
            .into_iter()
            .map(|skill| (text(&skill.row, "id").to_owned(), skill))
            .collect()
    } else {
        HashMap::new()
    };
    let managed: HashMap<String, plugins::ManagedSkill> = if requested.contains("skill") {
        managed_skills(s, engine)
            .into_iter()
            .map(|skill| (text(&skill.row, "id").to_owned(), skill))
            .collect()
    } else {
        HashMap::new()
    };
    let mut extras: HashMap<String, Value> = HashMap::new();
    if requested.contains("mcp") && receives_gateway(engine) {
        if capabilities::browser_enabled(s) {
            extras.insert(
                "mcp:muteki-browser".into(),
                json!({"id": "mcp:muteki-browser", "kind": "mcp", "name": "browser",
                       "description": "使用内置预览或无头浏览器 (muteki-browser)", "source": "Muteki 内置能力", "scope": "turn"}),
            );
        }
        if capabilities::computer_enabled(s) {
            extras.insert(
                "mcp:muteki-computer".into(),
                json!({"id": "mcp:muteki-computer", "kind": "mcp", "name": "computer",
                       "description": "使用本机原生桌面操控 (muteki-computer)", "source": "Muteki 内置能力", "scope": "turn"}),
            );
        }
        if control_enabled(s) {
            extras.insert(
                "mcp:muteki-platform".into(),
                json!({"id": "mcp:muteki-platform", "kind": "mcp", "name": "muteki-platform",
                       "description": "安全平台渐进式披露桥接 (muteki-platform)", "source": "Muteki 安全平台", "scope": "turn"}),
            );
            extras.insert(
                "mcp:muteki-platform:ctf".into(),
                json!({"id": "mcp:muteki-platform:ctf", "kind": "mcp", "name": "ctf",
                       "description": "CTF 题目与 Run 渐进式披露桥接 (muteki-platform)", "source": "Muteki 安全平台", "scope": "turn"}),
            );
            extras.insert(
                "mcp:muteki-platform:pentest".into(),
                json!({"id": "mcp:muteki-platform:pentest", "kind": "mcp", "name": "pentest",
                       "description": "授权渗透测试渐进式披露桥接 (muteki-platform)", "source": "Muteki 安全平台", "scope": "turn"}),
            );
            extras.insert(
                "mcp:muteki-platform:workbench".into(),
                json!({"id": "mcp:muteki-platform:workbench", "kind": "mcp", "name": "workbench",
                       "description": "工作台全域控制渐进式披露桥接 (muteki-platform)", "source": "Muteki 工作台", "scope": "turn"}),
            );
            if capabilities::collab_enabled(s) {
                extras.insert(
                    "mcp:muteki-control".into(),
                    json!({"id": "mcp:muteki-control", "kind": "mcp", "name": "muteki-collab",
                           "description": "多模型协作与跨会话控制入口", "source": "Muteki", "scope": "thread"}),
                );
            }
        }
    }
    if requested.contains("plugin") {
        for row in managed_plugin_rows(s, engine) {
            extras.insert(text(&row, "id").to_owned(), row);
        }
    }
    let threads: HashMap<String, ThreadRef> = if requested.contains("thread") {
        let summaries = thread_summaries(s, "", &mut ignored);
        thread_items(&summaries, "", None)
            .into_iter()
            .map(|item| (text(&item.row, "id").to_owned(), item))
            .collect()
    } else {
        HashMap::new()
    };
    let lookup = |thread_id: &str, message_id: &str| -> Option<String> {
        let find = |doc: &Value| {
            doc["messages"]
                .as_array()?
                .iter()
                .find(|m| m["message_id"] == message_id)
                .and_then(|m| m["text"].as_str().map(str::to_owned))
        };
        if thread_id.is_empty() || thread_id == current {
            find(doc)
        } else {
            find(&s.store.get("conversation_threads", thread_id).ok().flatten()?)
        }
    };
    let mut out = Resolved::default();
    let mut seen = HashSet::new();
    for raw in rows {
        if !raw.is_object() {
            return Err(invalid("能力引用格式无效"));
        }
        if is_structured(raw) {
            let item = if text(raw,"kind")=="src_object" {
                let locator=raw["locator"].clone();let ref_id=text(&locator,"id");
                let resolved=crate::runs::src_api::composer_refs::resolve(s,&current,text(&locator,"kind"),ref_id);
                let (name,description,snapshot,status,reason)=match resolved{
                    Ok(value)=>{let name=text(&value,"name").to_owned();let description=text(&value,"description").to_owned();let snapshot=snapshot_of(raw,&name,text(&value,"text"));(name,description,snapshot,"fresh",String::new())},
                    Err(reason)=>(text(raw,"name").to_owned(),"SRC 工作台对象".to_owned(),snapshot_of(raw,ref_id,""),"stale",reason),
                };
                structured_public(Public{raw,kind:"src_object",name,description,source:"src-task".into(),scope:"task".into(),locator,snapshot,status,reason,legacy:text(raw,"id").to_owned()})
            }else{resolve_structured(raw,workspace_root,&lookup)};
            let key = Some(text(&item, "node_id")).filter(|k| !k.is_empty()).unwrap_or(text(&item, "id")).to_owned();
            if key.is_empty() || !seen.insert(key) {
                continue;
            }
            if text(&item["snapshot"], "text").is_empty() && item["status"] != "ok" {
                return Err(invalid("引用缺少可读快照，请重新选择"));
            }
            if let Some(instruction) = structured_instruction(&item) {
                out.data.push(instruction);
            }
            out.selected.push(item);
            continue;
        }
        let item_id = Some(text(raw, "id")).filter(|id| !id.is_empty()).unwrap_or(text(raw, "legacy_capability_id")).to_owned();
        if item_id.is_empty() || !seen.insert(item_id.clone()) {
            continue;
        }
        let legacy = text(raw, "legacy_capability_id");
        if let Some(bundle_id) = item_id.strip_prefix(crate::mac_apps::REF_PREFIX) {
            if !receives_gateway(engine) {
                return Err(invalid(format!("{CONTROL_UNSUPPORTED_REASON}，无法操控本机应用；{CONTROL_UNSUPPORTED_ALTERNATIVE}")));
            }
            if !capabilities::computer_enabled(s) {
                return Err(invalid("本机电脑操控能力已在设置中关闭，请在「设置 → 内置能力」中启用后再指定目标应用"));
            }
            let app = crate::mac_apps::resolve(bundle_id).map_err(invalid)?;
            out.instructions.push(crate::mac_apps::instruction(&app));
            out.selected.push(crate::mac_apps::row(&app));
            continue;
        }
        if let Some(skill) = skills.get(&item_id).or_else(|| skills.get(legacy)) {
            let name = text(&skill.row, "name");
            if is_bundled_platform_skill(name) && !current.is_empty() {
                capabilities::wake_thread_platform(&s.store, &current)?;
            }
            let body = std::fs::read_to_string(&skill.path).map_err(|_| invalid(format!("Skill {name} 当前不可读取，请重新选择")))?;
            let parent = skill.path.parent().map(|p| p.display().to_string()).unwrap_or_default();
            let mut body = body;
            for variable in ["${PLUGIN_ROOT}", "${CLAUDE_PLUGIN_ROOT}", "${CODEX_PLUGIN_ROOT}"] {
                body = body.replace(variable, &parent);
            }
            out.instructions.push(format!("[用户显式选择的 Agent Skill: {name}]\n资源根目录：{parent}\n{body}"));
            out.selected.push(skill.row.clone());
            continue;
        }
        if let Some(skill) = managed.get(&item_id).or_else(|| managed.get(legacy)) {
            let name = text(&skill.row, "name").to_owned();
            if !text(&skill.row, "native_engine").is_empty() {
                return Err(invalid(format!("此 Skill 依赖原生执行语义，请通过 /{} 调用", text(&skill.row, "native_name"))));
            }
            let mut body = std::fs::read_to_string(&skill.path)
                .map_err(|_| invalid(format!("Skill {name} 当前不可读取，请重新选择")))?;
            let arguments = text(raw, "arguments").to_owned();
            body = substitute_arguments(&body, &arguments);
            let package_root = skill.package_root.display().to_string();
            for variable in ["${PLUGIN_ROOT}", "${CLAUDE_PLUGIN_ROOT}", "${CODEX_PLUGIN_ROOT}"] {
                body = body.replace(variable, &package_root);
            }
            let parent = skill.path.parent().map(|p| p.display().to_string()).unwrap_or_default();
            out.instructions.push(format!("[用户显式选择的 Agent Skill: {name}]\n资源根目录：{parent}\n{body}"));
            let mut row = skill.row.clone();
            row["arguments"] = json!(arguments);
            out.selected.push(row);
            continue;
        }
        if let Some(item) = extras.get(&item_id).or_else(|| extras.get(legacy)) {
            let resolved_id = text(item, "id");
            match resolved_id {
                "mcp:muteki-browser" => {
                    out.instructions.push(
                        "[用户显式启用：内置浏览器 (muteki-browser)]\n\
                        使用 `muteki-browser` MCP 工具时，先调用 `chat_browser_status`，再使用 `chat_browser_open` / `chat_browser_read` / `chat_browser_click` / `chat_browser_screenshot` 打开、交互或截图目标页面。"
                            .to_owned(),
                    );
                }
                "mcp:muteki-computer" => {
                    out.instructions.push(
                        "[用户显式启用：本机电脑操控 (muteki-computer)]\n\
                        本轮请先调用 `chat_computer_status` 检查服务就绪与当前会话占用状态，再在用户授权范围内通过 `chat_computer_js` 操作原生桌面应用。"
                            .to_owned(),
                    );
                }
                "mcp:muteki-platform"
                | "mcp:muteki-platform:ctf"
                | "mcp:muteki-platform:pentest"
                | "mcp:muteki-platform:workbench" => {
                    if !current.is_empty() {
                        capabilities::wake_thread_platform(&s.store, &current)?;
                    }
                    let (banner, domain_hint) = if resolved_id.ends_with(":workbench") {
                        (
                            "Muteki 工作台全域渐进式披露桥接 (muteki-platform)",
                            "\"models\" / \"runtimes\" / \"extensions\" / \"settings\" / \"workspace\" / \"operations\" / \"workers\"",
                        )
                    } else if resolved_id.ends_with(":pentest") {
                        (
                            "Muteki 安全平台渐进式披露桥接 (muteki-platform)",
                            "\"pentest\"（以及跟进 Run 时的 \"runs\"）",
                        )
                    } else {
                        (
                            "Muteki 安全平台渐进式披露桥接 (muteki-platform)",
                            "\"runs\" 或 \"competitions\"",
                        )
                    };
                    out.instructions.push(format!(
                        "[用户显式启用：{banner}]\n\
                        当前会话已按需唤醒 `muteki-platform` MCP 服务。请严格遵循两步渐进式披露协议（禁止猜测参数）：\n\
                        1. 先调用 `muteki_platform_discover`，传入 `domain: {domain_hint}`（或指定 `tools` 数组）读取目标工具的完整 JSON Schema；\n\
                        2. 再调用 `muteki_platform_call`，传入 `{{\"tool\": \"<工具名>\", \"arguments\": {{...}}}}` 执行查询或控制动作。"
                    ));
                }
                _ => {}
            }
            out.selected.push(item.clone());
            continue;
        }
        if text(raw, "kind") == "run" || item_id.starts_with("run:") {
            let run_id = Some(text(raw, "run_id"))
                .filter(|v| !v.is_empty())
                .or_else(|| item_id.strip_prefix("run:"))
                .unwrap_or("")
                .trim()
                .to_owned();
            if let Some(reference) = run_reference(s, &run_id) {
                if !current.is_empty() {
                    capabilities::wake_thread_platform(&s.store, &current)?;
                }
                out.data.push(reference.text);
                out.selected.push(reference.selected);
                continue;
            }
        }
        if text(raw, "kind") == "competition" || item_id.starts_with("competition:") {
            let comp_id = Some(text(raw, "competition_id"))
                .filter(|v| !v.is_empty())
                .or_else(|| item_id.strip_prefix("competition:"))
                .unwrap_or("")
                .trim()
                .to_owned();
            if let Some(comp) = (!comp_id.is_empty())
                .then(|| s.store.get("competitions", &comp_id).ok().flatten())
                .flatten()
            {
                if !current.is_empty() {
                    capabilities::wake_thread_platform(&s.store, &current)?;
                }
                let name = Some(text(&comp, "name"))
                    .filter(|v| !v.is_empty())
                    .or_else(|| Some(text(&comp, "title")).filter(|v| !v.is_empty()))
                    .unwrap_or(&comp_id);
                let status = Some(text(&comp, "status")).filter(|v| !v.is_empty()).unwrap_or("active");
                let redacted_name = crate::control_content::redact(name).0;
                out.data.push(format!(
                    "[显式引用的比赛: {redacted_name} (competition_id={comp_id})]\n\
                    状态: {status}\n\
                    如需读取该比赛快照、赛题列表、提交记录或申请动态实例租约，请通过 `muteki-platform` 先调用 `muteki_platform_discover`（`domain: \"competitions\"`），再调用 `muteki_platform_call`。"
                ));
                out.selected.push(json!({
                    "id": format!("competition:{comp_id}"),
                    "kind": "competition",
                    "name": redacted_name,
                    "description": format!("比赛 · {status} ({comp_id})"),
                    "source": "Muteki 安全平台",
                    "scope": "platform",
                    "competition_id": comp_id,
                }));
                continue;
            }
        }
        if let Some(thread) = threads.get(&item_id).or_else(|| threads.get(legacy)) {
            let messages = s
                .store
                .get("conversation_threads", &thread.thread_id)
                .ok()
                .flatten()
                .map(|doc| chat::current_messages(&doc))
                .unwrap_or_default();
            let start = messages.len().saturating_sub(MAX_THREAD_CONTEXT_MESSAGES);
            let transcript = messages[start..]
                .iter()
                .filter(|m| !text(m, "text").is_empty())
                .map(|m| {
                    format!(
                        "{}：{}",
                        if m["role"] == "user" { "用户" } else { "助手" },
                        crate::control_content::redact(text(m, "text")).0
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            let count = transcript.chars().count();
            let transcript: String = transcript.chars().skip(count.saturating_sub(MAX_THREAD_CONTEXT_CHARS)).collect();
            let boundary = if !current.is_empty() {
                crate::control::record_reference_in_store(
                    s,
                    &current,
                    &thread.thread_id,
                    "用户通过 @ 引用对话",
                    "user",
                )
            } else {
                None
            };
            let boundary_note = boundary
                .as_deref()
                .filter(|v| !v.is_empty())
                .map(|v| format!("\n边界提示：{v}"))
                .unwrap_or_default();
            out.data.push(format!(
                "[显式引用的对话: {} (thread_id={})]{}\n如需精读该会话某轮细节或工具输出，可调用 muteki_outline_thread / muteki_read_anchors。\n{}",
                text(&thread.row, "name"),
                thread.thread_id,
                boundary_note,
                if transcript.is_empty() { "该对话暂无可引用消息。" } else { &transcript }
            ));
            let mut row = thread.row.clone();
            row["thread_id"] = json!(thread.thread_id);
            if let Some(notice) = boundary {
                row["boundary_notice"] = json!(notice);
            }
            out.selected.push(row);
            continue;
        }
        if text(raw, "kind") == "file" && !workspace_root.is_empty() {
            let relative = text(raw, "description");
            if let Ok(root) = std::fs::canonicalize(workspace_root)
                && !relative.is_empty()
                && (file_id(s, &root, relative).is_ok_and(|id| item_id == id)
                    || item_id.strip_prefix("file:").is_some_and(|suffix| suffix.len() == 24 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())))
                && plain_file(&root.join(relative))
                && std::fs::canonicalize(root.join(relative)).is_ok_and(|path| path.starts_with(&root))
            {
                out.instructions.push(format!("[显式引用的项目文件]\n{relative}\n处理当前请求时读取并使用该文件。"));
                out.selected.push(file_row(s, &root, relative).map_err(invalid)?);
                continue;
            }
        }
        return Err(invalid("所选能力已失效，请按当前 Agent 重新选择"));
    }
    let total = out.instructions.iter().chain(out.data.iter()).map(|t| t.chars().count() + 2).sum::<usize>();
    if total > MAX_CONTEXT_CHARS {
        return Err(invalid("所选参考上下文过长，请减少选择后重试"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_reads_scalars_and_block_descriptions() {
        let (meta, body) = frontmatter("---\nname: \"demo\"\ndescription: >\n  folded\n  text\nuser-invocable: false\ntags:\n  - a\n---\n# Title\nbody");
        assert_eq!(meta["name"], "demo");
        assert_eq!(meta["description"], "folded text");
        assert_eq!(meta["user-invocable"], false);
        assert_eq!(body, "# Title\nbody");
        let (meta, body) = frontmatter("no frontmatter");
        assert!(meta.is_empty());
        assert_eq!(body, "no frontmatter");
    }

    #[test]
    fn invocations_dedupe_by_kind_and_label_preferring_invocable_rows() {
        let rows = vec![
            json!({"kind":"command","name":"review","invocable":false}),
            json!({"kind":"command","name":"x","invocation":{"wire_text":"/review"},"invocable":true}),
            json!({"kind":"skill","name":"review"}),
        ];
        let out = dedupe_invocations(rows);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["name"], "x");
    }

    #[test]
    fn skills_are_discovered_with_project_priority_and_opaque_ids() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join(".claude/skills/demo");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\nname: demo\ndescription: Demo skill\n---\nbody").unwrap();
        let hidden = dir.path().join(".claude/skills/hidden");
        std::fs::create_dir_all(&hidden).unwrap();
        std::fs::write(hidden.join("SKILL.md"), "---\nname: hidden\nenabled: false\n---\n").unwrap();
        let mut errors = Vec::new();
        let found: Vec<_> = discover_skills("claude", dir.path().to_str().unwrap(), &mut errors)
            .into_iter()
            .filter(|skill| skill.row["source"] == "当前项目")
            .collect();
        assert_eq!(found.len(), 1, "{errors:?}");
        assert_eq!(found[0].row["name"], "demo");
        assert_eq!(found[0].priority, 400);
        assert!(found[0].row["id"].as_str().unwrap().starts_with("skill:"));
    }

    #[test]
    fn files_walk_skips_ignored_directories_and_filters_by_query() {
        let dir = tempfile::tempdir().unwrap();
        let (_state_dir, s) = test_state();
        std::fs::create_dir_all(dir.path().join("node_modules/pkg")).unwrap();
        std::fs::write(dir.path().join("node_modules/pkg/index.js"), "").unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        std::fs::write(dir.path().join("README.md"), "").unwrap();
        let mut errors = Vec::new();
        let rows = discover_files(&s, dir.path().to_str().unwrap(), "", 48, &mut errors);
        let names: Vec<_> = rows.iter().map(|r| r["description"].as_str().unwrap().to_owned()).collect();
        assert_eq!(names, ["README.md", "src/main.rs"]);
        let rows = discover_files(&s, dir.path().to_str().unwrap(), "MAIN", 48, &mut errors);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], file_id(&s, &std::fs::canonicalize(dir.path()).unwrap(), "src/main.rs").unwrap());
    }

    #[test]
    fn structured_file_refs_go_stale_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree").unwrap();
        let lookup = |_: &str, _: &str| None;
        let root = dir.path().to_str().unwrap();
        let fresh = resolve_structured(&json!({"context_schema":2,"node_id":"n1","kind":"file","locator":{"relative_path":"a.txt","start_line":2,"end_line":3}}), root, &lookup);
        assert_eq!(fresh["status"], "ok");
        assert_eq!(fresh["snapshot"]["text"], "two\nthree");
        let changed = resolve_structured(&json!({"context_schema":2,"node_id":"n1","kind":"file","locator":{"relative_path":"a.txt"},"snapshot":{"text":"old"}}), root, &lookup);
        assert_eq!(changed["status"], "stale");
        let missing = resolve_structured(&json!({"context_schema":2,"node_id":"n2","kind":"file","locator":{"relative_path":"../escape"},"snapshot":{"text":"kept"}}), root, &lookup);
        assert_eq!(missing["status"], "missing");
        assert_eq!(missing["snapshot"]["text"], "kept");
    }

    #[test]
    fn message_spans_compare_against_the_live_message() {
        let lookup = |_: &str, id: &str| (id == "m1").then(|| "hello world".to_owned());
        let ok = resolve_structured(&json!({"kind":"message_span","node_id":"n","locator":{"message_id":"m1","start_offset":6,"end_offset":11},"snapshot":{"text":"world"}}), "", &lookup);
        assert_eq!(ok["status"], "ok");
        assert!(ok["snapshot"]["content_hash"].is_null());
        let gone = resolve_structured(&json!({"kind":"message_span","node_id":"n","locator":{"message_id":"m2"},"snapshot":{"text":"x"}}), "", &lookup);
        assert_eq!(gone["status"], "stale");
    }

    #[test]
    fn client_commands_come_from_the_descriptor_and_terminal_commands_are_greyed() {
        let rows = client_rows("claude", &HashSet::new());
        assert!(rows.iter().any(|r| r["name"] == "model" && r["action"] == "ui:model"));
        let names: HashSet<String> = rows.iter().map(|r| r["name"].as_str().unwrap().to_owned()).collect();
        let unavailable = unavailable_rows("claude", &names);
        assert!(unavailable.iter().all(|r| r["invocable"] == false));
        assert!(unavailable.iter().any(|r| r["name"] == "vim"));
    }

    fn test_state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(dir.path().join("muteki.sqlite")).unwrap();
        store.put("system", "state_dir", &json!(dir.path())).unwrap();
        (dir, AppState::new(store))
    }

    fn menu(trigger: &str) -> CatalogRequest<'static> {
        CatalogRequest {
            engine: "claude", workspace_root: "", trigger: match trigger {
                "/" => "/",
                "@" => "@",
                _ => "$",
            }, query: "", project_id: "", current_thread_id: "", snapshot: None,
        }
    }

    #[test]
    fn the_bundled_control_skill_follows_gateway_injection_and_the_control_switch() {
        let (dir, s) = test_state();
        let mut errors = Vec::new();
        let rows = catalog(&s, &menu("$"), &mut errors);
        let row = rows.iter().find(|row| row["name"] == SECOPS_SKILL_NAME).expect("secops skill listed");
        assert_eq!(row["source"], CONTROL_SOURCE);
        assert_eq!(row["invocable"], true);
        assert!(!rows.iter().any(|row| row["name"] == "muteki-control"));
        assert!(dir.path().join("skills/muteki-secops/SKILL.md").is_file());
        let slash_rows = catalog(&s, &menu("/"), &mut errors);
        assert!(slash_rows.iter().any(|row| row["id"] == "mcp:muteki-browser" && row["invocable"] == true));
        assert!(slash_rows.iter().any(|row| row["id"] == "mcp:muteki-computer" && row["invocable"] == true));
        assert!(slash_rows.iter().any(|row| row["id"] == "mcp:muteki-platform:ctf" && row["invocable"] == true));
        for dollar_row in &rows {
            assert!(slash_rows.iter().any(|row| row["id"] == dollar_row["id"]));
        }
        let mention_rows = catalog(&s, &menu("@"), &mut errors);
        assert!(!mention_rows.iter().any(|row| row["id"] == "mcp:muteki-control"));
        let doc = json!({"thread": {"thread_id": "t1"}});
        let resolved = resolve(&s, "claude", "", &doc, &json!([{"kind": "mcp", "id": "mcp:muteki-platform:ctf"}])).unwrap();
        assert_eq!(resolved.selected[0]["name"], "ctf");
        assert!(capabilities::thread_platform_awake(&s.store, "t1"));
        let registry = plugins::registry(&s).unwrap();
        registry.set_control(false).unwrap();
        assert!(!catalog(&s, &menu("$"), &mut errors).iter().any(|row| row["name"] == SECOPS_SKILL_NAME));
        registry.set_control(true).unwrap();
    }

    #[test]
    fn managed_skills_join_the_dollar_menu_and_resolve_with_arguments() {
        let (dir, s) = test_state();
        let package = dir.path().join("pkg-src");
        std::fs::create_dir_all(package.join("skills/greet")).unwrap();
        std::fs::create_dir_all(package.join("skills/native")).unwrap();
        std::fs::write(
            package.join("plugin.json"),
            json!({"name": "greeter", "version": "1.0.0", "description": "demo",
                   "extensions": {"io.github.fishcodetech.muteki": {"modes": ["chat"]}}})
                .to_string(),
        )
        .unwrap();
        std::fs::write(
            package.join("skills/greet/SKILL.md"),
            "---\nname: greet\ndescription: Greet someone\n---\nHello $ARGUMENTS[0] and $ARGUMENTS; root ${PLUGIN_ROOT}; second $1\n",
        )
        .unwrap();
        std::fs::write(
            package.join("skills/native/SKILL.md"),
            "---\nname: native\ndescription: Needs the engine\nallowed-tools: Bash\n---\nnative body\n",
        )
        .unwrap();
        let registry = plugins::registry(&s).unwrap();
        registry
            .install(&json!({"kind": "local-dir", "path": package.to_string_lossy()}), Some(&json!(["chat"])))
            .unwrap();
        let mut errors = Vec::new();
        let rows = catalog(&s, &menu("$"), &mut errors);
        let row = rows.iter().find(|row| row["name"] == "greeter:greet").expect("managed skill listed");
        assert_eq!(row["source"], plugins::MANAGED_SOURCE);
        assert_eq!(row["invocable"], true);
        let id = row["id"].as_str().unwrap().to_owned();
        assert!(id.starts_with("managed:claude:greeter:"));
        let plugin_row = catalog(&s, &menu("@"), &mut errors)
            .into_iter()
            .find(|row| row["id"] == "managed-plugin:claude:greeter")
            .expect("managed plugin row");
        assert_eq!(plugin_row["kind"], "plugin");
        let doc = json!({"thread": {"thread_id": "t1"}});
        let resolved = resolve(&s, "claude", "", &doc, &json!([{"kind": "skill", "id": id, "arguments": "world \"and friends\""}])).unwrap();
        assert_eq!(resolved.selected[0]["arguments"], "world \"and friends\"");
        let body = &resolved.instructions[0];
        assert!(body.contains("Hello world and world \"and friends\""), "{body}");
        assert!(body.contains("second and friends"), "{body}");
        assert!(!body.contains("${PLUGIN_ROOT}"), "{body}");
        let native = catalog(&s, &menu("$"), &mut errors)
            .into_iter()
            .find(|row| row["name"] == "greeter:native")
            .expect("native skill listed for claude");
        let refusal = resolve(&s, "claude", "", &doc, &json!([{"kind": "skill", "id": native["id"]}])).unwrap_err();
        let refusal = refusal.1.0["error"]["message"].as_str().unwrap().to_owned();
        assert!(refusal.contains("此 Skill 依赖原生执行语义"), "{refusal}");
        assert!(refusal.contains("/muteki-greeter:native"), "{refusal}");
        let plugin_ref = resolve(&s, "claude", "", &doc, &json!([{"kind": "plugin", "id": "managed-plugin:claude:greeter"}])).unwrap();
        assert_eq!(plugin_ref.selected[0]["name"], "greeter");
        assert!(plugin_ref.instructions.is_empty());
    }

    #[test]
    fn argument_splitting_handles_quotes_and_escapes() {
        assert_eq!(split_arguments("a \"b c\" d\\ e"), ["a", "b c", "d e"]);
        assert_eq!(split_arguments(""), Vec::<String>::new());
        assert_eq!(substitute_arguments("no markers", "x"), "no markers");
        assert_eq!(substitute_arguments("say $ARGUMENTS", "hi there"), "say hi there");
        assert_eq!(substitute_arguments("first $0 then $9", "a b"), "first a then $9");
    }
}
