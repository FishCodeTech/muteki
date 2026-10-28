"use client";

import { useState } from "react";
import { Button } from "@heroui/react";
import { Icon } from "@/components/Icon";
import type { IdentityContext, OastInteraction, ProtocolStream, TestObject } from "./RequestWorkspaceTypes";

export function IdentityObjectPanel({
  identities,
  objects,
  onVerify,
  onRestore,
  busyId,
}: {
  identities: IdentityContext[];
  objects: TestObject[];
  onVerify: (context: IdentityContext, expected: Record<string, unknown>) => void;
  onRestore: (object: TestObject) => void;
  busyId?: string;
}) {
  const [expectedDrafts, setExpectedDrafts] = useState<Record<string, string>>({});
  const [errors, setErrors] = useState<Record<string, string>>({});
  return <div className="grid gap-4 xl:grid-cols-2">
    <section aria-label="命名身份" className="grid content-start gap-3">
      <h2 className="text-base font-semibold">身份与会话</h2>
      {identities.length ? identities.map(identity => <article key={identity.context_id} className="grid gap-2 rounded-md border border-cx-line p-3">
        <div className="flex flex-wrap items-center gap-2"><strong>{identity.context_id}</strong><span className="pentest-chip pentest-chip--neutral">{identity.status || "identity_unverified"}</span><span className="text-xs text-cx-fg-3">identity v{identity.identity_revision} · browser g{identity.browser_generation}</span></div>
        <p className="text-xs text-cx-fg-3">{identity.account_ref || "无账号引用"} · {identity.role || "角色未知"} · {identity.tenant || "租户未知"}</p>
        {identity.recovery_state ? <p className="pentest-notice pentest-notice--warning" role="status">恢复状态：{identity.recovery_state}。磁盘 Profile 不代表标签页、sessionStorage 或业务状态仍有效。</p> : null}
        <dl className="grid grid-cols-[80px_1fr] gap-1 text-xs"><dt>期望映射</dt><dd><code>{JSON.stringify(identity.expected || {})}</code></dd><dt>实际映射</dt><dd><code>{JSON.stringify(identity.observed || {})}</code></dd><dt>最后页面</dt><dd className="break-all">{identity.last_url || "—"}</dd></dl>
        <label className="grid gap-1 text-xs">成功条件（JSON；必须由受保护接口、页面或后续请求实际验证）
          <textarea value={expectedDrafts[identity.context_id] ?? JSON.stringify(identity.expected || {}, null, 2)} onChange={event => setExpectedDrafts(current => ({ ...current, [identity.context_id]: event.target.value }))} className="min-h-[90px] rounded-md border border-cx-line bg-cx-bg p-2 font-mono text-xs" />
        </label>
        {errors[identity.context_id] ? <p role="alert" className="text-xs text-cx-danger">{errors[identity.context_id]}</p> : null}
        <div><Button size="sm" variant="secondary" isPending={busyId === identity.context_id} onPress={() => {
          try {
            const rawText = expectedDrafts[identity.context_id] ?? JSON.stringify(identity.expected || {}, null, 2);
            const parsed = JSON.parse(rawText) as unknown;
            if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) throw new Error("成功条件必须是 JSON 对象");
            setErrors(current => ({ ...current, [identity.context_id]: "" }));
            onVerify(identity, parsed as Record<string, unknown>);
          } catch (cause) { setErrors(current => ({ ...current, [identity.context_id]: cause instanceof Error ? cause.message : String(cause) })); }
        }}><Icon name="check" size={13} />执行身份核验</Button></div>
      </article>) : <div className="workspace-home-empty">尚无命名身份。Cookie 非空、HTTP 200 或头像不会在这里被当作登录成功。</div>}
    </section>
    <section aria-label="测试对象与恢复" className="grid content-start gap-3">
      <h2 className="text-base font-semibold">测试对象与恢复</h2>
      {objects.length ? objects.map(object => <article key={object.object_id} className="grid gap-2 rounded-md border border-cx-line p-3">
        <div className="flex flex-wrap items-center gap-2"><strong>{object.object_type} · {object.object_id}</strong><span className="pentest-chip pentest-chip--warning">{typeof object.restoration === "string" ? object.restoration || "not_applicable" : object.restoration?.status || "not_applicable"}</span></div>
        <p className="text-xs text-cx-fg-3">所有者 {object.owner || "未知"} · 租户 {object.tenant || "未知"}</p>
        <dl className="grid grid-cols-[80px_1fr] gap-1 text-xs"><dt>前态</dt><dd><code>{JSON.stringify(object.initial_state || {})}</code></dd><dt>当前态</dt><dd><code>{JSON.stringify(object.current_state || {})}</code></dd><dt>预期变更</dt><dd>{typeof object.expected_change === "string" ? object.expected_change || "—" : JSON.stringify(object.expected_change || {})}</dd><dt>残留/原因</dt><dd>{object.reason || "—"}</dd></dl>
        {object.lease ? <p className="text-xs text-cx-fg-3">租约：<code>{JSON.stringify(object.lease)}</code></p> : null}
        <div><Button size="sm" variant="outline" isPending={busyId === object.object_id} onPress={() => onRestore(object)}><Icon name="refresh" size={13} />执行恢复并读回状态</Button></div>
      </article>) : <div className="workspace-home-empty">尚无测试对象。恢复失败不会删除利用证据，也不会被显示为已清理。</div>}
    </section>
  </div>;
}

export function ProtocolEvidencePanel({ streams, interactions }: { streams: ProtocolStream[]; interactions: OastInteraction[] }) {
  const [protocol, setProtocol] = useState<"all" | "websocket" | "sse">("all");
  const [selectedId, setSelectedId] = useState("");
  const visible = streams.filter(row => protocol === "all" || row.protocol === protocol);
  const selected = visible.find(row => row.stream_id === selectedId) || visible[0];
  return <div className="grid gap-5">
    <section aria-label="WebSocket 与 SSE 证据" className="grid gap-3">
      <div className="flex flex-wrap items-center gap-2"><h2 className="flex-1 text-base font-semibold">协议流</h2>{(["all", "websocket", "sse"] as const).map(value => <Button key={value} size="sm" variant={protocol === value ? "secondary" : "ghost"} onPress={() => { setProtocol(value); setSelectedId(""); }}>{value === "all" ? "全部" : value === "websocket" ? "WebSocket" : "SSE"}</Button>)}</div>
      {visible.length ? <div className="grid gap-3 lg:grid-cols-[280px_1fr]">
        <div className="grid content-start gap-1" role="list" aria-label="协议流列表">{visible.map(stream => <button type="button" role="listitem" key={stream.stream_id} onClick={() => setSelectedId(stream.stream_id)} className={`rounded-md border p-2 text-left text-xs ${selected?.stream_id === stream.stream_id ? "border-cx-accent" : "border-cx-line"}`}><strong className="block">{stream.protocol} · {stream.stream_id}</strong><span className="text-cx-fg-3">{stream.context_id} · {stream.frames.length} frames · {stream.capture_state || "unknown"}</span></button>)}</div>
        {selected ? <article className="min-w-0 grid gap-2">
          <div className="flex flex-wrap gap-2 text-xs text-cx-fg-3"><span>连接：{selected.state || "unknown"}</span><span>身份：{selected.context_id} v{selected.identity_revision || 0}</span><span>generation：{selected.browser_generation || 0}</span></div>
          <pre className="max-h-[160px] overflow-auto rounded-md border border-cx-line bg-cx-bg p-2 text-xs whitespace-pre-wrap">{JSON.stringify(selected.handshake || {}, null, 2)}</pre>
          {selected.gaps?.length ? <p className="pentest-notice pentest-notice--warning" role="status">捕获存在缺口：{JSON.stringify(selected.gaps)}；{selected.termination_reason || "未记录终止原因"}</p> : null}
          <div className="overflow-auto rounded-md border border-cx-line" role="table" aria-label="协议帧顺序"><div className="grid grid-cols-[70px_100px_100px_1fr] gap-2 border-b border-cx-line p-2 text-xs font-medium" role="row"><span>顺序</span><span>方向/类型</span><span>时间</span><span>原始内容</span></div>{selected.frames.map(frame => <div className="grid grid-cols-[70px_100px_100px_1fr] gap-2 border-b border-cx-line p-2 font-mono text-xs last:border-b-0" role="row" key={frame.seq}><span>{frame.seq}</span><span>{frame.direction || frame.event || "—"}{frame.opcode != null ? ` / ${frame.opcode}` : ""}{frame.id ? ` / ${frame.id}` : ""}{frame.retry != null ? ` / retry=${frame.retry}` : ""}</span><span>{frame.timestamp ? new Date(frame.timestamp).toLocaleTimeString() : "—"}</span><pre className="whitespace-pre-wrap">{frame.payload ?? frame.payload_base64 ?? "—"}{frame.truncated ? " [partial]" : ""}</pre></div>)}</div>
        </article> : null}
      </div> : <div className="workspace-home-empty">尚无 WS/SSE 流证据。连接成功、消息发送、服务端确认和业务状态改变不会混为一谈。</div>}
    </section>
    <section aria-label="OAST 异步回连" className="grid gap-3">
      <h2 className="text-base font-semibold">OAST 回连</h2>
      {interactions.length ? interactions.map(interaction => <article key={interaction.interaction_id} className="grid gap-2 rounded-md border border-cx-line p-3">
        <div className="flex flex-wrap items-center gap-2"><strong>{interaction.interaction_id}</strong><span className="pentest-chip pentest-chip--neutral">{interaction.state || "active"}</span><span className="pentest-chip pentest-chip--neutral">health: {interaction.health || "unknown"}</span><span className="text-xs text-cx-fg-3">{interaction.protocols.join("/")}</span></div>
        <p className="break-all font-mono text-xs">token: {interaction.token} · trigger: {interaction.trigger_exchange_id || "—"}</p>
        <p className="text-xs text-cx-fg-3">创建于 {new Date(interaction.created_at).toLocaleString()} · 游标 {interaction.cursor || "—"} · {interaction.completeness || "完整性未知"}{interaction.error ? ` · ${interaction.error}` : ""}</p>
        {interaction.events?.length ? <div className="grid gap-1">{interaction.events.map(event => <div key={event.event_id} className="rounded-sm bg-cx-bg p-2 font-mono text-xs">{event.late ? "[迟到] " : ""}{event.health_probe ? "[健康探测] " : ""}{event.missing_before ? "[前页缺失] " : ""}{event.protocol} · {new Date(event.received_at).toLocaleString()} · {event.source_address || "来源未知"} · {event.path || "—"} · {event.raw_ref?.sha256 || "无 raw ref"}</div>)}</div> : <p className="text-xs text-cx-fg-3">没有回连不能证明无漏洞；请同时检查服务健康、延迟、过期与缺页状态。</p>}
      </article>) : <div className="workspace-home-empty">尚无 OAST 分配或回连。目标回连与服务健康探测会分别保存。</div>}
    </section>
  </div>;
}
