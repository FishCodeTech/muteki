"use client";

import { useCallback, useEffect, useState } from "react";
import { Badge, Button, Callout, CopyButton, Dialog, Input, Select, StatusDot, Switch, toast } from "@muteki/ui";
import { CodeBlock } from "@muteki/ui/agent";
import { RunnerResources } from "@/components/RunnerResources";
import { SettingsEmpty, SettingsRow, SettingsSection } from "@/components/settings/primitives";
import { apiFetch } from "@/lib/serviceAuth";
import {
  RunnerApiError,
  deleteRunnerNode, getRunnerListener, issueRunnerEnrollment, listRunnerAudit, listRunnerNodes, putRunnerListener,
  rotateRunnerNode, setRunnerNodeState,
  type RunnerAuditEvent, type RunnerEnrollment, type RunnerListener, type RunnerListenerConfig, type RunnerNode, type RunnerRotation,
} from "@/lib/runnerNodes";

const TTL_OPTIONS = [
  { value: "300", label: "5 分钟内有效" },
  { value: "600", label: "10 分钟内有效" },
  { value: "900", label: "15 分钟内有效" },
];
const STATE_LABEL: Record<RunnerNode["state"], { label: string; tone: "success" | "warning" | "danger" }> = {
  active: { label: "可调度", tone: "success" },
  draining: { label: "排空中", tone: "warning" },
  revoked: { label: "已撤销", tone: "danger" },
};
const AUDIT_LABEL: Record<string, string> = {
  enrollment_issued: "生成注册令牌",
  enrolled: "节点完成注册",
  session_started: "节点建立会话",
  credential_rotated: "轮换节点凭据",
  active: "恢复调度",
  draining: "开始排空",
  revoked: "撤销节点",
  removed: "删除节点",
};
const time = (seconds?: number) => seconds ? new Date(seconds * 1000).toLocaleString() : "—";
const message = (cause: unknown) => cause instanceof Error ? cause.message : String(cause);

function Mono({ children, title }: { children: string; title?: string }) {
  return <code className="min-w-0 truncate rounded bg-cx-hover px-1.5 py-px font-cx-mono text-cx-xs text-cx-fg-2" title={title ?? children}>{children}</code>;
}

function ListenerSection({ listener, onChange }: { listener: RunnerListener | null; onChange: (next: RunnerListener) => void }) {
  const [draft, setDraft] = useState<RunnerListenerConfig | null>(null);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");
  useEffect(() => { if (listener && !draft) setDraft(listener.config); }, [draft, listener]);
  if (!listener || !draft) return <SettingsSection title="远程接入"><SettingsEmpty>正在读取远程接入状态…</SettingsEmpty></SettingsSection>;
  const { status } = listener;
  const cli = status.managed_by_cli;
  const dirty = JSON.stringify(draft) !== JSON.stringify(listener.config);

  const apply = async (next: RunnerListenerConfig) => {
    setSaving(true); setError("");
    try {
      const saved = await putRunnerListener(next);
      onChange(saved); setDraft(saved.config);
      toast({ title: saved.status.running ? `远程接入已在 ${saved.status.address} 监听` : "远程接入已关闭", tone: "success" });
    } catch (cause) {
      setError(message(cause));
      if (cause instanceof RunnerApiError && cause.body && typeof cause.body === "object" && "status" in cause.body) {
        const body = cause.body as RunnerListener;
        onChange({ config: body.config ?? next, status: body.status });
      }
    } finally { setSaving(false); }
  };
  const downloadCa = async () => {
    try {
      const response = await apiFetch("/api/runner/ca.pem", { cache: "no-store" });
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      const url = URL.createObjectURL(await response.blob());
      const link = document.createElement("a");
      link.href = url; link.download = "muteki-runner-ca.pem"; link.click();
      URL.revokeObjectURL(url);
    } catch (cause) { toast({ title: `下载 CA 失败：${message(cause)}`, tone: "danger" }); }
  };

  return (
    <SettingsSection
      title="远程接入"
      description="开启后，Web 服务在单独的 HTTPS 端口接受远程 Runner 连接。证书由本机自动生成的私有 CA 签发，保存后立即生效。"
      actions={<Badge tone={status.running ? "success" : status.error ? "danger" : "neutral"} dot>{status.running ? "监听中" : status.error ? "启动失败" : "未开启"}</Badge>}
    >
      {cli ? <div className="px-5 pt-4"><Callout tone="neutral" title={`由启动参数管理（${cli}）`}>本进程使用 --runner-tls-listen 启动，远程接入端口不能在这里修改。</Callout></div> : null}
      <SettingsRow
        title="开启远程接入"
        description="关闭会立即断开所有远程节点；它们正在运行的 Worker 会按节点失联处理。"
        control={<Switch ariaLabel="开启远程接入" checked={draft.enabled} disabled={saving || Boolean(cli)} onCheckedChange={(enabled) => setDraft({ ...draft, enabled })} />}
      />
      <SettingsRow
        title="监听地址"
        description="Web 服务所在机器上的 IP:端口。0.0.0.0 表示所有网卡；仅本机测试可用 127.0.0.1。"
        control={<Input className="w-56 font-cx-mono" size="sm" aria-label="监听地址" value={draft.listen} disabled={saving || Boolean(cli)} placeholder="0.0.0.0:8443" onChange={(event) => setDraft({ ...draft, listen: event.target.value })} />}
      />
      <SettingsRow
        title="对外地址"
        description="远程节点访问这台服务的 HTTPS 地址，证书会签发给其中的主机名或 IP。经过端口映射或反向代理时填写映射后的地址。"
        control={<Input className="w-72 font-cx-mono" size="sm" aria-label="对外地址" value={draft.public_url} disabled={saving || Boolean(cli)} placeholder="https://192.168.1.10:8443" onChange={(event) => setDraft({ ...draft, public_url: event.target.value })} />}
      />
      {error || status.error ? <div className="px-5 py-3"><Callout tone="danger" role="alert">{error || status.error}</Callout></div> : null}
      {status.running ? (
        <SettingsRow
          title="证书"
          stacked
          description={<>监听 <Mono>{status.address ?? ""}</Mono>，证书包含 {(status.certificate_names ?? []).join("、") || "—"}，有效期至 {time(status.certificate_expires_at)}（到期前 30 天自动续签）。</>}
        >
          <div className="flex flex-wrap items-center gap-2">
            <span className="text-cx-xs text-cx-fg-3">CA 指纹（SHA-256）</span>
            <Mono>{status.ca_fingerprint ?? ""}</Mono>
            {status.ca_fingerprint ? <CopyButton text={status.ca_fingerprint} label="复制 CA 指纹" /> : null}
            <Button size="xs" variant="secondary" icon="download" onClick={() => void downloadCa()}>下载 CA 证书</Button>
          </div>
        </SettingsRow>
      ) : null}
      <SettingsRow
        title="应用"
        description={dirty ? "有未应用的修改。" : "当前设置已生效。"}
        control={<div className="flex gap-2">
          {dirty ? <Button size="sm" variant="ghost" disabled={saving} onClick={() => { setDraft(listener.config); setError(""); }}>还原</Button> : null}
          <Button size="sm" variant="primary" loading={saving} disabled={!dirty || saving || Boolean(cli)} onClick={() => void apply(draft)}>保存并应用</Button>
        </div>}
      />
    </SettingsSection>
  );
}

function EnrollSection({ listening, nodes, onIssued }: { listening: boolean; nodes: RunnerNode[] | null; onIssued: () => void }) {
  const [name, setName] = useState("");
  const [ttl, setTtl] = useState("600");
  const [issuing, setIssuing] = useState(false);
  const [error, setError] = useState("");
  const [issued, setIssued] = useState<(RunnerEnrollment & { name: string; issued_at: number }) | null>(null);
  const [now, setNow] = useState(() => Math.floor(Date.now() / 1000));
  const enrolled = issued ? nodes?.find((node) => node.name === issued.name && node.created_at >= issued.issued_at) : undefined;
  useEffect(() => {
    if (!issued || enrolled) return;
    const timer = window.setInterval(() => setNow(Math.floor(Date.now() / 1000)), 1000);
    return () => window.clearInterval(timer);
  }, [issued, enrolled]);
  const remaining = issued ? issued.expires_at - now : 0;

  const issue = async () => {
    setIssuing(true); setError("");
    try {
      const result = await issueRunnerEnrollment(name.trim(), Number(ttl));
      setIssued({ ...result, name: name.trim(), issued_at: result.expires_at - Number(ttl) }); setNow(Math.floor(Date.now() / 1000)); setName("");
      onIssued();
    } catch (cause) { setError(message(cause)); }
    finally { setIssuing(false); }
  };

  return (
    <SettingsSection title="添加节点" description="为远程机器生成一次性注册命令。远程机器需要 Docker 和与本服务相同版本的 muteki-server。">
      {!listening ? <div className="px-5 pt-4"><Callout tone="warning" title="远程接入尚未开启">请先在上方开启远程接入；否则只能生成令牌，无法生成带 CA 指纹的注册命令。</Callout></div> : null}
      <SettingsRow
        title="节点名称"
        description="用于在列表和任务里识别这台机器，例如 gpu-box 或 lab-mac。"
        control={<div className="flex flex-wrap items-center gap-2">
          <Input className="w-full sm:w-48" size="md" aria-label="节点名称" value={name} maxLength={120} placeholder="lab-runner" onChange={(event) => setName(event.target.value)}
            onKeyDown={(event) => { if (event.key === "Enter" && name.trim() && !issuing) void issue(); }} />
          <Select<string> ariaLabel="令牌有效期" size="sm" className="h-8 w-full sm:w-36" value={ttl} options={TTL_OPTIONS} onChange={setTtl} />
          <Button size="md" variant="primary" icon="plus" loading={issuing} disabled={!name.trim() || issuing} onClick={() => void issue()}>生成注册命令</Button>
        </div>}
      />
      {error ? <div className="px-5 py-3"><Callout tone="danger" role="alert">{error}</Callout></div> : null}
      {issued && enrolled ? (
        <div className="border-t border-cx-border-subtle px-5 py-4">
          <Callout tone="success" title={`“${issued.name}”已完成注册${enrolled.connected ? "并已上线" : "，等待 Runner 启动"}`} onDismiss={() => setIssued(null)}>
            注册令牌已使用。可在下方节点列表中管理它，并在“运行环境”中把它选为执行节点。
          </Callout>
        </div>
      ) : issued ? (
        <div className="flex flex-col gap-2 border-t border-cx-border-subtle px-5 py-4">
          <Callout
            tone={remaining > 0 ? "accent" : "warning"}
            title={remaining > 0 ? `“${issued.name}”的注册命令，${Math.floor(remaining / 60)} 分 ${remaining % 60} 秒内有效，只能使用一次` : "注册令牌已过期，请重新生成"}
            onDismiss={() => setIssued(null)}
          >
            令牌只显示这一次，关闭后无法再次查看。在远程机器上依次运行下面两条命令。
          </Callout>
          {issued.commands ? (
            <>
              <p className="text-cx-xs text-cx-fg-3">1. 注册（会校验服务端 CA 指纹，并把节点身份保存在 ~/.muteki-runner）</p>
              <CodeBlock code={issued.commands.enroll} language="bash" collapseAfter={0} wrapByDefault className="my-0" />
              <p className="text-cx-xs text-cx-fg-3">2. 启动 Runner（保持运行；可交给 systemd 或 launchd 托管）</p>
              <CodeBlock code={issued.commands.start} language="bash" collapseAfter={0} wrapByDefault className="my-0" />
              <p className="text-cx-xs leading-5 text-cx-fg-4">机器需要经代理访问时，Runner 会使用 HTTPS_PROXY / ALL_PROXY，并遵守 NO_PROXY。重新注册同一台机器时在注册命令末尾加 --replace。</p>
            </>
          ) : (
            <div className="flex items-center gap-2"><span className="text-cx-xs text-cx-fg-3">注册令牌</span><Mono>{issued.token}</Mono><CopyButton text={issued.token} label="复制令牌" /></div>
          )}
        </div>
      ) : null}
    </SettingsSection>
  );
}

type Confirm = { kind: "revoke" | "delete" | "rotate"; node: RunnerNode } | null;

function NodeRow({ node, selected, busy, onSelect, onAction }: {
  node: RunnerNode; selected: boolean; busy: boolean;
  onSelect: () => void; onAction: (kind: "drain" | "activate" | "revoke" | "delete" | "rotate") => void;
}) {
  const state = STATE_LABEL[node.state] ?? { label: node.state, tone: "warning" as const };
  const expiresSoon = node.state !== "revoked" && node.credential_expires_at - Date.now() / 1000 < 7 * 24 * 3600;
  return (
    <div className="cx-settings-row px-5 py-3">
      <div className="flex flex-wrap items-center gap-3">
        <StatusDot tone={node.connected ? "success" : "neutral"} label={node.connected ? "在线" : "离线"} />
        <button type="button" className="min-w-0 flex-1 text-left" onClick={onSelect} aria-expanded={selected}>
          <span className="flex items-center gap-2">
            <span className="truncate text-cx-sm font-medium text-cx-fg">{node.name}</span>
            <Badge tone={state.tone}>{state.label}</Badge>
            <Badge tone={node.connected ? "success" : "neutral"}>{node.connected ? "在线" : "离线"}</Badge>
          </span>
          <span className="mt-1 block truncate text-cx-xs text-cx-fg-3">
            <span className="font-cx-mono">{node.id}</span> · 注册于 {time(node.created_at)} · 凭据 v{node.credential_version}{node.state !== "revoked" ? `，有效期至 ${time(node.credential_expires_at)}` : ""}
            {expiresSoon ? <span className="text-cx-warning"> · 凭据即将过期，请轮换</span> : null}
          </span>
        </button>
        <div className="flex shrink-0 items-center gap-1">
          {node.state === "active" ? <Button size="xs" variant="secondary" disabled={busy} onClick={() => onAction("drain")}>排空</Button> : null}
          {node.state === "draining" ? <Button size="xs" variant="secondary" disabled={busy} onClick={() => onAction("activate")}>恢复调度</Button> : null}
          {node.state !== "revoked" ? <Button size="xs" variant="ghost" disabled={busy} onClick={() => onAction("rotate")}>轮换凭据</Button> : null}
          {node.state !== "revoked" ? <Button size="xs" variant="danger-soft" disabled={busy} onClick={() => onAction("revoke")}>撤销</Button> : null}
          {node.state === "revoked" ? <Button size="xs" variant="danger-soft" disabled={busy || node.connected} tooltip={node.connected ? "节点会话关闭后才能删除" : undefined} onClick={() => onAction("delete")}>删除</Button> : null}
          <Button size="xs" variant="ghost" icon={selected ? "chevronUp" : "chevronDown"} onClick={onSelect}>{selected ? "收起" : "并发"}</Button>
        </div>
      </div>
    </div>
  );
}

function AuditSection({ events }: { events: RunnerAuditEvent[] | null }) {
  const [limit, setLimit] = useState(12);
  return (
    <SettingsSection title="节点审计" description="注册、会话、状态变更和凭据轮换记录，最新的在前。">
      {!events ? <SettingsEmpty>正在读取审计记录…</SettingsEmpty> : !events.length ? <SettingsEmpty>还没有记录。</SettingsEmpty> : (
        <>
          {events.slice(0, limit).map((event) => (
            <div key={event.id} className="flex items-center gap-3 border-b border-cx-border-subtle px-5 py-2 text-cx-sm last:border-b-0">
              <span className="w-40 shrink-0 tabular-nums text-cx-fg-3">{time(event.at)}</span>
              <span className="w-28 shrink-0 text-cx-fg">{AUDIT_LABEL[event.action] ?? event.action}</span>
              <span className="min-w-0 flex-1 truncate text-cx-fg-3">{event.node_name ?? (event.node_id || "—")}</span>
            </div>
          ))}
          {events.length > limit ? <div className="px-5 py-2"><Button size="xs" variant="ghost" onClick={() => setLimit((value) => value + 30)}>显示更多（共 {events.length} 条）</Button></div> : null}
        </>
      )}
    </SettingsSection>
  );
}

export function NodesSection({ selectedNodeId, onNodesChanged }: { selectedNodeId: string; onNodesChanged?: (nodes: RunnerNode[]) => void }) {
  const [listener, setListener] = useState<RunnerListener | null>(null);
  const [nodes, setNodes] = useState<RunnerNode[] | null>(null);
  const [audit, setAudit] = useState<RunnerAuditEvent[] | null>(null);
  const [error, setError] = useState("");
  const [expanded, setExpanded] = useState<string>("local");
  const [busy, setBusy] = useState<string | null>(null);
  const [confirm, setConfirm] = useState<Confirm>(null);
  const [rotation, setRotation] = useState<(RunnerRotation & { name: string }) | null>(null);

  const refresh = useCallback(async (signal?: AbortSignal) => {
    const [listenerResult, nodesResult, auditResult] = await Promise.allSettled([getRunnerListener(signal), listRunnerNodes(signal), listRunnerAudit(signal)]);
    if (signal?.aborted) return;
    if (listenerResult.status === "fulfilled") setListener(listenerResult.value);
    if (nodesResult.status === "fulfilled") { setNodes(nodesResult.value); onNodesChanged?.(nodesResult.value); }
    if (auditResult.status === "fulfilled") setAudit(auditResult.value);
    const failed = [listenerResult, nodesResult, auditResult].find((result) => result.status === "rejected") as PromiseRejectedResult | undefined;
    setError(failed ? message(failed.reason) : "");
  }, [onNodesChanged]);

  useEffect(() => {
    const controller = new AbortController();
    void refresh(controller.signal);
    const timer = window.setInterval(() => { if (document.visibilityState === "visible") void refresh(controller.signal); }, 10000);
    return () => { controller.abort(); window.clearInterval(timer); };
  }, [refresh]);

  const act = async (node: RunnerNode, kind: "drain" | "activate" | "revoke" | "delete" | "rotate") => {
    setBusy(node.id); setConfirm(null);
    try {
      if (kind === "drain") await setRunnerNodeState(node.id, "draining");
      if (kind === "activate") await setRunnerNodeState(node.id, "active");
      if (kind === "revoke") await setRunnerNodeState(node.id, "revoked");
      if (kind === "delete") await deleteRunnerNode(node.id);
      if (kind === "rotate") setRotation({ ...(await rotateRunnerNode(node.id)), name: node.name });
      toast({ title: { drain: "节点已开始排空，不再接收新 Worker", activate: "节点已恢复调度", revoke: "节点已撤销", delete: "节点已删除", rotate: "节点凭据已轮换" }[kind], tone: "success" });
    } catch (cause) {
      toast({ title: message(cause), tone: "danger" });
    } finally {
      setBusy(null);
      await refresh();
    }
  };
  const request = (node: RunnerNode, kind: "drain" | "activate" | "revoke" | "delete" | "rotate") => {
    if (kind === "revoke" || kind === "delete" || kind === "rotate") setConfirm({ kind, node });
    else void act(node, kind);
  };
  const confirmCopy = confirm ? {
    revoke: { title: `撤销节点“${confirm.node.name}”？`, body: "撤销后节点的凭据和会话立即失效，且无法恢复；节点上运行中的 Worker 会在租约到期后停止。需要继续使用这台机器时，请重新生成注册命令并使用 --replace 注册。", action: "撤销节点", tone: "danger" as const },
    delete: { title: `删除节点“${confirm.node.name}”？`, body: "删除节点记录及其资源设置。审计记录会保留。", action: "删除", tone: "danger" as const },
    rotate: { title: `轮换节点“${confirm.node.name}”的凭据？`, body: "旧凭据和当前会话立即失效，节点会断开。轮换后需要在节点上停止 Runner、运行安装命令，再重新启动。", action: "轮换凭据", tone: "warning" as const },
  }[confirm.kind] : null;
  const selectedMissing = selectedNodeId !== "local" && nodes && !nodes.some((node) => node.id === selectedNodeId && node.state === "active");

  return (
    <div className="flex flex-col gap-8">
      {error ? <Callout tone="danger" role="alert" action={<Button size="xs" variant="secondary" onClick={() => void refresh()}>重试</Button>}>{error}</Callout> : null}
      {selectedMissing ? <Callout tone="warning" title="当前选择的执行节点不可用">“运行环境”中选择的节点不存在或未处于可调度状态，新任务会被拒绝。请恢复该节点，或在“运行环境”改选其他节点。</Callout> : null}
      <ListenerSection listener={listener} onChange={setListener} />
      <EnrollSection listening={Boolean(listener?.status.running)} nodes={nodes} onIssued={() => void refresh()} />
      <SettingsSection title="节点" description="选择节点查看和调整并发上限。新任务在哪个节点运行，在“运行环境”中选择。" actions={nodes ? <Badge>{nodes.length + 1} 个节点</Badge> : null} bodyClassName="overflow-hidden">
        <div className="cx-settings-row border-b border-cx-border-subtle px-5 py-3">
          <div className="flex flex-wrap items-center gap-3">
            <StatusDot tone="success" label="在线" />
            <button type="button" className="min-w-0 flex-1 text-left" onClick={() => setExpanded(expanded === "local" ? "" : "local")} aria-expanded={expanded === "local"}>
              <span className="flex items-center gap-2"><span className="text-cx-sm font-medium text-cx-fg">本机</span><Badge tone="success">可调度</Badge>{selectedNodeId === "local" ? <Badge tone="accent">新任务默认</Badge> : null}</span>
              <span className="mt-1 block text-cx-xs text-cx-fg-3">与 Web 服务同机运行的 Runner</span>
            </button>
            <Button size="xs" variant="ghost" icon={expanded === "local" ? "chevronUp" : "chevronDown"} onClick={() => setExpanded(expanded === "local" ? "" : "local")}>{expanded === "local" ? "收起" : "并发"}</Button>
          </div>
        </div>
        {expanded === "local" ? <div className="border-b border-cx-border-subtle bg-cx-bg-subtle p-4"><RunnerResources nodeId="local" title="本机并发" /></div> : null}
        {!nodes ? <SettingsEmpty>正在读取节点…</SettingsEmpty> : !nodes.length ? <SettingsEmpty>还没有远程节点。开启远程接入后，在上方生成注册命令。</SettingsEmpty> : nodes.map((node) => (
          <div key={node.id} className="border-b border-cx-border-subtle last:border-b-0">
            <NodeRow node={node} selected={expanded === node.id} busy={busy === node.id}
              onSelect={() => setExpanded(expanded === node.id ? "" : node.id)} onAction={(kind) => request(node, kind)} />
            {expanded === node.id ? (
              <div className="border-t border-cx-border-subtle bg-cx-bg-subtle p-4">
                {node.state === "revoked" ? <SettingsEmpty>已撤销的节点不再调度。</SettingsEmpty> : <RunnerResources nodeId={node.id} title={`${node.name} 并发`} description="远程节点上所有任务共用的 Worker 上限。节点连接后按其上报的资源计算推荐值。" />}
              </div>
            ) : null}
          </div>
        ))}
      </SettingsSection>
      <AuditSection events={audit} />

      <Dialog open={Boolean(confirm)} onOpenChange={(open) => { if (!open) setConfirm(null); }} title={confirmCopy?.title} tone={confirmCopy?.tone} icon="alert" size="sm"
        footer={<><Button variant="ghost" size="sm" onClick={() => setConfirm(null)}>取消</Button><Button variant={confirmCopy?.tone === "danger" ? "danger" : "primary"} size="sm" onClick={() => confirm && void act(confirm.node, confirm.kind)}>{confirmCopy?.action}</Button></>}>
        <p className="text-cx-sm leading-5 text-cx-fg-2">{confirmCopy?.body}</p>
      </Dialog>
      <Dialog open={Boolean(rotation)} onOpenChange={(open) => { if (!open) setRotation(null); }} title={`在“${rotation?.name ?? ""}”上安装新凭据`} size="lg" dismissable={false}
        footer={<Button variant="primary" size="sm" onClick={() => setRotation(null)}>我已复制</Button>}>
        <div className="flex flex-col gap-2 text-cx-sm leading-5 text-cx-fg-2">
          <p>新凭据 v{rotation?.version}，有效期至 {time(rotation?.expires_at)}。凭据只显示这一次。</p>
          <p className="text-cx-fg-3">先停止节点上的 Runner，再运行下面的命令，然后重新启动 Runner。</p>
          {rotation ? <CodeBlock code={rotation.commands.install} language="bash" collapseAfter={0} wrapByDefault className="my-0" /> : null}
        </div>
      </Dialog>
    </div>
  );
}
