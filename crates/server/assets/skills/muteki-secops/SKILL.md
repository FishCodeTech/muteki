---
name: muteki-secops
description: 调度与监控 Muteki 安全平台业务（CTF 题目求解、授权渗透测试、Run 状态与事件流、比赛平台同步、动态靶机租约与 Worker 席位）。仅在用户询问或操作上述平台业务时按需读取。
license: AGPL-3.0-or-later
compatibility: Requires the Muteki progressive-disclosure platform MCP server (muteki-platform).
metadata:
  author: FishCodeTech
  version: "2.0.0"
---

# Muteki 安全平台操作手册（渐进式披露）

Muteki 安全平台采用**渐进式披露（Progressive Disclosure）**架构，避免在普通编码对话中预加载数十个重型工具 Schema。请始终通过 `muteki-platform` MCP 服务提供的两个入口工具操作平台状态，禁止直接读取 SQLite 数据库、会话目录或本地缓存文件来猜测业务状态。

## 标准调用顺序

1. **按需发现工具 Schema**：
   调用 `muteki_platform_discover`，传入目标领域 `domain`（`runs` / `pentest` / `competitions` / `workers`）或 `query` 关键词，获取精确的工具名、参数 JSON Schema 与返回契约。
2. **执行查询或命令**：
   调用 `muteki_platform_call`，传入 `{"tool": "<工具名>", "arguments": {...}}`。
3. **核验命令回执**：
   所有写操作返回异步 `CommandReceipt`（位于 `structuredContent.receipt`）。先检查 `receipt.state`（如 `completed`），必要时通过 `muteki_get_command_receipt` 回读，确认成功后再向用户汇报；重试同一逻辑写入必须复用 `idempotency_key`。
4. **有界等待运行进展**：
   长耗时求解任务使用 `muteki_wait_run`（有界超时）跟进，需要事件级证据时再调用 `muteki_read_run_events` 或 `muteki_read_shared_graph`。

## 领域工作流速查

- **历史题目与运行记录（`domain="runs"`）**：
  先用 `muteki_list_tasks`、`muteki_get_task` 或 `muteki_list_runs` 取得真实 `task_id` / `run_id`，严禁凭目录名猜测 ID。
- **新建单题 CTF 求解（`domain="runs"`）**：
  `muteki_dispatch_challenge`（或 `muteki_create_task`）→ `muteki_create_run`（`binding_key` 传 `task_id`）→ `muteki_start_swarm` → `muteki_wait_run`；续跑未完成任务使用 `muteki_resolve_run`。
- **新建授权渗透测试（`domain="pentest"`）**：
  `muteki_dispatch_pentest` → `muteki_create_run(task_kind="pentest.target")` → `muteki_start_swarm(challenge.mode="pentest")`。必须严格保留用户明确授权的 `target`、`scope` 与 `goal`，严禁自行扩大测试范围。
- **比赛同步与动态靶机（`domain="competitions"`）**：
  先读取比赛（`muteki_list_competitions`）、平台连接（`muteki_list_connections`）、题目投影（`muteki_list_competition_challenges`）、提交记录与实例租约，再执行选定、入队（`muteki_queue_challenge`）、启停自动调度器或申请靶机租约（`muteki_ensure_challenge_instance`）。
- **Worker 容器、网络与凭据席位（`domain="workers"` / `domain="models"`）**：
  修改前先读取 `muteki_get_worker_settings`、`muteki_list_worker_profiles`、`muteki_get_worker_models` 与 `muteki_list_credential_accounts`（或 `muteki_list_credentials`）。刷新模型列表调用 `muteki_refresh_credential_models`；导入宿主机已有登录使用 `muteki_import_host_account`；导入本地凭据或 OpenVPN 文件仅传本地文件路径，严禁在对话或工具参数中传递明文密钥内容。工作台全域配置、自定义模型端点与系统管理见 `$muteki-workbench`（`/workbench`）。
