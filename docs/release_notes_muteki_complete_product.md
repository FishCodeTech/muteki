# Muteki Agent 原生完整产品实现说明

> 日期：2026-08-23
> 状态：M1 至 M6 和 P1 授权比赛平台验收已完成
> 分支：`codex/muteki-finalize-m1-m5-20260822`

本次收尾把 Platform Core、统一 Command API、Capability Gateway、九类 Runtime Adapter、Conversation、
标准 Swarm、Competition、Extension、Memory 和 Operations 接入同一 Web 产品启动路径。

## 主要结果

- 结构化 Runtime 由唯一 RuntimeAdapterFactory 解析并进入 Conversation、WorkerSessionSupervisor 和标准
  `muteki.swarm.swarm.Swarm`。app-server、SDK、ACP、RPC 和 Server 为优先 transport，CLI 只在明确能力
  不足时降级并显示原因。
- 全局 command 身份可以精确定位 Platform 与 Competition 领域；长操作分别保存 acceptance receipt、
  effect receipt、outbox 和领域事件，完成状态由实际效果决定。
- CompetitionServiceFactory 装配平台 Adapter、同步、Artifact、编译、RunBinding、Scheduler、实例租约、
  Submission、Gate bridge、Projection 和 Reconciler。五个服务循环由 WebPlatformStack 启动和关闭。
- 本地模拟平台完成连接、probe、同步、附件、Run、候选、Gate、审批、提交、裁定和同根目录重启恢复。
- 未验证候选、模板文本和聊天文本无法进入自动提交；人工覆盖使用独立命令、二次确认和审计事件。
- StartupRecoveryReport 按模块记录 ready、degraded、unavailable、影响和证据；核心恢复完成前 readiness
  保持关闭。Conversation 孤立 Turn、Competition outbox/租约/RunBinding/提交、enabled Extension 均恢复。
- Secret 页面支持创建、轮换、撤销与引用检查；平台凭据和浏览器 storage state 不进入响应、事件和日志。
- Extension 支持安装、确认、启用、禁用、升级、回滚、卸载、依赖恢复、故障隔离和声明式 UI；Catalog
  使用 Ed25519 签名、不可变来源、发布者撤销和管理员信任策略。
- Conversation 提供 ToolCall、Approval、UserInput、Artifact、Diff、Usage、Memory timeline 和事件流；
  Competition 提供连接向导、题目卡片、时间线、资源、实例、候选、提交、筛选、排序和有限批量操作。
- 全局任务与回执中心显示 accepted、running、waiting、failed 和 completed；Operations 页面提供 readiness、
  指标、告警、回执追踪、outbox、恢复预演、维护预演和脱敏诊断包。
- OpenAPI 与 TypeScript 类型确定性生成并进入 CI 漂移检查；API 校验错误统一返回脱敏 ErrorEnvelope。

## 验证结果

- 15 个产品相关后端测试文件执行到 100%，退出码 0。
- Ruff 检查通过。
- OpenAPI 漂移检查通过，schema 摘要为
  `950fed79453e816ee01172f796592f6e5681a1b586f16c6bf8ed2877ead91b31`。
- Next.js 生产构建通过，生成 14 个页面。
- 桌面端与 390×844 移动端完成真实浏览器操作，包含 Conversation、Memory、Competition 和 Operations。
- 使用相同 session/control root 重启后，readiness、租约、Run、候选、Memory、游标和服务循环恢复。

## 兼容性

- Web 省略 `swarm_class` 时继续使用 `muteki.swarm.swarm.Swarm`。
- F01～F11 及其他实验调度类没有进入 Web、TUI、Competition 或默认 CliSolver 路径。
- 旧 Run HTTP 路由保留认证和响应形状，业务规则由统一 Command API 处理。
- DeepSeek Harness 的审批能力、Pi/OMP 的带内审批能力等未由上游提供的功能保持 `false`，界面显示限制。

## P1 授权平台验收

2026-08-24 已按用户授权在隔离部署创建独立比赛、账号和题目，完成 CTFd、rCTF、GZCTF 与
Generic Browser 的同步、附件、提交、限速、动态实例、wrong 持久否决和重启恢复。完整记录见
[P1 授权比赛平台验收记录](validation/p1_authorized_platform_acceptance_2026-08-24.md)。

## 公开多架构镜像发布与主机验收

2026-08-24 已通过公开仓库 GitHub Actions 发布 `v0.3.1` 和 `v0.3.2` 的 Web、UI、完整 Worker
与 Slim Worker。每个 Tag 均为只包含 `linux/amd64` 和 `linux/arm64` 的 manifest index。

随后在 `root@38.247.145.244`（amd64）和 `dgx-spark-1`（ARM64）完成真实主机验收：8 个引用在
两台主机上分别拉取为原生架构，Web API、UI 和 UI 代理健康检查通过，完整/Slim Worker 均完成
Runtime Agent 反向连接、Health、Teardown 和九类引擎版本检查。临时容器与网络已清理，原有服务未被
重建或停止。8 个最终索引摘要、成功记录路径及日志校验值见
[R1 公开多架构镜像主机验收记录](validation/r1_public_multiarch_host_acceptance_2026-08-24.md)。
