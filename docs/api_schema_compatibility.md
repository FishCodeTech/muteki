# API 契约兼容记录

Muteki Web API 以 `apps/web/ui/lib/contracts/openapi.json` 作为完整 OpenAPI 快照，
以 `apps/web/ui/lib/contracts/openapi.ts` 和 `index.ts` 作为前端生成类型。CI 执行：

```bash
uv run --frozen python scripts/gen_contracts_ts.py --check
uv run --frozen python scripts/gen_openapi.py --check
```

兼容规则：

- 新增可选字段、新端点和新事件类型属于兼容变更。
- 删除或重命名字段、收紧字段类型、改变状态码或游标含义属于不兼容变更。
- 不兼容变更必须提高 `CONTRACT_SCHEMA_VERSION`，保留旧入口的认证和响应外形，
  并在本文件记录迁移方法。
- SSE 的 `after` 与 `Last-Event-ID` 都表示最后已消费的全局事件序号；snapshot
  提供 `watermark`，空闲期发送 heartbeat，客户端关闭连接后服务端停止读取。
- wait 使用同一聚合事件游标；过期或非法游标返回统一 `ErrorEnvelope`，不静默回到
  最新位置。

## 版本记录

### schema_version 1

- 初始统一 Command、Receipt、Event、Error、分页和游标契约。
- Competition、Extension、Conversation、Runtime、Capability 和运维端点纳入完整
  OpenAPI 快照。
- 旧 Run 路由保留兼容外形，内部状态修改统一进入 Command API。
