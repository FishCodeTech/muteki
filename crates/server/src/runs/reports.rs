//! Durable writeup/report records linked to runs, managed per workspace mode.
use super::*;
use axum::extract::Query;
use sha2::{Digest, Sha256};

const REPORTS: &str = "run_reports";
const FOLDERS: &str = "run_report_folders";
const GENERATION_SETTINGS: &str = "report_generation";
const MAX_FOLDER_DEPTH: u64 = 5;
const DEFAULT_MAX_CONCURRENT: usize = 3;
const DEFAULT_TEMPLATE_ID: &str = "builtin-standard";
const MAX_TEMPLATES: usize = 32;
const MAX_TEMPLATE_BYTES: usize = 200_000;

pub(super) const DEFAULT_WRITEUP_PROMPT: &str = r#"你是一名专业的 CTF 选手，请根据本次解题过程、原始产物与工作区脚本，撰写一篇结构清晰、可直接归档与技术分享的 CTF Writeup（Markdown）。

必须遵守的写作规范：
1. 严格基于工具查证到的真实题目信息、源码/反编译逻辑、关键请求与响应、以及实际跑通的利用脚本编写，严禁编造未观察到的细节或代码。
2. 严禁输出任何平台内部运行概念与流水账，包括但不限于：`event seq`、`art-XX`、`step_00X`、`board_query`、`solvegraph`、`blackboard`、`宿主确认`、`Worker`、`Coordinator`、`运行时间线`、`团队分工复盘`、`未完成/未达成/未验证项清单`。
3. 剔除无效试错与工具探测过程，聚焦“题目核心机制 → 漏洞成因/突破口 → 利用推导 → 获取 Flag → 完整可复现 Exp 脚本”。
4. 若有可直接运行的 Python/Bash 解题脚本或关键 Payload，必须完整保留在代码块中；若有提取出的源码或关键反汇编片段，精简贴出关键逻辑即可。
5. 如果题目最终未解出，不要写运行审计复盘，仅客观说明已分析出的题目逻辑、已尝试的关键方向与最终卡点。
6. 直接输出最终完整的 Markdown 正文，不要以“我将开始查看……”等过程性开场白开头，也不要输出文件路径替代正文。"#;

fn builtin_writeup_templates() -> Vec<Value> {
    vec![
        json!({
            "id": "builtin-standard",
            "name": "标准 CTF Writeup",
            "description": "包含题目信息、解题思路、详细分析与完整 Exp 脚本，适合赛后归档与技术分享。",
            "builtin": true,
            "prompt": "",
            "markdown": r#"# {{title}}

- **分类**：{{category}}
- **目标 / 附件**：{{target}}
- **Flag**：`{{flag}}`

## 解题思路

概括本题的核心考点、漏洞成因或算法突破口，以及整体利用链条。

## 详细分析

### 1. 信息收集与题目分析
结合源码审计、逆向逻辑、流量或服务探测，说明核心校验逻辑与关键限制条件。

### 2. 漏洞利用与推导过程
逐步说明绕过限制、构造 Payload 或推导密钥/偏移的具体原理与关键交互。

### 3. 获取 Flag
展示最终攻击载荷的触发过程、关键回显结果以及拿到的 Flag。

## Exp / 解题脚本

```python
# 在此放置完整、可直接复现的解题脚本或关键命令
```
"#,
        }),
        json!({
            "id": "builtin-concise",
            "name": "精简速记版",
            "description": "直击考点、关键 Payload 与 Exp，适合快速查阅与刷题记录。",
            "builtin": true,
            "prompt": "保持篇幅精炼，直奔核心漏洞原理、关键利用步骤和完整 Exp，省略冗余背景描述。",
            "markdown": r#"# {{title}}

> 分类：{{category}} ｜ 目标：{{target}} ｜ Flag：`{{flag}}`

## 核心考点与漏洞点

点明本题关键机制、绕过点或密码学/逆向突破口。

## 关键利用步骤

1. ...
2. ...

## Exp

```python
# 完整解题脚本或关键命令
```
"#,
        }),
        json!({
            "id": "builtin-competition",
            "name": "比赛官方交档版",
            "description": "规范的竞赛提交格式，涵盖赛题元数据、原理剖析、复现步骤与完整脚本。",
            "builtin": true,
            "prompt": "按正式赛事提交的 Writeup 规范撰写，确保原理分析严谨、复现步骤编号清晰、关键请求与响应完整、Exp 可独立复现。",
            "markdown": r#"# [Writeup] {{title}}

| 项目 | 内容 |
| --- | --- |
| 题目名称 | {{title}} |
| 题目分类 | {{category}} |
| 目标 / 环境 | {{target}} |
| Flag | `{{flag}}` |

## 一、题目概述与解题思路

简述赛题场景、考察知识点及整体求解路径。

## 二、原理分析

### 2.1 关键代码 / 逻辑分析
贴出关键源码、反编译伪代码或协议结构并指出缺陷位置。

### 2.2 绕过与利用构造
说明具体的绕过手法、参数构造依据或数学推导过程。

## 三、复现步骤与结果

1. 构造请求 / 运行脚本。
2. 展示关键响应输出及最终 Flag。

## 四、完整利用脚本 (Exp)

```python
# 完整可复现脚本
```
"#,
        }),
    ]
}

fn is_builtin_template_id(id: &str) -> bool {
    matches!(id, "builtin-standard" | "builtin-concise" | "builtin-competition")
}

pub(super) struct Failure {
    pub(super) code: String,
    pub(super) message: String,
    report: Option<Value>,
}
impl Failure {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: format!("run.report.{code}"),
            message: message.into(),
            report: None,
        }
    }
    fn reported(mut self, report: Value) -> Self {
        self.report = Some(report);
        self
    }
    fn response(self, status: StatusCode) -> Response {
        let mut body = json!({"ok":false,"error":{"code":self.code,"message":self.message}});
        if let Some(report) = self.report {
            body["report"] = report;
        }
        (status, Json(body)).into_response()
    }
    fn respond(self) -> Response {
        let status = self.status();
        self.response(status)
    }
    fn status(&self) -> StatusCode {
        match self.code.as_str() {
            "run.report.run_not_found"
            | "run.report.report_not_found"
            | "run.report.folder_not_found"
            | "run.report.writeup_not_found" => StatusCode::NOT_FOUND,
            "run.report.invalid_input"
            | "run.report.invalid_mode"
            | "run.report.not_supported"
            | "run.report.folder_depth_exceeded" => StatusCode::BAD_REQUEST,
            "run.report.revision_conflict"
            | "run.report.duplicate_import"
            | "run.report.run_changed"
            | "run.report.folder_cycle"
            | "run.report.generation_active" => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}
fn storage(e: impl std::fmt::Display) -> Failure {
    Failure::new("storage_failed", e.to_string())
}
impl From<followup::Failure> for Failure {
    fn from(e: followup::Failure) -> Self {
        Self {
            code: e.code,
            message: e.message,
            report: None,
        }
    }
}

fn sha256_hex(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn run_doc(state: &AppState, run_id: &str) -> Result<Option<Value>, Failure> {
    state.store.get("runs", run_id).map_err(storage)
}

fn validate_tags(value: &Value) -> Result<Vec<String>, Failure> {
    let Some(items) = value.as_array() else {
        return Err(Failure::new("invalid_input", "tags must be an array"));
    };
    let mut tags: Vec<String> = Vec::new();
    for item in items {
        let Some(raw) = item.as_str() else {
            return Err(Failure::new("invalid_input", "tags must be strings"));
        };
        let tag = raw.trim();
        if tag.is_empty() {
            continue;
        }
        if !tags.iter().any(|t| t == tag) {
            tags.push(tag.to_owned());
        }
    }
    Ok(tags)
}

fn validate_status(value: &Value) -> Result<&str, Failure> {
    match value.as_str() {
        Some(status @ ("draft" | "final")) => Ok(status),
        _ => Err(Failure::new(
            "invalid_input",
            "status must be \"draft\" or \"final\"",
        )),
    }
}

fn validate_source(body: &Value, mode: &str, run_bound: bool) -> Result<Value, Failure> {
    let source = &body["source"];
    if source.is_null() {
        return Ok(json!({"kind":"manual","ref":Value::Null}));
    }
    let kind = source["kind"].as_str().unwrap_or("");
    let reference = source["ref"].clone();
    let check_ref = |kind: &str| -> Result<Value, Failure> {
        if !(reference.is_null() || reference.as_str().is_some()) {
            return Err(Failure::new(
                "invalid_input",
                "source.ref must be a string or null",
            ));
        }
        Ok(json!({"kind":kind,"ref":reference}))
    };
    match kind {
        "manual" => Ok(json!({"kind":"manual","ref":source["ref"].clone()})),
        "import" => check_ref("import"),
        "ai_writeup" | "pentest_report" => {
            if !run_bound {
                return Err(Failure::new(
                    "invalid_input",
                    "source.kind requires a report bound to a run",
                ));
            }
            let expected = if mode == "pentest" {
                "pentest_report"
            } else {
                "ai_writeup"
            };
            if kind != expected {
                return Err(Failure::new(
                    "invalid_input",
                    format!("source.kind \"{kind}\" does not match run mode \"{mode}\""),
                ));
            }
            check_ref(kind)
        }
        _ => Err(Failure::new(
            "invalid_input",
            "source.kind must be \"manual\", \"ai_writeup\", \"pentest_report\" or \"import\"",
        )),
    }
}

fn validate_import_source(body: &Value) -> Result<Value, Failure> {
    if body.get("import_source").is_none() || body["import_source"].is_null() {
        return Ok(Value::Null);
    }
    let source = &body["import_source"];
    if source["path"].as_str().is_none() || source["sha256"].as_str().is_none() {
        return Err(Failure::new(
            "invalid_input",
            "import_source requires string \"path\" and \"sha256\"",
        ));
    }
    Ok(json!({"path":source["path"],"sha256":source["sha256"]}))
}

fn report_visible(row: &Value, run_id: &str) -> bool {
    row["run_id"].as_str() == Some(run_id)
}

fn list_jobs(state: &AppState) -> Result<Vec<Value>, Failure> {
    state.store.list(followup::JOBS).map_err(storage)
}

fn queued_writeup_position(jobs: &[Value], job_id: &str) -> Option<u64> {
    let mut queued: Vec<&Value> = jobs
        .iter()
        .filter(|r| r["kind"].as_str() == Some("writeup") && r["state"].as_str() == Some("queued"))
        .collect();
    queued.sort_by(|a, b| {
        a["created_at"]
            .as_f64()
            .partial_cmp(&b["created_at"].as_f64())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    queued
        .iter()
        .position(|r| r["id"].as_str() == Some(job_id))
        .map(|position| position as u64 + 1)
}

/// Live generation state is derived from the bound job row, never persisted.
fn decorate(jobs: &[Value], report: &mut Value) {
    if report.get("order").is_none() || report["order"].is_null() {
        report["order"] = report["created_at"].clone();
    }
    for key in [
        "folder_id",
        "import_source",
        "ai_sha256",
        "generation",
        "pending_update",
    ] {
        if report.get(key).is_none() {
            report[key] = Value::Null;
        }
    }
    let generation = report["generation"].clone();
    if generation["state"].as_str() != Some("active") {
        return;
    }
    let job_id = generation["job_id"].as_str().unwrap_or("").to_owned();
    let job_id = job_id.as_str();
    let job = jobs.iter().find(|r| r["id"].as_str() == Some(job_id));
    let mut live = generation;
    match job.and_then(|row| row["state"].as_str()) {
        Some("queued") => {
            live["state"] = json!("queued");
            if let Some(position) = queued_writeup_position(jobs, job_id) {
                live["queue_position"] = json!(position);
            }
        }
        Some("failed") => {
            live["state"] = json!("failed");
            live["error"] = job.and_then(|row| row.get("error").cloned()).unwrap_or(Value::Null);
            live["finished_at"] = job
                .and_then(|row| row.get("updated_at").cloned())
                .unwrap_or(Value::Null);
        }
        _ => {
            live["state"] = json!("running");
        }
    }
    if let Some(row) = job {
        live["progress"] = row["progress"].clone();
        live["updated_at"] = row["updated_at"].clone();
    }
    report["generation"] = live;
}

fn decorated(jobs: &[Value], mut report: Value) -> Value {
    decorate(jobs, &mut report);
    report
}

fn module_for(mode: &str) -> crate::business::Module {
    if mode == "pentest" {
        crate::business::Module::Pentest
    } else {
        crate::business::Module::Ctf
    }
}

pub(super) async fn list_all(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let mode = query.get("mode").map(String::as_str).unwrap_or("");
    if !matches!(mode, "ctf" | "pentest") {
        return Failure::new("invalid_mode", "mode must be \"ctf\" or \"pentest\"")
            .response(StatusCode::BAD_REQUEST);
    }
    if let Err(unavailable) = crate::business::require(&state, module_for(mode)) {
        return unavailable.into_response();
    }
    let result: Result<Json<Value>, Failure> = (|| {
        let jobs = list_jobs(&state)?;
        let mut reports: Vec<Value> = state
            .store
            .list(REPORTS)
            .map_err(storage)?
            .into_iter()
            .filter(|row| row["mode"].as_str() == Some(mode))
            .map(|row| decorated(&jobs, row))
            .collect();
        reports.sort_by(|a, b| {
            b["updated_at"]
                .as_f64()
                .partial_cmp(&a["updated_at"].as_f64())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut runs = Map::new();
        for report in &reports {
            let Some(run_id) = report["run_id"].as_str() else {
                continue;
            };
            if runs.contains_key(run_id) {
                continue;
            }
            if let Some(doc) = run_doc(&state, run_id)? {
                let mut summary = Map::new();
                for key in [
                    "name",
                    "category",
                    "mode",
                    "status",
                    "solved",
                    "finished",
                    "flag",
                    "folder_id",
                ] {
                    summary.insert(key.to_owned(), doc[key].clone());
                }
                runs.insert(run_id.to_owned(), Value::Object(summary));
            }
        }
        let folders: Vec<Value> = state
            .store
            .list(FOLDERS)
            .map_err(storage)?
            .into_iter()
            .filter(|row| row["mode"].as_str() == Some(mode))
            .collect();
        Ok(Json(json!({"ok":true,"reports":reports,"runs":runs,"folders":folders})))
    })();
    match result {
        Ok(body) => body.into_response(),
        Err(error) => error.response(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

pub(super) async fn list_for_run(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
) -> Response {
    let result: Result<Json<Value>, Failure> = (|| {
        if run_doc(&state, &run_id)?.is_none() {
            return Err(Failure::new("run_not_found", "Run not found"));
        }
        let jobs = list_jobs(&state)?;
        let mut reports: Vec<Value> = state
            .store
            .list(REPORTS)
            .map_err(storage)?
            .into_iter()
            .filter(|row| report_visible(row, &run_id))
            .map(|row| decorated(&jobs, row))
            .collect();
        reports.sort_by(|a, b| {
            b["updated_at"]
                .as_f64()
                .partial_cmp(&a["updated_at"].as_f64())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(Json(json!({"ok":true,"reports":reports})))
    })();
    match result {
        Ok(body) => body.into_response(),
        Err(error) => {
            let status = if error.code.ends_with("run_not_found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            error.response(status)
        }
    }
}

fn folder_row(state: &AppState, folder_id: &str) -> Result<Option<Value>, Failure> {
    state.store.get(FOLDERS, folder_id).map_err(storage)
}

fn check_folder_ref(state: &AppState, folder_id: &Value, mode: &str) -> Result<Value, Failure> {
    if folder_id.is_null() {
        return Ok(Value::Null);
    }
    let Some(id) = folder_id.as_str() else {
        return Err(Failure::new("invalid_input", "folder_id must be a string or null"));
    };
    match folder_row(state, id)? {
        Some(folder) if folder["mode"].as_str() == Some(mode) => Ok(json!(id)),
        _ => Err(Failure::new("folder_not_found", "Folder not found")),
    }
}

fn build_report(
    state: &AppState,
    run_id: Option<&str>,
    mode: &str,
    body: &Value,
) -> Result<Value, Failure> {
    let title = body["title"].as_str().unwrap_or("").trim().to_owned();
    if title.is_empty() {
        return Err(Failure::new("invalid_input", "title is required"));
    }
    let markdown = if body.get("markdown").is_none() || body["markdown"].is_null() {
        String::new()
    } else {
        body["markdown"]
            .as_str()
            .ok_or_else(|| Failure::new("invalid_input", "markdown must be a string"))?
            .to_owned()
    };
    let status = if body.get("status").is_none() || body["status"].is_null() {
        "draft"
    } else {
        validate_status(&body["status"])?
    };
    let tags = if body.get("tags").is_none() || body["tags"].is_null() {
        Vec::new()
    } else {
        validate_tags(&body["tags"])?
    };
    let source = validate_source(body, mode, run_id.is_some())?;
    let import_source = validate_import_source(body)?;
    let folder_id = check_folder_ref(
        state,
        body.get("folder_id").unwrap_or(&Value::Null),
        mode,
    )?;
    if !import_source.is_null() {
        let existing = state
            .store
            .list(REPORTS)
            .map_err(storage)?
            .into_iter()
            .find(|row| {
                row["mode"].as_str() == Some(mode)
                    && row["import_source"]["path"] == import_source["path"]
                    && row["import_source"]["sha256"] == import_source["sha256"]
            });
        if let Some(existing) = existing {
            return Err(
                Failure::new("duplicate_import", "An identical import already exists")
                    .reported(existing),
            );
        }
    }
    let now = now_secs();
    let id = format!("rpt_{}", uuid::Uuid::new_v4().simple());
    let order = body["order"].as_f64().unwrap_or(now);
    Ok(json!({
        "id": id,
        "run_id": run_id,
        "mode": mode,
        "title": title,
        "markdown": markdown,
        "status": status,
        "tags": tags,
        "source": source,
        "folder_id": folder_id,
        "order": order,
        "import_source": import_source,
        "ai_sha256": Value::Null,
        "generation": Value::Null,
        "pending_update": Value::Null,
        "created_at": now,
        "updated_at": now,
        "revision": 1,
    }))
}

pub(super) async fn create(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let result: Result<(StatusCode, Json<Value>), Failure> = (|| {
        let doc = run_doc(&state, &run_id)?
            .ok_or_else(|| Failure::new("run_not_found", "Run not found"))?;
        let mode = doc["mode"].as_str().unwrap_or("ctf");
        let report = build_report(&state, Some(&run_id), mode, &body)?;
        let id = report["id"].as_str().unwrap_or_default().to_owned();
        if !state
            .store
            .put_many_if(
                &[("runs", &run_id, Some(&doc))],
                &[(REPORTS, &id, &report)],
            )
            .map_err(storage)?
        {
            return Err(Failure::new(
                "run_changed",
                "Run changed or was deleted while creating report",
            ));
        }
        Ok((StatusCode::CREATED, Json(json!({"ok":true,"report":report}))))
    })();
    respond_created(result)
}

pub(super) async fn create_generic(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    respond_created(create_doc(&state, &body))
}

pub(super) async fn import(
    State(state): State<Arc<AppState>>,
    Json(mut body): Json<Value>,
) -> Response {
    if body.get("source").is_none() || body["source"].is_null() {
        body["source"] = json!({"kind":"import","ref":body["source"]["ref"]});
    }
    respond_created(create_doc(&state, &body))
}

fn create_doc(state: &Arc<AppState>, body: &Value) -> Result<(StatusCode, Json<Value>), Failure> {
    (|| {
        let run_id = body["run_id"].as_str().map(str::to_owned);
        if let Some(run_id) = run_id.as_deref() {
            let doc = run_doc(&state, run_id)?
                .ok_or_else(|| Failure::new("run_not_found", "Run not found"))?;
            let mode = doc["mode"].as_str().unwrap_or("ctf");
            if let Err(unavailable) = crate::business::require(&state, module_for(mode)) {
                return Err(Failure::new("invalid_input", unavailable.message));
            }
            let report = build_report(&state, Some(run_id), mode, &body)?;
            let id = report["id"].as_str().unwrap_or_default().to_owned();
            if !state
                .store
                .put_many_if(
                    &[("runs", run_id, Some(&doc))],
                    &[(REPORTS, &id, &report)],
                )
                .map_err(storage)?
            {
                return Err(Failure::new(
                    "run_changed",
                    "Run changed or was deleted while creating report",
                ));
            }
            return Ok((
                StatusCode::CREATED,
                Json(json!({"ok":true,"report":report})),
            ));
        }
        let mode = body["mode"].as_str().unwrap_or("");
        if !matches!(mode, "ctf" | "pentest") {
            return Err(Failure::new(
                "invalid_mode",
                "mode must be \"ctf\" or \"pentest\"",
            ));
        }
        if let Err(unavailable) = crate::business::require(&state, module_for(mode)) {
            return Err(Failure::new("invalid_input", unavailable.message));
        }
        let report = build_report(&state, None, mode, &body)?;
        let id = report["id"].as_str().unwrap_or_default().to_owned();
        state.store.put(REPORTS, &id, &report).map_err(storage)?;
        Ok((StatusCode::CREATED, Json(json!({"ok":true,"report":report}))))
    })()
}

fn respond_created(result: Result<(StatusCode, Json<Value>), Failure>) -> Response {
    match result {
        Ok((status, body)) => (status, body).into_response(),
        Err(error) => error.respond(),
    }
}

fn load_report(state: &AppState, run_id: &str, report_id: &str) -> Result<Value, Failure> {
    let row = state
        .store
        .get(REPORTS, report_id)
        .map_err(storage)?
        .filter(|row| report_visible(row, run_id))
        .ok_or_else(|| Failure::new("report_not_found", "Report not found"))?;
    Ok(row)
}

fn load_report_any(state: &AppState, report_id: &str) -> Result<Value, Failure> {
    state
        .store
        .get(REPORTS, report_id)
        .map_err(storage)?
        .ok_or_else(|| Failure::new("report_not_found", "Report not found"))
}

pub(super) async fn get_one(
    State(state): State<Arc<AppState>>,
    Path((run_id, report_id)): Path<(String, String)>,
) -> Response {
    match load_report(&state, &run_id, &report_id).and_then(|report| {
        Ok(decorated(&list_jobs(&state)?, report))
    }) {
        Ok(report) => Json(json!({"ok":true,"report":report})).into_response(),
        Err(error) => error.respond(),
    }
}

pub(super) async fn get_one_generic(
    State(state): State<Arc<AppState>>,
    Path(report_id): Path<String>,
) -> Response {
    match load_report_any(&state, &report_id).and_then(|report| {
        if let Err(unavailable) =
            crate::business::require(&state, module_for(report["mode"].as_str().unwrap_or("ctf")))
        {
            return Err(Failure::new("invalid_input", unavailable.message));
        }
        Ok(decorated(&list_jobs(&state)?, report))
    }) {
        Ok(report) => Json(json!({"ok":true,"report":report})).into_response(),
        Err(error) => error.respond(),
    }
}

fn apply_content_fields(before: &mut Value, body: &Value) -> Result<(), Failure> {
    if let Some(title) = body.get("title") {
        let title = title.as_str().unwrap_or("").trim().to_owned();
        if title.is_empty() {
            return Err(Failure::new("invalid_input", "title must be non-empty"));
        }
        before["title"] = json!(title);
    }
    if let Some(markdown) = body.get("markdown") {
        let Some(markdown) = markdown.as_str() else {
            return Err(Failure::new("invalid_input", "markdown must be a string"));
        };
        before["markdown"] = json!(markdown);
    }
    if let Some(status) = body.get("status").filter(|s| !s.is_null()) {
        before["status"] = json!(validate_status(status)?);
    }
    if let Some(tags) = body.get("tags").filter(|t| !t.is_null()) {
        before["tags"] = json!(validate_tags(tags)?);
    }
    Ok(())
}

/// Revision conflict only when the stored revision really differs; a concurrent
/// metadata write makes the CAS fail without touching revision, so retry reads.
fn update_report_doc(
    state: &AppState,
    report_id: &str,
    body: &Value,
    content: bool,
    metadata: bool,
    expected_revision: Option<u64>,
) -> Result<Value, Failure> {
    let mut last = Value::Null;
    for _ in 0..3 {
        let before = load_report_any(state, report_id)?;
        if let Some(expected) = expected_revision {
            if before["revision"].as_u64() != Some(expected) {
                return Err(
                    Failure::new("revision_conflict", "Report was modified elsewhere")
                        .reported(before),
                );
            }
        }
        let mut after = before.clone();
        if content {
            apply_content_fields(&mut after, body)?;
        }
        if metadata {
            if let Some(folder_id) = body.get("folder_id") {
                after["folder_id"] = check_folder_ref(
                    state,
                    folder_id,
                    after["mode"].as_str().unwrap_or("ctf"),
                )?;
            }
            if let Some(order) = body.get("order") {
                let Some(order) = order.as_f64() else {
                    return Err(Failure::new("invalid_input", "order must be a number"));
                };
                after["order"] = json!(order);
            }
        }
        if content {
            after["revision"] = json!(before["revision"].as_u64().unwrap_or(0) + 1);
            after["updated_at"] = json!(now_secs());
        }
        if state
            .store
            .put_many_if(
                &[(REPORTS, report_id, Some(&before))],
                &[(REPORTS, report_id, &after)],
            )
            .map_err(storage)?
        {
            return Ok(after);
        }
        if let Some(current) = state.store.get(REPORTS, report_id).map_err(storage)? {
            last = current;
        }
    }
    Err(Failure::new("revision_conflict", "Report was modified elsewhere").reported(last))
}

pub(super) async fn update(
    State(state): State<Arc<AppState>>,
    Path((run_id, report_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Response {
    let result: Result<Json<Value>, (StatusCode, Json<Value>)> = (|| {
        let before = load_report(&state, &run_id, &report_id).map_err(|error| {
            (
                error.status(),
                Json(json!({"ok":false,"error":{"code":error.code,"message":error.message}})),
            )
        })?;
        let expected_revision = body["expected_revision"].as_u64().ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"ok":false,"error":{"code":"run.report.invalid_input","message":"expected_revision is required"}})),
            )
        })?;
        if before["revision"].as_u64() != Some(expected_revision) {
            return Err((
                StatusCode::CONFLICT,
                Json(json!({"ok":false,"error":{"code":"run.report.revision_conflict","message":"Report was modified elsewhere"},"report":before})),
            ));
        }
        let mut after = before.clone();
        apply_content_fields(&mut after, &body).map_err(|error| {
            (
                error.status(),
                Json(json!({"ok":false,"error":{"code":error.code,"message":error.message}})),
            )
        })?;
        after["revision"] = json!(expected_revision + 1);
        after["updated_at"] = json!(now_secs());
        let committed = state
            .store
            .put_many_if(
                &[(REPORTS, &report_id, Some(&before))],
                &[(REPORTS, &report_id, &after)],
            )
            .map_err(|error| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"ok":false,"error":{"code":"run.report.storage_failed","message":error.to_string()}})),
                )
            })?;
        if !committed {
            let current = state
                .store
                .get(REPORTS, &report_id)
                .map_err(|error| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"ok":false,"error":{"code":"run.report.storage_failed","message":error.to_string()}})),
                    )
                })?
                .filter(|row| report_visible(row, &run_id))
                .unwrap_or(before);
            return Err((
                StatusCode::CONFLICT,
                Json(json!({"ok":false,"error":{"code":"run.report.revision_conflict","message":"Report was modified elsewhere"},"report":current})),
            ));
        }
        Ok(Json(json!({"ok":true,"report":after})))
    })();
    match result {
        Ok(body) => body.into_response(),
        Err((status, body)) => (status, body).into_response(),
    }
}

pub(super) async fn update_generic(
    State(state): State<Arc<AppState>>,
    Path(report_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let result: Result<Json<Value>, Failure> = (|| {
        let report = load_report_any(&state, &report_id)?;
        if let Err(unavailable) =
            crate::business::require(&state, module_for(report["mode"].as_str().unwrap_or("ctf")))
        {
            return Err(Failure::new("invalid_input", unavailable.message));
        }
        let content = ["title", "markdown", "status", "tags"]
            .iter()
            .any(|key| body.get(*key).is_some());
        let metadata = ["folder_id", "order"]
            .iter()
            .any(|key| body.get(*key).is_some());
        let expected_revision = body["expected_revision"].as_u64();
        if content && expected_revision.is_none() {
            return Err(Failure::new(
                "invalid_input",
                "expected_revision is required",
            ));
        }
        let after = update_report_doc(
            &state,
            &report_id,
            &body,
            content,
            metadata,
            expected_revision,
        )?;
        let jobs = list_jobs(&state)?;
        Ok(Json(json!({"ok":true,"report":decorated(&jobs, after)})))
    })();
    match result {
        Ok(body) => body.into_response(),
        Err(error) => error.respond(),
    }
}

pub(super) async fn remove(
    State(state): State<Arc<AppState>>,
    Path((run_id, report_id)): Path<(String, String)>,
) -> Response {
    match load_report(&state, &run_id, &report_id) {
        Ok(report) => {
            let id = report["id"].as_str().unwrap_or_default().to_owned();
            match state.store.delete(REPORTS, &id).map_err(storage) {
                Ok(_) => Json(json!({"ok":true})).into_response(),
                Err(error) => error.response(StatusCode::INTERNAL_SERVER_ERROR),
            }
        }
        Err(error) => error.respond(),
    }
}

pub(super) async fn remove_generic(
    State(state): State<Arc<AppState>>,
    Path(report_id): Path<String>,
) -> Response {
    match load_report_any(&state, &report_id) {
        Ok(report) => {
            if let Err(unavailable) = crate::business::require(
                &state,
                module_for(report["mode"].as_str().unwrap_or("ctf")),
            ) {
                return Failure::new("invalid_input", unavailable.message)
                    .response(StatusCode::BAD_REQUEST);
            }
            let id = report["id"].as_str().unwrap_or_default().to_owned();
            match state.store.delete(REPORTS, &id).map_err(storage) {
                Ok(_) => Json(json!({"ok":true})).into_response(),
                Err(error) => error.respond(),
            }
        }
        Err(error) => error.respond(),
    }
}

/// Bind a report to a writeup job before the job spawns. The base hash of the
/// markdown is captured under the same CAS that publishes the binding so a
/// user edit between read and bind is detected at completion.
pub(super) fn bind_generation(
    state: &AppState,
    report_id: &str,
    job_id: &str,
) -> Result<Value, Failure> {
    for _ in 0..3 {
        let before = load_report_any(state, report_id)?;
        let generation = &before["generation"];
        if generation["state"].as_str() == Some("active") {
            if generation["job_id"].as_str() == Some(job_id) {
                return Ok(before);
            }
            return Err(Failure::new(
                "generation_active",
                "Report already has an active generation",
            ));
        }
        let mut after = before.clone();
        let current = before["markdown"].as_str().unwrap_or("");
        let replaceable = current.is_empty()
            || before["ai_sha256"].as_str() == Some(sha256_hex(current).as_str());
        let base = if replaceable { sha256_hex(current) } else { String::new() };
        after["generation"] = json!({
            "job_id": job_id,
            "state": "active",
            "base_sha256": base,
            "started_at": now_secs(),
        });
        if state
            .store
            .put_many_if(
                &[(REPORTS, report_id, Some(&before))],
                &[(REPORTS, report_id, &after)],
            )
            .map_err(storage)?
        {
            return Ok(after);
        }
    }
    Err(storage("Report changed during generation binding"))
}

pub(super) fn unbind_generation(
    state: &AppState,
    report_id: &str,
    job_id: &str,
) -> Result<(), Failure> {
    for _ in 0..3 {
        let Some(before) = state.store.get(REPORTS, report_id).map_err(storage)? else {
            return Ok(());
        };
        if before["generation"]["job_id"].as_str() != Some(job_id)
            || before["generation"]["state"].as_str() != Some("active")
        {
            return Ok(());
        }
        let mut after = before.clone();
        after["generation"] = Value::Null;
        if state
            .store
            .put_many_if(
                &[(REPORTS, report_id, Some(&before))],
                &[(REPORTS, report_id, &after)],
            )
            .map_err(storage)?
        {
            return Ok(());
        }
    }
    Err(storage("Report changed during generation unbind"))
}

fn cas_update(state: &AppState, report_id: &str, apply: impl Fn(&mut Value)) -> Result<(), Failure> {
    for _ in 0..3 {
        let Some(before) = state.store.get(REPORTS, report_id).map_err(storage)? else {
            return Ok(());
        };
        let mut after = before.clone();
        apply(&mut after);
        if state
            .store
            .put_many_if(
                &[(REPORTS, report_id, Some(&before))],
                &[(REPORTS, report_id, &after)],
            )
            .map_err(storage)?
        {
            return Ok(());
        }
    }
    Err(storage("Report changed during generation sync"))
}

pub(super) enum WriteupOutcome {
    Completed(String),
    Failed { code: String, message: String },
    Cancelled,
}

fn apply_completed(report: &mut Value, markdown: &str, sha: &str, job_id: &str) {
    report["markdown"] = json!(markdown);
    report["source"] = json!({"kind":"ai_writeup","ref":job_id});
    report["ai_sha256"] = json!(sha);
    report["generation"] = Value::Null;
    report["pending_update"] = Value::Null;
    report["revision"] = json!(report["revision"].as_u64().unwrap_or(0) + 1);
    report["updated_at"] = json!(now_secs());
}

/// Apply a finished writeup job to the reports of its run. Completed results
/// fill bound and pristine AI copies; user-edited bodies get pending_update.
pub(super) fn sync_writeup_outcome(
    state: &AppState,
    run_id: &str,
    job_id: &str,
    outcome: &WriteupOutcome,
) -> Result<Option<String>, Failure> {
    let Some(run) = state.store.get("runs", run_id).map_err(storage)? else {
        return Ok(None);
    };
    if run["mode"].as_str() == Some("pentest") {
        return Ok(None);
    }
    let job_row = state.store.get(followup::JOBS, job_id).map_err(storage)?;
    if job_row
        .as_ref()
        .is_some_and(|row| row["kind"].as_str() != Some("writeup"))
    {
        return Ok(None);
    }
    let reports: Vec<Value> = state
        .store
        .list(REPORTS)
        .map_err(storage)?
        .into_iter()
        .filter(|row| row["run_id"].as_str() == Some(run_id))
        .collect();
    let mut synced_id: Option<String> = None;
    match outcome {
        WriteupOutcome::Completed(markdown) => {
            let sha = sha256_hex(markdown);
            for report in &reports {
                let id = report["id"].as_str().unwrap_or_default().to_owned();
                if report["markdown"].as_str() == Some(markdown.as_str())
                    && report["source"]["ref"].as_str() == Some(job_id)
                    && report["source"]["kind"].as_str() == Some("ai_writeup")
                    && report["generation"].is_null()
                    && report["pending_update"].is_null()
                {
                    if !id.is_empty() && synced_id.is_none() {
                        synced_id = Some(id);
                    }
                    continue;
                }
                let bound = report["generation"]["job_id"].as_str() == Some(job_id)
                    && report["generation"]["state"].as_str() == Some("active");
                let bytes = markdown.len() as u64;
                if bound {
                    if !id.is_empty() {
                        synced_id = Some(id.clone());
                    }
                    cas_update(state, &id, |r| {
                        if r["generation"]["job_id"].as_str() != Some(job_id)
                            || r["generation"]["state"].as_str() != Some("active")
                        {
                            return;
                        }
                        if sha256_hex(r["markdown"].as_str().unwrap_or(""))
                            == r["generation"]["base_sha256"].as_str().unwrap_or("")
                        {
                            apply_completed(r, markdown, &sha, job_id);
                        } else {
                            r["generation"] = Value::Null;
                            r["pending_update"] =
                                json!({"job_id":job_id,"created_at":now_secs(),"bytes":bytes});
                        }
                    })?;
                } else if report["source"]["kind"].as_str() == Some("ai_writeup") {
                    if !id.is_empty() && synced_id.is_none() {
                        synced_id = Some(id.clone());
                    }
                    let current = report["markdown"].as_str().unwrap_or("");
                    if !current.is_empty()
                        && report["ai_sha256"].as_str() == Some(sha256_hex(current).as_str())
                    {
                        cas_update(state, &id, |r| {
                            if r["source"]["kind"].as_str() == Some("ai_writeup") {
                                apply_completed(r, markdown, &sha, job_id);
                            }
                        })?;
                    } else if report["pending_update"]["job_id"].as_str() != Some(job_id) {
                        cas_update(state, &id, |r| {
                            r["pending_update"] =
                                json!({"job_id":job_id,"created_at":now_secs(),"bytes":bytes});
                        })?;
                    }
                }
            }
            if reports.is_empty() {
                let unbound = job_row
                    .as_ref()
                    .and_then(|row| row["report_ids"].as_array())
                    .is_none_or(|ids| ids.is_empty());
                if unbound {
                    let now = now_secs();
                    let id = format!("rpt_{}", uuid::Uuid::new_v4().simple());
                    let title = run["name"]
                        .as_str()
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or(run_id)
                        .to_owned();
                    let report = json!({
                        "id": id,
                        "run_id": run_id,
                        "mode": "ctf",
                        "title": title,
                        "markdown": markdown,
                        "status": "draft",
                        "tags": [],
                        "source": {"kind":"ai_writeup","ref":job_id},
                        "folder_id": Value::Null,
                        "order": now,
                        "import_source": Value::Null,
                        "ai_sha256": sha,
                        "generation": Value::Null,
                        "pending_update": Value::Null,
                        "created_at": now,
                        "updated_at": now,
                        "revision": 1,
                    });
                    state.store.put(REPORTS, &id, &report).map_err(storage)?;
                    synced_id = Some(id);
                }
            }
            if synced_id.is_none() {
                synced_id = reports
                    .first()
                    .and_then(|r| r["id"].as_str())
                    .filter(|id| !id.is_empty())
                    .map(ToOwned::to_owned);
            }
        }
        WriteupOutcome::Failed { code, message } => {
            for report in &reports {
                if report["generation"]["job_id"].as_str() != Some(job_id)
                    || report["generation"]["state"].as_str() != Some("active")
                {
                    continue;
                }
                let id = report["id"].as_str().unwrap_or_default().to_owned();
                cas_update(state, &id, |r| {
                    if r["generation"]["job_id"].as_str() != Some(job_id)
                        || r["generation"]["state"].as_str() != Some("active")
                    {
                        return;
                    }
                    let base = r["generation"]["base_sha256"].clone();
                    let started = r["generation"]["started_at"].clone();
                    r["generation"] = json!({
                        "job_id": job_id,
                        "state": "failed",
                        "base_sha256": base,
                        "started_at": started,
                        "finished_at": now_secs(),
                        "error": {"code":code,"message":message},
                    });
                })?;
            }
        }
        WriteupOutcome::Cancelled => {
            for report in &reports {
                if report["generation"]["job_id"].as_str() != Some(job_id)
                    || report["generation"]["state"].as_str() != Some("active")
                {
                    continue;
                }
                let id = report["id"].as_str().unwrap_or_default().to_owned();
                cas_update(state, &id, |r| {
                    if r["generation"]["job_id"].as_str() == Some(job_id)
                        && r["generation"]["state"].as_str() == Some("active")
                    {
                        r["generation"] = Value::Null;
                    }
                })?;
            }
        }
    }
    Ok(synced_id)
}

/// Outcome to replay for a job row already in a terminal state; None while a
/// job is still active or cannot produce a report effect.
pub(super) fn terminal_outcome(
    state: &AppState,
    job: Option<&Value>,
    job_id: &str,
) -> Option<WriteupOutcome> {
    match job.and_then(|row| row["state"].as_str()) {
        Some("completed") => {
            let artifact = state.store.get(followup::ARTIFACTS, job_id).ok().flatten();
            match artifact.and_then(|row| row["markdown"].as_str().map(str::to_owned)) {
                Some(markdown) => Some(WriteupOutcome::Completed(markdown)),
                None => Some(WriteupOutcome::Failed {
                    code: "run.followup.writeup_not_found".into(),
                    message: "Bound generation artifact is missing".into(),
                }),
            }
        }
        Some("failed") => Some(WriteupOutcome::Failed {
            code: job
                .and_then(|row| row["error"]["code"].as_str())
                .unwrap_or("run.followup.runtime_failed")
                .to_owned(),
            message: job
                .and_then(|row| row["error"]["message"].as_str())
                .unwrap_or("Generation job failed")
                .to_owned(),
        }),
        Some("cancelled") => Some(WriteupOutcome::Cancelled),
        _ => None,
    }
}

/// Heal report generation bindings after a restart. followup::reconcile runs
/// first and has already failed stale queued/running rows.
pub(super) fn reconcile(state: &AppState) -> Result<(), String> {
    for report in state.store.list(REPORTS).map_err(|e| e.to_string())? {
        if report["generation"]["state"].as_str() != Some("active") {
            continue;
        }
        let Some(job_id) = report["generation"]["job_id"].as_str().map(str::to_owned) else {
            continue;
        };
        let Some(run_id) = report["run_id"].as_str().map(str::to_owned) else {
            continue;
        };
        let job = state
            .store
            .get(followup::JOBS, &job_id)
            .map_err(|e| e.to_string())?;
        let outcome = match job.as_ref() {
            None => Some(WriteupOutcome::Failed {
                code: "run.followup.job_missing".into(),
                message: "Bound generation job is missing".into(),
            }),
            Some(row) => {
                if matches!(row["state"].as_str(), Some("queued" | "running" | "accepted"))
                    && row["owner_epoch"] == state.chat.epoch()
                {
                    None
                } else if matches!(row["state"].as_str(), Some("queued" | "running" | "accepted")) {
                    Some(WriteupOutcome::Failed {
                        code: "run.followup.server_restarted".into(),
                        message: "Server restarted during followup".into(),
                    })
                } else {
                    terminal_outcome(state, Some(row), &job_id)
                }
            }
        };
        if let Some(outcome) = outcome
            && let Err(error) = sync_writeup_outcome(state, &run_id, &job_id, &outcome)
        {
            tracing::error!(code=error.code,error=error.message,report=report["id"].as_str().unwrap_or(""),"Report generation reconcile failed");
        }
    }
    Ok(())
}

/// Process-wide cap for concurrent writeup generations, read live so a
/// settings change applies to queued waiters on their next poll.
pub(super) fn generation_limit(state: &AppState) -> Result<usize, Failure> {
    Ok(configured_limit(&read_generation_settings(state)?))
}

fn merge_writeup_templates(saved: Option<&Value>) -> Vec<Value> {
    let builtins = builtin_writeup_templates();
    let Some(items) = saved.and_then(Value::as_array) else {
        return builtins;
    };
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for item in items {
        let Some(id) = item["id"].as_str().map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        if !seen.insert(id.to_owned()) {
            continue;
        }
        let Some(name) = item["name"].as_str().map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        let markdown = item["markdown"].as_str().unwrap_or("").to_owned();
        let description = item["description"].as_str().unwrap_or("").trim().to_owned();
        let prompt = item["prompt"].as_str().unwrap_or("").trim().to_owned();
        out.push(json!({
            "id": id,
            "name": name,
            "description": description,
            "markdown": markdown,
            "prompt": prompt,
            "builtin": is_builtin_template_id(id),
        }));
    }
    for builtin in builtins {
        let id = builtin["id"].as_str().unwrap_or("");
        if !seen.contains(id) {
            out.push(builtin);
        }
    }
    out
}

fn valid_template_id(id: &str) -> bool {
    let mut chars = id.chars();
    !id.is_empty()
        && id.len() <= 64
        && chars.next().is_some_and(|ch| ch.is_ascii_alphanumeric())
        && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
}

fn validate_templates_input(value: &Value) -> Result<Vec<Value>, Failure> {
    let Some(items) = value.as_array() else {
        return Err(Failure::new("invalid_input", "templates must be an array"));
    };
    if items.len() > MAX_TEMPLATES {
        return Err(Failure::new(
            "invalid_input",
            format!("At most {MAX_TEMPLATES} templates are allowed"),
        ));
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for item in items {
        if !item.is_object() {
            return Err(Failure::new("invalid_input", "Each template must be an object"));
        }
        let id = item["id"].as_str().unwrap_or("").trim();
        if !valid_template_id(id) {
            return Err(Failure::new(
                "invalid_input",
                "Template id must be 1..=64 ASCII alphanumeric, '_' or '-' characters",
            ));
        }
        if !seen.insert(id.to_owned()) {
            return Err(Failure::new("invalid_input", format!("Duplicate template id: {id}")));
        }
        let name = item["name"].as_str().unwrap_or("").trim();
        if name.is_empty() || name.chars().count() > 80 {
            return Err(Failure::new(
                "invalid_input",
                "Template name must be 1..=80 characters",
            ));
        }
        let Some(markdown) = item["markdown"].as_str() else {
            return Err(Failure::new("invalid_input", "Template markdown must be a string"));
        };
        if markdown.len() > MAX_TEMPLATE_BYTES {
            return Err(Failure::new(
                "invalid_input",
                "Template markdown exceeds size limit",
            ));
        }
        let description = item["description"].as_str().unwrap_or("").trim();
        let prompt = item["prompt"].as_str().unwrap_or("").trim();
        if prompt.len() > MAX_TEMPLATE_BYTES {
            return Err(Failure::new(
                "invalid_input",
                "Template prompt exceeds size limit",
            ));
        }
        out.push(json!({
            "id": id,
            "name": name,
            "description": description,
            "markdown": markdown,
            "prompt": prompt,
            "builtin": is_builtin_template_id(id),
        }));
    }
    Ok(merge_writeup_templates(Some(&Value::Array(out))))
}

fn read_generation_settings(state: &AppState) -> Result<Value, Failure> {
    let row = state
        .store
        .get("settings", GENERATION_SETTINGS)
        .map_err(storage)?
        .unwrap_or_else(|| json!({}));
    if !row.is_object() {
        return Err(storage("Saved report generation settings are not an object"));
    }
    Ok(row)
}

fn configured_limit(row: &Value) -> usize {
    row["max_concurrent"].as_u64().unwrap_or(DEFAULT_MAX_CONCURRENT as u64).clamp(1, 8) as usize
}

fn generation_config(row: &Value) -> (String, String, Vec<Value>) {
    let writeup_prompt = row["writeup_prompt"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_WRITEUP_PROMPT)
        .to_owned();
    let templates = merge_writeup_templates(row.get("templates"));
    let requested_default = row["default_template_id"].as_str().unwrap_or(DEFAULT_TEMPLATE_ID);
    let default_template_id = if templates.iter().any(|t| t["id"].as_str() == Some(requested_default)) {
        requested_default.to_owned()
    } else {
        DEFAULT_TEMPLATE_ID.to_owned()
    };
    (writeup_prompt, default_template_id, templates)
}

pub(super) struct WriteupGenerationConfig {
    pub(super) prompt: String,
    pub(super) template_markdown: String,
    pub(super) template_id: String,
}

pub(super) fn resolve_writeup_generation_config(
    state: &AppState,
    requested_template_id: Option<&str>,
    custom_prompt: Option<&str>,
    custom_template: Option<&str>,
) -> Result<WriteupGenerationConfig, Failure> {
    let (global_prompt, default_template_id, templates) = generation_config(&read_generation_settings(state)?);
    let target_id = requested_template_id
        .map(str::trim)
        .filter(|s| !s.is_empty() && templates.iter().any(|t| t["id"].as_str() == Some(*s)))
        .unwrap_or(default_template_id.as_str())
        .to_owned();
    let chosen = templates
        .iter()
        .find(|t| t["id"].as_str() == Some(target_id.as_str()))
        .or_else(|| templates.first());

    let mut prompt = custom_prompt
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or(global_prompt);
    if custom_prompt.is_none_or(|s| s.trim().is_empty())
        && let Some(tpl_prompt) = chosen
            .and_then(|t| t["prompt"].as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    {
        prompt.push_str("\n\n【所选模板附加要求】\n");
        prompt.push_str(tpl_prompt);
    }

    let template_markdown = custom_template
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| chosen.and_then(|t| t["markdown"].as_str()).map(str::to_owned))
        .unwrap_or_default();

    Ok(WriteupGenerationConfig { prompt, template_markdown, template_id: target_id })
}

fn settings_response(row: &Value, jobs: &[Value]) -> Value {
    let (writeup_prompt, default_template_id, templates) = generation_config(row);
    let running = jobs
        .iter()
        .filter(|r| r["kind"].as_str() == Some("writeup") && r["state"].as_str() == Some("running"))
        .count();
    let queued = jobs
        .iter()
        .filter(|r| r["kind"].as_str() == Some("writeup") && r["state"].as_str() == Some("queued"))
        .count();
    json!({
        "ok": true,
        "max_concurrent": configured_limit(row),
        "running": running,
        "queued": queued,
        "writeup_prompt": writeup_prompt,
        "default_writeup_prompt": DEFAULT_WRITEUP_PROMPT,
        "default_template_id": default_template_id,
        "templates": templates,
        "builtin_templates": builtin_writeup_templates(),
    })
}

pub(super) async fn get_settings(State(state): State<Arc<AppState>>) -> Response {
    let result: Result<Json<Value>, Failure> = (|| {
        let jobs = list_jobs(&state)?;
        Ok(Json(settings_response(&read_generation_settings(&state)?, &jobs)))
    })();
    match result {
        Ok(body) => body.into_response(),
        Err(error) => error.respond(),
    }
}

pub(super) async fn put_settings(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let result: Result<Json<Value>, Failure> = (|| {
        let mut row = read_generation_settings(&state)?;

        if let Some(limit_val) = body.get("max_concurrent").filter(|v| !v.is_null()) {
            let Some(limit) = limit_val.as_u64() else {
                return Err(Failure::new(
                    "invalid_input",
                    "max_concurrent must be an integer",
                ));
            };
            if !(1..=8).contains(&limit) {
                return Err(Failure::new(
                    "invalid_input",
                    "max_concurrent must be between 1 and 8",
                ));
            }
            row["max_concurrent"] = json!(limit);
        }

        if let Some(prompt_val) = body.get("writeup_prompt") {
            if prompt_val.is_null() {
                row["writeup_prompt"] = Value::Null;
            } else if let Some(prompt) = prompt_val.as_str() {
                let trimmed = prompt.trim();
                if trimmed.len() > MAX_TEMPLATE_BYTES {
                    return Err(Failure::new(
                        "invalid_input",
                        "writeup_prompt exceeds size limit",
                    ));
                }
                row["writeup_prompt"] = if trimmed.is_empty() {
                    Value::Null
                } else {
                    json!(trimmed)
                };
            } else {
                return Err(Failure::new(
                    "invalid_input",
                    "writeup_prompt must be a string or null",
                ));
            }
        }

        if let Some(templates_val) = body.get("templates") {
            if templates_val.is_null() {
                row["templates"] = Value::Null;
            } else {
                let validated = validate_templates_input(templates_val)?;
                row["templates"] = Value::Array(validated);
            }
        }

        let effective_templates = merge_writeup_templates(row.get("templates"));
        if let Some(default_id_val) = body.get("default_template_id") {
            if default_id_val.is_null() {
                row["default_template_id"] = json!(DEFAULT_TEMPLATE_ID);
            } else if let Some(id) = default_id_val.as_str().map(str::trim) {
                if !effective_templates.iter().any(|t| t["id"].as_str() == Some(id)) {
                    return Err(Failure::new(
                        "invalid_input",
                        "default_template_id must match an existing template",
                    ));
                }
                row["default_template_id"] = json!(id);
            } else {
                return Err(Failure::new(
                    "invalid_input",
                    "default_template_id must be a string or null",
                ));
            }
        }

        let jobs = list_jobs(&state)?;
        row["updated_at"] = json!(now_secs());
        state
            .store
            .put("settings", GENERATION_SETTINGS, &row)
            .map_err(storage)?;
        Ok(Json(settings_response(&row, &jobs)))
    })();
    match result {
        Ok(body) => body.into_response(),
        Err(error) => error.respond(),
    }
}

pub(super) async fn generate(
    State(state): State<Arc<AppState>>,
    Path(report_id): Path<String>,
    raw_body: axum::body::Bytes,
) -> Response {
    let result: Result<(StatusCode, Json<Value>), Failure> = async {
        let req_body: Value = if raw_body.is_empty() {
            json!({})
        } else {
            serde_json::from_slice(&raw_body)
                .map_err(|_| Failure::new("invalid_input", "Request body must be valid JSON"))?
        };
        let report = load_report_any(&state, &report_id)?;
        let run_id = report["run_id"].as_str().unwrap_or("");
        if run_id.is_empty() || report["mode"].as_str() != Some("ctf") {
            return Err(Failure::new(
                "not_supported",
                "AI writeup generation requires a CTF run-bound report",
            ));
        }
        if let Err(unavailable) = crate::business::require(&state, module_for("ctf")) {
            return Err(Failure::new("invalid_input", unavailable.message));
        }
        let body = json!({
            "kind": "writeup",
            "request_id": uuid::Uuid::new_v4().simple().to_string(),
            "report_id": report_id,
            "template_id": req_body.get("template_id").cloned().unwrap_or(Value::Null),
            "custom_prompt": req_body.get("custom_prompt").cloned().unwrap_or(Value::Null),
            "custom_template": req_body.get("custom_template").cloned().unwrap_or(Value::Null),
        });
        let job = followup::admit_report(&state, run_id.to_owned(), body)
            .await
            .map_err(|e| Failure {
                code: e.code,
                message: e.message,
                report: None,
            })?;
        let status = if report["generation"]["state"] == "active" && report["generation"]["job_id"] == job["id"] {
            StatusCode::OK
        } else {
            StatusCode::ACCEPTED
        };
        let jobs = list_jobs(&state)?;
        let report = load_report_any(&state, &report_id)?;
        Ok((
            status,
            Json(json!({"ok":true,"report":decorated(&jobs, report),"job":job})),
        ))
    }
    .await;
    match result {
        Ok((status, body)) => (status, body).into_response(),
        Err(error) => {
            let status = if error.code.starts_with("run.followup.") {
                followup_status(&error.code)
            } else {
                error.status()
            };
            error.response(status)
        }
    }
}

fn followup_status(code: &str) -> StatusCode {
    if [
        "run.followup.busy",
        "run.followup.run_still_active",
        "run.followup.request_conflict",
    ]
    .contains(&code)
    {
        StatusCode::CONFLICT
    } else if code.ends_with("run_not_found") || code.ends_with("report_not_found") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::UNPROCESSABLE_ENTITY
    }
}

pub(super) async fn delete_generation(
    State(state): State<Arc<AppState>>,
    Path(report_id): Path<String>,
) -> Response {
    let result: Result<Json<Value>, Failure> = async {
        let report = load_report_any(&state, &report_id)?;
        let generation = report["generation"].clone();
        match generation["state"].as_str() {
            Some("failed") => {
                let id = report["id"].as_str().unwrap_or_default().to_owned();
                cas_update(&state, &id, |r| {
                    r["generation"] = Value::Null;
                })?;
            }
            Some("active") => {
                let job_id = generation["job_id"].as_str().unwrap_or("").to_owned();
                let bound: Vec<String> = state
                    .store
                    .list(REPORTS)
                    .map_err(storage)?
                    .iter()
                    .filter(|row| {
                        row["generation"]["job_id"].as_str() == Some(job_id.as_str())
                            && row["generation"]["state"].as_str() == Some("active")
                    })
                    .filter_map(|row| row["id"].as_str().map(str::to_owned))
                    .collect();
                if bound.len() > 1 {
                    unbind_generation(&state, &report_id, &job_id)?;
                    followup::detach_report(&state, &job_id, &report_id)?;
                } else if let Some(run_id) = report["run_id"].as_str() {
                    match followup::cancel_job(&state, run_id, &job_id).await {
                        Ok(_) => {}
                        Err(error) if error.code == "run.followup.job_not_found" => {}
                        Err(error) => {
                            return Err(Failure {
                                code: error.code,
                                message: error.message,
                                report: None,
                            });
                        }
                    }
                    unbind_generation(&state, &report_id, &job_id)?;
                } else {
                    unbind_generation(&state, &report_id, &job_id)?;
                }
            }
            _ => {}
        }
        let jobs = list_jobs(&state)?;
        let report = load_report_any(&state, &report_id)?;
        Ok(Json(json!({"ok":true,"report":decorated(&jobs, report)})))
    }
    .await;
    match result {
        Ok(body) => body.into_response(),
        Err(error) => {
            let status = if error.code.starts_with("run.followup.") {
                followup_status(&error.code)
            } else {
                error.status()
            };
            error.response(status)
        }
    }
}

pub(super) async fn pending_update(
    State(state): State<Arc<AppState>>,
    Path(report_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let result: Result<Json<Value>, Failure> = (|| {
        let report = load_report_any(&state, &report_id)?;
        let action = body["action"].as_str().unwrap_or("");
        if !matches!(action, "apply" | "dismiss") {
            return Err(Failure::new(
                "invalid_input",
                "action must be \"apply\" or \"dismiss\"",
            ));
        }
        let expected_revision = body["expected_revision"]
            .as_u64()
            .ok_or_else(|| Failure::new("invalid_input", "expected_revision is required"))?;
        if report["revision"].as_u64() != Some(expected_revision) {
            return Err(Failure::new("revision_conflict", "Report was modified elsewhere")
                .reported(report));
        }
        if report["pending_update"].is_null() {
            return Err(Failure::new("invalid_input", "Report has no pending update"));
        }
        match action {
            "apply" => {
                let job_id = report["pending_update"]["job_id"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned();
                let artifact = state
                    .store
                    .get(followup::ARTIFACTS, &job_id)
                    .map_err(storage)?
                    .ok_or_else(|| Failure::new("writeup_not_found", "Writeup not found"))?;
                let markdown = artifact["markdown"]
                    .as_str()
                    .ok_or_else(|| Failure::new("writeup_not_found", "Writeup not found"))?
                    .to_owned();
                let sha = sha256_hex(&markdown);
                let mut last = report.clone();
                for _ in 0..3 {
                    let before = load_report_any(&state, &report_id)?;
                    if before["revision"].as_u64() != Some(expected_revision) {
                        return Err(Failure::new(
                            "revision_conflict",
                            "Report was modified elsewhere",
                        )
                        .reported(before));
                    }
                    let mut after = before.clone();
                    apply_completed(&mut after, &markdown, &sha, &job_id);
                    if state
                        .store
                        .put_many_if(
                            &[(REPORTS, &report_id, Some(&before))],
                            &[(REPORTS, &report_id, &after)],
                        )
                        .map_err(storage)?
                    {
                        let jobs = list_jobs(&state)?;
                        return Ok(Json(
                            json!({"ok":true,"report":decorated(&jobs, after)}),
                        ));
                    }
                    if let Some(current) = state.store.get(REPORTS, &report_id).map_err(storage)? {
                        last = current;
                    }
                }
                Err(Failure::new("revision_conflict", "Report was modified elsewhere")
                    .reported(last))
            }
            _ => {
                cas_update(&state, &report_id, |r| {
                    r["pending_update"] = Value::Null;
                })?;
                let jobs = list_jobs(&state)?;
                let report = load_report_any(&state, &report_id)?;
                Ok(Json(json!({"ok":true,"report":decorated(&jobs, report)})))
            }
        }
    })();
    match result {
        Ok(body) => body.into_response(),
        Err(error) => error.respond(),
    }
}

pub(super) async fn candidates(
    State(state): State<Arc<AppState>>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let mode = query.get("mode").map(String::as_str).unwrap_or("");
    if !matches!(mode, "ctf" | "pentest") {
        return Failure::new("invalid_mode", "mode must be \"ctf\" or \"pentest\"")
            .response(StatusCode::BAD_REQUEST);
    }
    if let Err(unavailable) = crate::business::require(&state, module_for(mode)) {
        return unavailable.into_response();
    }
    let result: Result<Json<Value>, Failure> = (|| {
        let jobs = list_jobs(&state)?;
        let reports: Vec<Value> = state.store.list(REPORTS).map_err(storage)?;
        let mut candidates = Vec::new();
        for run in state.store.list("runs").map_err(storage)? {
            if run["mode"].as_str().unwrap_or("ctf") != mode
                || run["src_task_id"].as_str().is_some_and(|s| !s.is_empty())
                || run["bound_to_src"] == true
            {
                continue;
            }
            let run_id = run["run_id"].as_str().unwrap_or_default().to_owned();
            if run_id.is_empty() {
                continue;
            }
            let writeup_jobs: Vec<&Value> = jobs
                .iter()
                .filter(|r| r["run_id"].as_str() == Some(run_id.as_str()))
                .filter(|r| r["kind"].as_str() == Some("writeup"))
                .collect();
            let active_job = writeup_jobs
                .iter()
                .find(|r| followup::active(**r))
                .map(|r| {
                    json!({"id":r["id"],"state":r["state"],"created_at":r["created_at"]})
                });
            let latest_writeup = followup::latest_writeup_meta(&state, &run_id)?;
            let completed_at = writeup_jobs
                .iter()
                .filter(|r| r["state"].as_str() == Some("completed"))
                .filter_map(|r| r["updated_at"].as_f64())
                .fold(f64::NEG_INFINITY, f64::max);
            let last_failed_job = if active_job.is_none() {
                writeup_jobs
                    .iter()
                    .filter(|r| r["state"].as_str() == Some("failed"))
                    .filter(|r| r["updated_at"].as_f64().unwrap_or(0.0) > completed_at)
                    .max_by(|a, b| {
                        a["updated_at"]
                            .as_f64()
                            .partial_cmp(&b["updated_at"].as_f64())
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|r| {
                        json!({"id":r["id"],"updated_at":r["updated_at"],"error":r["error"]})
                    })
            } else {
                None
            };
            let report_ids: Vec<&str> = reports
                .iter()
                .filter(|row| row["run_id"].as_str() == Some(run_id.as_str()))
                .filter_map(|row| row["id"].as_str())
                .collect();
            let unavailable_reason = if run["running"] == true {
                Some("running")
            } else if run["status"].as_str() == Some("draft") {
                Some("draft")
            } else {
                None
            };
            candidates.push(json!({
                "run_id": run_id,
                "name": run["name"],
                "category": run["category"],
                "status": run["status"],
                "solved": run["solved"],
                "finished": run["finished"],
                "running": run["running"],
                "archived": run["archived"],
                "folder_id": run["folder_id"],
                "latest_writeup": latest_writeup,
                "active_job": active_job,
                "last_failed_job": last_failed_job,
                "report_ids": report_ids,
                "unavailable_reason": unavailable_reason,
            }));
        }
        Ok(Json(json!({"ok":true,"candidates":candidates})))
    })();
    match result {
        Ok(body) => body.into_response(),
        Err(error) => error.respond(),
    }
}

fn reports_of_run(state: &AppState, run_id: &str) -> Result<Vec<Value>, Failure> {
    Ok(state
        .store
        .list(REPORTS)
        .map_err(storage)?
        .into_iter()
        .filter(|row| row["run_id"].as_str() == Some(run_id))
        .collect())
}

fn run_error(code: &str, message: impl Into<String>) -> (String, String) {
    (format!("run.report.{code}"), message.into())
}

pub(super) async fn batch(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let action = body["action"].as_str().unwrap_or("");
    if !matches!(action, "generate" | "import_writeups") {
        return Failure::new(
            "invalid_input",
            "action must be \"generate\" or \"import_writeups\"",
        )
        .response(StatusCode::BAD_REQUEST);
    }
    let run_ids: Vec<String> = body["run_ids"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let result: Result<Json<Value>, Failure> = async {
        let folder_id = check_folder_ref(
            &state,
            body.get("folder_id").unwrap_or(&Value::Null),
            "ctf",
        )?;
        let template_id = body.get("template_id").cloned().unwrap_or(Value::Null);
        let custom_prompt = body.get("custom_prompt").cloned().unwrap_or(Value::Null);
        let custom_template = body.get("custom_template").cloned().unwrap_or(Value::Null);
        let mut results = Vec::new();
        for run_id in &run_ids {
            results.push(match action {
                "generate" => {
                    batch_generate(
                        &state,
                        run_id,
                        &folder_id,
                        &template_id,
                        &custom_prompt,
                        &custom_template,
                    )
                    .await
                }
                _ => batch_import(&state, run_id, &folder_id),
            });
        }
        Ok(Json(json!({"ok":true,"results":results})))
    }
    .await;
    match result {
        Ok(body) => body.into_response(),
        Err(error) => error.respond(),
    }
}

fn item_error(run_id: &str, code: String, message: String) -> Value {
    json!({"run_id":run_id,"ok":false,"error":{"code":code,"message":message}})
}

async fn batch_generate(
    state: &Arc<AppState>,
    run_id: &str,
    folder_id: &Value,
    template_id: &Value,
    custom_prompt: &Value,
    custom_template: &Value,
) -> Value {
    let run = match run_doc(state, run_id) {
        Ok(Some(run)) => run,
        Ok(None) => {
            return item_error(run_id, run_error("run_not_found", "Run not found").0, "Run not found".into())
        }
        Err(error) => return item_error(run_id, error.code, error.message),
    };
    if run["mode"].as_str().unwrap_or("ctf") != "ctf" {
        let (code, message) = run_error("not_supported", "AI writeup generation requires a CTF run");
        return item_error(run_id, code, message);
    }
    let reports = match reports_of_run(state, run_id) {
        Ok(reports) => reports,
        Err(error) => return item_error(run_id, error.code, error.message),
    };
    let newest = |rows: Vec<&Value>| -> Option<Value> {
        rows.into_iter()
            .max_by(|a, b| {
                a["updated_at"]
                    .as_f64()
                    .partial_cmp(&b["updated_at"].as_f64())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .cloned()
    };
    let target = newest(
        reports
            .iter()
            .filter(|r| r["source"]["kind"].as_str() == Some("ai_writeup"))
            .collect(),
    )
    .or_else(|| {
        newest(
            reports
                .iter()
                .filter(|r| r["markdown"].as_str().unwrap_or("").is_empty())
                .collect(),
        )
    });
    let (report, blank_created) = match target {
        Some(report) => (report, false),
        None => {
            let title = run["name"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(run_id)
                .to_owned();
            let now = now_secs();
            let id = format!("rpt_{}", uuid::Uuid::new_v4().simple());
            let report = json!({
                "id": id,
                "run_id": run_id,
                "mode": "ctf",
                "title": title,
                "markdown": "",
                "status": "draft",
                "tags": [],
                "source": {"kind":"ai_writeup","ref":Value::Null},
                "folder_id": folder_id,
                "order": now,
                "import_source": Value::Null,
                "ai_sha256": Value::Null,
                "generation": Value::Null,
                "pending_update": Value::Null,
                "created_at": now,
                "updated_at": now,
                "revision": 1,
            });
            if let Err(error) = state.store.put(REPORTS, &id, &report).map_err(storage) {
                return item_error(run_id, error.code, error.message);
            }
            (report, true)
        }
    };
    let report_id = report["id"].as_str().unwrap_or_default().to_owned();
    if report["generation"]["state"].as_str() == Some("active") {
        return json!({"run_id":run_id,"ok":true,"report_id":report_id,"job_id":report["generation"]["job_id"]});
    }
    let body = json!({
        "kind": "writeup",
        "request_id": uuid::Uuid::new_v4().simple().to_string(),
        "report_id": report_id,
        "template_id": template_id,
        "custom_prompt": custom_prompt,
        "custom_template": custom_template,
    });
    match followup::admit_report(state, run_id.to_owned(), body).await {
        Ok(job) => json!({"run_id":run_id,"ok":true,"report_id":report_id,"job_id":job["id"]}),
        Err(error) => {
            if blank_created {
                let _ = state.store.delete(REPORTS, &report_id);
            }
            item_error(run_id, error.code, error.message)
        }
    }
}

fn batch_import(state: &Arc<AppState>, run_id: &str, folder_id: &Value) -> Value {
    let run = match run_doc(state, run_id) {
        Ok(Some(run)) => run,
        Ok(None) => {
            let (code, message) = run_error("run_not_found", "Run not found");
            return item_error(run_id, code, message);
        }
        Err(error) => return item_error(run_id, error.code, error.message),
    };
    if run["mode"].as_str().unwrap_or("ctf") != "ctf" {
        let (code, message) = run_error("not_supported", "Writeup import requires a CTF run");
        return item_error(run_id, code, message);
    }
    match reports_of_run(state, run_id) {
        Ok(reports) if !reports.is_empty() => {
            return json!({"run_id":run_id,"ok":true,"skipped":"has_report"});
        }
        Ok(_) => {}
        Err(error) => return item_error(run_id, error.code, error.message),
    }
    let artifact = match followup::artifact_markdown(state, run_id) {
        Ok(artifact) => artifact,
        Err(error) if error.code.ends_with("writeup_not_found") => {
            return json!({"run_id":run_id,"ok":true,"skipped":"no_writeup"});
        }
        Err(error) => return item_error(run_id, error.code, error.message),
    };
    let markdown = artifact["markdown"].as_str().unwrap_or("").to_owned();
    let title = run["name"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(run_id)
        .to_owned();
    let now = now_secs();
    let id = format!("rpt_{}", uuid::Uuid::new_v4().simple());
    let report = json!({
        "id": id,
        "run_id": run_id,
        "mode": "ctf",
        "title": title,
        "markdown": markdown,
        "status": "draft",
        "tags": [],
        "source": {"kind":"ai_writeup","ref":artifact["job_id"]},
        "folder_id": folder_id,
        "order": now,
        "import_source": Value::Null,
        "ai_sha256": sha256_hex(&markdown),
        "generation": Value::Null,
        "pending_update": Value::Null,
        "created_at": now,
        "updated_at": now,
        "revision": 1,
    });
    match state.store.put(REPORTS, &id, &report).map_err(storage) {
        Ok(()) => json!({"run_id":run_id,"ok":true,"report_id":id}),
        Err(error) => item_error(run_id, error.code, error.message),
    }
}

fn folders(state: &AppState) -> Result<Vec<Value>, Failure> {
    state.store.list(FOLDERS).map_err(storage)
}

fn folder_depth(rows: &[Value], folder: &Value) -> u64 {
    let mut depth = 0_u64;
    let mut current = Some(folder.clone());
    let mut seen = HashSet::new();
    while let Some(node) = current {
        if !seen.insert(node["id"].as_str().unwrap_or_default().to_owned()) || depth > 64 {
            break;
        }
        depth += 1;
        current = node["parent_id"]
            .as_str()
            .and_then(|parent| rows.iter().find(|row| row["id"].as_str() == Some(parent)))
            .cloned();
    }
    depth
}

fn folder_height(rows: &[Value], folder_id: &str) -> u64 {
    let children: Vec<&Value> = rows
        .iter()
        .filter(|row| row["parent_id"].as_str() == Some(folder_id))
        .collect();
    1 + children
        .iter()
        .map(|child| folder_height(rows, child["id"].as_str().unwrap_or_default()))
        .max()
        .unwrap_or(0)
}

fn folder_descendants(rows: &[Value], folder_id: &str) -> HashSet<String> {
    let mut found = HashSet::new();
    let mut frontier = vec![folder_id.to_owned()];
    while let Some(id) = frontier.pop() {
        for row in rows
            .iter()
            .filter(|row| row["parent_id"].as_str() == Some(id.as_str()))
        {
            if let Some(child) = row["id"].as_str() {
                if found.insert(child.to_owned()) {
                    frontier.push(child.to_owned());
                }
            }
        }
    }
    found
}

fn validate_folder_parent(
    rows: &[Value],
    folder: Option<&Value>,
    parent_id: &Value,
    mode: &str,
) -> Result<Value, Failure> {
    if parent_id.is_null() {
        return Ok(Value::Null);
    }
    let Some(id) = parent_id.as_str() else {
        return Err(Failure::new("invalid_input", "parent_id must be a string or null"));
    };
    let Some(parent) = rows.iter().find(|row| row["id"].as_str() == Some(id)) else {
        return Err(Failure::new("folder_not_found", "Folder not found"));
    };
    if parent["mode"].as_str() != Some(mode) {
        return Err(Failure::new("folder_not_found", "Folder not found"));
    }
    let height = folder
        .map(|folder| folder_height(rows, folder["id"].as_str().unwrap_or_default()))
        .unwrap_or(1);
    if let Some(folder) = folder {
        let self_id = folder["id"].as_str().unwrap_or_default();
        if id == self_id || folder_descendants(rows, self_id).contains(id) {
            return Err(Failure::new(
                "folder_cycle",
                "Folder cannot be moved into itself or a descendant",
            ));
        }
    }
    if folder_depth(rows, parent) + height > MAX_FOLDER_DEPTH {
        return Err(Failure::new(
            "folder_depth_exceeded",
            "Folder depth would exceed 5",
        ));
    }
    Ok(json!(id))
}

fn validate_folder_name(body: &Value) -> Result<String, Failure> {
    let name = body["name"].as_str().unwrap_or("").trim().to_owned();
    if name.is_empty() || name.chars().count() > 80 {
        return Err(Failure::new(
            "invalid_input",
            "name must be 1..=80 characters",
        ));
    }
    Ok(name)
}

pub(super) async fn create_folder(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Response {
    let result: Result<(StatusCode, Json<Value>), Failure> = (|| {
        let mode = body["mode"].as_str().unwrap_or("");
        if !matches!(mode, "ctf" | "pentest") {
            return Err(Failure::new(
                "invalid_mode",
                "mode must be \"ctf\" or \"pentest\"",
            ));
        }
        if let Err(unavailable) = crate::business::require(&state, module_for(mode)) {
            return Err(Failure::new("invalid_input", unavailable.message));
        }
        let name = validate_folder_name(&body)?;
        let rows = folders(&state)?;
        let parent_id = validate_folder_parent(
            &rows,
            None,
            body.get("parent_id").unwrap_or(&Value::Null),
            mode,
        )?;
        let now = now_secs();
        let folder = json!({
            "id": format!("rfd_{}", uuid::Uuid::new_v4().simple()),
            "mode": mode,
            "name": name,
            "parent_id": parent_id,
            "order": body["order"].as_f64().unwrap_or(now),
            "revision": 1,
            "created_at": now,
            "updated_at": now,
        });
        let id = folder["id"].as_str().unwrap_or_default().to_owned();
        state.store.put(FOLDERS, &id, &folder).map_err(storage)?;
        Ok((StatusCode::CREATED, Json(json!({"ok":true,"folder":folder}))))
    })();
    respond_created(result)
}

pub(super) async fn update_folder(
    State(state): State<Arc<AppState>>,
    Path(folder_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let result: Result<Json<Value>, Failure> = (|| {
        let mut last = Value::Null;
        for _ in 0..3 {
            let before = folder_row(&state, &folder_id)?
                .ok_or_else(|| Failure::new("folder_not_found", "Folder not found"))?;
            if let Err(unavailable) = crate::business::require(
                &state,
                module_for(before["mode"].as_str().unwrap_or("ctf")),
            ) {
                return Err(Failure::new("invalid_input", unavailable.message));
            }
            if let Some(expected) = body["expected_revision"].as_u64() {
                if before["revision"].as_u64() != Some(expected) {
                    return Err(Failure::new(
                        "revision_conflict",
                        "Folder was modified elsewhere",
                    ));
                }
            }
            let rows = folders(&state)?;
            let mut after = before.clone();
            if body.get("name").is_some() {
                after["name"] = json!(validate_folder_name(&body)?);
            }
            if let Some(parent_id) = body.get("parent_id") {
                after["parent_id"] = validate_folder_parent(
                    &rows,
                    Some(&before),
                    parent_id,
                    before["mode"].as_str().unwrap_or("ctf"),
                )?;
            }
            if let Some(order) = body.get("order") {
                let Some(order) = order.as_f64() else {
                    return Err(Failure::new("invalid_input", "order must be a number"));
                };
                after["order"] = json!(order);
            }
            after["revision"] = json!(before["revision"].as_u64().unwrap_or(0) + 1);
            after["updated_at"] = json!(now_secs());
            if state
                .store
                .put_many_if(
                    &[(FOLDERS, &folder_id, Some(&before))],
                    &[(FOLDERS, &folder_id, &after)],
                )
                .map_err(storage)?
            {
                return Ok(Json(json!({"ok":true,"folder":after})));
            }
            if let Some(current) = folder_row(&state, &folder_id)? {
                last = current;
            }
        }
        let _ = last;
        Err(Failure::new(
            "revision_conflict",
            "Folder was modified elsewhere",
        ))
    })();
    match result {
        Ok(body) => body.into_response(),
        Err(error) => error.respond(),
    }
}

fn delete_folder_doc(
    state: &AppState,
    folder_id: &str,
    strategy: &str,
) -> Result<Json<Value>, Failure> {
        let folder = folder_row(state, folder_id)?
            .ok_or_else(|| Failure::new("folder_not_found", "Folder not found"))?;
        if let Err(unavailable) = crate::business::require(
            state,
            module_for(folder["mode"].as_str().unwrap_or("ctf")),
        ) {
            return Err(Failure::new("invalid_input", unavailable.message));
        }
        if !matches!(strategy, "move_up" | "delete_all") {
            return Err(Failure::new(
                "invalid_input",
                "strategy must be \"move_up\" or \"delete_all\"",
            ));
        }
        let rows = folders(state)?;
        let parent_id = folder["parent_id"].clone();
        match strategy {
            "move_up" => {
                for child in rows
                    .iter()
                    .filter(|row| row["parent_id"].as_str() == Some(folder_id))
                {
                    let mut next = (*child).clone();
                    next["parent_id"] = parent_id.clone();
                    next["revision"] = json!(next["revision"].as_u64().unwrap_or(0) + 1);
                    next["updated_at"] = json!(now_secs());
                    let id = child["id"].as_str().unwrap_or_default();
                    state.store.put(FOLDERS, id, &next).map_err(storage)?;
                }
                for report in state
                    .store
                    .list(REPORTS)
                    .map_err(storage)?
                    .iter()
                    .filter(|row| row["folder_id"].as_str() == Some(folder_id))
                {
                    let mut next = (*report).clone();
                    next["folder_id"] = parent_id.clone();
                    let id = report["id"].as_str().unwrap_or_default();
                    state.store.put(REPORTS, id, &next).map_err(storage)?;
                }
                state.store.delete(FOLDERS, folder_id).map_err(storage)?;
                Ok(Json(json!({"ok":true,"deleted_reports":0,"deleted_folders":1})))
            }
            _ => {
                let mut subtree = folder_descendants(&rows, folder_id);
                subtree.insert(folder_id.to_owned());
                let mut deleted_reports = 0_usize;
                for report in state
                    .store
                    .list(REPORTS)
                    .map_err(storage)?
                    .iter()
                    .filter(|row| {
                        row["folder_id"]
                            .as_str()
                            .is_some_and(|id| subtree.contains(id))
                    })
                {
                    let id = report["id"].as_str().unwrap_or_default();
                    if state.store.delete(REPORTS, id).map_err(storage)? {
                        deleted_reports += 1;
                    }
                }
                let mut deleted_folders = 0_usize;
                for id in &subtree {
                    if state.store.delete(FOLDERS, id).map_err(storage)? {
                        deleted_folders += 1;
                    }
                }
                Ok(Json(json!({"ok":true,"deleted_reports":deleted_reports,"deleted_folders":deleted_folders})))
            }
        }
}

pub(super) async fn delete_folder(
    State(state): State<Arc<AppState>>,
    Path(folder_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let strategy = query.get("strategy").map(String::as_str).unwrap_or("move_up");
    match delete_folder_doc(&state, &folder_id, strategy) {
        Ok(body) => body.into_response(),
        Err(error) => error.respond(),
    }
}

pub(super) async fn move_folder_up(
    State(state): State<Arc<AppState>>,
    Path(folder_id): Path<String>,
) -> Response {
    match delete_folder_doc(&state, &folder_id, "move_up") {
        Ok(body) => body.into_response(),
        Err(error) => error.respond(),
    }
}

pub(super) async fn delete_folder_all(
    State(state): State<Arc<AppState>>,
    Path(folder_id): Path<String>,
) -> Response {
    match delete_folder_doc(&state, &folder_id, "delete_all") {
        Ok(body) => body.into_response(),
        Err(error) => error.respond(),
    }
}
