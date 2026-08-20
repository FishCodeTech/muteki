# AGENTS.md — Project Muteki（無敵）

Muteki 是一个多模型 CTF / Pentest Agent 调度系统。项目通过完整的 CLI Agent 执行任务，
由 Coordinator 负责规划、调度、审查、运行控制和结果汇总。架构与行为以当前代码为准；
README、ROADMAP 和历史会话记录只提供背景信息。

## 当前架构

- **Worker 执行器**：`muteki/solver/cli_driver.py` 和
  `muteki/solver/cli_solver.py`。支持 Claude、Codex、Cursor、Pi、OMP、Kimi、
  Grok、OpenCode、DeepSeek Harness 九类引擎。本地与容器 Worker 均支持这九类引擎。
- **Worker Profile**：`muteki/solver/worker_profiles.py`。Profile 是实际调度单位，
  可以配置引擎、模型、凭据账户、运行环境、优先级和角色。普通角色包括 `race`、
  `bootstrap`、`explore`、`respond`，审查角色为 `review`。
- **Coordinator**：`muteki/swarm/swarm.py` 与 `muteki/swarm/coordinator_*.py`。
  Coordinator 负责 race-scout、Intent 调度、Worker 生命周期、Review、Operator 指令、
  Flag 汇总和运行结束条件。
- **共享图**：`muteki/swarm/shared_graph.py` 与 `muteki/swarm/graph_*.py`。
  SQLite 中保存求解事件和状态投影。新增事实、路线、分支、Review 提案、Operator 指令和
  Flag 结果通过事件记录；Intent claim、租约、摘要以及部分状态投影允许原子更新。
- **Review**：Review Worker 写入 `review_proposal`，Coordinator 通过
  `review_proposal_decision` 接受、拒绝或应用提案。Review Worker 不直接取得全局调度权限。
- **Operator 控制**：Operator 指令持久化为 `operator_directive`，具有最高调度优先级。
  指令属于调度输入，执行结果仍需由 Worker 产生事实、产物或 Flag 证据。
- **前端**：`apps/web/` 提供 FastAPI、SSE 和 Next.js UI，支持查看事件、发送指令、暂停、
  恢复以及管理 Worker；`apps/tui/` 提供 Textual 界面。
- **运行控制**：`muteki/control/` 管理 Worker 控制命令、运行状态和上下文交付。

## 执行模式

- Web 入口默认使用 Coordinator：`coordinator=True`、`cli_race=False`。
  默认构造类是 `muteki.swarm.swarm.Swarm`，循环在 `coordinator_loop.py`。
  请求体省略 `swarm_class` 时必须落到这个类。CTF 和渗透共用这一条路径。
- Coordinator 可以先运行一轮 race-scout，再使用共享图中的 Intent 调度后续 Worker。
- TUI 的 `--swarm` 路径显式使用直接 race。
- 直接构造 `Swarm` 时，应明确设置所需模式，不依赖历史默认行为。
- 单 Flag 任务在第一个有效 Flag 通过校验后完成。多 Flag 任务在收集到
  `Challenge.expected_flags` 个不同 Flag 后完成。

## 结果正确性

- Flag 只有在真实命令输出、stderr 或真实产物中出现时才可以接受。占位符、模板内容、
  模型自行声明的结果以及经过转述但没有执行来源的内容不能作为有效 Flag。
- Flag 接受逻辑位于 `muteki/solver/gate.py`，相关来源检查位于
  `muteki/solver/cli_solver.py`。修改求解流程时保持该校验入口独立。
- Operator 指令、知识库结果、Review 文本和普通聊天内容都不能直接成为 Flag 来源。
- 写入 verified fact 时提供可核查的 witness、命令输出或产物路径；尚未核查的信息写为
  candidate，并保留来源。
- challenged fact 暂不作为已确认依据。suppressed route 只有出现新证据后再 reopen。
  不同 branch 对应独立假设，应分别记录验证结果。

## 评测口径

- 正式求解率评测只向 Worker 提供目标、题目描述和选手可见文件。代码审计题可以把题目源码
  作为输入，但不提供 `solution.*`、参考解或官方 Writeup。
- 离线能力评测应关闭 WebSearch、WebFetch 和外部知识库。普通本地开发、调试和真实比赛
  可以使用联网模式。
- 如果显式覆盖离线限制，评测记录应注明联网条件，避免与离线结果合并统计。
- 求解率结论应附带真实运行记录，并能定位到 Worker 实际输出中的 Flag。

## 开发方式

1. 开始前查看 `git status --short`，确认当前分支、未提交文件和其他任务留下的改动。
2. 使用 `rg`、调用关系和当前实现定位功能。历史文档与代码不一致时，以代码为准并更新说明。
3. 功能实现任务先完成可运行的操作路径。用户确认功能后，再添加防护、回归测试和兼容性处理；
   用户明确要求测试，或正在修复已经复现的问题时，可以同时补充对应测试。
4. 验证范围由任务决定。文档任务检查内容和 diff；局部功能运行相关检查；发布或大范围改动再运行
   完整测试集。
5. 保留工作区中与当前任务无关的改动，不覆盖其他任务生成的文件。
6. 修改共享图时同时检查事件写入、状态投影、Coordinator 消费逻辑和前端事件解析。
7. 修改 Worker Profile、引擎或运行环境时同时检查 Web 设置、健康检查、容器支持和调度筛选。
8. F01–F11、ChainForce、PEX、DualRush、ReapClose、RoleSwarm、HypoLedger、Phased
   都是实验 / 评测臂。未经过正式讨论并写入产品默认之前，不得把它们接到 Web / TUI /
   渗透默认启动，不得改 `_resolve_swarm_class` 的空 spec 返回值，不得在
   `coordinator_loop` 或默认 `CliSolver` 路径里无条件调用这些包。约束见下文
   「实验框架」。

## 主要路径

| 内容 | 路径 |
|------|------|
| CLI 引擎驱动 | `muteki/solver/cli_driver.py` |
| Worker 执行循环 | `muteki/solver/cli_solver.py` |
| Worker Profile | `muteki/solver/worker_profiles.py` |
| Flag 校验 | `muteki/solver/gate.py` |
| Coordinator 主体 | `muteki/swarm/swarm.py` |
| Coordinator 循环与调度 | `muteki/swarm/coordinator_loop.py`、`coordinator_dispatch.py` |
| Race 与健康检查 | `muteki/swarm/coordinator_race.py` |
| Review | `muteki/swarm/coordinator_review.py` |
| Operator、Flag 与结束条件 | `muteki/swarm/coordinator_flags.py` |
| 共享图接口 | `muteki/swarm/shared_graph.py` |
| 共享图事件、事实、Intent、路线和锁 | `muteki/swarm/graph_*.py` |
| 黑板技能 | `skills/muteki-blackboard/` |
| 运行控制 | `muteki/control/` |
| Web 运行管理 | `apps/web/run_manager.py`、`apps/web/drivers.py` |
| Web 事件状态 | `apps/web/ui/lib/events.ts`、`apps/web/ui/lib/useRun.ts` |
| 配色方案引擎 | `apps/web/ui/lib/palette-engine.ts` |
| Worker 设置界面 | `apps/web/ui/components/WorkerOrchestration.tsx` |
| TUI | `apps/tui/` |
| Worker 容器 | `docker/worker/`、`docker/worker-slim/` |

## 常用命令

```bash
./run.sh tui
./run.sh tui --swarm --key <value>
./run.sh web
./run.sh web --backend-only
uv run --extra dev python -m pytest -q <相关测试路径>
```

`./run.sh web` 使用生产构建后的 Next.js 服务，默认前端端口为 `3001`，FastAPI 端口为
`8000`。端口被已有进程占用时，先确认对应进程属于哪个运行任务，再决定是否停止。

## 实验框架（不得默认接入）

`muteki/frameworks/f01_*` … `f11_*`，以及 `muteki/solver/swarm_chainforce.py`、
`swarm_pex.py`、`swarm_dualrush.py`、`swarm_reapclose.py`、`swarm_roleswarm.py`、
`swarm_hypoledger.py`、`swarm_phased.py`，都是评测和研究用的实验臂。它们不完善，
不是产品 Coordinator。

未经过正式讨论并明确写入产品默认之前：

- Web `/start` 省略 `swarm_class` 时必须构造 `muteki.swarm.swarm.Swarm`。
- 前端不得发送 `swarm_class`。
- 渗透模式只改题目产品（报告、完整性、复现、价值裁定），不换调度类。
- 评测脚本若要跑实验臂，必须在请求或命令行里写明完整
  `module.path:ClassName`，并在评测记录里注明。不得把这种指定写进产品默认。

历史原因：一次 research 提交曾把 Web 默认改成 `SwarmChainForce`
（评测基线臂）。该默认已改回 `Swarm`。

## Blackboard

Worker 通过 `skills/muteki-blackboard/blackboard.py` 访问共享图，数据库路径来自
`$MUTEKI_BLACKBOARD_DB`。常用协议包括：

- `read-directives`：读取 Operator 指令。
- `read-review`、`read-deadends`、`read-facts`：读取审查结果、失败路线和事实。
- `read-routes`、`read-branches`、`read-flags`：读取路线、假设分支和已收集 Flag。
- `list-intents`、`claim`：查看并原子领取开放 Intent。
- `claim-resource`、`release-resource`：管理端口、监听器、目标会话等独占资源。

以 `skills/muteki-blackboard/SKILL.md` 和 `blackboard.py --help` 显示的当前命令为准。
`scripts/install_blackboard_skill.sh` 用于刷新 Claude、Cursor 和 Codex 的用户级技能副本；
源码运行和容器运行还会通过各自的启动路径提供黑板脚本。

## Worker 容器说明

`docker/worker/AGENTS.md` 是完整版和 slim Worker 共用的说明来源。修改后，
`docker/worker-slim/build.sh` 会把它复制到 slim 构建上下文。不要单独维护被忽略的
`docker/worker-slim/AGENTS.md`。
