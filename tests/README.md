# 测试套件范围

## 目标

默认测试套件用于验证当前产品执行路径和已经启用的运行基础。测试应当能够阻止真实回归，且可以在项目解释器下独立、稳定地运行。

当前保留范围包括：

- CLI Worker、九类引擎、模型参数和结果解析；
- Flag/Finding 来源校验、多 Flag 和提交入口；
- Worker 全局后端、网络配置、Credential/Seat 模型和账号注入；
- 容器执行、控制通道、运行终态和清理；
- Coordinator、共享图、Operator 指令、Review 和 F11 Agent Team；
- Web API、认证、事件流、运行控制和当前 UI 行为；
- Protocol 2 已启用的 admission、budget、capture、closure、network、receipt、supervisor 和 Web adapter；
- Pentest 当前已实现的 Finding 证据校验；
- TUI、学习产物和 mock 运行的基础行为。

## 保留标准

新增或保留测试至少应满足以下条件之一：

1. 覆盖当前生产入口实际调用的行为；
2. 对应已经复现的缺陷，并验证用户可观察的结果；
3. 保护独立的正确性边界，例如结果来源、凭据注入、终态、清理或原子状态更新；
4. 验证当前公开配置或接口契约。

同时应满足以下要求：

- 使用当前接口，不继续构造已经删除的 `runtime_profiles`、`Environment` 或 Seat 级运行环境；
- 不从其他 `test_*.py` 导入夹具或辅助函数；共享辅助代码应放入普通测试辅助模块；
- 默认运行不调用收费模型、外部评测平台或真实比赛目标；
- 避免只检查源码中是否存在某个字符串；优先调用公开函数、HTTP 接口或实际状态转换；
- 避免精确复制完整命令行；只断言影响行为和权限的关键参数；
- 不为已经暂缓的研究版本、影子实验或历史候选方案增加默认测试。

## 不进入默认套件的内容

以下内容需要重新启动对应研究或产品方向后，另行建立隔离的评测入口：

- C6 来源、分配、runner 和评测权威链；
- 认知策略版本序列、holdout、shadow、A/B study 和离线模型 runner；
- F01–F10 可选研究框架；
- NYU/TSec 专用评测脚本；
- 默认关闭的认知选择、复现和验证研究链；
- 需要真实 API Key、实际费用或外部服务状态的 smoke test。

这些内容的代码和历史文档可以继续保留。恢复时应单独定义数据、预算、环境、超时和结果记录，避免重新混入产品回归套件。

## 运行命令

始终通过项目 Python 启动 pytest，避免解析到宿主环境中的全局 pytest：

```bash
# 收集检查
uv run --extra dev python -m pytest --collect-only -q

# 完整套件
uv run --extra dev python -m pytest -q

# 局部测试
uv run --extra dev python -m pytest -q tests/test_worker_config.py
```

不要使用 `uv run pytest` 作为项目验证命令。
