# Tsecbench 平台适配器（Agent Plugins 1.2.7）

在 Muteki **扩展**页用「本地目录」安装本目录，启用后比赛大厅平台类型会出现
「Tsecbench 测评 · 扩展」。

## 安装

1. 设置 → 扩展 → 来源类型：本地目录
2. 路径：`examples/extensions/tsecbench`
3. 生成安装预览 → 安装 → 启用
4. 比赛大厅 → 新建平台连接 → 选择 Tsecbench → 填入
   `BENCHMARK_BASE_URL` 与 `BENCHMARK_TOKEN`。如需精确对齐倒计时，也可填入
   包含 `token`、`started_at`、`ends_at` 的 JSON。凭据作为 Secret 保存，
   不进入比赛数据库。

## 连接 VPN（macOS）

从 Tsecbench 当前测评批次下载 `.ovpn` 文件，然后在终端执行：

```bash
sudo /opt/homebrew/sbin/openvpn --config ~/Downloads/<tsecbench-vpn-config>.ovpn
```

插件不读取 VPN 配置，也不管理 OpenVPN 进程。比赛大厅中的“检查 VPN 连接”
会访问 Tsecbench 官方健康地址 `http://10.0.100.58`；检查通过后再调用平台 API。
插件的健康检查和平台 API 请求会绕过宿主的 `HTTP_PROXY` / `ALL_PROXY`，保证
流量按 OpenVPN 注入的系统路由直连；解题 Worker 会把当前靶场主机加入
`NO_PROXY`，模型 API 仍可继续使用用户配置的代理。

## 配置（扩展详情）

| 键 | 说明 |
|----|------|
| `max_instances` | 默认 3（平台硬上限） |
| `auth_header` | 默认 `BENCHMARK_TOKEN` |

## 运维注意

- 靶场地址为 `10.0.x.x`，解题流量需 VPN。
- 插件在每次平台调用前检查 VPN。未连接时会返回上面的 OpenVPN 命令，
  不启动调度，也不消耗平台实例槽位。
- Tsecbench 在首次认证 API 调用时启动 21600 秒倒计时。插件持久化该批次的
  `started_at` / `ends_at`，同步时持续输出剩余时间；Muteki 页面按结束时间
  每秒显示并由后台同步校准，不拉取控制台历史事件。
- LLM `*.tsecbench.gw` 网关属于 Solver 配置，不在本插件内。
- Token **不**在启用扩展时解析：凭据在比赛大厅建连接时写入 PlatformSecretStore，调用时经 RPC 注入。
- 注册比赛时会按 probe `policy_hints` 默认套用 `tsec_eval` 策略档。
- 插件按最近一次完整测评的有效作答耗时固定题目优先级。三个实例槽位始终
  从尚未启动的最高优先级题目开始补位：

  ```text
  d-02 e1-06 d-01 e2-01 e2-03 a-10 e1-02 d-05 a-01 f1-04
  e1-01 d-03 a-08 e3-02 a-04 e3-03 e1-05 e1-03 a-06 a-09
  e3-01 a-02 f1-03 f2-03 a-15 f1-05 f2-02 a-12 a-11 c-07
  f1-02 c-05 f2-08 d-06 c-01 f2-07 e1-04 f1-01 a-17 f2-04
  d-04 e2-04 e2-02 c-08 a-13 c-04 a-14 c-06 f2-06 a-07
  c-03 a-03 c-09 f2-01 c-02 a-16 a-18 a-05
  e3-04 f2-05 b-01 b-02 b-03
  ```
- 插件会完整查询平台的 `container_status` 与地址列表。重新启动题目时会生成
  新的环境 ID；即使平台复用了相同地址，Muteki 也能识别这是新的环境代，
  同时保留原 Run 的历史记录。
- 所有题目均为一次持续执行。插件不设置轮次时间盒、复访等待或租约 TTL，
  已启动的 Run 和远端实例持续保留到题目判对、测评批次结束或平台明确报错。
- `e3-04`、`f2-05`、`b-01`、`b-02`、`b-03` 为最终阶段题目；其余题目
  全部进入终态后，才按上述顺序占用空闲槽位。
- `renew_instance` 是平台状态查询，不再用本地缓存模拟续租。只有平台返回
  `available` 才确认环境仍可用；查询到地址变化时会返回新的环境 ID。
- `release_instance` 在关闭接口返回 `closed=true`、列表接口确认状态为
  `stopped`，或平台明确返回批次任务 `already finished` 时报告释放成功。
  同步确认测评批次结束后会清除该连接的本地实例记录；其他未知结果仍保留，
  供后续核对。
- 平台未提供实例 UUID、暂停、恢复或续租接口。插件的环境 ID 是本地隔离标识，
  不代表平台承诺容器状态能够跨关闭与重开保留。
- 从不保存环境序号的旧版升级时，需要把已有比赛的访问次数一次性写入
  `environment_sequences`。之后插件会持续保存序号，后续升级和重启无需重复迁移。
- 插件不包含模拟平台路径；probe、同步、启动、查询、提交与关闭均调用
  真实 Tsecbench API，网络、凭据或平台状态异常会直接返回错误。
- Flag 发出前会持久化不含明文的提交日志，并保存已收到的终态回执。宿主在
  回执丢失或重启后通过 `reconcile_submission` 核对，已有终态不会重复提交；
  不能可靠归因的结果保持待核对状态。
- 最后一个 Flag 判对后立即关闭该题环境；比赛同步也会回收已完成题目遗留的
  远端容器，避免已解题继续占用 Tsec 的三个实例槽位。

验收通过真实比赛任务观察：平台实例数、环境截止时间、关闭回执、原 Run ID、
新执行代以及 flag 提交结果。
