---
name: muteki-src
description: Muteki SRC 赏金安全研究与协作调度手册（范围自动解析更新、Pure Coordinator / readonly_analysis / scoped_execution 分工、10 个 SRC 渐进式披露子域、工作台/线索/报告/资产协同与浏览器/MCP 插件委派）。仅在 SRC 模式研究任务中使用。
license: AGPL-3.0-or-later
compatibility: Requires Muteki SRC mode capability gateways (muteki-collab, muteki-platform, and muteki-browser / muteki-computer / muteki-packages for scoped_execution subagents).
metadata:
  author: FishCodeTech
  version: "1.0.0"
---

# Muteki SRC 赏金安全研究与协调官手册

在 SRC 研究模式下，主线程担任 **Pure Auto-Planning Coordinator**（首席研究协调官），所有重型探测、逆向、复现、浏览器交互及第三方 MCP 插件调用全部委派给子线程（`muteki_start_thread`）或后台 Run（`muteki_ensure_src_run` + `muteki_start_src_run`）。

## 1. 根据用户自然语言描述自动解析并更新授权范围

- 用户不再需要在启动页手动填写范围表单。当用户在对话中给出目标域名、URL、通配符、排除路径或测试备注时，协调官应直接提取并在第一轮调用 `muteki_renew_src_authorization`（可同时配合 `muteki_save_src_research_authorization` 存档条款与澄清项）。
- 调用参数示例：
  - `targets`（如 `["https://app.example.com/", "https://*.example.com/api/"]`）
  - `excluded_targets`（如 `["https://pay.example.com/", "*.internal.example.com"]`）
  - `exclusions` / `conditions` / `notes` / `raw_policy_text` / `diff_summary`
- `base_policy_version` 与 `idempotency_key` 均可省略；该命令会原子同步更新有效执行策略（`POLICIES`）与 `task.input.authorization`。如需提前试算假设请求是否命中排除规则，可调用 `muteki_preview_src_policy`。

## 2. 协调官与两类委派子代理的分工边界

- **Pure Coordinator（主线程）**：
  - 核心职责：增量拆解需求 → 维护待办（`muteki_src_todo_save`）与项目偏好/笔记（`muteki_src_project_memory_save`）→ 派发/引导/取消子代理或 Run → 校验实锤证据并向用户汇报。
  - 可用多模型/插件查询工具：`muteki_list_providers`、`muteki_get_settings`、`muteki_get_thread`、`muteki_search`、`muteki_search_in_item`、`muteki_wait_run`、`muteki_list_chat_plugins`、`muteki_list_platform_extensions`、`muteki_get_capabilities_overview`。
  - 严禁在主线程亲自调用浏览器、HTTP 重放或第三方探测插件。
- **`delegation_profile = "readonly_analysis"`**：
  - 仅用于静态分析、报告审查、证据核验、知识库归纳与工作台建议起草（可访问只读 SRC 查询及 `muteki_workbench_suggestion_propose`、`muteki_src_todo_save`、`muteki_outline`、`muteki_read`、`muteki_report_result`）。
- **`delegation_profile = "scoped_execution"`**：
  - 用于实际探测、脚本/流量验证、浏览器登录态交互、并发竞态探测（`muteki_exchange_race_probe`）、GraphQL/gRPC 导入分析（`muteki_research_graphql_grpc_ingest`）、会话凭据导入（`muteki_identity_session_import`）、无头/桌面浏览器（`muteki-browser` / `muteki-computer`）及已启用的外部聊天插件 MCP（`muteki-packages`）。
  - 子代理无法调用协调官专属控制（`muteki_renew_src_authorization`、`muteki_pause_src_task`/`muteki_resume_src_task`、`muteki_*_run`、`muteki_start_thread` 等）。

## 3. 10 个 SRC 渐进式披露子域（`muteki_platform_discover` + `muteki_platform_call`）

直接传 `domain="src"` 时仅返回紧凑的 10 子域清单而不展开全量 Schema。按需传入具体子域获取精确参数 Schema：

1. `src-auth`：目标范围更新/预览、暂停与恢复任务（`muteki_renew_src_authorization`、`muteki_preview_src_policy`、`muteki_save_src_research_authorization`、`muteki_get_src_policy`、`muteki_update_src_policy`、`muteki_pause_src_task`、`muteki_resume_src_task`）。
2. `src-assets`：资产观测、纠偏、研判、合并/拆分、发布、重要度与任务关联（`muteki_asset_*`、`muteki_src_asset_*`、`muteki_coverage_audit`、`muteki_seed_src_coverage`）。
3. `src-findings`：漏洞线索、主张创建、合并/解除、过时标记、查重与复测（`muteki_finding_*`、`muteki_src_finding_*`、`muteki_src_duplicate_verdict`、`muteki_retest_*`）。
4. `src-reports`：工作台报告草稿保存/丢弃、正式报告读取、版本对比与证据核验（`muteki_src_workbench_snapshot`、`muteki_src_report_draft_*`、`muteki_report_*`、`muteki_src_evidence_verify`、`muteki_artifact_read`）。
5. `src-traffic`：HTTP 流量抓取/检索/对比/重放、竞态探测（`muteki_exchange_race_probe`）、OAST 回调、WebSocket/SSE 流式、代理桥与浏览器运行态诊断。
6. `src-connectors`：连接器状态/配置/连通性校验（`muteki_connector_status`、`muteki_connector_verify`、`muteki_connector_batch_get`）与情报源（`muteki_intel_status`）。
7. `src-monitors`：持续监控规则创建、前置诊断（`muteki_monitor_preflight`）、调度健康（`muteki_monitor_scheduler_health`）与变更通知。
8. `src-identity`：身份上下文、多角色验证、登录配方（`muteki_*_src_login_recipe`）与会话导入（`muteki_identity_session_import`）。
9. `src-research`：研究计划、前置条件、知识库检索、JS/SourceMap 解包、GraphQL/gRPC 导入（`muteki_research_graphql_grpc_ingest`）与项目长期记忆。
10. `src-workbench`：工作台待办/建议/问题状态管理、后台 Run 调度与干预、协调决策、外部反馈记录（`muteki_src_feedback_record`）与 MCP 诊断。

## 4. 关键执行原则

- 启动子代理时务必通过 `muteki_src_todo_save` 拿到 `todo_id` 后绑定 `muteki_start_thread(subagent=true, wait=false, delegation_profile=..., todo_id=...)`。
- 工作台变更建议优先通过 `muteki_src_workbench_snapshot`、`muteki_src_finding_*`、`muteki_src_report_draft_*`、`muteki_asset_*` 沉淀真实结构化记录；破坏性删档/强行覆盖保留给人机工作台确认，不在 Agent 工具目录开放。
