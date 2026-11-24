use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;

use agent::{AgentEvent, ItemContent, ItemStatus, TurnStatus};
use axum::{
    Json,
    extract::{Path as AxPath, State},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256, Sha512};

use crate::{
    AppState,
    agent_runtime::{StartRequest, access, credentials, descriptors},
    native_chat, runtime_api,
};

use super::continuation;
use super::{emit_event, mark_run_finished, now_secs, session_dir};

pub const LOCAL_PI_SOLVER_ID: &str = "cli-pi#1";
pub const LOCAL_PI_ADAPTER_ID: &str = "pi.rpc";
pub const LOCAL_PI_SKILL_VERSION: &str = "local-pi-skill/v1";

pub const LOCAL_PI_SYSTEM_PROMPT: &str = r#"你是运行在 Muteki 工作台中的 CTF 本地 Pi 单会话解题助手。当前一个 Run 绑定一个持久 Pi 会话，直接与用户以对话流方式协作解题。

## 与 Muteki 交互方式
1. **工作区与材料位置**
   - 当前工作目录（`./`）即本 Run 的独立分析工作区，题目附件已放置在 `./` 及只读备份目录 `./uploads/`，题目说明见 `./CHALLENGE.md`。
   - 工具完整输出会自动归档至 `./arts/art-<seq>.json`。
2. **提交 Flag（必选交互）**
   - 一旦通过实际命令或脚本输出确认了真实 Flag，立即执行以下命令向 Muteki 提交：
     `python3 "$MUTEKI_BLACKBOARD_SCRIPT" submit-flag '<Flag>'`
   - 当该命令输出 `SUBMITTED` 后，在回复中简要说明解题过程、关键证据与最终 Flag。
3. **调用外部 MCP 工具或请求人工输入（按需）**
   - 若用户在扩展中心为当前模式启用了 MCP 服务，可通过以下命令查看与调用：
     - 列出工具：`python3 "$MUTEKI_BLACKBOARD_SCRIPT" mcp-tools`
     - 查看参数 Schema：`python3 "$MUTEKI_BLACKBOARD_SCRIPT" mcp-schema '<server.tool>'`
     - 调用工具：`python3 "$MUTEKI_BLACKBOARD_SCRIPT" mcp-call '<server.tool>' ./args.json`
   - 若遇到必须人工提供的外部信息或凭据，可执行 `python3 "$MUTEKI_BLACKBOARD_SCRIPT" request-input '<需要人工提供的信息>'` 并在对话中说明。

## 参考禁止事项与边界
- 保持 `./uploads/` 下原始附件只读，不要覆盖、移动或删除 `./uploads/` 中的原始文件；中间脚本或生成产物请写在当前目录或 `./work/`。
- 不要执行 `pip install`、`apt/apk/brew install` 或 `git clone` 下载安装外部工具链；优先使用当前环境已有命令（如 `python3`、`file`、`strings`、`xxd`、`openssl` 等）与内置工具（`read`、`bash`、`edit`、`write`、`grep`、`find`、`ls`）。
- 严禁猜测、伪造或提交未经实际输出验证的 Flag，不要把题目示例占位符（如 `flag{...}`、`flag{example}`）当作真实 Flag 提交。
- 只有从题目目标或附件的真实运行结果中得到的 Flag 才算数；工具输出里出现的 `flag{...}` 字样可能只是示例或干扰，必须先确认来源。

## 工作方法
- 每做一步实验前先写下预期，拿到实测结果后对照预期；不一致时先弄清差异来自哪里，再改下一步。
- 同一个假设连续两次出现同样的失败，换一种信息来源或观察方式，不要只改参数重试。
- 同一个假设做了几次实验仍没有新进展时，停下来，不要无休止地重复：把当前状态写入 `./work/STATE_CARD.md`（覆盖旧内容），再在回复里告诉用户，等待指示。状态卡包含三部分：卡住的点、已确认的事实（附证据所在的文件或命令）、建议的下一步。
- 会话续跑时若 `./work/STATE_CARD.md` 已存在，先读它再继续。"#;

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn load_run_doc(state: &AppState, run_id: &str) -> Option<Value> {
    state.store.get("runs", run_id).ok().flatten()
}

fn persist_run_doc(state: &AppState, run_id: &str, doc: &Value) {
    let _ = state.store.put("runs", run_id, doc);
}

fn emit_local_pi_receipt(
    state: &AppState,
    run_id: &str,
    command_id: &str,
    action: &str,
    status: &str,
    detail: &str,
) {
    let cmd_status = match status {
        "applied" => "effect_observed",
        "queued" => "persisted",
        "rejected" => "rejected",
        "failed" => "failed",
        other => other,
    };
    let payload = json!({
        "command_id": command_id,
        "action": action,
        "target": LOCAL_PI_SOLVER_ID,
        "solver_id": LOCAL_PI_SOLVER_ID,
        "main_thread": true,
        "status": cmd_status,
        "receipt_status": status,
        "state": cmd_status,
        "detail": detail,
        "ts": now_secs(),
    });
    emit_event(
        state,
        run_id,
        Some(LOCAL_PI_SOLVER_ID),
        "control.receipt",
        payload.clone(),
    );
    emit_event(
        state,
        run_id,
        Some(LOCAL_PI_SOLVER_ID),
        "control.command",
        payload,
    );
}

#[derive(Clone, Debug)]
pub struct BuiltinLocalPiSkill {
    pub id: &'static str,
    pub name: &'static str,
    pub version: &'static str,
    pub requires: &'static str,
    pub required_binaries: &'static [&'static str],
    pub completion_basis: &'static str,
    pub summary: &'static str,
    pub body: &'static str,
}

pub const BUILTIN_LOCAL_PI_SKILLS: &[BuiltinLocalPiSkill] = &[
    BuiltinLocalPiSkill {
        id: "ctf-workspace-guide",
        name: "材料与工作区说明",
        version: LOCAL_PI_SKILL_VERSION,
        requires: "文件、附件与产物管理",
        required_binaries: &["file", "python3"],
        completion_basis: "原始材料可追溯，输出归属于当前 Run",
        summary: "介绍附件位置（./ 与只读 uploads/）、工作目录、产物输出位置与可用工具边界。",
        body: r#"---
name: ctf-workspace-guide
description: 介绍当前 Run 的附件位置、工作目录、输出位置与可用工具；开始分析题目材料前读取
version: local-pi-skill/v1
---

# 材料与工作区说明

1. **目录结构**
   - 当前工作目录（`.`）即本 Run 的独立分析工作区 `sessions/<run_id>`，题目说明见 `./CHALLENGE.md`。
   - 题目附件已复制到工作区根目录 `./`，原始副本位于只读目录 `uploads/`；不要覆盖、移动或删除 `uploads/` 下的原始文件。
   - 生成的中间脚本、转换结果或汇总文件请保存在工作区根目录或 `work/` 子目录中，以便工作台“材料与产物”面板追踪来源。
   - 工具完整输出会自动归档至 `arts/art-<seq>.json`。

2. **证据与溯源要求**
   - 结论必须指明所依据的具体文件路径（如 `uploads/...` 或生成脚本路径）及实际命令输出。
   - 不要凭文件名或题目名称猜测内容；大文件或二进制材料应使用读取/切片或分析脚本按需查看，避免将整份大文件直接刷入对话。

3. **结果提交**
   - 通过实际输出确认真实 Flag 后，执行 `python3 "$MUTEKI_BLACKBOARD_SCRIPT" submit-flag '<Flag>'` 提交到当前 Run。
"#,
    },
    BuiltinLocalPiSkill {
        id: "ctf-file-text-analysis",
        name: "文件与文本分析",
        version: LOCAL_PI_SKILL_VERSION,
        requires: "读取、搜索、已安装解析工具",
        required_binaries: &["file", "strings", "xxd", "python3"],
        completion_basis: "输出依据指向具体文件或完整工具记录",
        summary: "辅助查看文件格式、魔数头部、文本编码、结构化内容和元数据。",
        body: r#"---
name: ctf-file-text-analysis
description: 辅助查看文件格式、文本编码、结构化内容和元数据；遇到未知文件或文本材料时读取
version: local-pi-skill/v1
---

# 文件与文本分析

1. **格式与编码识别**
   - 先核对文件大小、魔数（magic bytes）、字符编码与行结构，再决定后续解析方式。
   - 对 JSON、XML、CSV、日志、源码或配置材料，优先使用精确检索（`grep` / `read`）或脚本提取关键字段。

2. **工具与依赖边界**
   - 若某个外部解析命令未安装，直接报告缺失命令名称与原因，并改用现有 `read` 或 `python3` 标准库处理，不要自行安装系统软件包。
   - 所有提取出的字符串、偏移量或结构化字段必须保留真实工具记录支持。
"#,
    },
    BuiltinLocalPiSkill {
        id: "ctf-math-data",
        name: "数学与数据处理",
        version: LOCAL_PI_SKILL_VERSION,
        requires: "本地计算环境",
        required_binaries: &["python3", "openssl"],
        completion_basis: "保留输入、计算文件和结果，不以模型自述替代计算结果",
        summary: "对数学题、编码转换、密码学参数或结构化数据进行可复核脚本计算。",
        body: r#"---
name: ctf-math-data
description: 对题目中的数学计算、编码数据和密码学参数进行可复核的脚本计算；需要计算或解码时读取
version: local-pi-skill/v1
---

# 数学与数据处理

1. **可复核计算原则**
   - 涉及编码解码（Base64/Hex/压缩）、大整数运算、模运算、线性方程或统计校验时，必须编写并运行脚本（如 `work/solve_calc.py`）获得真实输出。
   - 严禁用模型心算代替实际计算结果。

2. **保留计算产物**
   - 将计算脚本与关键输出保存在工作区文件中（例如 `work/` 下），并在回复中引用脚本路径与实际运行输出。
"#,
    },
    BuiltinLocalPiSkill {
        id: "ctf-media-inspection",
        name: "图像与音频材料说明",
        version: LOCAL_PI_SKILL_VERSION,
        requires: "实际可用的解码和预览能力",
        required_binaries: &["file", "exiftool", "ffmpeg", "binwalk", "zsteg"],
        completion_basis: "不支持的格式明确提示，不能仅凭扩展名宣称已读取",
        summary: "说明图像、音频及容器文件的元数据、数据块检查与格式支持边界。",
        body: r#"---
name: ctf-media-inspection
description: 说明图像、音频和容器类材料的元数据与数据块检查方式及支持边界；处理媒体附件时读取
version: local-pi-skill/v1
---

# 图像与音频材料说明

1. **容器与元数据核验**
   - 检查文件真实头部签名（PNG IHDR/IDAT/IEND、JPEG EXIF、RIFF/WAV chunk、ZIP/PDF 结构等），确认扩展名与实际文件头是否一致。
   - 检查尾部附加数据、文本注释块或异常 chunk 尺寸。

2. **诚实报告能力边界**
   - 若当前环境缺少特定解码器（如 `exiftool`、`ffmpeg`、`zsteg`），或文件格式不受支持，必须明确告知用户缺失的工具或格式限制，不得仅凭扩展名宣称已读取媒体内容。
"#,
    },
    BuiltinLocalPiSkill {
        id: "ctf-result-summary",
        name: "状态与结果整理",
        version: LOCAL_PI_SKILL_VERSION,
        requires: "当前 session 与 Run 记录",
        required_binaries: &[],
        completion_basis: "所有结论有来源；没有完成也能清楚交接",
        summary: "整理已验证事实、未决问题、候选结果及产物文件引用，便于人工接手。",
        body: r#"---
name: ctf-result-summary
description: 用户要求整理进度或交接时，汇总已验证事实、未解决问题及产物引用
version: local-pi-skill/v1
---

# 状态与结果整理

1. **结构化交接**
   - **已验证事实**：列出已通过文件或脚本输出确认的事实，并标注对应文件路径或命令记录。
   - **生成产物**：列出本轮新增或更新的脚本、解码文件及路径。
   - **未决问题与下一步建议**：如果尚未得出最终答案，清楚说明当前停留在哪一步、还需要哪些信息或人工确认，不把阶段性分析伪装成已完成。

2. **保存状态卡**
   - 需要跨会话交接时，把上述内容写入 `./work/STATE_CARD.md`；下次恢复会话时工作台会提示先读取它。
"#,
    },
];

fn binary_purpose(bin: &str) -> &'static str {
    match bin {
        "python3" => "数学与数据处理脚本执行",
        "file" => "文件魔数与格式识别",
        "strings" => "二进制可打印字符串提取",
        "xxd" => "十六进制切片与转储",
        "openssl" => "基础密码学与证书格式解析",
        "exiftool" => "图像与媒体元数据检查",
        "ffmpeg" => "音视频容器与流信息检查",
        "binwalk" => "固件与嵌套容器签名扫描",
        "zsteg" => "PNG/BMP LSB 数据检查",
        _ => "可选宿主命令",
    }
}

fn frontmatter_field(body: &str, key: &str) -> Option<String> {
    let rest = body.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    rest[..end].lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        (name.trim() == key)
            .then(|| value.trim().trim_matches(['"', '\'']).to_owned())
            .filter(|v| !v.is_empty())
    })
}

fn skill_digest(body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(body.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn builtin_skills_catalog_with_defaults(default_enabled_ids: Option<&[String]>) -> Vec<Value> {
    let enabled_set: Option<HashSet<&str>> =
        default_enabled_ids.map(|list| list.iter().map(String::as_str).collect());
    BUILTIN_LOCAL_PI_SKILLS
        .iter()
        .map(|skill| {
            let digest = skill_digest(skill.body);
            let mut dependencies = Vec::with_capacity(skill.required_binaries.len());
            let mut missing_dependencies = Vec::new();
            for &bin in skill.required_binaries {
                let path = which_binary(bin);
                if path.is_none() {
                    missing_dependencies.push(bin.to_owned());
                }
                dependencies.push(json!({
                    "name": bin,
                    "purpose": binary_purpose(bin),
                    "available": path.is_some(),
                    "path": path,
                }));
            }
            let default_enabled = enabled_set
                .as_ref()
                .map(|s| s.contains(skill.id))
                .unwrap_or(true);
            json!({
                "id": skill.id,
                "name": skill.name,
                "version": skill.version,
                "requires": skill.requires,
                "completion_basis": skill.completion_basis,
                "summary": skill.summary,
                "body": skill.body,
                "stage_path": format!(".pi/skills/{}/SKILL.md", skill.id),
                "delivery_mode": "正文落地于 .pi/skills/<id>/SKILL.md 并通过 --skill 挂载；Pi 只在系统提示中列出名称与描述，正文按需读取",
                "sha256_prefix": &digest[..12],
                "default_enabled": default_enabled,
                "dependencies": dependencies,
                "missing_dependencies": missing_dependencies,
                "all_dependencies_met": missing_dependencies.is_empty(),
            })
        })
        .collect()
}

pub fn candidate_open_source_skills() -> Vec<Value> {
    vec![
        json!({
            "repo": "ljagiello/ctf-skills",
            "license": "MIT",
            "focus": "覆盖 reverse / crypto / forensics / pwn / web / misc 多方向解题说明",
            "status": "candidate_only",
            "reason": "仓库声明 MIT，作为 5 项内置离线材料分析说明的设计参考；不整包挂载。需要时可在扩展中心导入并为 CTF 模式启用。",
        }),
        json!({
            "repo": "Huoxing999/ctf-skills",
            "license": "未确认",
            "focus": "分类资料与工具说明集合",
            "status": "not_bundled",
            "reason": "README 显示 MIT 标记，但 GitHub 元数据未识别许可证，未进一步核验；仅保留地址供后续选用。",
        }),
        json!({
            "repo": "ZianTT/ctf-plat-skills",
            "license": "未确认",
            "focus": "比赛平台交互资料",
            "status": "not_bundled",
            "reason": "比赛平台连接由 Muteki 工作台控制面统一管理，不向单会话模型直接暴露平台凭据与控制工具。",
        }),
        json!({
            "repo": "Real-Simplicity/CTF-Tiller",
            "license": "未确认",
            "focus": "产品设计参考（以设计材料和平台适配为主）",
            "status": "not_bundled",
            "reason": "公开内容未见完整调度/会话实现，不能当成现成运行内核；仅作设计参考。",
        }),
    ]
}

pub fn default_skill_ids() -> Vec<String> {
    BUILTIN_LOCAL_PI_SKILLS
        .iter()
        .map(|s| s.id.to_owned())
        .collect()
}

pub fn resolve_selected_skills(requested: Option<&[String]>) -> Vec<&'static BuiltinLocalPiSkill> {
    match requested {
        Some(list) => {
            let set: HashSet<&str> = list.iter().map(String::as_str).collect();
            BUILTIN_LOCAL_PI_SKILLS
                .iter()
                .filter(|skill| set.contains(skill.id))
                .collect()
        }
        None => BUILTIN_LOCAL_PI_SKILLS.iter().collect(),
    }
}

pub fn loaded_tools_manifest() -> Vec<Value> {
    vec![
        json!({
            "name": "read",
            "category": "file_inspection",
            "category_label": "文件与目录检查",
            "source": "pi.builtin",
            "scope": "当前 Run 工作区 (sessions/<run_id> 及 uploads/)",
            "enabled": true,
            "description": "读取工作区与 uploads/ 下的文本或图像材料",
        }),
        json!({
            "name": "grep",
            "category": "file_inspection",
            "category_label": "文件与目录检查",
            "source": "pi.builtin",
            "scope": "当前 Run 工作区 (sessions/<run_id>)",
            "enabled": true,
            "description": "在 Run 工作区材料中按正则或字面量检索内容",
        }),
        json!({
            "name": "find",
            "category": "file_inspection",
            "category_label": "文件与目录检查",
            "source": "pi.builtin",
            "scope": "当前 Run 工作区 (sessions/<run_id>)",
            "enabled": true,
            "description": "按文件名模式查找工作区内的附件与产物",
        }),
        json!({
            "name": "ls",
            "category": "file_inspection",
            "category_label": "文件与目录检查",
            "source": "pi.builtin",
            "scope": "当前 Run 工作区 (sessions/<run_id>)",
            "enabled": true,
            "description": "查看工作区目录与文件列表",
        }),
        json!({
            "name": "write",
            "category": "artifact_authoring",
            "category_label": "脚本与产物编写",
            "source": "pi.builtin",
            "scope": "当前 Run 工作区 (建议写入 work/)",
            "enabled": true,
            "description": "在工作区写入计算脚本或保存分析产物文件（严禁覆盖 uploads/ 原始材料）",
        }),
        json!({
            "name": "edit",
            "category": "artifact_authoring",
            "category_label": "脚本与产物编写",
            "source": "pi.builtin",
            "scope": "当前 Run 工作区 (sessions/<run_id>)",
            "enabled": true,
            "description": "精确修改工作区内的脚本或分析文档",
        }),
        json!({
            "name": "bash",
            "category": "local_computation",
            "category_label": "本地计算与静态分析",
            "source": "pi.builtin",
            "scope": "当前 Run 工作区 (完整输出归档至 arts/art-<seq>.json)",
            "enabled": true,
            "description": "在当前 Run 工作区执行本地计算、格式转换与静态检查命令",
        }),
    ]
}

pub fn muteki_readonly_tools_manifest() -> Vec<Value> {
    vec![
        json!({
            "name": "muteki_get_task",
            "category": "task_context_readonly",
            "category_label": "Muteki 当前题目只读能力",
            "source": "muteki.readonly",
            "scope": "当前绑定 Run",
            "delivery": "工作区 CHALLENGE.md 投影 + 启动首轮题目上下文",
            "enabled": true,
            "description": "只读获取当前绑定 CTF 题目的名称、分类、期望 Flag 格式、题目描述与 uploads/ 附件清单",
        }),
        json!({
            "name": "muteki_get_run_snapshot",
            "category": "task_context_readonly",
            "category_label": "Muteki 当前题目只读能力",
            "source": "muteki.readonly",
            "scope": "当前绑定 Run",
            "delivery": "Run 会话快照与 arts/ 产物索引",
            "enabled": true,
            "description": "只读获取当前绑定 Run 的会话状态、已记录工具归档与材料索引，不允许跨 Run 访问",
        }),
        json!({
            "name": "muteki_read_run_events",
            "category": "task_context_readonly",
            "category_label": "Muteki 当前题目只读能力",
            "source": "muteki.readonly",
            "scope": "当前绑定 Run",
            "delivery": "events.jsonl 与 arts/art-<seq>.json 持久记录",
            "enabled": true,
            "description": "只读回放当前绑定 Run 的历史事件与完整工具输出文件",
        }),
    ]
}

pub fn disabled_capabilities_manifest() -> Vec<Value> {
    vec![
        json!({
            "id": "swarm_orchestration",
            "name": "Worker / 子 Agent / Swarm 编排",
            "source": "muteki.swarm",
            "enabled": false,
            "reason": "本地 Pi 单会话模式（一个 Run 对应一个 Pi 主会话）不加载 Swarm、Decide、reviewer、verifier 或子 Agent 工具",
        }),
        json!({
            "id": "fgs_graph_ritual",
            "name": "FGS 推理图提交仪式 (fgs_Ripple)",
            "source": "muteki.fgs",
            "enabled": false,
            "reason": "单会话模式不要求小参数模型维护或提交 FGS 推理图，避免占用上下文窗口与因格式错误中断解题",
        }),
        json!({
            "id": "control_plane_mutation",
            "name": "平台连接、账号与其他 Run 控制面管理",
            "source": "muteki.control",
            "enabled": false,
            "reason": "控制面管理留在工作台，不向单会话分析环境暴露跨 Run 或账号修改能力",
        }),
        json!({
            "id": "browser_external_mcp",
            "name": "浏览器自动化与外部联网 MCP",
            "source": "external.mcp",
            "enabled": false,
            "reason": "Pi 进程以 --no-mcp --no-extensions --no-prompt-templates --no-context-files 启动，不直接挂载浏览器或外部 MCP；扩展中心为 CTF 模式启用的 MCP 只能经 Muteki blackboard 代理调用",
        }),
    ]
}

pub fn probe_optional_binaries() -> Vec<Value> {
    let candidates = [
        ("python3", "数学与数据处理脚本执行"),
        ("file", "文件魔数与格式识别"),
        ("strings", "二进制可打印字符串提取"),
        ("xxd", "十六进制切片与转储"),
        ("openssl", "基础密码学与证书格式解析"),
        ("exiftool", "图像与媒体元数据检查"),
        ("ffmpeg", "音视频容器与流信息检查"),
        ("binwalk", "固件与嵌套容器签名扫描"),
        ("zsteg", "PNG/BMP LSB 数据检查"),
    ];
    candidates
        .into_iter()
        .map(|(bin, purpose)| {
            let path = which_binary(bin);
            json!({
                "name": bin,
                "purpose": purpose,
                "available": path.is_some(),
                "path": path,
            })
        })
        .collect()
}

fn which_binary(name: &str) -> Option<String> {
    let path_var = crate::shell_env::path_os()?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

pub fn verify_isolation_environment(workspace: &Path, offline_env: bool) -> Value {
    let pi_binary = which_binary("pi");
    let cwd_display = workspace.display().to_string();
    json!({
        "runtime": "host_process",
        "adapter_id": LOCAL_PI_ADAPTER_ID,
        "pi_binary": pi_binary,
        "cwd": cwd_display,
        "os_sandbox_verified": false,
        "network_isolation_verified": false,
        "status": "unverified_host_process",
        "summary": "宿主进程模式（cwd 限定于当前 Run 工作区；已锁定外部扩展与 MCP；操作系统沙箱与内核级网络隔离未验证）",
        "checks": [
            {
                "id": "workspace_cwd",
                "label": "Run 独立工作目录限定 (cwd)",
                "status": "verified",
                "detail": format!("进程工作目录固定为 {cwd_display}，原始附件隔离于 uploads/，完整工具输出归档于 arts/"),
            },
            {
                "id": "pi_cli_lockdown",
                "label": "Pi 外部 MCP / 扩展 / 全局 Skills 锁定",
                "status": "verified",
                "detail": "启动参数固定包含 --no-mcp --no-extensions --no-skills --no-prompt-templates --no-context-files；只通过 --skill 传入本 Run 勾选的内置 Skills 和扩展中心为 Pi / CTF 启用的 Skills",
            },
            {
                "id": "offline_env_marker",
                "label": "Pi 启动网络活动关闭 (PI_OFFLINE=1)",
                "status": if offline_env { "verified" } else { "disabled" },
                "detail": if offline_env {
                    "已注入 PI_OFFLINE=1：Pi 不在启动时刷新模型目录或检查更新。该变量不限制 bash 等工具的网络访问"
                } else {
                    "已在本地 Pi 设置中关闭：Pi 启动时可能刷新模型目录或检查更新"
                },
            },
            {
                "id": "os_process_sandbox",
                "label": "操作系统级进程沙箱",
                "status": "unverified",
                "detail": "未验证（当前为本机宿主进程直连模式，未启用强制容器或 OS 内核沙箱，不作沙箱安全承诺）",
            },
            {
                "id": "network_namespace_isolation",
                "label": "操作系统级网络隔离",
                "status": "unverified",
                "detail": "未验证（共享本机宿主网络栈以访问本地模型端口，未启用内核网络命名空间断网）",
            },
        ],
        "optional_binaries": probe_optional_binaries(),
    })
}

fn extract_candidate_flags(text: &str, flag_format: &str) -> Vec<String> {
    let mut found = Vec::new();
    let prefixes = {
        let mut list: Vec<String> = ["flag{", "FLAG{", "ctf{", "CTF{"]
            .iter()
            .map(|p| (*p).to_owned())
            .collect();
        if let Some((prefix, _)) = flag_format.split_once('{') {
            let p = prefix.trim();
            if !p.is_empty() {
                list.push(format!("{p}{{"));
            }
        }
        list
    };
    for prefix in &prefixes {
        let prefix = prefix.as_str();
        let mut search_from = 0;
        while let Some(rel_idx) = text[search_from..].find(prefix) {
            let start = search_from + rel_idx;
            let after_open = start + prefix.len();
            if let Some(end_rel) = text[after_open..].find('}') {
                let candidate = &text[start..=after_open + end_rel];
                if candidate.len() <= 160
                    && !candidate.contains('\n')
                    && !candidate.contains("...")
                    && !found.iter().any(|c| c == candidate)
                {
                    found.push(candidate.to_owned());
                }
                search_from = after_open + end_rel + 1;
            } else {
                break;
            }
        }
    }
    found
}

#[derive(Debug)]
pub enum LocalPiControl {
    UserMessage {
        command_id: String,
        action: String,
        text: String,
        steer: bool,
    },
    InterruptTurn {
        command_id: String,
        action: String,
    },
    Continue {
        command_id: String,
        action: String,
        text: Option<String>,
    },
    Stop {
        command_id: String,
    },
}

struct LiveLocalPiHandle {
    thread_id: String,
    state: Arc<AppState>,
}

static LOCAL_PI_SESSIONS: LazyLock<Mutex<HashMap<String, LiveLocalPiHandle>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Default)]
struct TapState {
    flags_seeded: bool,
    seen_flags: HashSet<String>,
    tool_starts: HashMap<String, (String, Instant, Value)>,
}

static LOCAL_PI_TAPS: LazyLock<Mutex<HashMap<String, TapState>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static LOCAL_PI_TICKERS: LazyLock<Mutex<HashSet<String>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

pub fn has_live_session(run_id: &str) -> bool {
    LOCAL_PI_SESSIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(run_id)
}

fn live_handle(run_id: &str) -> Option<(String, Arc<AppState>)> {
    LOCAL_PI_SESSIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(run_id)
        .map(|h| (h.thread_id.clone(), h.state.clone()))
}

fn register_session(run_id: &str, thread_id: &str, state: &Arc<AppState>) {
    LOCAL_PI_SESSIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            run_id.to_owned(),
            LiveLocalPiHandle {
                thread_id: thread_id.to_owned(),
                state: state.clone(),
            },
        );
}

fn unregister_session(run_id: &str) {
    LOCAL_PI_SESSIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(run_id);
    LOCAL_PI_TAPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(run_id);
}

pub(super) fn failure_text(failure: &(axum::http::StatusCode, Json<Value>)) -> String {
    let body = &failure.1.0;
    body["error"]["message"]
        .as_str()
        .or_else(|| body["detail"].as_str())
        .or_else(|| body["message"].as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| body.to_string())
}

async fn chat_command(state: &Arc<AppState>, tid: &str, body: Value) -> Result<Value, String> {
    crate::chat::command(State(state.clone()), AxPath(tid.to_owned()), Json(body))
        .await
        .map(|Json(value)| value)
        .map_err(|failure| failure_text(&failure))
}

fn chat_send_body(command_id: &str, text: &str) -> Value {
    json!({
        "command_id": command_id,
        "command_type": "conversation.turn.send",
        "payload": {"text": text, "client_message_id": command_id},
    })
}

fn running_turn_of(state: &AppState, tid: &str) -> Option<String> {
    crate::chat::load(state, tid)
        .ok()
        .and_then(|v| v["thread"]["state"]["running_turn_id"].as_str().map(str::to_owned))
}

/// Run-level control for a CTF Pi run. Every operation lands on the chat
/// thread bound to the run, so the chat composer and these controls share one
/// session.
pub async fn dispatch_live_control(
    run_id: &str,
    cmd: LocalPiControl,
) -> Result<(), LocalPiControl> {
    let Some((tid, state)) = live_handle(run_id) else {
        return Err(cmd);
    };
    match cmd {
        LocalPiControl::Stop { command_id } => {
            if running_turn_of(&state, &tid).is_some() {
                let body = json!({
                    "command_id": format!("{command_id}:stop-interrupt"),
                    "command_type": "conversation.turn.interrupt",
                    "payload": {},
                });
                let _ = chat_command(&state, &tid, body).await;
            }
            let workspace = session_dir(&state, run_id);
            let ingress_dir = workspace
                .join(".muteki-blackboard-ingress")
                .join(LOCAL_PI_SOLVER_ID);
            let _ = super::worker::drain_blackboard_ingress(
                &state,
                run_id,
                LOCAL_PI_SOLVER_ID,
                &ingress_dir,
                &workspace,
            );
            native_chat::close(&state, &tid, "operator_stop").await;
            unregister_session(run_id);
            if let Some(mut doc) = load_run_doc(&state, run_id) {
                doc["awaiting_operator"] = Value::Null;
                doc["local_pi_session"]["session_state"] = json!("stopped");
                doc["local_pi_session"]["turn_active"] = json!(false);
                doc["local_pi_session"]["queued_messages"] = json!(0);
                doc["local_pi_session"]["updated_at"] = json!(now_iso());
                persist_run_doc(&state, run_id, &doc);
            }
            emit_event(
                &state,
                run_id,
                Some(LOCAL_PI_SOLVER_ID),
                "worker.status",
                json!({
                    "solver_id": LOCAL_PI_SOLVER_ID,
                    "main_thread": true,
                    "status": "stopped",
                    "reason": "operator_stopped",
                }),
            );
            mark_run_finished(
                &state,
                run_id,
                Some(LOCAL_PI_SOLVER_ID),
                false,
                "operator_stop",
                Some("本地 Pi 会话已由用户停止"),
            );
            emit_local_pi_receipt(
                &state,
                run_id,
                &command_id,
                "stop",
                "applied",
                "Pi session terminated",
            );
            Ok(())
        }
        LocalPiControl::InterruptTurn { command_id, action } => {
            let body = json!({
                "command_id": format!("{command_id}:interrupt"),
                "command_type": "conversation.turn.interrupt",
                "payload": {},
            });
            match chat_command(&state, &tid, body).await {
                Ok(_) => {
                    emit_local_pi_receipt(
                        &state,
                        run_id,
                        &command_id,
                        &action,
                        "applied",
                        "已向 Pi 会话发送中断，会话保持可继续状态",
                    );
                    Ok(())
                }
                Err(_) => Err(LocalPiControl::InterruptTurn { command_id, action }),
            }
        }
        LocalPiControl::UserMessage {
            command_id,
            action,
            text,
            steer,
        } => {
            let running = running_turn_of(&state, &tid);
            let body = match (&running, steer) {
                (Some(turn), true) => json!({
                    "command_id": format!("{command_id}:steer"),
                    "command_type": "conversation.turn.steer",
                    "payload": {
                        "text": text,
                        "expected_turn_id": turn,
                        "client_message_id": command_id,
                    },
                }),
                _ => chat_send_body(&format!("{command_id}:send"), &text),
            };
            match chat_command(&state, &tid, body).await {
                Ok(_) => emit_local_pi_receipt(
                    &state,
                    run_id,
                    &command_id,
                    &action,
                    "applied",
                    "消息已送达 Pi 会话",
                ),
                Err(reason) => emit_local_pi_receipt(
                    &state,
                    run_id,
                    &command_id,
                    &action,
                    "failed",
                    &format!("发送消息失败: {reason}"),
                ),
            }
            Ok(())
        }
        LocalPiControl::Continue {
            command_id,
            action,
            text,
        } => {
            if running_turn_of(&state, &tid).is_some() {
                emit_local_pi_receipt(
                    &state,
                    run_id,
                    &command_id,
                    &action,
                    "applied",
                    "当前会话已在运行中",
                );
                return Ok(());
            }
            let prompt_text = text
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    "请继续上一轮的分析，验证尚未确认的线索或汇总当前结果。".to_owned()
                });
            match chat_command(
                &state,
                &tid,
                chat_send_body(&format!("{command_id}:continue"), &prompt_text),
            )
            .await
            {
                Ok(_) => emit_local_pi_receipt(
                    &state,
                    run_id,
                    &command_id,
                    &action,
                    "applied",
                    "已在当前 Pi 会话启动新一轮分析",
                ),
                Err(reason) => emit_local_pi_receipt(
                    &state,
                    run_id,
                    &command_id,
                    &action,
                    "failed",
                    &format!("继续失败: {reason}"),
                ),
            }
            Ok(())
        }
    }
}

pub fn stage_local_pi_workspace(
    state: &AppState,
    run_id: &str,
    run_doc: &Value,
    selected_skill_ids: Option<&[String]>,
) -> Result<(PathBuf, Vec<Value>), String> {
    let workspace = session_dir(state, run_id);
    let uploads_dir = workspace.join("uploads");
    let arts_dir = workspace.join("arts");
    let work_dir = workspace.join("work");
    let skills_root = workspace.join(".pi").join("skills");
    let blackboard_skill_dir = workspace.join(".muteki-skills").join("muteki-blackboard");
    let ingress_dir = workspace
        .join(".muteki-blackboard-ingress")
        .join(LOCAL_PI_SOLVER_ID);

    std::fs::create_dir_all(&uploads_dir).map_err(|e| format!("create uploads dir: {e}"))?;
    std::fs::create_dir_all(&arts_dir).map_err(|e| format!("create arts dir: {e}"))?;
    std::fs::create_dir_all(&work_dir).map_err(|e| format!("create work dir: {e}"))?;
    std::fs::create_dir_all(&skills_root).map_err(|e| format!("create .pi/skills dir: {e}"))?;
    std::fs::create_dir_all(&blackboard_skill_dir)
        .map_err(|e| format!("create muteki-blackboard dir: {e}"))?;
    std::fs::create_dir_all(&ingress_dir)
        .map_err(|e| format!("create blackboard ingress dir: {e}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&ingress_dir, std::fs::Permissions::from_mode(0o700));
    }

    let skill_md_path = blackboard_skill_dir.join("SKILL.md");
    let blackboard_py_path = blackboard_skill_dir.join("blackboard.py");
    std::fs::write(&skill_md_path, super::worker::SKILL_MD)
        .map_err(|e| format!("write blackboard SKILL.md: {e}"))?;
    std::fs::write(&blackboard_py_path, super::worker::BLACKBOARD_PY)
        .map_err(|e| format!("write blackboard.py: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&blackboard_py_path, std::fs::Permissions::from_mode(0o755));
    }

    let exec_gen = run_doc
        .get("execution_generation")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1);
    let canonical_ingress = std::fs::canonicalize(&ingress_dir).unwrap_or_else(|_| ingress_dir.clone());
    let env_json = json!({
        "MUTEKI_BLACKBOARD_INGRESS_DIR": canonical_ingress.to_string_lossy(),
        "MUTEKI_WORKER_ID": LOCAL_PI_SOLVER_ID,
        "MUTEKI_EXECUTION_GENERATION": exec_gen.to_string(),
        "MUTEKI_CHALLENGE_MODE": "ctf",
        "MUTEKI_BLACKBOARD_ROLE": "solve",
        "MUTEKI_BLACKBOARD_PROFILE": "solve",
        "MUTEKI_WORKER_EXECUTION_MODE": "step",
        "MUTEKI_PLATFORM_CONFIRMATION_REQUIRED": "0",
        "MUTEKI_TARGET_EPOCH": "1",
    });
    let _ = std::fs::write(blackboard_skill_dir.join("env.json"), env_json.to_string());

    if let Some(attachments) = run_doc.get("attachments").and_then(Value::as_array) {
        for att in attachments {
            let Some(raw_path) = att.as_str().or_else(|| att.get("path").and_then(Value::as_str)) else {
                continue;
            };
            let src = Path::new(raw_path);
            if src.is_file() {
                if let Some(fname) = src.file_name() {
                    let dst_upload = uploads_dir.join(fname);
                    if src != dst_upload && !dst_upload.exists() {
                        let _ = std::fs::copy(src, &dst_upload);
                    }
                    let dst_root = workspace.join(fname);
                    if src != dst_root && !dst_root.exists() {
                        let _ = std::fs::copy(src, &dst_root);
                    }
                }
            }
        }
    }

    if let Ok(entries) = std::fs::read_dir(&uploads_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let dst_root = workspace.join(entry.file_name());
                if !dst_root.exists() {
                    let _ = std::fs::copy(&path, &dst_root);
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444));
                }
            }
        }
    }

    if let Ok(entries) = std::fs::read_dir(&skills_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.symlink_metadata().is_ok() {
                let _ = std::fs::remove_file(&path).or_else(|_| std::fs::remove_dir_all(&path));
            }
        }
    }

    if let Ok(registry) = crate::plugins::registry(state) {
        let _ = registry.stage_worker_components(&workspace, "pi", "ctf");
    }

    // Extension-center skills are linked under hashed directory names, so a
    // builtin id can only collide with a stale or foreign directory.
    for skill in resolve_selected_skills(selected_skill_ids) {
        let dir = skills_root.join(skill.id);
        if dir.symlink_metadata().is_ok() {
            return Err(format!(
                "内置 Skill `{}` 与工作区 .pi/skills 中已有目录冲突",
                skill.id
            ));
        }
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("create skill dir {}: {e}", skill.id))?;
        std::fs::write(dir.join("SKILL.md"), skill.body)
            .map_err(|e| format!("write skill {}: {e}", skill.id))?;
    }

    let mut loaded_skills = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&skills_root) {
        for entry in entries.flatten() {
            let skill_file = entry.path().join("SKILL.md");
            if !skill_file.is_file() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            let body = std::fs::read_to_string(&skill_file)
                .map_err(|e| format!("read skill {id}: {e}"))?;
            let digest = skill_digest(&body);
            let rel_path = format!(".pi/skills/{id}/SKILL.md");
            match BUILTIN_LOCAL_PI_SKILLS.iter().find(|s| s.id == id) {
                Some(builtin) => loaded_skills.push(json!({
                    "id": builtin.id,
                    "name": builtin.name,
                    "version": builtin.version,
                    "summary": builtin.summary,
                    "requires": builtin.requires,
                    "completion_basis": builtin.completion_basis,
                    "required_binaries": builtin.required_binaries,
                    "source": "builtin",
                    "path": rel_path,
                    "sha256_prefix": &digest[..12],
                    "exists": true,
                    "staged": true,
                })),
                None => loaded_skills.push(json!({
                    "id": id,
                    "name": frontmatter_field(&body, "name").unwrap_or_else(|| id.clone()),
                    "version": "extension",
                    "summary": frontmatter_field(&body, "description")
                        .unwrap_or_else(|| "扩展中心为 Pi / CTF 模式启用的 Skill".to_owned()),
                    "source": "extension",
                    "path": rel_path,
                    "sha256_prefix": &digest[..12],
                    "exists": true,
                    "staged": true,
                })),
            }
        }
    }
    loaded_skills.sort_by(|a, b| {
        (a["source"] != "builtin", a["id"].as_str()).cmp(&(b["source"] != "builtin", b["id"].as_str()))
    });

    let name = run_doc
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(run_id);
    let category = run_doc
        .get("category")
        .and_then(Value::as_str)
        .unwrap_or("misc");
    let description = run_doc
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    let flag_format = run_doc
        .get("flag_format")
        .and_then(Value::as_str)
        .unwrap_or("flag{...}");

    let mut upload_lines = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&uploads_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let fname = entry.file_name().to_string_lossy().into_owned();
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                upload_lines.push(format!("- `{fname}` / `uploads/{fname}` ({size} bytes)"));
            }
        }
    }
    upload_lines.sort();

    let challenge_md = format!(
        "# Challenge: {name}\n\n- **Category**: {category}\n- **Expected Flag Format**: `{flag_format}`\n- **Mode**: 本地 Pi 单会话 (`local_pi`)\n\n## Description\n\n{}\n\n## Uploaded Materials (`./` & `uploads/`)\n\n{}\n",
        if description.trim().is_empty() {
            "(No description provided)"
        } else {
            description.trim()
        },
        if upload_lines.is_empty() {
            "- (No uploaded attachments)".to_owned()
        } else {
            upload_lines.join("\n")
        }
    );
    let _ = std::fs::write(workspace.join("CHALLENGE.md"), challenge_md);

    Ok((workspace, loaded_skills))
}

fn build_challenge_brief(run_doc: &Value, workspace: &Path) -> String {
    let name = run_doc
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("CTF Challenge");
    let category = run_doc
        .get("category")
        .and_then(Value::as_str)
        .unwrap_or("misc");
    let description = run_doc
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    let flag_format = run_doc
        .get("flag_format")
        .and_then(Value::as_str)
        .unwrap_or("flag{...}");
    let target = run_doc
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or("");

    let uploads_dir = workspace.join("uploads");
    let mut upload_files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&uploads_dir) {
        for entry in entries.flatten() {
            if entry.path().is_file() {
                let fname = entry.file_name().to_string_lossy().into_owned();
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                upload_files.push(format!("`./{fname}` (`uploads/{fname}`, {size} bytes)"));
            }
        }
    }
    upload_files.sort();

    let mut prompt = format!(
        "# Challenge: {name}\n\
         - Category: {category}\n\
         - Expected Flag Format: `{flag_format}`\n"
    );
    if !target.trim().is_empty() {
        prompt.push_str(&format!("- Target: `{}`\n", target.trim()));
    }
    if !upload_files.is_empty() {
        prompt.push_str(&format!(
            "- Attached Files (already in `./` and read-only `./uploads/`): {}\n",
            upload_files.join(", ")
        ));
    }
    if !description.trim().is_empty() {
        prompt.push_str(&format!("\n## Description\n{}\n", description.trim()));
    }
    if let Some(card) = read_state_card(workspace) {
        prompt.push_str(&format!(
            "\n## 上次保存的状态卡 (`./work/STATE_CARD.md`)\n{card}\n"
        ));
    }
    prompt
}

const DEFAULT_LOCAL_PI_REQUEST: &str = "请先检查当前工作区下的题目材料并开始分析。确认真实 Flag 后请调用 `python3 \"$MUTEKI_BLACKBOARD_SCRIPT\" submit-flag '<Flag>'` 提交，并简要总结解题过程。";

fn read_state_card(workspace: &Path) -> Option<String> {
    std::fs::read_to_string(workspace.join("work").join("STATE_CARD.md"))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

#[derive(Clone, Debug)]
pub struct ValidatedResumeHandle {
    pub provider_session_id: String,
    pub session_file: String,
    pub cursor: Value,
}

/// Validates that the chat thread bound to this Run still holds a readable
/// Pi resume cursor. Never falls back to a fresh session when `resume` is expected.
pub fn validate_saved_resume(
    state: &AppState,
    run_id: &str,
    run_doc: &Value,
) -> Result<ValidatedResumeHandle, String> {
    let tid = run_doc
        .get("chat_thread_id")
        .or_else(|| run_doc.pointer("/local_pi_session/chat_thread_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "本地 Pi 会话恢复失败：该 Run 没有已保存的对话线程，拒绝隐式新建会话".to_owned()
        })?;
    let thread = crate::chat::load(state, tid).map_err(|failure| {
        format!(
            "本地 Pi 会话恢复失败：对话线程 `{tid}` 无法读取（{}），拒绝隐式新建会话",
            failure_text(&failure)
        )
    })?;
    let cont = continuation::load(state, run_id, LOCAL_PI_SOLVER_ID).ok().flatten();

    let cursor_val = thread["native_session"]["resume"]
        .as_object()
        .map(|_| thread["native_session"]["resume"].clone())
        .or_else(|| {
            cont.as_ref()
                .and_then(|c| c.get("resume").cloned())
                .filter(|v| v.is_object())
        })
        .ok_or_else(|| {
            "本地 Pi 会话恢复失败：resume_cursor 缺失或已损坏，拒绝隐式冷启动新会话".to_owned()
        })?;

    let cursor_session_id = cursor_val
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "本地 Pi 会话恢复失败：resume_cursor 缺少有效的 session_id 字段".to_owned()
        })?
        .to_owned();

    let session_file = cursor_val
        .get("session_file")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "本地 Pi 会话恢复失败：resume_cursor 缺少有效的 session_file 路径，拒绝隐式新建会话"
                .to_owned()
        })?
        .to_owned();

    let metadata = std::fs::metadata(Path::new(&session_file)).map_err(|e| {
        format!(
            "本地 Pi 会话恢复失败：原生会话文件 `{session_file}` 不存在或不可读 ({e})，拒绝隐式新建会话"
        )
    })?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(format!(
            "本地 Pi 会话恢复失败：原生会话文件 `{session_file}` 为空或不是普通文件，拒绝隐式新建会话"
        ));
    }

    Ok(ValidatedResumeHandle {
        provider_session_id: cursor_session_id,
        session_file,
        cursor: cursor_val,
    })
}

pub fn record_resume_failure(
    state: &AppState,
    run_id: &str,
    reason: &str,
    command_id: Option<(&str, &str)>,
) {
    let mut run_doc = load_run_doc(state, run_id).unwrap_or_else(|| json!({ "run_id": run_id }));
    if !run_doc.get("local_pi_session").is_some_and(Value::is_object) {
        run_doc["local_pi_session"] = json!({});
    }
    run_doc["running"] = json!(false);
    run_doc["status"] = json!("failed");
    run_doc["outcome_reason"] = json!("resume_failed");
    run_doc["outcome_detail"] = json!(reason);
    run_doc["local_pi_session"]["session_state"] = json!("resume_failed");
    run_doc["local_pi_session"]["turn_active"] = json!(false);
    run_doc["local_pi_session"]["last_error"] = json!({
        "code": "local_pi.resume_failed",
        "message": reason,
        "at": now_iso(),
    });
    run_doc["awaiting_operator"] = json!("resume_failed");
    run_doc["updated"] = json!(now_secs());
    persist_run_doc(state, run_id, &run_doc);

    emit_event(
        state,
        run_id,
        Some(LOCAL_PI_SOLVER_ID),
        "worker.Alert",
        json!({
            "solver_id": LOCAL_PI_SOLVER_ID,
            "main_thread": true,
            "code": "local_pi.resume_failed",
            "severity": "error",
            "message": reason,
        }),
    );
    emit_event(
        state,
        run_id,
        Some(LOCAL_PI_SOLVER_ID),
        "worker.status",
        json!({
            "solver_id": LOCAL_PI_SOLVER_ID,
            "main_thread": true,
            "status": "failed",
            "reason": "resume_failed",
            "error": reason,
        }),
    );
    if let Some((cmd_id, action)) = command_id {
        emit_local_pi_receipt(state, run_id, cmd_id, action, "failed", reason);
    }
}

pub struct LocalPiSpawnOptions {
    pub execution_generation: u64,
    pub resume_required: bool,
    pub initial_prompt: Option<String>,
    pub triggered_by_command: Option<(String, String)>,
}

pub fn spawn_local_pi_session(
    state: Arc<AppState>,
    run_id: String,
    opts: LocalPiSpawnOptions,
) {
    if has_live_session(&run_id) {
        return;
    }
    let context=(run_id.clone(),opts.execution_generation);
    tokio::spawn(super::terminal::GENERATION.scope(context,async move {
        start_chat_backed_session(&state, &run_id, &opts).await;
    }));
}

fn fail_local_pi_start(
    state: &Arc<AppState>,
    run_id: &str,
    opts: &LocalPiSpawnOptions,
    code: &str,
    reason: String,
) {
    unregister_session(run_id);
    let mut doc = load_run_doc(state, run_id).unwrap_or_else(|| json!({ "run_id": run_id }));
    if !doc.get("local_pi_session").is_some_and(Value::is_object) {
        doc["local_pi_session"] = json!({});
    }
    doc["kind"] = json!("local_pi");
    doc["local_pi_session"]["adapter_id"] = json!(LOCAL_PI_ADAPTER_ID);
    doc["local_pi_session"]["solver_id"] = json!(LOCAL_PI_SOLVER_ID);
    doc["local_pi_session"]["execution_generation"] = json!(opts.execution_generation);
    doc["local_pi_session"]["session_state"] = json!("failed");
    doc["local_pi_session"]["turn_active"] = json!(false);
    doc["local_pi_session"]["last_error"] = json!({
        "code": code,
        "message": reason,
        "at": now_iso(),
    });
    doc["local_pi_session"]["updated_at"] = json!(now_iso());
    persist_run_doc(state, run_id, &doc);
    emit_event(
        state,
        run_id,
        Some(LOCAL_PI_SOLVER_ID),
        "worker.Alert",
        json!({
            "solver_id": LOCAL_PI_SOLVER_ID,
            "main_thread": true,
            "code": code,
            "severity": "error",
            "message": reason,
        }),
    );
    if let Some((cmd_id, action)) = opts.triggered_by_command.as_ref() {
        emit_local_pi_receipt(state, run_id, cmd_id, action, "failed", &reason);
    }
    mark_run_finished(
        state,
        run_id,
        Some(LOCAL_PI_SOLVER_ID),
        false,
        "preflight_failed",
        Some(&reason),
    );
}

/// Normalises a user-entered local model endpoint to the OpenAI-compatible
/// base URL Pi's `ollama` provider expects.
fn openai_compatible_base_url(endpoint: &str) -> Result<String, String> {
    let trimmed = endpoint.trim().trim_end_matches('/');
    let url = url::Url::parse(trimmed).map_err(|_| format!("模型端点不是合法 URL: {trimmed}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(format!("模型端点必须是 http(s)://host[:port] 形式: {trimmed}"));
    }
    Ok(if trimmed.ends_with("/v1") {
        trimmed.to_owned()
    } else {
        format!("{trimmed}/v1")
    })
}

pub(crate) struct PiAgentDirOverride {
    pub env: (String, String),
    pub base_url: String,
}

/// For `ollama/<id>` models, writes a run-private Pi agent dir whose
/// `models.json` declares the model and points the `ollama` provider at the
/// selected endpoint. Other models keep the account's own Provider config.
fn ollama_agent_env(
    agent_root: &Path,
    model: &str,
    endpoint: Option<&str>,
) -> Result<Option<PiAgentDirOverride>, String> {
    let Some(ollama_model_id) = model.strip_prefix("ollama/") else {
        return Ok(None);
    };
    let endpoint_base = endpoint
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(openai_compatible_base_url)
        .transpose()?;
    let host_agent_dir = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".pi").join("agent"));
    let runtime_agent_dir = agent_root.join(".pi-runtime").join("agent");
    std::fs::create_dir_all(&runtime_agent_dir)
        .map_err(|e| format!("create Pi runtime agent dir: {e}"))?;
    if let Some(host_agent_dir) = &host_agent_dir {
        for file_name in ["auth.json", "settings.json", "trust.json"] {
            let src = host_agent_dir.join(file_name);
            let dst = runtime_agent_dir.join(file_name);
            if src.is_file() && !dst.exists() {
                std::fs::copy(&src, &dst).map_err(|e| format!("copy Pi {file_name}: {e}"))?;
            }
        }
    }
    // Re-derived from the host file on every start so an endpoint change in
    // settings replaces, rather than stacks on, the previous override.
    let host_models = host_agent_dir
        .as_ref()
        .map(|dir| dir.join("models.json"))
        .filter(|path| path.is_file());
    let mut models_doc: Value = match host_models {
        Some(path) => {
            let raw = std::fs::read_to_string(&path).map_err(|e| format!("read Pi models.json: {e}"))?;
            serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?
        }
        None => json!({ "providers": {} }),
    };
    let models_path = runtime_agent_dir.join("models.json");
    if !models_doc.get("providers").is_some_and(Value::is_object) {
        models_doc["providers"] = json!({});
    }
    if !models_doc["providers"].get("ollama").is_some_and(Value::is_object) {
        models_doc["providers"]["ollama"] = json!({
            "api": "openai-completions",
            "apiKey": "ollama",
            "baseUrl": "http://127.0.0.1:11434/v1",
            "models": []
        });
    }
    if let Some(base) = &endpoint_base {
        models_doc["providers"]["ollama"]["baseUrl"] = json!(base);
    }
    if !models_doc["providers"]["ollama"]
        .get("models")
        .is_some_and(Value::is_array)
    {
        models_doc["providers"]["ollama"]["models"] = json!([]);
    }
    if let Some(arr) = models_doc["providers"]["ollama"]["models"].as_array_mut() {
        let exists = arr
            .iter()
            .any(|item| item.get("id").and_then(Value::as_str) == Some(ollama_model_id));
        if !exists {
            arr.push(json!({
                "id": ollama_model_id,
                "input": ["text"],
                "reasoning": true,
                "contextWindow": 131072
            }));
        }
    }
    let base_url = models_doc["providers"]["ollama"]["baseUrl"]
        .as_str()
        .unwrap_or("http://127.0.0.1:11434/v1")
        .to_owned();
    let serialized = serde_json::to_string_pretty(&models_doc)
        .map_err(|e| format!("serialize Pi models.json: {e}"))?;
    std::fs::write(&models_path, serialized).map_err(|e| format!("write Pi models.json: {e}"))?;
    Ok(Some(PiAgentDirOverride {
        env: (
            "PI_CODING_AGENT_DIR".to_owned(),
            runtime_agent_dir.to_string_lossy().into_owned(),
        ),
        base_url,
    }))
}

/// Creates (or reuses) the chat thread bound to a CTF Pi run and sends the
/// first turn. From then on the thread owns the Pi session: model choice,
/// slash commands, compaction, steering and stopping all go through the chat
/// stack, and only the CTF hooks in `apply_chat_start` / `chat_tap` stay here.
async fn start_chat_backed_session(
    state: &Arc<AppState>,
    run_id: &str,
    opts: &LocalPiSpawnOptions,
) {
    let Some(run_doc) = load_run_doc(state, run_id) else {
        return;
    };

    let prior_thread = run_doc
        .pointer("/local_pi_session/chat_thread_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    if opts.resume_required && prior_thread.is_none() {
        record_resume_failure(
            state,
            run_id,
            "本地 Pi 会话恢复失败：该 Run 没有已保存的对话线程，拒绝隐式新建会话",
            opts.triggered_by_command
                .as_ref()
                .map(|(id, act)| (id.as_str(), act.as_str())),
        );
        return;
    }

    let local_cfg = run_doc
        .get("local_pi_config")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let selected_skills: Option<Vec<String>> = local_cfg
        .get("skills")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).map(str::to_owned).collect());

    let (workspace, loaded_skills) =
        match stage_local_pi_workspace(state, run_id, &run_doc, selected_skills.as_deref()) {
            Ok(res) => res,
            Err(err) => {
                fail_local_pi_start(
                    state,
                    run_id,
                    opts,
                    "local_pi.workspace_error",
                    format!("准备本地 Pi 工作区失败: {err}"),
                );
                return;
            }
        };
    let workspace = std::fs::canonicalize(&workspace).unwrap_or(workspace);

    let text_of = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let requested_model = text_of(local_cfg.get("model"))
        .or_else(|| text_of(run_doc.pointer("/effective_configuration/model")));
    let requested_account =
        text_of(local_cfg.get("account_id")).unwrap_or_else(|| "system_login".to_owned());
    let requested_effort = text_of(local_cfg.get("effort"))
        .or_else(|| text_of(run_doc.pointer("/effective_configuration/effort")));
    let requested_access_mode = text_of(local_cfg.get("access_mode"))
        .or_else(|| text_of(run_doc.pointer("/effective_configuration/access_mode")))
        .unwrap_or_else(|| access::FULL_ACCESS.to_owned());

    let Some((desc, _)) = descriptors::for_adapter(LOCAL_PI_ADAPTER_ID) else {
        fail_local_pi_start(
            state,
            run_id,
            opts,
            "local_pi.descriptor_missing",
            format!("未知引擎适配器描述符: {LOCAL_PI_ADAPTER_ID}"),
        );
        return;
    };
    let effective_account =
        match resolve_local_pi_account_id(state, desc.engine(), &requested_account).await {
            Ok(account) => account,
            Err(msg) => {
                fail_local_pi_start(state, run_id, opts, "local_pi.credential_missing", msg);
                return;
            }
        };

    let script = workspace
        .join(".muteki-skills")
        .join("muteki-blackboard")
        .join("blackboard.py");
    let ingress_dir = workspace
        .join(".muteki-blackboard-ingress")
        .join(LOCAL_PI_SOLVER_ID);
    let mut env_pairs: Vec<(String, String)> = vec![
        (
            "MUTEKI_BLACKBOARD_SCRIPT".to_owned(),
            script.to_string_lossy().into_owned(),
        ),
        (
            "MUTEKI_BLACKBOARD_INGRESS_DIR".to_owned(),
            ingress_dir.to_string_lossy().into_owned(),
        ),
        ("MUTEKI_WORKER_ID".to_owned(), LOCAL_PI_SOLVER_ID.to_owned()),
        (
            "MUTEKI_EXECUTION_GENERATION".to_owned(),
            run_doc
                .get("execution_generation")
                .and_then(Value::as_u64)
                .unwrap_or(opts.execution_generation)
                .max(1)
                .to_string(),
        ),
        ("MUTEKI_CHALLENGE_MODE".to_owned(), "ctf".to_owned()),
        ("MUTEKI_BLACKBOARD_ROLE".to_owned(), "solve".to_owned()),
        ("MUTEKI_BLACKBOARD_PROFILE".to_owned(), "solve".to_owned()),
        ("MUTEKI_WORKER_EXECUTION_MODE".to_owned(), "step".to_owned()),
        ("MUTEKI_PLATFORM_CONFIRMATION_REQUIRED".to_owned(), "0".to_owned()),
        ("MUTEKI_TARGET_EPOCH".to_owned(), "1".to_owned()),
    ];
    let requested_endpoint = text_of(local_cfg.get("endpoint"));
    let offline_env = local_cfg
        .get("offline_env")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let agent_override = if requested_account.starts_with("provider:") {
        match local_pi_provider_agent_dir(state, &requested_account, requested_model.as_deref()) {
            Ok(value) => Some(value),
            Err((code, detail)) => {
                fail_local_pi_start(state, run_id, opts, code, detail);
                return;
            }
        }
    } else {
        match requested_model.as_deref() {
            Some(model) => match ollama_agent_env(&workspace, model, requested_endpoint.as_deref()) {
                Ok(value) => value,
                Err(err) => {
                    fail_local_pi_start(
                        state,
                        run_id,
                        opts,
                        "local_pi.endpoint_invalid",
                        format!("准备本地模型端点失败: {err}"),
                    );
                    return;
                }
            },
            None => None,
        }
    };
    let effective_endpoint = match &agent_override {
        Some(found) => found.base_url.clone(),
        None if requested_endpoint.is_some() => {
            "Pi 账号 / Provider 自身配置（端点设置只作用于 ollama/ 模型）".to_owned()
        }
        None => "Pi 账号 / Provider 自身配置".to_owned(),
    };
    if let Some(found) = agent_override {
        env_pairs.push(found.env);
    }
    if offline_env {
        env_pairs.push(("PI_OFFLINE".to_owned(), "1".to_owned()));
    }

    let mut extra_args = vec![
        "--tools".to_owned(),
        "read,bash,edit,write,grep,find,ls".to_owned(),
        "--no-mcp".to_owned(),
        "--no-extensions".to_owned(),
        "--no-prompt-templates".to_owned(),
        "--no-context-files".to_owned(),
        "--no-skills".to_owned(),
    ];
    for skill in &loaded_skills {
        if skill["exists"]
            .as_bool()
            .or_else(|| skill["staged"].as_bool())
            .unwrap_or(true)
            && let Some(rel) = skill["path"].as_str()
        {
            let skill_file = workspace.join(rel);
            if skill_file.exists() {
                extra_args.push("--skill".to_owned());
                extra_args.push(skill_file.to_string_lossy().into_owned());
            }
        }
    }

    let binding = json!({
        "run_id": run_id,
        "cwd": workspace.to_string_lossy(),
        "env": env_pairs.iter().map(|(k, v)| json!([k, v])).collect::<Vec<_>>(),
        "extra_args": extra_args,
        "execution_generation": opts.execution_generation,
    });

    let tid = prior_thread
        .clone()
        .unwrap_or_else(|| format!("ctf-pi-{run_id}"));
    let title = run_doc
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("CTF Pi")
        .to_owned();

    if crate::chat::load(state, &tid).is_err() {
        let mut runtime = json!({
            "adapter_id": LOCAL_PI_ADAPTER_ID,
            "instance_id": "default",
            "credential_id": effective_account,
            "access_mode": requested_access_mode,
        });
        if let Some(model) = &requested_model {
            runtime["model"] = json!(model);
        }
        if let Some(effort) = &requested_effort {
            runtime["effort"] = json!(effort);
        }
        let body = json!({
            "command_id": format!("ctf-pi-create:{run_id}"),
            "thread_id": tid,
            "ctf_run_id": run_id,
            "title": title,
            "title_source": "user",
            "runtime": runtime,
        });
        if let Err(failure) =
            crate::chat::create(State(state.clone()), Json(body)).await
        {
            fail_local_pi_start(
                state,
                run_id,
                opts,
                "local_pi.thread_create_failed",
                format!("创建 Pi 对话线程失败: {}", failure_text(&failure)),
            );
            return;
        }
    }
    {
        let _guard = state.chat.active.lock().await;
        let bound = crate::chat::load(state, &tid).and_then(|mut v| {
            v["ctf_binding"] = binding;
            v["thread"]["ctf_run_id"] = json!(run_id);
            crate::chat::save(state, &tid, &mut v)
        });
        if let Err(failure) = bound {
            fail_local_pi_start(
                state,
                run_id,
                opts,
                "local_pi.thread_bind_failed",
                format!("绑定 Pi 对话线程失败: {}", failure_text(&failure)),
            );
            return;
        }
    }

    let prev_turn_count = run_doc
        .pointer("/local_pi_session/turn_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let mut doc = load_run_doc(state, run_id).unwrap_or(run_doc);
    if !doc.get("local_pi_session").is_some_and(Value::is_object) {
        doc["local_pi_session"] = json!({});
    }
    doc["kind"] = json!("local_pi");
    doc["status"] = json!("running");
    doc["running"] = json!(true);
    doc["awaiting_operator"] = Value::Null;
    doc["chat_thread_id"] = json!(tid);
    doc["local_pi_session"]["chat_thread_id"] = json!(tid);
    doc["local_pi_session"]["adapter_id"] = json!(LOCAL_PI_ADAPTER_ID);
    doc["local_pi_session"]["solver_id"] = json!(LOCAL_PI_SOLVER_ID);
    doc["local_pi_session"]["execution_generation"] = json!(opts.execution_generation);
    doc["local_pi_session"]["session_state"] = json!("starting");
    doc["local_pi_session"]["turn_active"] = json!(true);
    doc["local_pi_session"]["turn_count"] = json!(prev_turn_count);
    doc["local_pi_session"]["resumed"] = json!(prior_thread.is_some());
    doc["local_pi_session"]["model"] = json!({
        "id": requested_model.clone().unwrap_or_else(|| "default".to_owned()),
        "account_id": effective_account,
        "endpoint": effective_endpoint,
        "effort": requested_effort,
        "access_mode": requested_access_mode,
        "readiness": "starting",
        "context_window": Value::Null,
        "max_tokens": Value::Null,
        "cost": Value::Null,
    });
    doc["local_pi_session"]["loaded_tools"] = json!(loaded_tools_manifest());
    doc["local_pi_session"]["disabled_capabilities"] = json!(disabled_capabilities_manifest());
    doc["local_pi_session"]["loaded_skills"] = json!(loaded_skills);
    doc["local_pi_session"]["environment"] = verify_isolation_environment(&workspace, offline_env);
    doc["local_pi_session"]["updated_at"] = json!(now_iso());
    doc["updated"] = json!(now_secs());
    persist_run_doc(state, run_id, &doc);

    emit_event(
        state,
        run_id,
        Some(LOCAL_PI_SOLVER_ID),
        "worker.spawned",
        json!({
            "solver_id": LOCAL_PI_SOLVER_ID,
            "cli": "pi",
            "adapter_id": LOCAL_PI_ADAPTER_ID,
            "model": requested_model.clone().unwrap_or_else(|| "default".to_owned()),
            "backend": "local_pi",
            "execution_generation": opts.execution_generation,
            "resumed": prior_thread.is_some(),
            "main_thread": true,
            "chat_thread_id": tid,
        }),
    );
    emit_event(
        state,
        run_id,
        Some(LOCAL_PI_SOLVER_ID),
        "local_pi.session",
        json!({
            "solver_id": LOCAL_PI_SOLVER_ID,
            "main_thread": true,
            "chat_thread_id": tid,
            "resumed": prior_thread.is_some(),
            "execution_generation": opts.execution_generation,
        }),
    );

    LOCAL_PI_TAPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(run_id);
    register_session(run_id, &tid, state);
    ensure_ingress_ticker(state, run_id, workspace.clone());

    let first_text = if prior_thread.is_some() {
        let request = opts.initial_prompt.clone().unwrap_or_else(|| {
            "请基于当前会话上下文与工作区材料继续分析，给出下一步验证结果或整理当前发现。"
                .to_owned()
        });
        // A resumed Pi session keeps the system prompt it started with, so a
        // saved state card can only reach it through the turn. Point at the file
        // instead of pasting it, which would flood the visible chat.
        match read_state_card(&workspace) {
            Some(_) => format!(
                "（工作区里有上次保存的状态卡 `./work/STATE_CARD.md`，请先读取它再继续。）\n\n{request}"
            ),
            None => request,
        }
    } else {
        local_cfg
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| opts.initial_prompt.clone().filter(|s| !s.trim().is_empty()))
            .unwrap_or_else(|| DEFAULT_LOCAL_PI_REQUEST.to_owned())
    };
    let command_id = format!("ctf-pi-start:{run_id}:{}", uuid::Uuid::new_v4().simple());
    match chat_command(state, &tid, chat_send_body(&command_id, &first_text)).await {
        Ok(_) => {
            if let Some((cmd_id, action)) = opts.triggered_by_command.as_ref() {
                emit_local_pi_receipt(
                    state,
                    run_id,
                    cmd_id,
                    action,
                    "applied",
                    "已在 Pi 对话线程启动新一轮分析",
                );
            }
        }
        Err(reason) => {
            let reason = format!("向 Pi 对话线程发送初始请求失败: {reason}");
            if prior_thread.is_some() {
                record_resume_failure(
                    state,
                    run_id,
                    &reason,
                    opts.triggered_by_command
                        .as_ref()
                        .map(|(id, act)| (id.as_str(), act.as_str())),
                );
                unregister_session(run_id);
            } else {
                fail_local_pi_start(state, run_id, opts, "local_pi.send_turn_failed", reason);
            }
        }
    }
}

/// Drains the blackboard ingress directory while the Pi session is alive, so a
/// `submit-flag` from the model reaches the Run without waiting for a turn to
/// end. It stops once the session has existed and then gone away; the next
/// session start (`chat_tap`) starts it again.
fn ensure_ingress_ticker(state: &Arc<AppState>, run_id: &str, workspace: PathBuf) {
    if !LOCAL_PI_TICKERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(run_id.to_owned())
    {
        return;
    }
    let state = state.clone();
    let run_id = run_id.to_owned();
    tokio::spawn(async move {
        let ingress_dir = workspace
            .join(".muteki-blackboard-ingress")
            .join(LOCAL_PI_SOLVER_ID);
        let started = Instant::now();
        let mut alive_seen = false;
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(120));
        loop {
            tick.tick().await;
            let Some((tid, _)) = live_handle(&run_id) else {
                break;
            };
            match state.agents.get(&tid).await {
                Some(session) if session.is_open() => alive_seen = true,
                _ => {
                    if alive_seen || started.elapsed() > std::time::Duration::from_secs(180) {
                        break;
                    }
                }
            }
            let _ = super::worker::drain_blackboard_ingress(
                &state,
                &run_id,
                LOCAL_PI_SOLVER_ID,
                &ingress_dir,
                &workspace,
            );
        }
        let _ = super::worker::drain_blackboard_ingress(
            &state,
            &run_id,
            LOCAL_PI_SOLVER_ID,
            &ingress_dir,
            &workspace,
        );
        LOCAL_PI_TICKERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&run_id);
    });
}

/// Start-time overrides for a chat thread that is bound to a CTF Pi run:
/// the CTF instructions, the workspace tool restrictions and the blackboard
/// environment replace the generic chat session extras. Returns false for
/// ordinary threads.
pub(crate) fn apply_chat_start(
    state: &AppState,
    doc: &Value,
    request: &mut StartRequest,
) -> bool {
    let Some(binding) = doc.get("ctf_binding").filter(|b| b.is_object()) else {
        return false;
    };
    let run_id = binding["run_id"].as_str().unwrap_or("");
    let workspace = PathBuf::from(binding["cwd"].as_str().unwrap_or(""));
    let run_doc = load_run_doc(state, run_id).unwrap_or_else(|| json!({}));
    request.instructions = Some(format!(
        "{LOCAL_PI_SYSTEM_PROMPT}\n\n---\n\n{}",
        build_challenge_brief(&run_doc, &workspace)
    ));
    request.mcp_servers.clear();
    for row in binding["env"].as_array().into_iter().flatten() {
        if let (Some(key), Some(value)) = (row[0].as_str(), row[1].as_str()) {
            request.env.retain(|(name, _)| name != key);
            request.env.push((key.to_owned(), value.to_owned()));
        }
    }
    request.extra_args.extend(
        binding["extra_args"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|arg| arg.as_str().map(str::to_owned)),
    );
    true
}

fn write_tool_artifact(
    state: &AppState,
    run_id: &str,
    workspace: &Path,
    item_id: &str,
    tool_name: &str,
    command: String,
    status: ItemStatus,
    exit_code: Value,
    input: Value,
    output: String,
    referenced: Vec<String>,
    started: Instant,
) {
    let Some(doc) = load_run_doc(state, run_id) else {
        return;
    };
    let seq = doc
        .pointer("/local_pi_session/artifact_seq")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        + 1;
    let turn_index = doc
        .pointer("/local_pi_session/turn_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let art_id = format!("art-{seq:04}");
    let rel_art = format!("arts/{art_id}.json");
    let duration_ms = started.elapsed().as_millis() as u64;
    let record = json!({
        "seq": seq,
        "artifact_id": art_id,
        "tool_call_id": item_id,
        "tool_name": tool_name,
        "command": command,
        "status": format!("{status:?}"),
        "exit_code": exit_code,
        "duration_ms": duration_ms,
        "input": input,
        "output": output,
        "content": output,
        "referenced_files": referenced,
        "turn_index": turn_index,
        "created_at": now_iso(),
    });
    let _ = std::fs::write(
        workspace.join(&rel_art),
        serde_json::to_string_pretty(&record).unwrap_or_default(),
    );
    record_tool_artifact_on_run(state, run_id, seq, &rel_art, item_id, tool_name, &referenced);
    let preview = output.chars().take(500).collect::<String>();
    emit_event(
        state,
        run_id,
        Some(LOCAL_PI_SOLVER_ID),
        "tool.result",
        json!({
            "solver_id": LOCAL_PI_SOLVER_ID,
            "main_thread": true,
            "artifact_id": art_id,
            "tool": tool_name,
            "tool_call_id": item_id,
            "call_id": item_id,
            "tool_name": tool_name,
            "success": !matches!(status, ItemStatus::Failed),
            "duration_ms": duration_ms,
            "result_preview": preview,
            "result": {"condensed": format!("[artifact_id={art_id}] {preview}")},
            "artifact_path": rel_art,
            "referenced_files": referenced,
            "turn_index": turn_index,
        }),
    );
}

/// CTF-side bookkeeping for a bound chat thread. The chat `pump` calls this
/// for every Pi event under its thread lock, so it must stay synchronous and
/// quick: it records artifacts and candidate flags, keeps token totals, and
/// mirrors turn state onto the Run. The thread transcript itself is written by
/// the chat translator, not here.
fn update_local_pi_context(
    doc: &mut Value,
    used_tokens: Option<u64>,
    limit: Option<u64>,
    total_processed_tokens: Option<u64>,
) {
    if used_tokens.is_none() && limit.is_none() && total_processed_tokens.is_none() {
        return;
    }
    let prev = doc
        .pointer("/local_pi_session/context")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let next_used = used_tokens
        .map(|v| json!(v))
        .unwrap_or_else(|| prev["used_tokens"].clone());
    let next_limit = limit
        .map(|v| json!(v))
        .unwrap_or_else(|| prev["limit"].clone());
    let next_processed = total_processed_tokens
        .map(|v| json!(v))
        .unwrap_or_else(|| prev["total_processed_tokens"].clone());
    doc["local_pi_session"]["context"] = json!({
        "used_tokens": next_used,
        "limit": next_limit,
        "total_processed_tokens": next_processed,
        "updated_at": now_iso(),
        "source": "usage_event",
    });
    if let Some(cw) = limit {
        if !doc["local_pi_session"]["model"].is_object() {
            doc["local_pi_session"]["model"] = json!({});
        }
        doc["local_pi_session"]["model"]["context_window"] = json!(cw);
    }
}

pub fn read_latest_pi_session_context_used(path: &Path) -> Option<u64> {
    let raw = std::fs::read_to_string(path).ok()?;
    for line in raw.lines().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if val.pointer("/message/role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(usage) = val.pointer("/message/usage").filter(|v| v.is_object()) else {
            continue;
        };
        if let Some(total) = usage
            .get("totalTokens")
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
        {
            return Some(total);
        }
        let fallback_sum = ["input", "output", "cacheRead", "cacheWrite"]
            .iter()
            .filter_map(|k| usage.get(*k).and_then(Value::as_u64))
            .sum::<u64>();
        if fallback_sum > 0 {
            return Some(fallback_sum);
        }
    }
    None
}

pub(crate) fn chat_tap(
    state: &Arc<AppState>,
    tid: &str,
    binding: &Value,
    event: &AgentEvent,
) {
    let Some(run_id) = binding["run_id"].as_str() else {
        return;
    };
    let mut current = event;
    loop {
        match current {
            AgentEvent::Origin {
                agent_id: None,
                event,
                ..
            } => current = &**event,
            AgentEvent::Origin { .. } => return,
            _ => break,
        }
    }
    let workspace = PathBuf::from(binding["cwd"].as_str().unwrap_or(""));
    let mut taps = LOCAL_PI_TAPS.lock().unwrap_or_else(|e| e.into_inner());
    let tap = taps.entry(run_id.to_owned()).or_default();
    if !tap.flags_seeded {
        tap.flags_seeded = true;
        if let Some(doc) = load_run_doc(state, run_id) {
            for flag in doc.get("flags").and_then(Value::as_array).into_iter().flatten() {
                if let Some(value) = flag
                    .as_str()
                    .or_else(|| flag.get("value").and_then(Value::as_str))
                {
                    tap.seen_flags.insert(value.to_owned());
                }
            }
        }
    }

    match current {
        AgentEvent::SessionStarted {
            provider_session_id,
            resume,
            model,
        } => {
            if !has_live_session(run_id) {
                register_session(run_id, tid, state);
            }
            ensure_ingress_ticker(state, run_id, workspace.clone());
            let exec_gen = load_run_doc(state, run_id)
                .and_then(|doc| doc.get("execution_generation").and_then(Value::as_u64))
                .unwrap_or(1);
            let _ = continuation::save_local_pi(
                state,
                run_id,
                LOCAL_PI_SOLVER_ID,
                &workspace,
                &resume.0,
                Some(provider_session_id),
                exec_gen,
            );
            if let Some(mut doc) = load_run_doc(state, run_id) {
                doc["local_pi_session"]["provider_session_id"] = json!(provider_session_id);
                doc["local_pi_session"]["resume_cursor"] = resume.0.clone();
                if let Some(m) = model {
                    doc["local_pi_session"]["model"]["id"] = json!(m);
                }
                doc["local_pi_session"]["model"]["readiness"] = json!("ready");
                doc["local_pi_session"]["updated_at"] = json!(now_iso());
                persist_run_doc(state, run_id, &doc);
            }
            emit_event(
                state,
                run_id,
                Some(LOCAL_PI_SOLVER_ID),
                "local_pi.session",
                json!({
                    "solver_id": LOCAL_PI_SOLVER_ID,
                    "main_thread": true,
                    "chat_thread_id": tid,
                    "provider_session_id": provider_session_id,
                    "model": model,
                }),
            );
        }
        AgentEvent::TokenUsage(usage) => {
            if usage.used_tokens.is_some()
                || usage.context_window.is_some()
                || usage.total_processed_tokens.is_some()
            {
                drop(taps);
                if let Some(mut doc) = load_run_doc(state, run_id) {
                    update_local_pi_context(
                        &mut doc,
                        usage.used_tokens,
                        usage.context_window,
                        usage.total_processed_tokens,
                    );
                    persist_run_doc(state, run_id, &doc);
                }
            }
        }
        AgentEvent::TurnStarted { .. } => {
            if let Some(mut doc) = load_run_doc(state, run_id) {
                let turn_count = doc
                    .pointer("/local_pi_session/turn_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    + 1;
                doc["status"] = json!("running");
                doc["running"] = json!(true);
                doc["awaiting_operator"] = Value::Null;
                doc["local_pi_session"]["session_state"] = json!("running");
                doc["local_pi_session"]["turn_active"] = json!(true);
                doc["local_pi_session"]["turn_count"] = json!(turn_count);
                doc["local_pi_session"]["updated_at"] = json!(now_iso());
                doc["updated"] = json!(now_secs());
                persist_run_doc(state, run_id, &doc);
                emit_event(
                    state,
                    run_id,
                    Some(LOCAL_PI_SOLVER_ID),
                    "local_pi.session",
                    json!({
                        "solver_id": LOCAL_PI_SOLVER_ID,
                        "main_thread": true,
                        "chat_thread_id": tid,
                        "turn_index": turn_count,
                    }),
                );
                emit_event(
                    state,
                    run_id,
                    Some(LOCAL_PI_SOLVER_ID),
                    "worker.status",
                    json!({
                        "solver_id": LOCAL_PI_SOLVER_ID,
                        "main_thread": true,
                        "status": "running",
                        "turn_index": turn_count,
                    }),
                );
            }
        }
        AgentEvent::ItemStarted(item) => match &item.content {
            ItemContent::CommandExecution { command, .. } => {
                tap.tool_starts.insert(
                    item.id.clone(),
                    ("bash".to_owned(), Instant::now(), json!({ "command": command })),
                );
            }
            ItemContent::ToolCall { name, input, .. } => {
                tap.tool_starts
                    .insert(item.id.clone(), (name.clone(), Instant::now(), input.clone()));
            }
            ItemContent::FileChange { changes, .. } => {
                let paths: Vec<String> = changes.iter().map(|c| c.path.clone()).collect();
                tap.tool_starts.insert(
                    item.id.clone(),
                    ("file_change".to_owned(), Instant::now(), json!({ "paths": paths })),
                );
            }
            _ => {}
        },
        AgentEvent::ItemCompleted(item) => {
            let mut candidate_texts: Vec<String> = Vec::new();
            match &item.content {
                ItemContent::AssistantMessage { text } => {
                    candidate_texts.push(text.clone());
                }
                ItemContent::CommandExecution {
                    command,
                    output,
                    exit_code,
                    status,
                } => {
                    candidate_texts.push(output.clone());
                    let (_, started, input) = tap.tool_starts.remove(&item.id).unwrap_or_else(|| {
                        ("bash".to_owned(), Instant::now(), json!({ "command": command }))
                    });
                    let referenced = extract_referenced_files(&input, output.as_str());
                    write_tool_artifact(
                        state,
                        run_id,
                        &workspace,
                        &item.id,
                        "bash",
                        command.clone(),
                        *status,
                        json!(exit_code),
                        input,
                        output.clone(),
                        referenced,
                        started,
                    );
                }
                ItemContent::ToolCall {
                    name,
                    input,
                    output,
                    status,
                    ..
                } => {
                    let out = output.clone().unwrap_or_default();
                    candidate_texts.push(out.clone());
                    let (started_name, started, started_input) = tap
                        .tool_starts
                        .remove(&item.id)
                        .unwrap_or_else(|| (name.clone(), Instant::now(), input.clone()));
                    let tool_name = if name.is_empty() { started_name } else { name.clone() };
                    let eff_input = if input.is_null() { started_input } else { input.clone() };
                    let referenced = extract_referenced_files(&eff_input, &out);
                    write_tool_artifact(
                        state,
                        run_id,
                        &workspace,
                        &item.id,
                        &tool_name,
                        format!("{tool_name} {eff_input}"),
                        *status,
                        Value::Null,
                        eff_input,
                        out,
                        referenced,
                        started,
                    );
                }
                ItemContent::FileChange { changes, status } => {
                    let (_, started, input) = tap
                        .tool_starts
                        .remove(&item.id)
                        .unwrap_or_else(|| ("file_change".to_owned(), Instant::now(), json!({})));
                    let paths: Vec<String> = changes.iter().map(|c| c.path.clone()).collect();
                    write_tool_artifact(
                        state,
                        run_id,
                        &workspace,
                        &item.id,
                        "file_change",
                        "file_change".to_owned(),
                        *status,
                        Value::Null,
                        input,
                        paths.join("\n"),
                        paths,
                        started,
                    );
                }
                _ => {}
            }
            let flag_format = load_run_doc(state, run_id)
                .and_then(|doc| doc.get("flag_format").and_then(Value::as_str).map(str::to_owned))
                .unwrap_or_else(|| "flag{...}".to_owned());
            for text in &candidate_texts {
                for flag in extract_candidate_flags(text, &flag_format) {
                    if tap.seen_flags.insert(flag.clone()) {
                        emit_event(
                            state,
                            run_id,
                            Some(LOCAL_PI_SOLVER_ID),
                            "local_pi.flag_candidate",
                            json!({
                                "solver_id": LOCAL_PI_SOLVER_ID,
                                "main_thread": true,
                                "flag": flag,
                            }),
                        );
                    }
                }
            }
        }
        AgentEvent::TurnCompleted { status, usage, .. } => {
            tap.tool_starts.clear();
            let candidate_flags_count = tap.seen_flags.len();
            drop(taps);
            let ingress_dir = workspace
                .join(".muteki-blackboard-ingress")
                .join(LOCAL_PI_SOLVER_ID);
            let _ = super::worker::drain_blackboard_ingress(
                state,
                run_id,
                LOCAL_PI_SOLVER_ID,
                &ingress_dir,
                &workspace,
            );
            let interrupted = matches!(status, TurnStatus::Interrupted);
            let Some(mut doc) = load_run_doc(state, run_id) else {
                return;
            };
            let read_u64 = |doc: &Value, path: &str| {
                doc.pointer(path).and_then(Value::as_u64).unwrap_or(0)
            };
            let mut input_tokens = read_u64(&doc, "/token_usage/input_tokens");
            let mut output_tokens = read_u64(&doc, "/token_usage/output_tokens");
            let mut cached_tokens = read_u64(&doc, "/token_usage/cached_input_tokens");
            let turn_usage = match usage {
                Some(u) => {
                    let in_tok = u.input_tokens.unwrap_or(0);
                    let out_tok = u.output_tokens.unwrap_or(0);
                    input_tokens = input_tokens.saturating_add(in_tok);
                    output_tokens = output_tokens.saturating_add(out_tok);
                    cached_tokens =
                        cached_tokens.saturating_add(u.cached_input_tokens.unwrap_or(0));
                    json!({
                        "input_tokens": u.input_tokens,
                        "output_tokens": u.output_tokens,
                        "cached_input_tokens": u.cached_input_tokens,
                        "total_tokens": in_tok + out_tok,
                        "cost": u.cost_usd,
                    })
                }
                None => Value::Null,
            };
            let total_tokens = input_tokens + output_tokens;
            let is_solved = doc.get("solved").and_then(Value::as_bool) == Some(true);
            let turn_count = read_u64(&doc, "/local_pi_session/turn_count");
            doc["tokens_used"] = json!(total_tokens);
            doc["token_usage"] = json!({
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "cached_input_tokens": cached_tokens,
                "total_tokens": total_tokens,
            });
            doc["local_pi_session"]["turn_active"] = json!(false);
            doc["local_pi_session"]["queued_messages"] = json!(0);
            doc["local_pi_session"]["last_turn_usage"] = turn_usage.clone();
            doc["local_pi_session"]["last_turn_completed_at"] = json!(now_iso());
            doc["local_pi_session"]["updated_at"] = json!(now_iso());
            if let Some(u) = usage {
                update_local_pi_context(
                    &mut doc,
                    u.used_tokens,
                    u.context_window,
                    u.total_processed_tokens,
                );
            }
            doc["updated"] = json!(now_secs());
            if !interrupted && is_solved {
                doc["awaiting_operator"] = Value::Null;
                doc["local_pi_session"]["session_state"] = json!("stopped");
                persist_run_doc(state, run_id, &doc);
                emit_event(
                    state,
                    run_id,
                    Some(LOCAL_PI_SOLVER_ID),
                    "local_pi.turn_completed",
                    json!({
                        "solver_id": LOCAL_PI_SOLVER_ID,
                        "main_thread": true,
                        "turn_index": turn_count,
                        "interrupted": false,
                        "solved": true,
                        "usage": turn_usage,
                        "candidate_flags_count": candidate_flags_count,
                    }),
                );
                emit_event(
                    state,
                    run_id,
                    Some(LOCAL_PI_SOLVER_ID),
                    "worker.status",
                    json!({
                        "solver_id": LOCAL_PI_SOLVER_ID,
                        "main_thread": true,
                        "status": "solved",
                        "reason": "solved",
                        "turn_index": turn_count,
                    }),
                );
                unregister_session(run_id);
                mark_run_finished(
                    state,
                    run_id,
                    Some(LOCAL_PI_SOLVER_ID),
                    true,
                    "solved",
                    Some("本地 Pi 已解出并提交 Flag"),
                );
                let close_state = state.clone();
                let close_tid = tid.to_owned();
                tokio::spawn(async move {
                    native_chat::close(&close_state, &close_tid, "solved").await;
                });
            } else {
                let awaiting_reason = if interrupted {
                    "turn_interrupted"
                } else {
                    "turn_completed"
                };
                doc["running"] = json!(false);
                doc["status"] = json!(if is_solved {
                    "solved"
                } else if interrupted {
                    "interrupted"
                } else {
                    "paused"
                });
                doc["awaiting_operator"] = json!(awaiting_reason);
                doc["local_pi_session"]["session_state"] =
                    json!(if interrupted { "interrupted" } else { "awaiting_operator" });
                persist_run_doc(state, run_id, &doc);
                emit_event(
                    state,
                    run_id,
                    Some(LOCAL_PI_SOLVER_ID),
                    "local_pi.turn_completed",
                    json!({
                        "solver_id": LOCAL_PI_SOLVER_ID,
                        "main_thread": true,
                        "turn_index": turn_count,
                        "interrupted": interrupted,
                        "usage": turn_usage,
                        "candidate_flags_count": candidate_flags_count,
                    }),
                );
                emit_event(
                    state,
                    run_id,
                    Some(LOCAL_PI_SOLVER_ID),
                    "worker.status",
                    json!({
                        "solver_id": LOCAL_PI_SOLVER_ID,
                        "main_thread": true,
                        "status": "idle",
                        "reason": awaiting_reason,
                        "turn_index": turn_count,
                    }),
                );
            }
        }
        AgentEvent::Error { message, fatal } => {
            emit_event(
                state,
                run_id,
                Some(LOCAL_PI_SOLVER_ID),
                "worker.Alert",
                json!({
                    "solver_id": LOCAL_PI_SOLVER_ID,
                    "main_thread": true,
                    "code": "local_pi.runtime_error",
                    "severity": if *fatal { "error" } else { "warning" },
                    "message": message,
                }),
            );
            if let Some(mut doc) = load_run_doc(state, run_id) {
                doc["local_pi_session"]["last_error"] = json!({
                    "code": "local_pi.runtime_error",
                    "message": message,
                    "fatal": fatal,
                    "at": now_iso(),
                });
                if *fatal {
                    doc["running"] = json!(false);
                    doc["status"] = json!("idle");
                    doc["awaiting_operator"] = json!("session_error");
                    doc["local_pi_session"]["session_state"] = json!("failed");
                    doc["local_pi_session"]["turn_active"] = json!(false);
                }
                persist_run_doc(state, run_id, &doc);
            }
        }
        _ => {}
    }
}

fn extract_referenced_files(input: &Value, output: &str) -> Vec<String> {
    let mut refs = Vec::new();
    for key in ["path", "file", "file_path", "filename", "target"] {
        if let Some(p) = input.get(key).and_then(Value::as_str) {
            let trimmed = p.trim();
            if !trimmed.is_empty() && !refs.contains(&trimmed.to_owned()) {
                refs.push(trimmed.to_owned());
            }
        }
    }
    if let Some(cmd) = input.get("command").and_then(Value::as_str) {
        for token in cmd.split_whitespace() {
            let clean =
                token.trim_matches(|c: char| matches!(c, '"' | '\'' | ';' | ',' | ')' | '('));
            if (clean.starts_with("uploads/")
                || clean.starts_with("work/")
                || clean.ends_with(".py")
                || clean.ends_with(".txt")
                || clean.ends_with(".json")
                || clean.ends_with(".bin"))
                && !clean.contains("..")
                && !refs.contains(&clean.to_owned())
            {
                refs.push(clean.to_owned());
            }
        }
    }
    let _ = output;
    refs
}

fn record_tool_artifact_on_run(
    state: &AppState,
    run_id: &str,
    seq: u64,
    rel_art: &str,
    tool_call_id: &str,
    tool_name: &str,
    referenced_files: &[String],
) {
    let Some(mut doc) = load_run_doc(state, run_id) else {
        return;
    };
    doc["local_pi_session"]["artifact_seq"] = json!(seq);
    let mut list = doc
        .pointer("/local_pi_session/recorded_artifacts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    list.push(json!({
        "seq": seq,
        "artifact_id": format!("art-{seq:04}"),
        "artifact_path": rel_art,
        "tool_call_id": tool_call_id,
        "tool_name": tool_name,
        "referenced_files": referenced_files,
        "created_at": now_iso(),
    }));
    doc["local_pi_session"]["recorded_artifacts"] = Value::Array(list);
    persist_run_doc(state, run_id, &doc);
}

/// Builds the separated Tools/Skills vs Materials/Artifacts inspector payload (Section 6, 7, 10).
fn resolve_inspector_context(
    state: &AppState,
    session_meta: &Value,
    run_doc: &Value,
    resume_check: &Result<ValidatedResumeHandle, String>,
    model_obj: &mut Value,
) -> Value {
    let chat_thread_id = session_meta
        .get("chat_thread_id")
        .or_else(|| run_doc.get("chat_thread_id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let thread_doc = chat_thread_id.and_then(|tid| crate::chat::load(state, tid).ok());

    let mut limit = session_meta
        .pointer("/context/limit")
        .and_then(Value::as_u64)
        .or_else(|| model_obj.get("context_window").and_then(Value::as_u64))
        .or_else(|| {
            thread_doc.as_ref().and_then(|d| {
                d.pointer("/context_window/limit")
                    .or_else(|| d.pointer("/thread/state/usage/model_context_window"))
                    .and_then(Value::as_u64)
            })
        });
    if limit.is_none() {
        if let Some(target_m) = model_obj.get("id").and_then(Value::as_str) {
            for item in read_host_pi_models() {
                if item.get("id").and_then(Value::as_str) == Some(target_m)
                    || item.get("raw_name").and_then(Value::as_str) == Some(target_m)
                {
                    if let Some(cw) = item.get("context_window").and_then(Value::as_u64) {
                        limit = Some(cw);
                        break;
                    }
                }
            }
        }
    }
    if model_obj.get("context_window").is_none_or(Value::is_null) {
        if let Some(cw) = limit {
            if !model_obj.is_object() {
                *model_obj = json!({});
            }
            model_obj["context_window"] = json!(cw);
        }
    }

    let run_processed_sum = ["input_tokens", "output_tokens", "cached_input_tokens"]
        .iter()
        .filter_map(|k| {
            run_doc
                .get("token_usage")
                .and_then(|tu| tu.get(*k))
                .and_then(Value::as_u64)
        })
        .sum::<u64>();
    let total_processed_tokens = session_meta
        .pointer("/context/total_processed_tokens")
        .and_then(Value::as_u64)
        .or_else(|| {
            thread_doc.as_ref().and_then(|d| {
                d.pointer("/thread/state/usage/total_processed_tokens")
                    .and_then(Value::as_u64)
            })
        })
        .or_else(|| (run_processed_sum > 0).then_some(run_processed_sum));

    let live_used = session_meta
        .pointer("/context/used_tokens")
        .and_then(Value::as_u64);
    let thread_used = thread_doc
        .as_ref()
        .and_then(|d| d.pointer("/context_window/total").and_then(Value::as_u64))
        .filter(|t| {
            let looks_summed =
                total_processed_tokens == Some(*t) && limit.is_some_and(|l| *t > l);
            !looks_summed
        });
    let file_used = if live_used.is_none() && thread_used.is_none() {
        resume_check
            .as_ref()
            .ok()
            .and_then(|h| read_latest_pi_session_context_used(Path::new(&h.session_file)))
    } else {
        None
    };

    let (used_tokens, context_source) = if let Some(u) = live_used {
        (Some(u), "live")
    } else if let Some(u) = thread_used {
        (Some(u), "thread")
    } else if let Some(u) = file_used {
        (Some(u), "session_file")
    } else if limit.is_some() {
        (None, "model_catalog")
    } else {
        (None, "unknown")
    };

    let updated_at = session_meta
        .pointer("/context/updated_at")
        .or_else(|| {
            thread_doc
                .as_ref()
                .and_then(|d| d.pointer("/context_window/updated_at"))
        })
        .cloned()
        .unwrap_or(Value::Null);

    json!({
        "used_tokens": used_tokens,
        "limit": limit,
        "total_processed_tokens": total_processed_tokens,
        "source": context_source,
        "updated_at": updated_at,
    })
}

pub fn build_inspector_snapshot(state: &AppState, run_id: &str, run_doc: &Value) -> Value {
    let workspace = session_dir(state, run_id);
    let session_meta = run_doc
        .get("local_pi_session")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let cont = continuation::load(state, run_id, LOCAL_PI_SOLVER_ID).ok().flatten();
    let resume_check = validate_saved_resume(state, run_id, run_doc);

    let mut skills_list = session_meta
        .get("loaded_skills")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for skill in &mut skills_list {
        if let Some(rel) = skill.get("path").and_then(Value::as_str) {
            let full = workspace.join(rel);
            let exists = full.is_file();
            skill["exists"] = json!(exists);
        }
    }

    let uploads_dir = workspace.join("uploads");
    let mut uploads = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&uploads_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let fname = entry.file_name().to_string_lossy().into_owned();
            let meta = std::fs::metadata(&path).ok();
            let readable = std::fs::File::open(&path).is_ok();
            uploads.push(json!({
                "name": fname,
                "rel_path": format!("uploads/{fname}"),
                "size_bytes": meta.as_ref().map(|m| m.len()).unwrap_or(0),
                "exists": meta.is_some(),
                "readable": readable,
                "kind": "upload",
            }));
        }
    }
    uploads.sort_by(|a, b| {
        a.get("name")
            .and_then(Value::as_str)
            .cmp(&b.get("name").and_then(Value::as_str))
    });

    let mut workspace_files = Vec::new();
    collect_workspace_files(&workspace, &workspace, &mut workspace_files);
    workspace_files.sort_by(|a, b| {
        a.get("rel_path")
            .and_then(Value::as_str)
            .cmp(&b.get("rel_path").and_then(Value::as_str))
    });

    let mut tool_artifacts = Vec::new();
    let recorded = session_meta
        .get("recorded_artifacts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut seen_art_paths = HashSet::new();

    for rec in &recorded {
        let rel_path = rec
            .get("artifact_path")
            .and_then(Value::as_str)
            .unwrap_or("");
        if rel_path.is_empty() {
            continue;
        }
        seen_art_paths.insert(rel_path.to_owned());
        let full = workspace.join(rel_path);
        let exists = full.is_file();
        let content = if exists {
            std::fs::read_to_string(&full).ok()
        } else {
            None
        };
        let readable = content.is_some();
        let parsed: Option<Value> = content
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok());

        let ref_files = rec
            .get("referenced_files")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let verified_refs: Vec<Value> = ref_files
            .iter()
            .filter_map(Value::as_str)
            .map(|r| {
                let ref_full = workspace.join(r);
                let ref_exists = ref_full.exists();
                let ref_readable = std::fs::File::open(&ref_full).is_ok();
                json!({
                    "path": r,
                    "exists": ref_exists,
                    "readable": ref_readable,
                    "status": if !ref_exists {
                        "missing"
                    } else if !ref_readable {
                        "unreadable"
                    } else {
                        "ok"
                    },
                })
            })
            .collect();

        tool_artifacts.push(json!({
            "seq": rec.get("seq"),
            "artifact_id": rec.get("artifact_id"),
            "rel_path": rel_path,
            "tool_call_id": rec.get("tool_call_id"),
            "tool_name": rec.get("tool_name"),
            "created_at": rec.get("created_at"),
            "exists": exists,
            "readable": readable,
            "status": if !exists {
                "missing"
            } else if !readable {
                "unreadable"
            } else {
                "ok"
            },
            "preview": parsed
                .as_ref()
                .and_then(|p| p.get("output"))
                .and_then(Value::as_str)
                .map(|s| s.chars().take(240).collect::<String>()),
            "referenced_files": verified_refs,
        }));
    }

    let arts_dir = workspace.join("arts");
    if let Ok(entries) = std::fs::read_dir(&arts_dir) {
        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().into_owned();
            let rel_path = format!("arts/{fname}");
            if seen_art_paths.contains(&rel_path) || !fname.ends_with(".json") {
                continue;
            }
            let full = entry.path();
            let content = std::fs::read_to_string(&full).ok();
            let parsed: Option<Value> = content
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok());
            tool_artifacts.push(json!({
                "seq": parsed.as_ref().and_then(|p| p.get("seq")),
                "artifact_id": fname.trim_end_matches(".json"),
                "rel_path": rel_path,
                "tool_call_id": parsed.as_ref().and_then(|p| p.get("tool_call_id")),
                "tool_name": parsed.as_ref().and_then(|p| p.get("tool_name")),
                "created_at": parsed.as_ref().and_then(|p| p.get("created_at")),
                "exists": true,
                "readable": content.is_some(),
                "status": if content.is_some() { "ok" } else { "unreadable" },
                "preview": parsed
                    .as_ref()
                    .and_then(|p| p.get("output"))
                    .and_then(Value::as_str)
                    .map(|s| s.chars().take(240).collect::<String>()),
                "referenced_files": [],
            }));
        }
    }

    let mut model_obj = session_meta.get("model").cloned().unwrap_or_else(|| {
        json!({
            "id": "default",
            "readiness": "unknown",
            "context_window": Value::Null,
            "max_tokens": Value::Null,
            "cost": Value::Null,
        })
    });
    let context_info = resolve_inspector_context(
        state,
        &session_meta,
        run_doc,
        &resume_check,
        &mut model_obj,
    );

    json!({
        "run_id": run_id,
        "kind": run_doc.get("kind").and_then(Value::as_str).unwrap_or("local_pi"),
        "operator_label": run_doc.get("operator_label").cloned().unwrap_or(Value::Null),
        "operator_label_updated_at": run_doc.get("operator_label_updated_at").cloned().unwrap_or(Value::Null),
        "tools_and_skills": {
            "adapter_id": LOCAL_PI_ADAPTER_ID,
            "solver_id": LOCAL_PI_SOLVER_ID,
            "chat_thread_id": session_meta.get("chat_thread_id").or_else(|| run_doc.get("chat_thread_id")).cloned().unwrap_or(Value::Null),
            "live_process_active": has_live_session(run_id),
            "provider_session_id": session_meta.get("provider_session_id").cloned().unwrap_or(Value::Null),
            "execution_generation": session_meta.get("execution_generation").or_else(|| run_doc.get("execution_generation")).cloned().unwrap_or(json!(0)),
            "session_state": session_meta.get("session_state").cloned().unwrap_or(json!("idle")),
            "turn_active": session_meta.get("turn_active").and_then(Value::as_bool).unwrap_or(false),
            "turn_count": session_meta.get("turn_count").and_then(Value::as_u64).unwrap_or(0),
            "resume_status": match &resume_check {
                Ok(h) => json!({
                    "resumable": true,
                    "provider_session_id": h.provider_session_id,
                    "session_file": h.session_file,
                    "continuation_row_present": cont.is_some(),
                }),
                Err(err) => json!({
                    "resumable": false,
                    "error": err,
                    "continuation_row_present": cont.is_some(),
                }),
            },
            "model": model_obj,
            "context": context_info,
            "token_usage": run_doc.get("token_usage").cloned().unwrap_or(Value::Null),
            "loaded_tools": session_meta.get("loaded_tools").cloned().unwrap_or_else(|| json!(loaded_tools_manifest())),
            "muteki_readonly_tools": muteki_readonly_tools_manifest(),
            "disabled_capabilities": session_meta.get("disabled_capabilities").cloned().unwrap_or_else(|| json!(disabled_capabilities_manifest())),
            "loaded_skills": skills_list,
            "environment": session_meta.get("environment").cloned().unwrap_or_else(|| verify_isolation_environment(&workspace, run_doc.pointer("/local_pi_config/offline_env").and_then(Value::as_bool).unwrap_or(true))),
            "last_turn_usage": session_meta.get("last_turn_usage").cloned().unwrap_or(Value::Null),
            "last_error": session_meta.get("last_error").cloned().unwrap_or(Value::Null),
        },
        "materials_and_artifacts": {
            "workspace_root": workspace.display().to_string(),
            "uploads": uploads,
            "workspace_files": workspace_files,
            "tool_artifacts": tool_artifacts,
        },
    })
}

fn collect_workspace_files(root: &Path, current: &Path, out: &mut Vec<Value>) {
    let Ok(entries) = std::fs::read_dir(current) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.')
            || matches!(
                name.as_str(),
                "uploads" | "arts" | "shared" | "workers" | "events.jsonl"
            )
        {
            continue;
        }
        if path.is_dir() {
            collect_workspace_files(root, &path, out);
        } else if path.is_file() {
            let rel = path
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| name.clone());
            let meta = std::fs::metadata(&path).ok();
            let readable = std::fs::File::open(&path).is_ok();
            out.push(json!({
                "name": name,
                "rel_path": rel,
                "size_bytes": meta.as_ref().map(|m| m.len()).unwrap_or(0),
                "exists": meta.is_some(),
                "readable": readable,
                "kind": "workspace_file",
            }));
        }
    }
}

pub fn load_local_pi_settings(state: &AppState) -> Result<Value, crate::chat::Failure> {
    let stored = state
        .store
        .get("settings", "local_pi")
        .map_err(crate::control::storage)?
        .unwrap_or_else(|| json!({}));

    let default_account_id = stored
        .get("default_account_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("system_login");

    let default_model = stored
        .get("default_model")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");

    let default_endpoint = stored
        .get("default_endpoint")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");

    let default_access_mode = match stored.get("default_access_mode").and_then(Value::as_str) {
        Some("supervised") => "supervised",
        Some("auto-accept-edits") => "auto-accept-edits",
        _ => "full-access",
    };

    let valid_skill_ids: HashSet<&str> = BUILTIN_LOCAL_PI_SKILLS.iter().map(|s| s.id).collect();
    let default_skills: Vec<String> = match stored.get("default_skills").and_then(Value::as_array) {
        Some(arr) => arr
            .iter()
            .filter_map(Value::as_str)
            .filter(|id| valid_skill_ids.contains(id))
            .map(str::to_owned)
            .collect(),
        None => default_skill_ids(),
    };

    let offline_env = stored
        .get("offline_env")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let enabled = stored
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let use_agent_credentials = stored
        .get("use_agent_credentials")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let providers: Vec<Value> = stored
        .get("providers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let public_providers: Vec<Value> = providers
        .iter()
        .map(|p| {
            let mut out = p.clone();
            let pid = p.get("id").and_then(Value::as_str).unwrap_or("");
            out["has_api_key"] = json!(local_pi_provider_api_key(state, pid).is_some());
            out
        })
        .collect();

    Ok(json!({
        "enabled": enabled,
        "use_agent_credentials": use_agent_credentials,
        "default_account_id": default_account_id,
        "default_model": default_model,
        "default_endpoint": default_endpoint,
        "default_access_mode": default_access_mode,
        "default_skills": default_skills,
        "offline_env": offline_env,
        "providers": public_providers,
        "updated_at": stored.get("updated_at").cloned().unwrap_or(Value::Null),
    }))
}

/// Private credentials home of an independent Local Pi endpoint
/// (`provider:<id>`). The API key lives only inside this 0700 tree; it never
/// reaches settings, journals, logs or the run workspace.
fn local_pi_provider_root(state: &AppState, provider_id: &str) -> PathBuf {
    native_chat::state_dir(state)
        .join("runtime-credentials")
        .join("local-pi")
        .join(provider_id)
}

fn local_pi_provider_api_key(state: &AppState, provider_id: &str) -> Option<String> {
    let raw = std::fs::read_to_string(local_pi_provider_root(state, provider_id).join("API_KEY")).ok()?;
    let key = raw.trim();
    (!key.is_empty()).then(|| key.to_owned())
}

fn local_pi_settings_invalid(detail: String) -> crate::chat::Failure {
    crate::control::failure(
        axum::http::StatusCode::BAD_REQUEST,
        "local_pi.settings_invalid",
        detail,
    )
}

fn local_pi_provider_io(e: crate::agent_runtime::RuntimeError) -> crate::chat::Failure {
    crate::control::failure(axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.code, e.message)
}

fn write_local_pi_provider_key(
    state: &AppState,
    provider_id: &str,
    key: &str,
) -> Result<(), crate::chat::Failure> {
    let state_dir = native_chat::state_dir(state);
    let root = local_pi_provider_root(state, provider_id);
    for dir in [
        state_dir.join("runtime-credentials"),
        state_dir.join("runtime-credentials").join("local-pi"),
        root.clone(),
    ] {
        credentials::private_dir(&dir).map_err(local_pi_provider_io)?;
    }
    credentials::private_file(&root.join("API_KEY"), format!("{key}\n").as_bytes())
        .map_err(local_pi_provider_io)
}

fn valid_local_pi_provider_id(id: &str) -> bool {
    id.strip_prefix("lp-").is_some_and(|suffix| {
        suffix.len() == 8 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn check_provider_cleanup(result: std::io::Result<()>) -> Result<(), crate::chat::Failure> {
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(crate::control::failure(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "local_pi.credential_cleanup_failed",
            format!("接入点凭据未清理完成，本次设置未保存。修复文件系统错误后可重试（{kind:?}）。", kind = error.kind()),
        )),
    }
}

/// Validates one `providers[]` entry from a save patch. `api_key` and
/// `clear_api_key` are honoured by the caller as side effects and never
/// persist. A missing id mints a fresh `lp-<8 hex>` slug.
fn validate_local_pi_provider(p: &Value, taken: &mut HashSet<String>) -> Result<Value, crate::chat::Failure> {
    let id = match p
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(id) => {
            if !valid_local_pi_provider_id(id) {
                return Err(local_pi_settings_invalid(format!(
                    "接入点 ID 无效，须为 lp-<8 hex>: {id}"
                )));
            }
            if !taken.insert(id.to_owned()) {
                return Err(local_pi_settings_invalid(format!("接入点 ID 重复: {id}")));
            }
            id.to_owned()
        }
        None => {
            let mut id;
            let mut tries = 0;
            loop {
                id = format!("lp-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
                if taken.insert(id.clone()) {
                    break;
                }
                tries += 1;
                if tries > 16 {
                    return Err(local_pi_settings_invalid(
                        "无法生成接入点 ID".to_owned(),
                    ));
                }
            }
            id
        }
    };
    let name = p
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| id.clone());
    let api = p
        .get("api")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if api != "openai-completions" && api != "anthropic-messages" {
        return Err(local_pi_settings_invalid(format!(
            "接入点 {name}: api 必须是 openai-completions 或 anthropic-messages"
        )));
    }
    let base_url = p
        .get("base_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| local_pi_settings_invalid(format!("接入点 {name}: base_url 必填")))?
        .to_owned();
    let url = url::Url::parse(&base_url)
        .map_err(|_| local_pi_settings_invalid(format!("接入点 {name}: base_url 不是合法 URL")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(local_pi_settings_invalid(format!(
            "接入点 {name}: base_url 必须是 http(s)://host[:port]"
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(local_pi_settings_invalid(format!(
            "接入点 {name}: base_url 不能包含凭据"
        )));
    }
    let mut models = Vec::new();
    for m in p.get("models").and_then(Value::as_array).into_iter().flatten() {
        let mid = m
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                local_pi_settings_invalid(format!("接入点 {name}: 模型 id 不能为空"))
            })?;
        let mut mo = json!({ "id": mid });
        if let Some(n) = m
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            mo["name"] = json!(n);
        }
        if let Some(v) = m.get("context_window").and_then(Value::as_u64) {
            mo["context_window"] = json!(v);
        }
        if let Some(v) = m.get("max_tokens").and_then(Value::as_u64) {
            mo["max_tokens"] = json!(v);
        }
        if let Some(v) = m.get("reasoning").and_then(Value::as_bool) {
            mo["reasoning"] = json!(v);
        }
        models.push(mo);
    }
    Ok(json!({
        "id": id,
        "name": name,
        "api": api,
        "base_url": base_url,
        "models": models,
    }))
}

/// Builds the run-private Pi agent dir for an independent Local Pi endpoint
/// (`provider:<id>`). Mirrors `agent_runtime::credentials`' pi_account_env: a
/// private models.json declares the endpoint with its API key, so no secret
/// reaches the run workspace, journal or settings. Errors are (code, detail)
/// pairs for the caller's typed failure path.
fn local_pi_provider_agent_dir(
    state: &AppState,
    account_id: &str,
    model: Option<&str>,
) -> Result<PiAgentDirOverride, (&'static str, String)> {
    let provider_id = account_id
        .trim()
        .strip_prefix("provider:")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            (
                "local_pi.provider_missing",
                "接入点 Local Pi ID 无效".to_owned(),
            )
        })?;
    let settings = load_local_pi_settings(state).map_err(|f| {
        ("local_pi.settings_error", failure_text(&f))
    })?;
    let provider = settings
        .get("providers")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .find(|p| p.get("id").and_then(Value::as_str) == Some(provider_id))
                .cloned()
        })
        .ok_or_else(|| {
            (
                "local_pi.provider_missing",
                format!("本地 Pi 接入点不存在: {provider_id}"),
            )
        })?;
    let provider_name = provider
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(provider_id)
        .to_owned();
    let model_full = model
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            (
                "local_pi.model_not_configured",
                format!("接入点 {provider_name} 未指定模型"),
            )
        })?;
    let prefix = format!("{provider_id}/");
    let model_name = model_full
        .strip_prefix(&prefix)
        .unwrap_or(model_full)
        .to_owned();
    let model_def = provider
        .get("models")
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter()
                .find(|m| m.get("id").and_then(Value::as_str) == Some(model_name.as_str()))
                .cloned()
        })
        .ok_or_else(|| {
            (
                "local_pi.model_not_configured",
                format!("模型 {model_full} 不在接入点 {provider_name} 的配置中"),
            )
        })?;

    let state_dir = native_chat::state_dir(state);
    let root = local_pi_provider_root(state, provider_id);
    let digest = Sha512::digest(model_name.as_bytes());
    let profile = root.join("pi-agent").join(
        format!("{:x}", digest)
            .chars()
            .take(32)
            .collect::<String>(),
    );
    for dir in [
        state_dir.join("runtime-credentials"),
        state_dir.join("runtime-credentials").join("local-pi"),
        root.clone(),
        profile.parent().map(Path::to_owned).unwrap_or_else(|| profile.clone()),
        profile.clone(),
    ] {
        credentials::private_dir(&dir)
            .map_err(|e| ("local_pi.provider_dir_error", e.message))?;
    }

    let mut definition = json!({
        "id": model_name,
        "input": model_def.get("input").cloned().unwrap_or_else(|| json!(["text"])),
        "reasoning": model_def.get("reasoning").and_then(Value::as_bool).unwrap_or(false),
    });
    for (source, target) in [
        ("context_window", "contextWindow"),
        ("contextWindow", "contextWindow"),
        ("max_tokens", "maxTokens"),
        ("maxTokens", "maxTokens"),
        ("thinkingLevelMap", "thinkingLevelMap"),
    ] {
        if let Some(value) = model_def.get(source) {
            definition[target] = value.clone();
        }
    }
    if let Some(name) = model_def
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        definition["name"] = json!(name);
    }
    let mut compaction = json!({ "enabled": true });
    if let Some(window) = definition["contextWindow"].as_u64() {
        compaction["reserveTokens"] =
            json!(((window as f64 * 0.10).round() as u64).max(16_384));
    }
    let api_key = local_pi_provider_api_key(state, provider_id)
        .unwrap_or_else(|| "muteki-keyless".to_owned());
    let base_url = provider
        .get("base_url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let api = provider
        .get("api")
        .and_then(Value::as_str)
        .unwrap_or("openai-completions");
    let models = json!({
        "providers": {
            provider_id: {
                "baseUrl": base_url,
                "api": api,
                "apiKey": api_key,
                "models": [definition],
            }
        }
    });
    let agent_settings = json!({
        "httpIdleTimeoutMs": 300_000,
        "compaction": compaction,
        "retry": { "provider": { "timeoutMs": 300_000, "maxRetries": 0 } },
    });
    credentials::private_file(&profile.join("models.json"), format!("{models}\n").as_bytes())
        .map_err(|e| ("local_pi.provider_dir_error", e.message))?;
    credentials::private_file(
        &profile.join("settings.json"),
        format!("{agent_settings}\n").as_bytes(),
    )
    .map_err(|e| ("local_pi.provider_dir_error", e.message))?;
    Ok(PiAgentDirOverride {
        env: (
            "PI_CODING_AGENT_DIR".to_owned(),
            profile.to_string_lossy().into_owned(),
        ),
        base_url,
    })
}

pub fn save_local_pi_settings(state: &AppState, patch: &Value) -> Result<Value, crate::chat::Failure> {
    let mut current = load_local_pi_settings(state)?;
    if let Some(enabled) = patch.get("enabled") {
        let Some(b) = enabled.as_bool() else {
            return Err(local_pi_settings_invalid(
                "enabled 必须是布尔值".to_owned(),
            ));
        };
        current["enabled"] = json!(b);
    }
    if let Some(flag) = patch.get("use_agent_credentials") {
        let Some(b) = flag.as_bool() else {
            return Err(local_pi_settings_invalid(
                "use_agent_credentials 必须是布尔值".to_owned(),
            ));
        };
        current["use_agent_credentials"] = json!(b);
    }
    let mut removed_ids = Vec::new();
    let mut key_writes = Vec::new();
    let mut key_clears = Vec::new();
    if let Some(providers) = patch.get("providers") {
        let Some(arr) = providers.as_array() else {
            return Err(local_pi_settings_invalid(
                "providers 必须是数组".to_owned(),
            ));
        };
        let old_ids: HashSet<String> = current
            .get("providers")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|p| p.get("id").and_then(Value::as_str))
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let mut taken: HashSet<String> = HashSet::new();
        let mut new_providers = Vec::new();
        for p in arr {
            let sanitized = validate_local_pi_provider(p, &mut taken)?;
            let id = sanitized["id"].as_str().unwrap_or_default().to_owned();
            if let Some(key) = p
                .get("api_key")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                key_writes.push((id.clone(), key.to_owned()));
            } else if p.get("clear_api_key").and_then(Value::as_bool) == Some(true) {
                key_clears.push(id.clone());
            }
            new_providers.push(sanitized);
        }
        current["providers"] = json!(new_providers);
        for id in old_ids.difference(&taken) {
            if !valid_local_pi_provider_id(id) {
                return Err(local_pi_settings_invalid("已存接入点 ID 无效，无法安全清理凭据".to_owned()));
            }
            removed_ids.push(id.clone());
        }
    }
    if let Some(account_id) = patch.get("default_account_id").and_then(Value::as_str) {
        let trimmed = account_id.trim();
        current["default_account_id"] = json!(if trimmed.is_empty() {
            "system_login"
        } else {
            trimmed
        });
    }
    if let Some(model) = patch.get("default_model").and_then(Value::as_str) {
        current["default_model"] = json!(model.trim());
    }
    if let Some(endpoint) = patch.get("default_endpoint").and_then(Value::as_str) {
        current["default_endpoint"] = json!(endpoint.trim());
    }
    if let Some(mode) = patch.get("default_access_mode").and_then(Value::as_str) {
        let normalized = match mode.trim() {
            "supervised" => "supervised",
            "auto-accept-edits" => "auto-accept-edits",
            "full-access" => "full-access",
            other => return Err(crate::control::failure(axum::http::StatusCode::BAD_REQUEST, "local_pi.settings_invalid", format!("不支持的权限模式: {other}"))),
        };
        current["default_access_mode"] = json!(normalized);
    }
    if let Some(skills) = patch.get("default_skills").and_then(Value::as_array) {
        let valid_skill_ids: HashSet<&str> = BUILTIN_LOCAL_PI_SKILLS.iter().map(|s| s.id).collect();
        let filtered: Vec<String> = skills
            .iter()
            .filter_map(Value::as_str)
            .filter(|id| valid_skill_ids.contains(id))
            .map(str::to_owned)
            .collect();
        current["default_skills"] = json!(filtered);
    }
    if let Some(offline) = patch.get("offline_env").and_then(Value::as_bool) {
        current["offline_env"] = json!(offline);
    }
    if let Some(arr) = current["providers"].as_array_mut() {
        for p in arr.iter_mut() {
            if let Some(obj) = p.as_object_mut() {
                obj.remove("has_api_key");
                obj.remove("api_key");
                obj.remove("clear_api_key");
            }
        }
    }
    for id in removed_ids {
        let directory = local_pi_provider_root(state, &id);
        check_provider_cleanup(std::fs::remove_dir_all(directory))?;
    }
    for id in key_clears {
        let key = local_pi_provider_root(state, &id).join("API_KEY");
        check_provider_cleanup(std::fs::remove_file(key))?;
    }
    for (id, key) in key_writes {
        write_local_pi_provider_key(state, &id, &key)?;
    }
    current["updated_at"] = json!(now_iso());
    state
        .store
        .put("settings", "local_pi", &current)
        .map_err(crate::control::storage)?;
    load_local_pi_settings(state)
}

async fn detect_pi_version(pi_path: &str) -> Option<String> {
    let mut cmd = crate::host_process::command(pi_path);
    if let Ok(path_env) = crate::agent_runtime::binaries::launch_path(Path::new(pi_path)) {
        cmd.env("PATH", path_env);
    }
    cmd.arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_millis(1200), cmd.output())
        .await
        .ok()?
        .ok()?;
    if !output.stdout.is_empty() {
        let s = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !s.is_empty() {
            return Some(s);
        }
    }
    if !output.stderr.is_empty() {
        let s = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

async fn probe_local_ollama_models(custom_endpoint: Option<&str>) -> (Value, Vec<Value>) {
    let base = custom_endpoint
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("http://127.0.0.1:11434")
        .trim_end_matches('/');
    let tags_url = if base.ends_with("/v1") {
        format!("{}/api/tags", base.trim_end_matches("/v1"))
    } else {
        format!("{base}/api/tags")
    };
    let client = match crate::network_proxy::http_client_builder()
        .timeout(std::time::Duration::from_millis(700))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                json!({
                    "reachable": false,
                    "endpoint": base,
                    "model_count": 0,
                    "detail": format!("HTTP 客户端初始化失败: {e}"),
                }),
                Vec::new(),
            );
        }
    };
    let resp = match client.get(&tags_url).send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            return (
                json!({
                    "reachable": false,
                    "endpoint": base,
                    "model_count": 0,
                    "detail": format!("Ollama 接口返回 HTTP {}", r.status()),
                }),
                Vec::new(),
            );
        }
        Err(e) => {
            return (
                json!({
                    "reachable": false,
                    "endpoint": base,
                    "model_count": 0,
                    "detail": format!("{tags_url} 未响应 Ollama 模型列表（{e}）；不使用 ollama/ 模型时可忽略"),
                }),
                Vec::new(),
            );
        }
    };
    let body: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            return (
                json!({
                    "reachable": false,
                    "endpoint": base,
                    "model_count": 0,
                    "detail": format!("{tags_url} 返回的模型列表不是合法 JSON: {e}"),
                }),
                Vec::new(),
            );
        }
    };
    let mut models = Vec::new();
    if let Some(arr) = body.get("models").and_then(Value::as_array) {
        for item in arr {
            let Some(name) = item.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
                continue;
            };
            let param_size = item
                .pointer("/details/parameter_size")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let quant = item
                .pointer("/details/quantization_level")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let family = item
                .pointer("/details/family")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let size_bytes = item.get("size").and_then(Value::as_u64);
            let id = if name.contains('/') {
                name.to_owned()
            } else {
                format!("ollama/{name}")
            };
            let meta: Vec<&str> = [param_size.as_deref(), quant.as_deref()]
                .into_iter()
                .flatten()
                .collect();
            let label = if meta.is_empty() {
                id.clone()
            } else {
                format!("{id} ({})", meta.join(" · "))
            };
            models.push(json!({
                "id": id,
                "raw_name": name,
                "label": label,
                "source": "ollama_local",
                "source_label": "本机 Ollama 实测发现",
                "endpoint": base,
                "parameter_size": param_size,
                "quantization_level": quant,
                "family": family,
                "size_bytes": size_bytes,
                "context_window": Value::Null,
                "max_tokens": Value::Null,
                "cost": Value::Null,
                "readiness": "discovered",
            }));
        }
    }
    (
        json!({
            "reachable": true,
            "endpoint": base,
            "model_count": models.len(),
            "detail": if models.is_empty() {
                "Ollama 服务在线，但尚未拉取本地模型（可执行 ollama pull 拉取）".to_owned()
            } else {
                format!("已发现 {} 个本机 Ollama 模型", models.len())
            },
        }),
        models,
    )
}

fn read_host_pi_models() -> Vec<Value> {
    let mut out = Vec::new();
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return out;
    };
    let candidates = [
        home.join(".pi").join("agent").join("models.json"),
        home.join(".pi").join("models.json"),
    ];
    for file in candidates {
        let Ok(raw) = std::fs::read_to_string(&file) else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if let Some(providers) = parsed.get("providers").and_then(Value::as_object) {
            for (prov_id, prov_val) in providers {
                let endpoint = prov_val
                    .get("baseUrl")
                    .or_else(|| prov_val.get("base_url"))
                    .and_then(Value::as_str)
                    .unwrap_or("~/.pi/agent/models.json");
                if let Some(models) = prov_val.get("models").and_then(Value::as_array) {
                    for m in models {
                        let Some(mid) = m.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
                            continue;
                        };
                        let full_id = if mid.contains('/') {
                            mid.to_owned()
                        } else {
                            format!("{prov_id}/{mid}")
                        };
                        let name = m.get("name").and_then(Value::as_str).unwrap_or(&full_id);
                        let ctx = m
                            .get("contextWindow")
                            .or_else(|| m.get("context_window"))
                            .and_then(Value::as_u64);
                        let max_tok = m
                            .get("maxTokens")
                            .or_else(|| m.get("max_tokens"))
                            .and_then(Value::as_u64);
                        out.push(json!({
                            "id": full_id,
                            "raw_name": mid,
                            "label": format!("{name} ({prov_id})"),
                            "source": "host_pi_config",
                            "source_label": "本机 ~/.pi/agent/models.json",
                            "endpoint": endpoint,
                            "context_window": ctx,
                            "max_tokens": max_tok,
                            "cost": m.get("cost").cloned().unwrap_or(Value::Null),
                            "readiness": "configured",
                        }));
                    }
                }
            }
        }
    }
    out
}

pub fn external_pi_extensions_summary(state: &AppState) -> Value {
    let mut managed_skills = Vec::new();
    if let Ok(rows) = state.store.list("agent_native_skills") {
        for row in rows {
            if row.get("engine").and_then(Value::as_str) == Some("pi") {
                managed_skills.push(json!({
                    "id": row.get("id"),
                    "name": row.get("name"),
                    "mode": row.get("mode"),
                    "enabled": row.get("enabled"),
                    "description": row.get("description"),
                    "source": "muteki.agent_native_skills",
                }));
            }
        }
    }
    let mut managed_packages = Vec::new();
    if let Ok(rows) = state.store.list("agent_extension_packages") {
        for row in rows {
            if row.get("engine").and_then(Value::as_str) == Some("pi") {
                managed_packages.push(json!({
                    "id": row.get("id"),
                    "spec": row.get("spec"),
                    "mode": row.get("mode"),
                    "enabled": row.get("enabled"),
                    "install_status": row.get("install_status"),
                    "source": "muteki.agent_extension_packages",
                }));
            }
        }
    }
    let mut host_skill_dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let host_dir = home.join(".pi").join("agent").join("skills");
        if let Ok(entries) = std::fs::read_dir(&host_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    host_skill_dirs.push(entry.file_name().to_string_lossy().into_owned());
                }
            }
        }
    }
    host_skill_dirs.sort();
    json!({
        "isolation_policy": "本地 Pi 单会话启动时固定传入 --no-skills --no-extensions --no-mcp，不读取宿主 ~/.pi/agent/skills 等全局 Skills 或第三方扩展包。当前 Run 勾选的内置 CTF Skills 写入工作区 .pi/skills/<id>/SKILL.md，扩展中心为 Pi / CTF 模式启用的 Skills 以只读快照链接到 .pi/skills/，两者都通过 --skill <path> 显式传入；为 CTF 模式启用的 MCP 只能经 Muteki blackboard 代理调用。",
        "managed_native_skills": managed_skills,
        "managed_packages": managed_packages,
        "host_global_skills": host_skill_dirs,
    })
}

pub fn multi_run_load_summary(state: &AppState) -> Value {
    let mut total_runs = 0usize;
    let mut active_runs = 0usize;
    let mut generating_runs = 0usize;
    let mut waiting_operator_runs = 0usize;
    let mut completed_runs = 0usize;
    let mut failed_runs = 0usize;

    if let Ok(runs) = state.store.list("runs") {
        for r in runs {
            let kind = r
                .get("kind")
                .and_then(Value::as_str)
                .or_else(|| r.pointer("/effective_configuration/kind").and_then(Value::as_str))
                .unwrap_or("swarm");
            if kind != "local_pi" {
                continue;
            }
            total_runs += 1;
            let rid = r.get("run_id").and_then(Value::as_str).unwrap_or("");
            let live = !rid.is_empty() && has_live_session(rid);
            if live {
                active_runs += 1;
            }
            let turn_active = r
                .pointer("/local_pi_session/turn_active")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let session_state = r
                .pointer("/local_pi_session/session_state")
                .and_then(Value::as_str)
                .unwrap_or("");
            let status = r.get("status").and_then(Value::as_str).unwrap_or("");
            if live && turn_active {
                generating_runs += 1;
            } else if live
                || r.get("awaiting_operator").is_some_and(|v| !v.is_null())
                || session_state == "idle"
            {
                waiting_operator_runs += 1;
            }
            if r.get("solved").and_then(Value::as_bool) == Some(true) || status == "completed" || status == "solved" {
                completed_runs += 1;
            } else if status == "failed" || session_state == "failed" || session_state == "resume_failed" {
                failed_runs += 1;
            }
        }
    }

    json!({
        "total_runs": total_runs,
        "active_runs": active_runs,
        "generating_runs": generating_runs,
        "waiting_operator_runs": waiting_operator_runs,
        "completed_runs": completed_runs,
        "failed_runs": failed_runs,
    })
}

/// `POST /api/runs/local-pi/providers/models` — fetches the real model list
/// from an independent Local Pi endpoint. Accepts `{provider_id}` (api /
/// base_url resolved from settings; stored key used when `api_key` omitted) or
/// an ad-hoc `{api, base_url, api_key?}` for an unsaved endpoint. Returns
/// `{ok, models}` or `{ok:false, code, detail}`; never fabricates a list.
pub async fn fetch_local_pi_provider_models(state: &Arc<AppState>, body: &Value) -> Value {
    let fail = |code: &str, detail: String| json!({"ok": false, "code": code, "detail": detail});

    let provider_id = body
        .get("provider_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let stored_provider = provider_id.as_deref().and_then(|pid| {
        load_local_pi_settings(state)
            .ok()
            .and_then(|s| s.get("providers").and_then(Value::as_array).cloned())
            .and_then(|arr| {
                arr.into_iter()
                    .find(|p| p.get("id").and_then(Value::as_str) == Some(pid))
            })
    });
    if let Some(pid) = provider_id.as_deref()
        && stored_provider.is_none()
    {
        return fail(
            "local_pi.provider_missing",
            format!("本地 Pi 接入点不存在: {pid}"),
        );
    }

    let api = body
        .get("api")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            stored_provider
                .as_ref()
                .and_then(|p| p.get("api").and_then(Value::as_str))
                .map(str::to_owned)
        })
        .unwrap_or_default();
    let base_url = body
        .get("base_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            stored_provider
                .as_ref()
                .and_then(|p| p.get("base_url").and_then(Value::as_str))
                .map(str::to_owned)
        })
        .unwrap_or_default();
    if base_url.is_empty() {
        return fail(
            "local_pi.settings_invalid",
            "base_url 必填".to_owned(),
        );
    }
    let api_key = body
        .get("api_key")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            provider_id
                .as_deref()
                .and_then(|pid| local_pi_provider_api_key(state, pid))
        });

    let client = match crate::network_proxy::http_client_builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return fail(
                "local_pi.http_client_error",
                format!("HTTP 客户端初始化失败: {e}"),
            );
        }
    };

    let base = base_url.trim_end_matches('/');
    let resp = match api.as_str() {
        "openai-completions" => {
            let url = format!("{base}/models");
            let mut req = client.get(&url);
            if let Some(key) = &api_key {
                req = req.bearer_auth(key);
            }
            req.send().await.map_err(|e| (url, e))
        }
        "anthropic-messages" => {
            let url = if base.ends_with("/v1") {
                format!("{base}/models")
            } else {
                format!("{base}/v1/models")
            };
            let mut req = client
                .get(&url)
                .header("anthropic-version", "2023-06-01");
            if let Some(key) = &api_key {
                req = req.header("x-api-key", key);
            }
            req.send().await.map_err(|e| (url, e))
        }
        _ => {
            return fail(
                "local_pi.settings_invalid",
                format!("api 必须是 openai-completions 或 anthropic-messages，实际: {api}"),
            );
        }
    };
    let resp = match resp {
        Ok(r) => r,
        Err((url, e)) => {
            return fail(
                "local_pi.provider_unreachable",
                format!("{url} 未响应: {e}"),
            );
        }
    };
    let status = resp.status();
    let text = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            return fail(
                "local_pi.provider_bad_response",
                format!("读取接入点响应正文失败: {e}"),
            );
        }
    };
    if !status.is_success() {
        return fail(
            "local_pi.provider_http_error",
            format!("接入点返回 HTTP {status}: {text}"),
        );
    }
    let doc: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            return fail(
                "local_pi.provider_bad_response",
                format!("模型列表响应不是合法 JSON: {e}"),
            );
        }
    };
    let mut models = Vec::new();
    if let Some(arr) = doc
        .get("data")
        .or_else(|| doc.get("models"))
        .and_then(Value::as_array)
    {
        for item in arr {
            let id = item
                .get("id")
                .or_else(|| item.get("name"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            if let Some(id) = id {
                let mut mo = json!({ "id": id });
                if let Some(n) = item
                    .get("display_name")
                    .or_else(|| item.get("name"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && *s != id)
                {
                    mo["name"] = json!(n);
                }
                models.push(mo);
            }
        }
    }
    json!({"ok": true, "models": models})
}

pub async fn build_full_local_pi_catalog(state: &Arc<AppState>) -> Result<Value, crate::chat::Failure> {
    let saved_settings = load_local_pi_settings(state)?;
    let default_skills_vec: Vec<String> = saved_settings
        .get("default_skills")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_else(default_skill_ids);

    let instance_row = runtime_api::instance(&state.store, LOCAL_PI_ADAPTER_ID, "default")
        .unwrap_or_else(|| json!({}));
    let pi_binary = instance_row["binary_path"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| which_binary("pi"));
    let pi_version = match pi_binary.as_deref() {
        Some(path) => detect_pi_version(path).await,
        None => None,
    };
    let host_login_status = crate::agent_runtime::host_login::detect("pi", false).await;
    let host_login_available = host_login_status == "present";
    let host_login_detail = if host_login_available {
        "检测到本机 Pi 凭据或环境变量配置".to_owned()
    } else {
        "使用本机 ~/.pi/agent 配置、环境变量或本地 Ollama 端点".to_owned()
    };

    let use_agent_credentials = saved_settings
        .get("use_agent_credentials")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut accounts = vec![json!({
        "id": "system_login",
        "label": "本机 Pi 配置 / 本地 Ollama 默认 (system_login)",
        "source": "system",
        "connection": "local_or_system",
        "endpoint": saved_settings.get("default_endpoint").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or("http://127.0.0.1:11434 / ~/.pi/agent"),
        "usable": pi_binary.is_some(),
        "detail": host_login_detail,
    })];
    for p in saved_settings
        .get("providers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let pid = p.get("id").and_then(Value::as_str).unwrap_or("");
        if pid.is_empty() {
            continue;
        }
        accounts.push(json!({
            "id": format!("provider:{pid}"),
            "label": p.get("name").and_then(Value::as_str).unwrap_or(pid),
            "source": "local_pi_provider",
            "api": p.get("api").and_then(Value::as_str).unwrap_or(""),
            "connection": "local_pi_provider",
            "endpoint": p.get("base_url").and_then(Value::as_str).unwrap_or(""),
            "usable": true,
            "has_api_key": p.get("has_api_key").and_then(Value::as_bool).unwrap_or(false),
        }));
    }
    let mut credential_account_rows: Vec<Value> = Vec::new();
    if use_agent_credentials
        && let Ok(cred_rows) = state.store.list("credential_accounts")
    {
        for row in cred_rows {
                let eng = row
                    .get("engine")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                if eng != "pi" && eng != "cli-pi" {
                    continue;
                }
                let id = row.get("id").and_then(Value::as_str).unwrap_or("");
                if id.is_empty() {
                    continue;
                }
                let account_key = if id.starts_with("account:") {
                    id.to_owned()
                } else {
                    format!("account:{id}")
                };
                credential_account_rows.push(row.clone());
                accounts.push(json!({
                    "id": account_key,
                    "label": row.get("label").and_then(Value::as_str).unwrap_or(id),
                    "source": "credential_account",
                    "connection": row.get("connection").and_then(Value::as_str).unwrap_or("official"),
                    "endpoint": row.get("base_url").and_then(Value::as_str).unwrap_or(""),
                    "default_model": row.get("default_model").and_then(Value::as_str).unwrap_or(""),
                    "usable": row.get("enabled").and_then(Value::as_bool).unwrap_or(true),
                    "detail": row.get("base_url").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or("已配置 Pi 凭据账号"),
                }));
        }
    }

    let custom_ep = saved_settings
        .get("default_endpoint")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let (ollama_status, ollama_models) = probe_local_ollama_models(custom_ep).await;
    let host_pi_models = read_host_pi_models();

    let mut models = Vec::new();
    let mut seen_ids = HashSet::new();
    for m in ollama_models.into_iter().chain(host_pi_models) {
        if let Some(id) = m.get("id").and_then(Value::as_str) {
            let key = format!("system_login::{id}");
            if seen_ids.insert(key.clone()) {
                let mut m = m;
                m["source_ref"] = json!("system_login");
                m["key"] = json!(key);
                if m.get("source_label").is_none() {
                    m["source_label"] = json!("本机 Pi 配置");
                }
                models.push(m);
            }
        }
    }
    for cat in runtime_api::native_catalogs(&state.store, "system:pi").values() {
        if let Some(items) = cat.get("discovered_models").and_then(Value::as_array) {
            for item in items {
                let id = item
                    .as_str()
                    .or_else(|| item.get("id").and_then(Value::as_str))
                    .unwrap_or("");
                let key = format!("system_login::{id}");
                if !id.is_empty() && seen_ids.insert(key.clone()) {
                    let label = item
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or(id);
                    models.push(json!({
                        "id": id,
                        "key": key,
                        "raw_name": id,
                        "label": label,
                        "source": "muteki_catalog",
                        "source_ref": "system_login",
                        "source_label": "本机 Pi 配置",
                        "endpoint": "system:pi",
                        "context_window": Value::Null,
                        "max_tokens": Value::Null,
                        "cost": Value::Null,
                        "readiness": "configured",
                    }));
                }
            }
        }
    }

    // Independent Local Pi endpoints (provider:<id>) — model ids sent to Pi are
    // `<provider_id>/<model_id>` so Pi resolves them against the generated
    // models.json in the run-private agent dir.
    for p in saved_settings
        .get("providers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let pid = p.get("id").and_then(Value::as_str).unwrap_or("");
        if pid.is_empty() {
            continue;
        }
        let pname = p.get("name").and_then(Value::as_str).unwrap_or(pid);
        for m in p.get("models").and_then(Value::as_array).into_iter().flatten() {
            let mid = m.get("id").and_then(Value::as_str).unwrap_or("");
            if mid.is_empty() {
                continue;
            }
            let pi_id = format!("{pid}/{mid}");
            let key = format!("provider:{pid}::{pi_id}");
            if !seen_ids.insert(key.clone()) {
                continue;
            }
            let label = m
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(mid);
            models.push(json!({
                "id": pi_id,
                "key": key,
                "raw_name": mid,
                "label": label,
                "source": "local_pi_provider",
                "source_ref": format!("provider:{pid}"),
                "source_label": pname,
                "endpoint": p.get("base_url").and_then(Value::as_str).unwrap_or(""),
                "context_window": m.get("context_window").cloned().unwrap_or(Value::Null),
                "max_tokens": m.get("max_tokens").cloned().unwrap_or(Value::Null),
                "cost": Value::Null,
                "readiness": "configured",
            }));
        }
    }

    for row in &credential_account_rows {
        let id = row.get("id").and_then(Value::as_str).unwrap_or("");
        let account_key = if id.starts_with("account:") {
            id.to_owned()
        } else {
            format!("account:{id}")
        };
        let label = row.get("label").and_then(Value::as_str).unwrap_or(id);
        let mut account_model_ids: Vec<String> = Vec::new();
        if let Some(dm) = row
            .get("default_model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            account_model_ids.push(dm.to_owned());
        }
        for m in row.get("models").and_then(Value::as_array).into_iter().flatten() {
            let mid = m
                .as_str()
                .or_else(|| m.get("id").and_then(Value::as_str))
                .map(str::trim)
                .unwrap_or("");
            if !mid.is_empty() && !account_model_ids.iter().any(|x| x == mid) {
                account_model_ids.push(mid.to_owned());
            }
        }
        for mid in account_model_ids {
            let key = format!("{account_key}::{mid}");
            if !seen_ids.insert(key.clone()) {
                continue;
            }
            models.push(json!({
                "id": mid,
                "key": key,
                "raw_name": mid,
                "label": mid,
                "source": "credential_account",
                "source_ref": account_key,
                "source_label": label,
                "endpoint": row.get("base_url").and_then(Value::as_str).unwrap_or(""),
                "context_window": Value::Null,
                "max_tokens": Value::Null,
                "cost": Value::Null,
                "readiness": "configured",
            }));
        }
    }

    let mut probe_results = serde_json::Map::new();
    let probe_rows = state
        .store
        .list("local_pi_probe_results")
        .map_err(crate::control::storage)?;
    for row in probe_rows {
        if let Some(k) = row.get("probe_key").and_then(Value::as_str) {
            probe_results.insert(k.to_owned(), row);
        }
    }
    let latest_probe = state
        .store
        .get("local_pi_probe_results", "latest")
        .map_err(crate::control::storage)?
        .unwrap_or(Value::Null);

    let default_workspace = native_chat::state_dir(state).join("sessions").join("<run_id>");

    Ok(json!({
        "ok": true,
        "settings": saved_settings,
        "runtime": {
            "adapter_id": LOCAL_PI_ADAPTER_ID,
            "solver_id": LOCAL_PI_SOLVER_ID,
            "pi_binary": pi_binary,
            "pi_version": pi_version,
            "ready": pi_binary.is_some(),
            "host_login": {
                "available": host_login_available,
                "status": host_login_status,
                "detail": host_login_detail,
            },
            "ollama_status": ollama_status,
        },
        "accounts": accounts,
        "providers": saved_settings.get("providers").cloned().unwrap_or_else(|| json!([])),
        "models": models,
        "latest_probe": latest_probe,
        "probe_results": probe_results,
        "tools": local_pi_tools_all_tiers(),
        "pi_builtin_tools": loaded_tools_manifest(),
        "muteki_readonly_tools": muteki_readonly_tools_manifest(),
        "disabled_capabilities": disabled_capabilities_manifest(),
        "skills": builtin_skills_catalog_with_defaults(Some(&default_skills_vec)),
        "external_extensions": external_pi_extensions_summary(state),
        "candidate_sources": candidate_open_source_skills(),
        "environment": verify_isolation_environment(
            &default_workspace,
            saved_settings["offline_env"].as_bool().unwrap_or(true),
        ),
        "multi_run_summary": multi_run_load_summary(state),
    }))
}

fn local_pi_tools_all_tiers() -> Vec<Value> {
    let mut out = loaded_tools_manifest();
    out.extend(muteki_readonly_tools_manifest());
    out
}

async fn resolve_local_pi_account_id(
    state: &AppState,
    engine_name: &str,
    requested_account: &str,
) -> Result<String, String> {
    let trimmed = requested_account.trim();
    if trimmed == "system_login" || trimmed == "system:pi" || trimmed.starts_with("provider:") {
        // provider:<id> endpoints authenticate through the private pi-agent dir
        // built by local_pi_provider_agent_dir; the base credential stays the
        // host Pi login (system:pi).
        return Ok(format!("system:{engine_name}"));
    }
    if !trimmed.is_empty() {
        return Ok(trimmed.to_owned());
    }
    let (_, cred) = super::settings::resolve_effective_credential(state, engine_name, "").await;
    if cred.is_empty() {
        Err(format!("{engine_name} 缺少可用凭据或本机 CLI 登录"))
    } else {
        Ok(cred)
    }
}

pub async fn run_local_pi_compatibility_probe(state: &Arc<AppState>, req: &Value) -> Result<Value, crate::chat::Failure> {
    let started = Instant::now();
    let saved = load_local_pi_settings(state)?;
    let requested_account = req
        .get("account_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| saved.get("default_account_id").and_then(Value::as_str))
        .unwrap_or("system_login")
        .to_owned();
    let requested_model = req
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            saved
                .get("default_model")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .map(str::to_owned);
    let requested_endpoint = req
        .get("endpoint")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            saved
                .get("default_endpoint")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .map(str::to_owned);

    let probe_key = format!(
        "{}::{}",
        requested_account,
        requested_model.as_deref().unwrap_or("default")
    );

    let fail_probe = |code: &str, message: String| -> Result<Value, crate::chat::Failure> {
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let rec = json!({
            "probe_key": probe_key,
            "ok": false,
            "code": code,
            "detail": message,
            "account_id": requested_account,
            "requested_model": requested_model,
            "served_model": Value::Null,
            "endpoint": requested_endpoint.clone().unwrap_or_else(|| "default".to_owned()),
            "elapsed_ms": elapsed_ms,
            "first_token_ms": Value::Null,
            "tested_at": now_iso(),
            "checks": {
                "text_response": { "status": "failed", "ok": false, "detail": message },
                "streaming": { "status": "unknown", "delta_count": 0, "detail": "请求未完成，流式状态未知" },
                "tool_call": { "status": "unknown", "tool_names": [], "detail": "请求未完成，工具调用表示未知" },
                "usage_reporting": {
                    "status": "unknown",
                    "input_tokens": Value::Null,
                    "output_tokens": Value::Null,
                    "total_tokens": Value::Null,
                    "detail": "未知（不填充为零）",
                },
                "context_window": {
                    "status": "unknown",
                    "context_window": Value::Null,
                    "max_tokens": Value::Null,
                    "detail": "未知（不从模型名称猜测）",
                },
            },
        });
        let _ = state.store.put("local_pi_probe_results", &probe_key, &rec);
        let _ = state.store.put("local_pi_probe_results", "latest", &rec);
        Ok(rec)
    };

    let Some((desc, _)) = descriptors::for_adapter(LOCAL_PI_ADAPTER_ID) else {
        return fail_probe(
            "descriptor_missing",
            format!("未找到适配器描述符 {LOCAL_PI_ADAPTER_ID}"),
        );
    };

    let instance_row = runtime_api::instance(&state.store, LOCAL_PI_ADAPTER_ID, "default")
        .unwrap_or_else(|| json!({}));
    let selected_bin = instance_row["binary_path"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(Path::new);
    let binary = match crate::agent_runtime::binaries::resolve(desc, selected_bin).await {
        Ok(b) => b,
        Err(err) => {
            return fail_probe(
                "binary_missing",
                format!("本机未找到可执行 Pi CLI ({}): {}", err.code, err.message),
            );
        }
    };

    let effective_account =
        match resolve_local_pi_account_id(state, desc.engine(), &requested_account).await {
            Ok(cred) => cred,
            Err(msg) => {
                return fail_probe("credential_missing", msg);
            }
        };

    let state_dir = native_chat::state_dir(state);
    let resolved_cred = match credentials::resolve(
        &state.store,
        &state_dir,
        desc,
        &effective_account,
        requested_model.as_deref(),
    )
    .await
    {
        Ok(r) => r,
        Err(err) => {
            return fail_probe(
                "credential_error",
                format!("凭据解析失败 ({}): {}", err.code, err.message),
            );
        }
    };

    let mut env_map: HashMap<String, String> = resolved_cred.env.into_iter().collect();
    if let Ok(refs) = credentials::env_refs_with_platform(state, &instance_row["env_refs"]) {
        for (k, v) in refs {
            env_map.insert(k, v);
        }
    }
    if let Some(obj) = instance_row["env"].as_object() {
        for (k, v) in obj {
            if let Some(s) = v.as_str() {
                env_map.insert(k.clone(), s.to_owned());
            }
        }
    }
    if saved["offline_env"].as_bool().unwrap_or(true) {
        env_map.insert("PI_OFFLINE".to_owned(), "1".to_owned());
    }

    let scratch = match tempfile::Builder::new()
        .prefix("muteki-local-pi-probe-")
        .tempdir()
    {
        Ok(d) => d,
        Err(e) => {
            return fail_probe("io_error", format!("创建临时探针目录失败: {e}"));
        }
    };
    let _ = std::fs::write(scratch.path().join("probe.txt"), "MUTEKI_PI_PROBE_OK\n");
    let agent_override = if requested_account.starts_with("provider:") {
        match local_pi_provider_agent_dir(state, &requested_account, requested_model.as_deref()) {
            Ok(value) => Some(value),
            Err((code, detail)) => return fail_probe(code.trim_start_matches("local_pi."), detail),
        }
    } else {
        match requested_model.as_deref() {
            Some(model) => match ollama_agent_env(scratch.path(), model, requested_endpoint.as_deref()) {
                Ok(value) => value,
                Err(err) => return fail_probe("endpoint_invalid", err),
            },
            None => None,
        }
    };
    let effective_endpoint = match &agent_override {
        Some(found) => found.base_url.clone(),
        None => "Pi 账号 / Provider 自身配置".to_owned(),
    };
    if let Some(found) = agent_override {
        env_map.insert(found.env.0, found.env.1);
    }

    let mut argv = vec![
        binary.path.to_string_lossy().into_owned(),
        "--mode".to_owned(),
        "json".to_owned(),
        "-p".to_owned(),
        "--no-session".to_owned(),
        "--no-mcp".to_owned(),
        "--no-extensions".to_owned(),
        "--no-skills".to_owned(),
        "--no-prompt-templates".to_owned(),
        "--no-context-files".to_owned(),
        "--tools".to_owned(),
        "read".to_owned(),
    ];
    if let Some(ref m) = requested_model {
        argv.push("--model".to_owned());
        argv.push(m.clone());
    }
    argv.push("兼容性检查（无攻击内容）：如果可用请调用 read 工具读取 probe.txt，并简短回复 OK。".to_owned());

    let mut command = tokio::process::Command::from(agent::supervised_command(&argv[0]));
    if !env_map.contains_key("PATH")
        && let Ok(path_env) = crate::agent_runtime::binaries::launch_path(&binary.path)
    {
        command.env("PATH", path_env);
    }
    command
        .args(&argv[1..])
        .current_dir(scratch.path())
        .envs(&env_map)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            return fail_probe("spawn_failed", format!("启动 Pi 进程失败: {e}"));
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let collect_fut = async {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt};
        let mut lines = Vec::new();
        let mut first_token_ms: Option<u64> = None;
        if let Some(out) = stdout {
            let mut reader = tokio::io::BufReader::new(out).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                if first_token_ms.is_none() && !line.trim().is_empty() {
                    first_token_ms = Some(started.elapsed().as_millis() as u64);
                }
                lines.push(line);
            }
        }
        let mut err_buf = String::new();
        if let Some(mut err) = stderr {
            let _ = err.read_to_string(&mut err_buf).await;
        }
        let status = child.wait().await;
        (status, lines, err_buf, first_token_ms)
    };

    let (status_res, lines, err_buf, first_token_ms) =
        match tokio::time::timeout(std::time::Duration::from_secs(35), collect_fut).await {
            Ok(res) => res,
            Err(_) => {
                return fail_probe(
                    "probe_timeout",
                    "本地 Pi 兼容性检查超时（35s），请检查本地模型服务或网络连接是否就绪".to_owned(),
                );
            }
        };

    let exit_ok = status_res.as_ref().map(|s| s.success()).unwrap_or(false);
    let mut reply_text = String::new();
    let mut streaming_deltas = 0usize;
    let mut tool_calls = Vec::new();
    let mut served_model: Option<String> = None;
    let mut input_tokens: Option<u64> = None;
    let mut output_tokens: Option<u64> = None;
    let mut total_tokens: Option<u64> = None;
    let mut declared_context_window: Option<u64> = None;
    let mut declared_max_tokens: Option<u64> = None;
    let mut error_msg: Option<String> = None;

    for line in &lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<Value>(trimmed) {
            let ty = val.get("type").and_then(Value::as_str).unwrap_or("");
            if ty.contains("delta") || ty == "message_update" || val.get("assistantMessageEvent").is_some() {
                streaming_deltas += 1;
            }
            if ty.contains("tool")
                || val.pointer("/assistantMessageEvent/type").and_then(Value::as_str) == Some("toolcall_end")
                || val.pointer("/toolName").is_some()
            {
                let tname = val
                    .get("toolName")
                    .or_else(|| val.pointer("/assistantMessageEvent/toolCall/name"))
                    .and_then(Value::as_str)
                    .unwrap_or("read");
                if !tool_calls.iter().any(|x| x == tname) {
                    tool_calls.push(tname.to_owned());
                }
            }
            for ptr in [
                "/usage",
                "/message/usage",
                "/response/usage",
                "/assistantMessageEvent/usage",
            ] {
                if let Some(u) = val.pointer(ptr).filter(|v| v.is_object()) {
                    let inp = u
                        .get("input")
                        .or_else(|| u.get("input_tokens"))
                        .or_else(|| u.get("prompt_tokens"))
                        .and_then(Value::as_u64);
                    let outp = u
                        .get("output")
                        .or_else(|| u.get("output_tokens"))
                        .or_else(|| u.get("completion_tokens"))
                        .and_then(Value::as_u64);
                    let tot = u
                        .get("totalTokens")
                        .or_else(|| u.get("total_tokens"))
                        .and_then(Value::as_u64);
                    if inp.is_some() || outp.is_some() || tot.is_some() {
                        input_tokens = inp.or(input_tokens);
                        output_tokens = outp.or(output_tokens);
                        total_tokens = tot.or(total_tokens).or_else(|| match (input_tokens, output_tokens) {
                            (Some(i), Some(o)) => Some(i + o),
                            _ => None,
                        });
                    }
                }
            }
            if let Some(cw) = val
                .pointer("/model/contextWindow")
                .or_else(|| val.pointer("/context_window"))
                .and_then(Value::as_u64)
            {
                declared_context_window = Some(cw);
            }
            if let Some(mt) = val
                .pointer("/model/maxTokens")
                .or_else(|| val.pointer("/max_tokens"))
                .and_then(Value::as_u64)
            {
                declared_max_tokens = Some(mt);
            }
        }
        if let Some(ev) = super::container_exec::parse_container_cli_json_line(trimmed) {
            if let Some(err) = ev.error_message {
                error_msg = Some(err);
            }
            if let Some(m) = ev.reported_model {
                served_model = Some(m);
            }
            for t in ev.assistant_texts {
                reply_text.push_str(&t);
            }
        }
    }

    if declared_context_window.is_none() {
        if let Some(ref target_m) = served_model.as_ref().or(requested_model.as_ref()) {
            for item in read_host_pi_models() {
                if item.get("id").and_then(Value::as_str) == Some(target_m.as_str())
                    || item.get("raw_name").and_then(Value::as_str) == Some(target_m.as_str())
                {
                    declared_context_window = item.get("context_window").and_then(Value::as_u64);
                    declared_max_tokens = item.get("max_tokens").and_then(Value::as_u64);
                }
            }
        }
    }

    if let Some(err) = error_msg {
        return fail_probe("model_error", err);
    }
    if !exit_ok || reply_text.trim().is_empty() {
        let stderr_trimmed = err_buf.trim();
        let reason = if !stderr_trimmed.is_empty() {
            format!("Pi 兼容性检查未返回有效响应: {stderr_trimmed}")
        } else {
            "Pi 兼容性检查未返回有效文本响应，请检查默认模型配置或显式选择可用模型".to_owned()
        };
        return fail_probe("empty_response", reason);
    }

    let elapsed_ms = started.elapsed().as_millis() as u64;
    let reply_preview: String = reply_text.trim().chars().take(120).collect();
    let usage_reported = input_tokens.is_some() || output_tokens.is_some() || total_tokens.is_some();

    let rec = json!({
        "probe_key": probe_key,
        "ok": true,
        "code": "ok",
        "detail": format!(
            "兼容性检查通过（{} ms）：响应 `{}`",
            elapsed_ms,
            reply_preview
        ),
        "account_id": effective_account,
        "requested_model": requested_model,
        "served_model": served_model.clone().or_else(|| requested_model.clone()).unwrap_or_else(|| "default".to_owned()),
        "endpoint": effective_endpoint,
        "elapsed_ms": elapsed_ms,
        "first_token_ms": first_token_ms,
        "tested_at": now_iso(),
        "checks": {
            "text_response": {
                "status": "supported",
                "ok": true,
                "detail": format!("已返回文本响应: {reply_preview}"),
            },
            "streaming": {
                "status": if streaming_deltas > 0 { "supported" } else { "not_observed" },
                "delta_count": streaming_deltas,
                "detail": if streaming_deltas > 0 {
                    format!("已观测到 {streaming_deltas} 条流式增量事件")
                } else {
                    "本次短响应未观测到独立流式分片（整包返回）".to_owned()
                },
            },
            "tool_call": {
                "status": if !tool_calls.is_empty() { "supported" } else { "not_used" },
                "tool_names": tool_calls,
                "detail": if !tool_calls.is_empty() {
                    format!("已确认工具调用表示 ({})", tool_calls.join(", "))
                } else {
                    "模型直接文本回复，未触发工具调用（不判定为不支持）".to_owned()
                },
            },
            "usage_reporting": {
                "status": if usage_reported { "reported" } else { "unknown" },
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "total_tokens": total_tokens,
                "detail": if usage_reported {
                    format!(
                        "真实上报 input={} / output={}",
                        input_tokens.map(|v| v.to_string()).unwrap_or_else(|| "未知".to_owned()),
                        output_tokens.map(|v| v.to_string()).unwrap_or_else(|| "未知".to_owned())
                    )
                } else {
                    "未知（当前端点未返回 Token 用量，按规范保持未知，不填充为 0）".to_owned()
                },
            },
            "context_window": {
                "status": if declared_context_window.is_some() { "declared" } else { "unknown" },
                "context_window": declared_context_window,
                "max_tokens": declared_max_tokens,
                "detail": if let Some(cw) = declared_context_window {
                    format!("已声明上下文窗口 {cw} tokens")
                } else {
                    "未知（端点未声明上下文窗口长度，按规范不从模型名称猜测）".to_owned()
                },
            },
        },
    });
    let _ = state.store.put("local_pi_probe_results", &probe_key, &rec);
    let _ = state.store.put("local_pi_probe_results", "latest", &rec);
    Ok(rec)
}


/// The CTF adapter implements the conversation engine's shared Run contract.
pub struct ConversationRunBridge;
impl crate::conversation_policy::RunBridge for ConversationRunBridge {
    fn check(&self,state:&AppState,thread:&str)->Result<(),crate::agent_runtime::RuntimeError>{
        let doc=crate::chat::load(state,thread).map_err(|error|crate::agent_runtime::RuntimeError::new("runtime.policy_unavailable",format!("{error:?}")))?;
        if let Some(binding)=doc.get("ctf_binding").filter(|value|value.is_object()){
            let run=binding["run_id"].as_str().filter(|id|!id.is_empty()).ok_or_else(||crate::agent_runtime::RuntimeError::new("run.binding_invalid","Run conversation has no Run identity"))?;
            crate::business::require_run(state,run).map_err(|error|crate::agent_runtime::RuntimeError::retryable(error.code,error.message))?;
        }
        Ok(())
    }
    fn prepare(&self,state:&AppState,doc:&Value,request:&mut StartRequest)->Result<bool,crate::agent_runtime::RuntimeError>{
        if let Some(binding)=doc.get("ctf_binding").filter(|value|value.is_object()){
            let run=binding["run_id"].as_str().unwrap_or("");
            state.store.get("runs",run).map_err(|error|crate::agent_runtime::RuntimeError::new("storage.unavailable",error.to_string()))?.ok_or_else(||crate::agent_runtime::RuntimeError::new("run.binding_invalid","Run conversation owner is missing"))?;
            crate::business::require_run(state,run).map_err(|error|crate::agent_runtime::RuntimeError::retryable(error.code,error.message))?;
        }
        Ok(apply_chat_start(state,doc,request))
    }
    fn observe(&self,state:&Arc<AppState>,thread:&str,binding:&Value,event:&AgentEvent){chat_tap(state,thread,binding,event)}
}
