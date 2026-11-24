---
name: muteki-workbench
description: 控制 Muteki 工作台全量组件状态（模型凭据与自定义端点、Pi/Ollama 模型列表刷新、配额与定价、Runtime 实例、聊天插件与扩展、UI/侧栏设置、工作区与目录、对话记忆与分享、运维状态与系统更新）。用户要求在聊天模式查看或修改工作台配置、模型端点或组件细节时按需读取。
license: AGPL-3.0-or-later
compatibility: Requires the Muteki progressive-disclosure platform MCP server (muteki-platform).
metadata:
  author: FishCodeTech
  version: "1.0.0"
---

# Muteki 工作台全域控制手册（渐进式披露）

通过 `muteki-platform` 渐进式披露入口（`muteki_platform_discover` + `muteki_platform_call`）查看与控制 Muteki 各工作台组件，无需通过 `chat_computer_*` 或 `chat_browser_*` 去点击设置界面。禁止直接读写 SQLite 数据库或内部状态文件来替代工具调用。

## 标准调用顺序

1. **先发现再调用**：
   先调用 `muteki_platform_discover`（传目标 `domain` 或 `tools` 数组）读取准确参数 JSON Schema，再调用 `muteki_platform_call`。
2. **配置写入与回读**：所有工作台可编辑配置均通过对应写工具操作，沿用设置页的类型、版本和保存校验。模型账号使用 `muteki_upsert_credential_account`，HTTP 端点使用 `muteki_upsert_model_endpoint`，支持更新密钥和地址；保存后回读脱敏账号，勿将密钥复制到公开模板或输出中。
3. **遵循控制授权模型（`muteki-control`）**：
   只读检查通常只需 `observe`；更新设置或工作台配置需 `operate`（并按需叠加 `settings_write`）；删除/清理/回滚操作需 `delete`。遇到权限不足时调用 `muteki_request_grant` 向用户申请对应层/类别授权，严禁绕过。

## 领域速查

- **模型、凭据、端点、配额与定价（`domain="models"`）**
  - 先读取：`muteki_list_credentials`、`muteki_get_local_pi`、`muteki_get_usage_and_quota`、`muteki_get_pricing`。
  - **新增或更新无密钥/自定义端点（如 Pi Ollama / vLLM / LM Studio）**：优先使用 `muteki_upsert_credential_account`（例如 `account_id="account:pi-ollama"`、`worker_engine="pi"`、`connection="custom_endpoint"`、`base_url`、`model_list_url`、`provider`、`target_model`、`models`）；`muteki_upsert_model_endpoint` 可创建或更新 HTTP 端点（新无密钥端点请走 `muteki_upsert_credential_account`）。
  - **刷新模型列表**：调用 `muteki_refresh_credential_models`（可指定 `account_id`、`stale_only`、`force` 或 `base_url`/`connection`/`provider`/`engine`），再用 `muteki_test_credential_account` 或 `muteki_test_llm_endpoint` 验证连通性。
  - 其他操作：`muteki_delete_credential_account`、`muteki_delete_model_endpoint`、`muteki_update_local_pi`、`muteki_refresh_quota`、`muteki_update_pricing`。

- **Runtime 描述符与实例（`domain="runtimes"`）**
  - `muteki_list_runtime_descriptors`、`muteki_list_runtime_instances`、`muteki_manage_runtime_instance`（新增/更新/删除/检测 Runtime 实例；upsert_profile/delete_profile 管理 Worker Profile）。

- **内置能力、聊天插件与平台扩展（`domain="extensions"`）**
  - 内置能力与线程开关：`muteki_get_capabilities_overview`、`muteki_update_capability`。
  - 聊天插件包（Skills / MCP 插件包）：`muteki_list_chat_plugins`、`muteki_manage_chat_plugin`。
  - 平台扩展模块：`muteki_list_platform_extensions`、`muteki_manage_extension`（preview → install/upgrade；config 通过 enable 保存并启用）。

- **UI 设置、控制状态与会话历史导入（`domain="settings"`）**
  - UI / 侧栏 / 自动续跑 / 线程交互模式：`muteki_get_ui_settings`、`muteki_update_ui_settings`（默认局部合并，谨慎传 `replace=true`）。
  - 控制授权与待审批状态（读取快照）：`muteki_get_control_state`。
  - `muteki_get_ui_settings(section=目标)` / `muteki_update_ui_settings(target=目标, values={...})` 支持 `control`（含 orchestration、orchestration_mode、orchestration_overrides、subagent_policy 和 grants）、`control_policy`（自动归档和 source_control）、`thread_grant`（仅写入，values={thread_id,grant}；用 muteki_get_thread 回读）、`browser`、`agent_version_policy`、`competition_advisor`、`runner_listener`、`host_credential`、`access`、`system_update`。先发现工具读取字段契约，写入后回读。access 保留当前密码校验；凭据操作需 credentials。
  - `muteki_patch_settings` 接受 `{patch:{type:"orchestration",mode?:"conversation"|"src",content:{preset,auto_inject_instructions,default_role?,roles:[...]}}}`（传 `mode:"src"` 且 `content:null` 恢复跟随聊天）；roles 是完整角色列表，每项含 id、label、description、enabled、mode、provider、profile、model、effort、guidelines。
  - `muteki_control_thread` 的 `set_runtime` 可更新真实 runtime 字段，需 settings_write；`muteki_start_thread` 可显式给 access_mode，同样需 settings_write。
  - 外部 Agent 历史扫描与导入：`muteki_scan_provider_history`、`muteki_import_provider_history`（仅限 `~/.claude` 与 `~/.codex` 默认目录，需 `settings_write`）。

- **工作区、目录、对话记忆、进程追踪与分享（`domain="workspace"`）**
  - `muteki_list_workspaces`、`muteki_manage_workspace`、`muteki_manage_folder`。
  - `muteki_get_thread_memory`、`muteki_manage_thread_memory`、`muteki_get_turn_process`。
  - `muteki_list_conversation_shares`、`muteki_manage_conversation_share`（新建分享需 `settings_write`，撤销需 `organize`）。

- **运维总览、平台副作用、资源调度与系统更新（`domain="operations"`）**
  - `muteki_get_operations_overview`、`muteki_manage_platform_effect`、`muteki_maintenance`、`muteki_manage_domain_module`。
  - `muteki_get_runner_resources`、`muteki_update_runner_resources`（policy、set_priority、enroll、set_state、rotate、delete）、`muteki_get_system_status`、`muteki_check_system_update`。

- **Worker 配置、容器镜像与 Run 保留策略（`domain="workers"`）**
  - `muteki_get_worker_settings`、`muteki_update_worker_settings`、`muteki_get_worker_models`、`muteki_list_worker_profiles`、`muteki_test_llm_endpoint`、`muteki_get_worker_image`、`muteki_pull_worker_image`、`muteki_get_run_retention`、`muteki_update_run_retention`。

## 跨 MCP 协同

- 会话编排、Git/PR、终端读取与圆桌讨论：使用 `muteki-collab` MCP 工具。
- 需要实际打开或交互网页（如验证前端渲染）时，优先用用户指定的可用浏览器工具或技能。用户未指定时，推荐 `muteki-browser` MCP 工具。
- 需要操作网页之外的原生 macOS 桌面应用：先检查 `chat_computer_status`，再使用 `muteki-computer` MCP 工具。
