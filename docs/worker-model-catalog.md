# Worker 模型目录

配置中心的 Worker 模型分为两类：

- 手工维护：`apps/web/worker_models.manual.json`。用于保存公开模型、稳定别名和无法在未登录状态下发现的模型。
- 自动发现：在配置中心的 Worker 阵容页面点击“发现模型”。后端在当前 profile 对应的本地环境或 Worker 镜像中执行 CLI 模型目录命令，并把最近一次成功结果保存到 `sessions/_worker_model_discovery.json`。

自动发现不设置定时任务，也不会修改手工清单或已经保存的 Worker 模型。发现失败时继续使用手工清单和上一次成功结果。

## 当前发现方式

| 引擎 | 方式 | 登录要求 |
| --- | --- | --- |
| Codex | `codex debug models`；远程目录不可用时尝试 `codex debug models --bundled` | 远程目录可能需要登录；CLI 内置目录不依赖账号 |
| Cursor | `cursor-agent models` | 由 Cursor CLI 和当前环境决定，账号级结果通常需要登录或 API Key |
| Claude Code | 使用手工维护的公开模型和官方别名 | Claude Models API 需要 API Key；Claude Code 没有稳定的非交互订阅模型列表命令 |
| Pi / OMP | 暂未接入 | 使用手工清单 |

## 手工更新

编辑 `apps/web/worker_models.manual.json` 中对应引擎的数组。每项包含：

```json
{"id": "模型 ID", "label": "界面显示名称"}
```

同时更新文件顶层的 `updated_at`。服务重新启动后即可读取新清单，不需要修改 Python 或前端代码。

Claude 公共模型以 Anthropic 的 Models overview 为准；Claude Code 的 `default`、`best`、`sonnet`、`opus`、`haiku`、`sonnet[1m]`、`opus[1m]`、`opusplan` 等别名以 Claude Code model configuration 为准。公开清单只表示模型名称已经公开，账号权限仍需通过“测模型”确认。
