"use client";

import { useEffect, useRef, useState } from "react";
import { Badge, Button, Callout, Select } from "@muteki/ui";
import { NumberField } from "@/components/NumberField";
import { SettingsEmpty, SettingsRow, SettingsSection } from "@/components/settings/primitives";
import { apiFetch } from "@/lib/serviceAuth";

type Policy = { mode: "recommended" | "unlimited" } | { mode: "manual"; max_workers: number; max_runs: number };
type Mode = Policy["mode"];
type Snapshot = {
  policy: Policy;
  error?: string;
  pending?: boolean;
  connected?: boolean;
  sampled_at?: string;
  docker?: { available: boolean; cpus: number | null; memory_bytes: number | null };
  recommendation: { max_workers: number; max_runs: number; estimated_worker_bytes?: number };
  components?: { browser_runs: number; estimated_browser_bytes: number; reserved_browser_bytes: number; estimated: boolean };
  memory_pressure?: { available: boolean; host_available_bytes?: number; reserve_bytes?: number; cooldown_pending?: boolean };
  usage?: { available: boolean; complete?: boolean; sampled_at?: string; error?: string;
    measured_container_memory_bytes?: number; measured_processes_and_threads?: number };
  physical_runtimes?: { container_id: string; attribution: string;
    runs: { run_id: string; generation: number; state: string }[];
    configured_limits?: { memory: string; cpus: number; pids_limit: number } | null;
    limit_sources?: Record<string, string> | null;
    measurement_available: boolean;
    measurement?: { memory_working_set_bytes: number; memory_limit_bytes: number; processes_and_threads: number | null; cpu_percent: number | null } | null }[];
  effective_limits?: { max_workers: number; max_runs: number } | null;
  dispatches: { id: string; run_id: string; worker_id: string; status: string; priority?: number; waiting?: { reason: string; required_bytes?: number; budget_bytes?: number } }[];
};

const STATUS: Record<string, { label: string; tone: "neutral" | "running" | "warning" | "danger" }> = {
  queued: { label: "等待容量", tone: "warning" },
  running: { label: "运行中", tone: "running" },
  detached: { label: "等待结果确认", tone: "warning" },
  interrupted: { label: "启动前中断", tone: "danger" },
};
const MODES: { value: Mode; label: string }[] = [
  { value: "recommended", label: "按机器性能推荐" },
  { value: "manual", label: "手动设置上限" },
  { value: "unlimited", label: "不限制" },
];
const PRIORITIES = [{ value: "10", label: "高" }, { value: "0", label: "普通" }, { value: "-10", label: "低" }];
const gib = (bytes?: number | null) => bytes ? `${(bytes / 1024 ** 3).toFixed(1)} GiB` : "未知";
const mib = (bytes?: number | null) => `${((bytes ?? 0) / 1024 ** 2).toFixed(1)} MiB`;

function resourcePath(nodeId: string) {
  return nodeId === "local" ? "/api/runner/resources" : `/api/runner/nodes/${encodeURIComponent(nodeId)}/resources`;
}

async function readResources(nodeId: string, signal?: AbortSignal): Promise<Snapshot> {
  const response = await apiFetch(resourcePath(nodeId), { signal, cache: "no-store" });
  if (!response.ok) throw new Error(`读取节点容量失败（HTTP ${response.status}）`);
  const data = await response.json() as Snapshot;
  if (data.error) throw new Error(`执行节点暂不可用：${data.error}`);
  return data;
}

export function RunnerResources({ nodeId = "local", title = "节点并发", description }: { nodeId?: string; title?: string; description?: string }) {
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [mode, setMode] = useState<Mode>("recommended");
  const [workers, setWorkers] = useState("1");
  const [runs, setRuns] = useState("1");
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [saving, setSaving] = useState(false);
  const [prioritySaving, setPrioritySaving] = useState<string | null>(null);
  const [retry, setRetry] = useState(0);
  const initialized = useRef(false);
  const writing = useRef(false);
  const revision = useRef(0);

  useEffect(() => {
    initialized.current = false;
    const controller = new AbortController();
    let timer: ReturnType<typeof setTimeout>;
    async function refresh() {
      const requestRevision = revision.current;
      try {
        if (document.visibilityState === "visible" || !initialized.current) {
          const next = await readResources(nodeId, controller.signal);
          if (controller.signal.aborted || writing.current || requestRevision !== revision.current) return;
          setSnapshot(next);
          setError("");
          if (!initialized.current) {
            initialized.current = true;
            setMode(next.policy.mode);
            const limits = next.policy.mode === "manual" ? next.policy : next.recommendation;
            setWorkers(String(Math.max(1, limits.max_workers)));
            setRuns(String(Math.max(1, limits.max_runs)));
          }
        }
      } catch (cause) {
        if (!controller.signal.aborted) setError(cause instanceof Error ? cause.message : "读取节点容量失败");
      } finally {
        if (!controller.signal.aborted) timer = setTimeout(refresh, 10000);
      }
    }
    void refresh();
    return () => { controller.abort(); clearTimeout(timer); };
  }, [nodeId, retry]);

  const valid = [workers, runs].every((value) => Number.isInteger(Number(value)) && Number(value) > 0 && Number(value) <= 4294967295);
  const policy: Policy = mode === "manual" ? { mode, max_workers: Number(workers), max_runs: Number(runs) } : { mode };
  const dirty = Boolean(snapshot) && JSON.stringify(snapshot?.policy) !== JSON.stringify(policy);
  const busy = saving || prioritySaving !== null;

  async function save() {
    if (writing.current) return;
    writing.current = true; revision.current += 1;
    setSaving(true); setError(""); setNotice("");
    try {
      const response = await apiFetch(resourcePath(nodeId), { method: "PUT", headers: { "Content-Type": "application/json" }, body: JSON.stringify(policy) });
      if (!response.ok) {
        const body = await response.json().catch(() => null) as { error?: string } | null;
        throw new Error(`保存失败：${body?.error || `HTTP ${response.status}`}`);
      }
      setSnapshot(await readResources(nodeId));
      setNotice("已保存。排队中的 Worker 按新上限调度，运行中的 Worker 不受影响。");
    } catch (cause) { setError(cause instanceof Error ? cause.message : "保存失败"); }
    finally { writing.current = false; setSaving(false); }
  }

  async function savePriority(id: string, priority: number) {
    if (writing.current) return;
    writing.current = true; revision.current += 1;
    setPrioritySaving(id); setError(""); setNotice("");
    try {
      const response = await apiFetch(`/api/runner/dispatches/${encodeURIComponent(id)}/priority`, {
        method: "PUT", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ priority }),
      });
      if (response.status === 409) throw new Error("该 Worker 已离开等待队列，无法再调整优先级。");
      if (!response.ok) throw new Error(`调整优先级失败（HTTP ${response.status}）`);
      setSnapshot(await readResources(nodeId));
    } catch (cause) { setError(cause instanceof Error ? cause.message : "调整优先级失败"); }
    finally { writing.current = false; setPrioritySaving(null); }
  }

  const effective = snapshot
    ? snapshot.effective_limits === undefined
      ? policy.mode === "manual" ? `${policy.max_workers} 个 Worker / ${policy.max_runs} 个任务` : policy.mode === "unlimited" ? "不限制" : `${snapshot.recommendation.max_workers} 个 Worker / ${snapshot.recommendation.max_runs} 个任务`
      : snapshot.effective_limits ? `${snapshot.effective_limits.max_workers} 个 Worker / ${snapshot.effective_limits.max_runs} 个任务` : "不限制"
    : "";
  const hardware = snapshot?.pending
    ? "节点尚未上报资源，推荐值暂按 1 个 Worker 估算。节点连接后会自动更新。"
    : snapshot?.docker?.available
      ? `Docker 可用：${snapshot.docker.cpus ?? "未知"} 核，${gib(snapshot.docker.memory_bytes)} 内存。推荐同时运行 ${snapshot.recommendation.max_workers} 个 Worker、${snapshot.recommendation.max_runs} 个任务${snapshot.recommendation.estimated_worker_bytes ? `（每个 Worker 约 ${gib(snapshot.recommendation.estimated_worker_bytes)}）` : ""}。`
      : "Docker 暂不可用，按宿主机资源估算。";

  return (
    <SettingsSection
      title={title}
      description={description ?? "这台机器上所有任务共用的 Worker 上限，单独保存。单个任务的阵容、超时和成本预算仍按各自设置执行。"}
      actions={effective ? <Badge tone="accent">当前生效：{effective}</Badge> : null}
    >
      {error ? (
        <div className="px-5 pt-4"><Callout tone="danger" role="alert" action={<Button size="xs" variant="secondary" onClick={() => setRetry((value) => value + 1)}>重新读取</Button>}>{error}</Callout></div>
      ) : null}
      {!snapshot && !error ? <SettingsEmpty>正在读取节点容量…</SettingsEmpty> : null}
      {snapshot ? (
        <>
          <SettingsRow
            title="并发策略"
            description={hardware}
            control={<Select<Mode> ariaLabel="节点并发策略" className="w-44" value={mode} disabled={busy} options={MODES} onChange={(next) => { setMode(next); setNotice(""); }} />}
          />
          {mode === "manual" ? (
            <SettingsRow
              title="手动上限"
              description={valid ? "Worker 上限计算所有任务的 Worker 总数；任务上限计算同时运行的任务数。" : <span className="text-cx-danger">两个上限都需要填写大于 0 的整数。</span>}
              control={
                <div className="flex items-center gap-2">
                  <NumberField ariaLabel="节点 Worker 上限" value={workers} min={1} suffix="Worker" disabled={busy} onChange={setWorkers} />
                  <NumberField ariaLabel="节点任务上限" value={runs} min={1} suffix="任务" disabled={busy} onChange={setRuns} />
                </div>
              }
            />
          ) : null}
          {mode === "unlimited" ? (
            <div className="px-5 py-3"><Callout tone="warning">不按机器容量拦截启动。容器的内存、CPU 和进程配额仍然生效，但同时运行过多 Worker 可能耗尽机器内存。</Callout></div>
          ) : null}
          {snapshot.policy.mode === "recommended" && snapshot.effective_limits?.max_workers === 0 ? (
            <div className="px-5 py-3"><Callout tone="warning">可用内存不足，推荐策略暂停启动新 Worker，已有任务不受影响。可以改为手动上限或不限制。</Callout></div>
          ) : null}
          <SettingsRow
            title="保存节点上限"
            description={notice || "节点上限单独保存，保存后立即生效。"}
            control={<Button size="sm" variant="primary" loading={saving} disabled={!dirty || busy || (mode === "manual" && !valid)} onClick={() => void save()}>保存</Button>}
          />
          <details className="group border-t border-cx-border-subtle">
            <summary className="flex cursor-pointer list-none items-center justify-between px-5 py-3 text-cx-sm text-cx-fg-2 hover:bg-cx-hover">
              <span>实际用量与容器上限</span>
              <span className="text-cx-fg-4 group-open:hidden">展开</span>
            </summary>
            <div className="flex flex-col gap-2 px-5 pb-4 text-cx-sm leading-5 text-cx-fg-3">
              {snapshot.memory_pressure?.available ? <p>Docker 主机可用内存 {gib(snapshot.memory_pressure.host_available_bytes)}，预留 {gib(snapshot.memory_pressure.reserve_bytes)}。{snapshot.memory_pressure.cooldown_pending ? "推荐值正在等待持续采样确认，当前上限先按可用内存收紧。" : ""}</p> : null}
              {snapshot.usage?.available
                ? <p>容器合计：内存 {mib(snapshot.usage.measured_container_memory_bytes)}，进程与线程 {snapshot.usage.measured_processes_and_threads ?? "未知"}{snapshot.usage.sampled_at ? `，采样于 ${new Date(snapshot.usage.sampled_at).toLocaleTimeString()}` : ""}。{snapshot.usage.complete === false ? "部分容器指标不可用，合计不完整。" : ""}内存不含空闲文件缓存。</p>
                : <p>{snapshot.usage?.error ? `资源采样不可用：${snapshot.usage.error}` : "这个节点尚未提供容器资源采样。"}</p>}
              {snapshot.components ? <p>浏览器按每个任务约 {gib(snapshot.components.estimated_browser_bytes)} 预留，同一任务的多个 Worker 只计一份{snapshot.components.browser_runs > 0 ? `；当前 ${snapshot.components.browser_runs} 个任务共预留 ${gib(snapshot.components.reserved_browser_bytes)}` : ""}。</p> : null}
              {snapshot.physical_runtimes?.length ? (
                <ul className="flex flex-col gap-2">
                  {snapshot.physical_runtimes.map((runtime) => (
                    <li key={runtime.container_id} className="rounded-lg border border-cx-border-subtle bg-cx-bg px-3 py-2">
                      <p className="text-cx-fg-2">{runtime.attribution === "shared_container" ? "共享容器" : "独立容器"} · {runtime.runs.map((run) => run.run_id).join("、")}</p>
                      <p>
                        {runtime.measurement_available && runtime.measurement ? `实际内存 ${mib(runtime.measurement.memory_working_set_bytes)}，进程与线程 ${runtime.measurement.processes_and_threads ?? "未知"}，CPU ${runtime.measurement.cpu_percent?.toFixed(1) ?? "未知"}%（100% = 1 核）。` : "实际用量暂不可用。"}
                        {runtime.configured_limits ? ` 上限：内存 ${runtime.configured_limits.memory}，${runtime.configured_limits.cpus} 核，进程与线程 ${runtime.configured_limits.pids_limit}。` : ""}
                        {runtime.attribution === "shared_container" ? " 共享容器的用量是合计值。" : ""}
                      </p>
                    </li>
                  ))}
                </ul>
              ) : <p>当前没有登记中的任务容器。</p>}
            </div>
          </details>
          <details className="group border-t border-cx-border-subtle" open={snapshot.dispatches.some((row) => row.status === "queued") || undefined}>
            <summary className="flex cursor-pointer list-none items-center justify-between px-5 py-3 text-cx-sm text-cx-fg-2 hover:bg-cx-hover">
              <span>Worker 队列（{snapshot.dispatches.length}）</span>
              <span className="text-cx-fg-4 group-open:hidden">展开</span>
            </summary>
            {snapshot.dispatches.length ? (
              <div className="flex flex-col gap-1 px-5 pb-4">
                <p className="pb-1 text-cx-xs leading-5 text-cx-fg-4">有容量时先调度高优先级；同等优先级在任务间轮转，等待越久优先级越高。</p>
                {snapshot.dispatches.map((row) => {
                  const status = STATUS[row.status] ?? { label: row.status, tone: "neutral" as const };
                  return (
                    <div key={row.id} className="flex flex-wrap items-center gap-2 rounded-lg border border-cx-border-subtle bg-cx-bg px-3 py-2 text-cx-sm">
                      <Badge tone={status.tone} dot>{status.label}</Badge>
                      <span className="min-w-0 flex-1 truncate font-cx-mono text-cx-xs text-cx-fg-2" title={`${row.run_id} / ${row.worker_id}`}>{row.run_id} / {row.worker_id}</span>
                      {row.waiting?.reason === "component_memory_estimate" ? <span className="text-cx-xs text-cx-fg-3">含浏览器需 {gib(row.waiting.required_bytes)}，预算 {gib(row.waiting.budget_bytes)}</span> : null}
                      {row.waiting?.reason === "fair_queue_turn" ? <span className="text-cx-xs text-cx-fg-3">等待同优先级轮转</span> : null}
                      {row.status === "queued" ? (
                        <Select<string>
                          ariaLabel={`${row.worker_id} 的队列优先级`}
                          size="sm"
                          className="w-28"
                          disabled={busy}
                          value={String(row.priority ?? 0)}
                          options={PRIORITIES.some((item) => item.value === String(row.priority ?? 0)) ? PRIORITIES : [...PRIORITIES, { value: String(row.priority), label: `自定义 ${row.priority}` }]}
                          onChange={(next) => void savePriority(row.id, Number(next))}
                        />
                      ) : null}
                    </div>
                  );
                })}
              </div>
            ) : <p className="px-5 pb-4 text-cx-sm text-cx-fg-3">当前没有运行中或排队的 Worker。</p>}
          </details>
        </>
      ) : null}
    </SettingsSection>
  );
}
