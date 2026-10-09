"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import { Button, Modal } from "@heroui/react";
import { Icon } from "@/components/Icon";
import { apiFetch } from "@/lib/useRun";

type Alert = { code: string; severity: string; detail: string };
type OutboxSummary = {
  by_state?: Record<string, number>;
  oldest_pending_seconds?: number;
};
type Metrics = {
  uptime_seconds?: number;
  platform?: {
    event_watermark?: number;
    pending_receipts?: number;
    projection_lag_max?: number;
    outbox?: OutboxSummary;
    effects?: Record<string, number>;
    effect_duration_avg_seconds?: Record<string, number>;
    orphan_effects?: number;
  };
  competition?: {
    pending_receipts?: number;
    outbox?: OutboxSummary;
    submissions?: Record<string, number>;
    sync_cursors?: number;
    sync_errors?: number;
  };
  runtime?: {
    registered_instances?: number;
    sessions_total?: number;
    sessions_active?: number;
    closed_duration_avg_seconds?: number;
    start_duration_avg_seconds?: number;
    events?: Record<string, number>;
  };
  sse?: Record<string, number>;
  extensions?: {
    installed?: number;
    enabled?: number;
    start_count?: number;
    degraded?: number;
  };
};
type Overview = {
  generated_at: string;
  instance: { id: string; pid: number; backend_url: string; control_port?: number };
  health: { status: string; ready: boolean };
  metrics: Metrics;
  alerts: Alert[];
};
type OutboxRow = {
  domain?: string;
  outbox_id?: string;
  destination?: string;
  status?: string;
  attempts?: number;
  command_id?: string;
  created_at?: string;
  updated_at?: string;
  last_error?: string | null;
};
type RecoveryStep = {
  name?: string;
  state?: string;
  core?: boolean;
  detail?: string;
  impact?: string;
  evidence?: Record<string, unknown>;
};
type Recovery = {
  dry_run?: boolean;
  writes_performed?: boolean;
  platform?: {
    event_watermark?: number;
    stream_gap_count?: number;
    pending_receipts?: number;
    pending_outbox?: number;
    projection_watermarks?: Record<string, number>;
  };
  competition?: {
    event_watermark?: number;
    pending_receipts?: number;
    pending_outbox?: number;
  };
  last_startup_recovery?: {
    started_at?: string;
    completed_at?: string | null;
    ready?: boolean;
    state?: string;
    steps?: RecoveryStep[];
  };
};
type Maintenance = {
  dry_run: boolean;
  writes_performed: boolean;
  count: number;
  candidates: Array<{
    candidate_id: string;
    kind: string;
    target: string;
    age_seconds?: number;
    expires_at?: string;
  }>;
};
type ModuleState = "registered" | "ready" | "unavailable" | "disabled";
type DomainModule = {
  descriptor: { id: string; version?: string; workspace_kind?: string };
  state: ModuleState;
  origin?: string;
  error?: { message?: string; code?: string } | null;
  pending_components?: string[];
};
type ReceiptTrace = {
  domain?: string;
  receipt?: Record<string, unknown>;
  effects?: Array<Record<string, unknown>>;
  outbox?: Array<Record<string, unknown>>;
};
type Feedback = { kind: "ok" | "bad"; text: string } | null;

const KIND_LABELS: Record<string, string> = {
  temporary_file: "临时文件",
  orphan_extension_package: "未引用扩展包",
  expired_browser_session: "已过期浏览器会话",
};

const WORKSPACE_LABELS: Record<string, string> = {
  conversation: "对话",
  "single-security-task": "单题",
  competition: "比赛",
};

async function jsonRequest(path: string, init?: RequestInit) {
  const response = await apiFetch(path, init);
  const body = await response.json().catch(() => ({}));
  if (!response.ok) {
    const message = (body as { error?: { message?: string } }).error?.message;
    throw new Error(message || `请求失败（HTTP ${response.status}）`);
  }
  return body;
}

function asRecord(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : {};
}

function asString(value: unknown): string {
  return typeof value === "string" ? value : value == null ? "" : String(value);
}

function asNumber(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? value : 0;
}

function formatTime(value?: string | null): string {
  if (!value) return "—";
  const time = Date.parse(value);
  return Number.isNaN(time)
    ? value
    : new Date(time).toLocaleString("zh-CN", { hour12: false });
}

function formatClock(value?: string | null): string {
  if (!value) return "—";
  const time = Date.parse(value);
  return Number.isNaN(time)
    ? value
    : new Date(time).toLocaleTimeString("zh-CN", { hour12: false });
}

function formatDuration(seconds?: number): string {
  const value = asNumber(seconds);
  if (value <= 0) return "0 秒";
  if (value < 60) return `${Math.round(value)} 秒`;
  if (value < 3600) return `${Math.round(value / 60)} 分钟`;
  if (value < 86400) return `${(value / 3600).toFixed(1)} 小时`;
  return `${(value / 86400).toFixed(1)} 天`;
}

function display(value: unknown): string {
  if (value == null || value === "") return "—";
  if (typeof value === "boolean") return value ? "是" : "否";
  if (typeof value === "number") return Number.isFinite(value) ? String(value) : "—";
  if (typeof value === "string") return value;
  return JSON.stringify(value);
}

function pairs(value: unknown): Array<[string, string]> {
  return Object.entries(asRecord(value)).map(([key, item]) => [key, display(item)]);
}

function pendingOutboxCount(summary?: OutboxSummary): number {
  const states = summary?.by_state || {};
  return Object.entries(states).reduce((total, [state, count]) => (
    state === "delivered" ? total : total + asNumber(count)
  ), 0);
}

function moduleStateLabel(state: ModuleState): string {
  switch (state) {
    case "ready":
      return "已启用";
    case "registered":
      return "已注册";
    case "disabled":
      return "已停用";
    case "unavailable":
      return "不可用";
    default: {
      const _exhaustive: never = state;
      return _exhaustive;
    }
  }
}

function healthTone(ready: boolean, status: string): "ok" | "warn" | "bad" {
  if (ready && status === "ready") return "ok";
  if (ready) return "warn";
  return "bad";
}

function alertTone(severity: string): "critical" | "warning" | "info" {
  if (severity === "critical") return "critical";
  if (severity === "warning") return "warning";
  return "info";
}

function recoveryTone(state?: string): "ok" | "warn" | "bad" {
  if (state === "ready") return "ok";
  if (state === "degraded") return "warn";
  return "bad";
}

function outboxTone(status?: string): "ok" | "warn" | "bad" | "muted" {
  if (status === "delivered") return "ok";
  if (status === "dead_letter" || status === "failed") return "bad";
  if (status === "pending" || status === "processing") return "warn";
  return "muted";
}

function receiptTone(state?: string): "ok" | "warn" | "bad" | "muted" {
  if (state === "completed") return "ok";
  if (state === "failed" || state === "conflict" || state === "cancelled") return "bad";
  if (state === "accepted" || state === "running" || state === "waiting") return "warn";
  return "muted";
}

function StatePill({
  tone,
  children,
}: {
  tone: "ok" | "warn" | "bad" | "muted";
  children: string;
}) {
  return <span className={`operations-pill ${tone}`}><i />{children}</span>;
}

function StatGrid({ items }: { items: Array<{ label: string; value: string }> }) {
  return (
    <dl className="operations-stats">
      {items.map((item) => (
        <div key={item.label}>
          <dt>{item.label}</dt>
          <dd>{item.value}</dd>
        </div>
      ))}
    </dl>
  );
}

function CountChips({ value }: { value: unknown }) {
  const items = pairs(value);
  if (!items.length) return <span className="operations-muted">无</span>;
  return (
    <div className="operations-chips">
      {items.map(([key, item]) => (
        <span key={key}><code>{key}</code>{item}</span>
      ))}
    </div>
  );
}

function FieldTable({ rows }: { rows: Array<{ label: string; value: unknown }> }) {
  const visible = rows.filter((row) => row.value != null && row.value !== "");
  if (!visible.length) return <p className="operations-empty-copy">没有可展示的字段。</p>;
  return (
    <dl className="operations-fields">
      {visible.map((row) => (
        <div key={row.label}>
          <dt>{row.label}</dt>
          <dd>{display(row.value)}</dd>
        </div>
      ))}
    </dl>
  );
}

export function OperationsSettings() {
  const [overview, setOverview] = useState<Overview | null>(null);
  const [outbox, setOutbox] = useState<OutboxRow[]>([]);
  const [recovery, setRecovery] = useState<Recovery | null>(null);
  const [maintenance, setMaintenance] = useState<Maintenance | null>(null);
  const [modules, setModules] = useState<DomainModule[]>([]);
  const [receiptId, setReceiptId] = useState("");
  const [receipt, setReceipt] = useState<ReceiptTrace | null>(null);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [moduleBusy, setModuleBusy] = useState("");
  const [feedback, setFeedback] = useState<Feedback>(null);
  const [maintenanceConfirmOpen, setMaintenanceConfirmOpen] = useState(false);
  const [moduleToDisable, setModuleToDisable] = useState<DomainModule | null>(null);

  const load = useCallback(async () => {
    try {
      const [overviewBody, outboxBody, recoveryBody, maintenanceBody, modulesBody] = await Promise.all([
        jsonRequest("/api/operations/overview"),
        jsonRequest("/api/operations/outbox"),
        jsonRequest("/api/operations/recovery-preview"),
        jsonRequest("/api/operations/maintenance-preview"),
        jsonRequest("/api/domain-modules"),
      ]);
      setOverview(overviewBody as Overview);
      setOutbox(Array.isArray((outboxBody as { items?: OutboxRow[] }).items)
        ? (outboxBody as { items: OutboxRow[] }).items
        : []);
      setRecovery(recoveryBody as Recovery);
      setMaintenance(maintenanceBody as Maintenance);
      setModules(Array.isArray(modulesBody) ? modulesBody as DomainModule[] : []);
      setFeedback((current) => (current?.kind === "bad" ? null : current));
    } catch (exc) {
      setFeedback({ kind: "bad", text: exc instanceof Error ? exc.message : String(exc) });
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
    const timer = window.setInterval(() => void load(), 15_000);
    return () => window.clearInterval(timer);
  }, [load]);

  const lookupReceipt = async () => {
    if (!receiptId.trim()) return;
    setBusy(true);
    setFeedback(null);
    try {
      setReceipt(await jsonRequest(`/api/operations/receipts/${encodeURIComponent(receiptId.trim())}`) as ReceiptTrace);
    } catch (exc) {
      setReceipt(null);
      setFeedback({ kind: "bad", text: exc instanceof Error ? exc.message : String(exc) });
    } finally {
      setBusy(false);
    }
  };

  const downloadBundle = async () => {
    setBusy(true);
    setFeedback(null);
    try {
      const response = await apiFetch("/api/operations/diagnostic-bundle");
      if (!response.ok) throw new Error(`诊断包生成失败（HTTP ${response.status}）`);
      const url = URL.createObjectURL(await response.blob());
      const anchor = document.createElement("a");
      anchor.href = url;
      anchor.download = "muteki-diagnostic.zip";
      anchor.click();
      URL.revokeObjectURL(url);
      setFeedback({ kind: "ok", text: "诊断包已生成；内容已经过凭据、答案和本机路径清理。" });
    } catch (exc) {
      setFeedback({ kind: "bad", text: exc instanceof Error ? exc.message : String(exc) });
    } finally {
      setBusy(false);
    }
  };

  const applyMaintenance = async () => {
    const ids = maintenance?.candidates.map((item) => item.candidate_id) || [];
    if (!ids.length) return;
    setBusy(true);
    setFeedback(null);
    try {
      const body = await jsonRequest("/api/operations/maintenance", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ confirm: true, candidate_ids: ids }),
      }) as { applied?: number; audit?: string };
      setFeedback({
        kind: "ok",
        text: `维护完成：已处理 ${body.applied || 0} 项，审计记录 ${body.audit || "已写入"}。`,
      });
      await load();
    } catch (exc) {
      setFeedback({ kind: "bad", text: exc instanceof Error ? exc.message : String(exc) });
    } finally {
      setBusy(false);
      setMaintenanceConfirmOpen(false);
    }
  };

  const changeModule = async (item: DomainModule, action: "enable" | "disable") => {
    const moduleId = item.descriptor.id;
    setModuleBusy(moduleId);
    setFeedback(null);
    try {
      const body = await jsonRequest(`/api/domain-modules/${encodeURIComponent(moduleId)}/${action}`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({}),
      }) as { receipt?: { command_id?: string } };
      const commandId = String(body.receipt?.command_id || "");
      setFeedback({
        kind: "ok",
        text: `${moduleId} 已${action === "enable" ? "启用" : "停用"}${commandId ? ` · 回执 ${commandId}` : ""}`,
      });
      await load();
    } catch (exc) {
      setFeedback({ kind: "bad", text: exc instanceof Error ? exc.message : String(exc) });
    } finally {
      setModuleBusy("");
      setModuleToDisable(null);
    }
  };

  const metrics = overview?.metrics || {};
  const pendingReceipts = asNumber(metrics.platform?.pending_receipts) + asNumber(metrics.competition?.pending_receipts);
  const pendingOutbox = pendingOutboxCount(metrics.platform?.outbox) + pendingOutboxCount(metrics.competition?.outbox);
  const readyModules = modules.filter((item) => item.state === "ready").length;
  const criticalAlerts = overview?.alerts.filter((item) => item.severity === "critical").length || 0;
  const health = overview ? healthTone(overview.health.ready, overview.health.status) : "muted";

  const recoverySteps = recovery?.last_startup_recovery?.steps || [];
  const receiptRecord = asRecord(receipt?.receipt);
  const receiptEffects = receipt?.effects || [];
  const receiptOutbox = receipt?.outbox || [];

  const summaryItems = useMemo(() => {
    if (!overview) return [];
    return [
      {
        label: "健康",
        value: overview.health.ready ? "可写" : "不可写",
        note: overview.health.status,
        tone: health,
      },
      {
        label: "告警",
        value: String(overview.alerts.length),
        note: criticalAlerts ? `${criticalAlerts} 条 critical` : "未达阈值或仅为 warning",
        tone: criticalAlerts ? "bad" as const : overview.alerts.length ? "warn" as const : "ok" as const,
      },
      {
        label: "未完成回执",
        value: String(pendingReceipts),
        note: `platform ${asNumber(metrics.platform?.pending_receipts)} · competition ${asNumber(metrics.competition?.pending_receipts)}`,
        tone: pendingReceipts ? "warn" as const : "ok" as const,
      },
      {
        label: "未完成 outbox",
        value: String(pendingOutbox),
        note: `记录 ${outbox.length} 条`,
        tone: pendingOutbox ? "warn" as const : "ok" as const,
      },
      {
        label: "领域模块",
        value: `${readyModules}/${modules.length}`,
        note: "ready / 全部",
        tone: readyModules === modules.length && modules.length ? "ok" as const : "warn" as const,
      },
      {
        label: "维护清理",
        value: String(maintenance?.count || 0),
        note: maintenance?.count ? "有可清理项" : "当前没有可清理项",
        tone: maintenance?.count ? "warn" as const : "muted" as const,
      },
    ];
  }, [criticalAlerts, health, maintenance?.count, metrics.competition?.pending_receipts, metrics.platform?.pending_receipts, modules.length, outbox.length, overview, pendingOutbox, pendingReceipts, readyModules]);

  if (loading && !overview) {
    return (
      <main className="operations-settings">
        <div className="operations-loading" aria-busy="true">
          <Icon name="refresh" size={18} />
          <strong>正在读取服务、回执、outbox 和恢复状态…</strong>
        </div>
      </main>
    );
  }

  return (
    <main className="operations-settings">
      <header className="operations-settings-toolbar">
        <span className="operations-updated">
          <Icon name="clock" size={13} />
          {overview ? `更新 ${formatClock(overview.generated_at)}` : "尚未读取"}
          {overview ? ` · 运行 ${formatDuration(metrics.uptime_seconds)}` : ""}
        </span>
        <div>
          <Button type="button" isDisabled={busy} onClick={() => void load()}>
            <Icon name="refresh" size={14} />
            刷新
          </Button>
          <Button type="button" isDisabled={busy} onClick={() => void downloadBundle()}>
            <Icon name="download" size={14} />
            下载诊断包
          </Button>
        </div>
      </header>

      {feedback ? (
        <div className={`operations-feedback ${feedback.kind === "ok" ? "success" : "error"}`} role={feedback.kind === "ok" ? "status" : "alert"}>
          <Icon name={feedback.kind === "ok" ? "check" : "alert"} size={15} />
          {feedback.text}
        </div>
      ) : null}

      {overview ? (
        <section className="operations-summary" aria-label="运维摘要">
          {summaryItems.map((item) => (
            <div key={item.label}>
              <span>{item.label}</span>
              <strong className={item.tone}>{item.value}</strong>
              <small>{item.note}</small>
            </div>
          ))}
        </section>
      ) : null}

      {overview ? (
        <section className="operations-instance" aria-label="实例">
          <div>
            <span>实例</span>
            <strong>{overview.instance.id}</strong>
          </div>
          <div>
            <span>PID</span>
            <strong>{overview.instance.pid}</strong>
          </div>
          <div>
            <span>控制端口</span>
            <strong>{overview.instance.control_port || "不可用"}</strong>
          </div>
          <div>
            <span>后端</span>
            <strong>{overview.instance.backend_url}</strong>
          </div>
        </section>
      ) : null}

      <div className="operations-split">
        <section className="operations-section" aria-labelledby="operations-alerts-heading">
          <header>
            <div>
              <h2 id="operations-alerts-heading">告警</h2>
            </div>
            <p>来自 overview 阈值检查，包含 outbox、回执、投影、扩展和孤立效果。</p>
          </header>
          <div className="operations-section-body">
            {overview?.alerts.length ? (
              <div className="operations-alert-list">
                {overview.alerts.map((item) => (
                  <article key={item.code} className={`operations-alert ${alertTone(item.severity)}`}>
                    <StatePill tone={item.severity === "critical" ? "bad" : "warn"}>{item.severity}</StatePill>
                    <div>
                      <strong>{item.code}</strong>
                      <span>{item.detail}</span>
                    </div>
                  </article>
                ))}
              </div>
            ) : (
              <div className="operations-empty">
                <strong>当前没有达到阈值的告警。</strong>
                <span>outbox 延迟、未完成回执、投影滞后、扩展降级和孤立效果会在这里出现。</span>
              </div>
            )}
          </div>
        </section>

        <section className="operations-section" aria-labelledby="operations-receipt-heading">
          <header>
            <div>
              <h2 id="operations-receipt-heading">回执</h2>
            </div>
            <p>按全局 command_id 查询 acceptance receipt、effect receipt 和 outbox 状态。</p>
          </header>
          <div className="operations-section-body">
            <div className="operations-receipt-form">
              <label htmlFor="operations-receipt">
                <span>全局 command_id</span>
                <input
                  id="operations-receipt"
                  value={receiptId}
                  onChange={(event) => setReceiptId(event.target.value)}
                  onKeyDown={(event) => {
                    if (event.key === "Enter") void lookupReceipt();
                  }}
                  placeholder="command_id"
                  autoComplete="off"
                />
              </label>
              <Button type="button" isDisabled={busy || !receiptId.trim()} onClick={() => void lookupReceipt()}>
                <Icon name="search" size={14} />
                查询
              </Button>
            </div>
            {receipt ? (
              <div className="operations-receipt-result">
                <div className="operations-receipt-head">
                  <StatePill tone={receiptTone(asString(receiptRecord.state))}>{asString(receiptRecord.state) || "未知"}</StatePill>
                  <strong>{asString(receiptRecord.command_id) || receiptId.trim()}</strong>
                  <span>{receipt.domain || "未分域"}</span>
                </div>
                <FieldTable rows={[
                  { label: "receipt_id", value: receiptRecord.receipt_id },
                  { label: "state", value: receiptRecord.state },
                  { label: "run_id", value: receiptRecord.run_id },
                  { label: "submitted_at", value: receiptRecord.submitted_at },
                  { label: "deduplicated", value: receiptRecord.deduplicated },
                  { label: "event_cursor", value: receiptRecord.event_cursor },
                  { label: "error", value: asRecord(receiptRecord.error).message },
                ]} />
                {receiptEffects.length ? (
                  <div className="operations-sublist">
                    <h3>effect receipt · {receiptEffects.length}</h3>
                    {receiptEffects.map((item, index) => {
                      const row = asRecord(item);
                      return (
                        <article key={asString(row.effect_id) || `${asString(row.destination)}-${index}`}>
                          <header>
                            <strong>{asString(row.destination) || asString(row.effect_id) || `effect ${index + 1}`}</strong>
                            <StatePill tone={receiptTone(asString(row.state))}>{asString(row.state) || "—"}</StatePill>
                          </header>
                          <span>{asString(row.effect_id)} · 尝试 {display(row.attempts)}</span>
                        </article>
                      );
                    })}
                  </div>
                ) : <p className="operations-empty-copy">没有 effect receipt。</p>}
                {receiptOutbox.length ? (
                  <div className="operations-sublist">
                    <h3>outbox · {receiptOutbox.length}</h3>
                    {receiptOutbox.map((item, index) => {
                      const row = asRecord(item);
                      return (
                        <article key={asString(row.outbox_id) || `${asString(row.destination)}-${index}`}>
                          <header>
                            <strong>{asString(row.destination) || asString(row.outbox_id) || `outbox ${index + 1}`}</strong>
                            <StatePill tone={outboxTone(asString(row.status))}>{asString(row.status) || "—"}</StatePill>
                          </header>
                          <span>{asString(row.last_error) || `尝试 ${display(row.attempts)}`}</span>
                        </article>
                      );
                    })}
                  </div>
                ) : <p className="operations-empty-copy">没有 outbox 记录。</p>}
              </div>
            ) : (
              <div className="operations-empty compact">
                <strong>尚未查询回执</strong>
                <span>查询结果同时显示 acceptance receipt、effect receipt 和 outbox 状态。</span>
              </div>
            )}
          </div>
        </section>
      </div>

      <section className="operations-section" aria-labelledby="operations-metrics-heading">
        <header>
          <div>
            <h2 id="operations-metrics-heading">指标</h2>
          </div>
          <p>来自 overview.metrics：platform、competition、runtime、SSE 与扩展。</p>
        </header>
        <div className="operations-section-body">
          <div className="operations-metric-grid">
            <article>
              <h3>platform</h3>
              <StatGrid items={[
                { label: "event_watermark", value: display(metrics.platform?.event_watermark) },
                { label: "pending_receipts", value: display(metrics.platform?.pending_receipts) },
                { label: "projection_lag_max", value: display(metrics.platform?.projection_lag_max) },
                { label: "orphan_effects", value: display(metrics.platform?.orphan_effects) },
                { label: "oldest_pending", value: formatDuration(metrics.platform?.outbox?.oldest_pending_seconds) },
              ]} />
              <h4>outbox</h4>
              <CountChips value={metrics.platform?.outbox?.by_state} />
              <h4>effects</h4>
              <CountChips value={metrics.platform?.effects} />
              <h4>effect_duration_avg_seconds</h4>
              <CountChips value={metrics.platform?.effect_duration_avg_seconds} />
            </article>
            <article>
              <h3>competition</h3>
              <StatGrid items={[
                { label: "pending_receipts", value: display(metrics.competition?.pending_receipts) },
                { label: "sync_cursors", value: display(metrics.competition?.sync_cursors) },
                { label: "sync_errors", value: display(metrics.competition?.sync_errors) },
                { label: "oldest_pending", value: formatDuration(metrics.competition?.outbox?.oldest_pending_seconds) },
              ]} />
              <h4>outbox</h4>
              <CountChips value={metrics.competition?.outbox?.by_state} />
              <h4>submissions</h4>
              <CountChips value={metrics.competition?.submissions} />
            </article>
            <article>
              <h3>runtime</h3>
              <StatGrid items={[
                { label: "registered_instances", value: display(metrics.runtime?.registered_instances) },
                { label: "sessions_total", value: display(metrics.runtime?.sessions_total) },
                { label: "sessions_active", value: display(metrics.runtime?.sessions_active) },
                { label: "closed_duration_avg", value: formatDuration(metrics.runtime?.closed_duration_avg_seconds) },
                { label: "start_duration_avg", value: formatDuration(metrics.runtime?.start_duration_avg_seconds) },
              ]} />
              <h4>events</h4>
              <CountChips value={metrics.runtime?.events} />
            </article>
            <article>
              <h3>SSE / 扩展</h3>
              <StatGrid items={[
                { label: "extensions.installed", value: display(metrics.extensions?.installed) },
                { label: "extensions.enabled", value: display(metrics.extensions?.enabled) },
                { label: "extensions.start_count", value: display(metrics.extensions?.start_count) },
                { label: "extensions.degraded", value: display(metrics.extensions?.degraded) },
              ]} />
              <h4>sse</h4>
              <CountChips value={metrics.sse} />
            </article>
          </div>
        </div>
      </section>

      <section className="operations-section" aria-labelledby="operations-recovery-heading">
        <header>
          <div>
            <h2 id="operations-recovery-heading">恢复预览</h2>
          </div>
          <p>
            dry-run {recovery?.dry_run ? "开启" : "关闭"}
            {recovery?.writes_performed ? " · 已写入" : " · 未写入"}
          </p>
        </header>
        <div className="operations-section-body">
          <div className="operations-metric-grid recovery">
            <article>
              <h3>platform</h3>
              <StatGrid items={[
                { label: "event_watermark", value: display(recovery?.platform?.event_watermark) },
                { label: "stream_gap_count", value: display(recovery?.platform?.stream_gap_count) },
                { label: "pending_receipts", value: display(recovery?.platform?.pending_receipts) },
                { label: "pending_outbox", value: display(recovery?.platform?.pending_outbox) },
              ]} />
              <h4>projection_watermarks</h4>
              <CountChips value={recovery?.platform?.projection_watermarks} />
            </article>
            <article>
              <h3>competition</h3>
              <StatGrid items={[
                { label: "event_watermark", value: display(recovery?.competition?.event_watermark) },
                { label: "pending_receipts", value: display(recovery?.competition?.pending_receipts) },
                { label: "pending_outbox", value: display(recovery?.competition?.pending_outbox) },
              ]} />
            </article>
          </div>
          <div className="operations-recovery-steps">
            <div className="operations-recovery-head">
              <h3>last_startup_recovery</h3>
              <StatePill tone={recoveryTone(recovery?.last_startup_recovery?.state)}>
                {recovery?.last_startup_recovery?.state || "未记录"}
              </StatePill>
              <span>
                {formatTime(recovery?.last_startup_recovery?.started_at)}
                {recovery?.last_startup_recovery?.completed_at ? ` → ${formatTime(recovery.last_startup_recovery.completed_at)}` : ""}
              </span>
            </div>
            {recoverySteps.length ? recoverySteps.map((step) => (
              <article key={step.name || JSON.stringify(step)}>
                <header>
                  <strong>{step.name || "未命名步骤"}</strong>
                  <StatePill tone={recoveryTone(step.state)}>{step.state || "—"}</StatePill>
                  {step.core ? <em>core</em> : null}
                </header>
                {step.detail ? <p>{step.detail}</p> : null}
                {step.impact ? <small>{step.impact}</small> : null}
              </article>
            )) : <p className="operations-empty-copy">没有启动恢复步骤。</p>}
          </div>
        </div>
      </section>

      <section className="operations-section" aria-labelledby="operations-outbox-heading">
        <header>
          <div>
            <h2 id="operations-outbox-heading">Outbox</h2>
          </div>
          <p>platform 与 competition 的外部效果队列，最多 200 条。</p>
        </header>
        <div className="operations-section-body">
          {outbox.length ? (
            <div className="operations-table-wrap" tabIndex={0} aria-label="Outbox 状态表，可横向滚动">
              <table>
                <thead>
                  <tr>
                    <th>领域</th>
                    <th>目标</th>
                    <th>状态</th>
                    <th>尝试</th>
                    <th>command_id</th>
                    <th>更新时间</th>
                    <th>最近错误</th>
                  </tr>
                </thead>
                <tbody>
                  {outbox.map((item) => (
                    <tr key={`${item.domain || ""}:${item.outbox_id || item.command_id || ""}`}>
                      <td>{item.domain || "—"}</td>
                      <td>{item.destination || "—"}</td>
                      <td><StatePill tone={outboxTone(item.status)}>{item.status || "—"}</StatePill></td>
                      <td>{display(item.attempts)}</td>
                      <td><code>{item.command_id || "—"}</code></td>
                      <td>{formatTime(item.updated_at)}</td>
                      <td>{item.last_error || "—"}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          ) : (
            <div className="operations-empty compact">
              <strong>当前没有 outbox 记录。</strong>
              <span>未完成或已投递的外部效果会按创建时间倒序出现在这里。</span>
            </div>
          )}
        </div>
      </section>

      <section className="operations-section" aria-labelledby="operations-modules-heading">
        <header>
          <div>
            <h2 id="operations-modules-heading">领域模块</h2>
          </div>
          <p>停用后对应入口会从工作台隐藏，历史对象和回执仍可读取。恢复时沿用原有配置。</p>
        </header>
        <div className="operations-section-body">
          {modules.length ? (
            <div className="operations-module-list">
              {modules.map((item) => {
                const disabled = item.state === "disabled";
                const unavailable = item.state === "unavailable";
                const title = WORKSPACE_LABELS[item.descriptor.workspace_kind || ""]
                  || item.descriptor.workspace_kind
                  || item.descriptor.id;
                return (
                  <article key={item.descriptor.id} className="operations-module-row">
                    <div>
                      <strong>{title}</strong>
                      <small>
                        {item.descriptor.id}
                        {` · v${item.descriptor.version || "—"}`}
                        {` · ${item.origin || "builtin"}`}
                      </small>
                      {item.error?.message ? <em>{item.error.message}</em> : null}
                      {item.pending_components?.length ? (
                        <em>未就绪组件：{item.pending_components.join("、")}</em>
                      ) : null}
                    </div>
                    <span className={`operations-module-state ${item.state}`}>{moduleStateLabel(item.state)}</span>
                    <Button
                      type="button"
                      className={disabled ? "primary" : ""}
                      isDisabled={Boolean(moduleBusy) || unavailable}
                      onClick={() => (disabled ? void changeModule(item, "enable") : setModuleToDisable(item))}
                    >
                      {moduleBusy === item.descriptor.id ? "处理中…" : disabled ? "启用" : "停用"}
                    </Button>
                  </article>
                );
              })}
            </div>
          ) : (
            <div className="operations-empty compact">
              <strong>当前没有领域模块。</strong>
              <span>注册表返回空列表时，工作台入口也不会出现。</span>
            </div>
          )}
        </div>
      </section>

      <section className="operations-section operations-maintenance" aria-labelledby="operations-maintenance-heading">
        <header>
          <div>
            <h2 id="operations-maintenance-heading">维护清理</h2>
          </div>
          <p>范围仅包含超过一小时的临时文件、未被安装记录引用的扩展包和已过期浏览器会话。</p>
        </header>
        <div className="operations-section-body">
          {maintenance?.candidates.length ? (
            <ul className="operations-candidate-list">
              {maintenance.candidates.map((item) => (
                <li key={item.candidate_id}>
                  <strong>{KIND_LABELS[item.kind] || item.kind}</strong>
                  <code>{item.target}</code>
                  <span>
                    {item.age_seconds ? formatDuration(item.age_seconds) : ""}
                    {item.expires_at ? `到期 ${formatTime(item.expires_at)}` : ""}
                  </span>
                </li>
              ))}
            </ul>
          ) : (
            <div className="operations-empty compact">
              <strong>当前没有可清理项。</strong>
              <span>预览为 dry-run，只有确认后才会写入并留下审计记录。</span>
            </div>
          )}
          <footer>
            <Button
              type="button"
              className="danger"
              isDisabled={busy || !maintenance?.candidates.length}
              onClick={() => setMaintenanceConfirmOpen(true)}
            >
              <Icon name="trash" size={14} />
              确认并执行预览中的清理
            </Button>
          </footer>
        </div>
      </section>

      <Modal isOpen={maintenanceConfirmOpen} onOpenChange={setMaintenanceConfirmOpen}>
        <Modal.Backdrop isDismissable={!busy}><Modal.Container><Modal.Dialog className="cx-settings-dialog">
          <Modal.Header className="flex flex-col gap-1"><Modal.Heading>执行维护清理</Modal.Heading><small className="font-normal text-muted">只处理当前预览中的候选目标，并为每项操作写入审计记录。</small></Modal.Header>
          <Modal.Body>
            <div className="action-dialog-summary"><strong>{maintenance?.candidates.length || 0} 项待处理</strong><span>临时文件、未引用扩展包和已过期浏览器会话。</span></div>
            {maintenance?.candidates.length ? <div className="action-dialog-options">{maintenance.candidates.slice(0, 6).map((item) => <div key={item.candidate_id} className="action-dialog-summary"><strong>{KIND_LABELS[item.kind] || item.kind}</strong><span>{item.target}</span></div>)}</div> : null}
            {(maintenance?.candidates.length || 0) > 6 ? <p>另有 {(maintenance?.candidates.length || 0) - 6} 项，将按当前预览一并处理。</p> : null}
          </Modal.Body>
          <Modal.Footer><Button variant="ghost" onPress={() => setMaintenanceConfirmOpen(false)} isDisabled={busy}>取消</Button><Button variant="danger" isPending={busy} isDisabled={!maintenance?.candidates.length} onPress={() => void applyMaintenance()}>清理 {maintenance?.candidates.length || 0} 项</Button></Modal.Footer>
        </Modal.Dialog></Modal.Container></Modal.Backdrop>
      </Modal>

      <Modal isOpen={Boolean(moduleToDisable)} onOpenChange={(open) => !open && setModuleToDisable(null)}>
        <Modal.Backdrop isDismissable={!moduleBusy}><Modal.Container><Modal.Dialog className="cx-settings-dialog">
          <Modal.Header className="flex flex-col gap-1"><Modal.Heading>停用领域模块</Modal.Heading><small className="font-normal text-muted">对应工作台入口会立即隐藏；已有工作区、历史记录和命令回执继续保留。</small></Modal.Header>
          <Modal.Body><div className="action-dialog-summary"><strong>{moduleToDisable?.descriptor.workspace_kind || moduleToDisable?.descriptor.id}</strong><span>{moduleToDisable?.descriptor.id}</span></div></Modal.Body>
          <Modal.Footer><Button variant="ghost" onPress={() => setModuleToDisable(null)} isDisabled={Boolean(moduleBusy)}>取消</Button><Button variant="danger" isPending={Boolean(moduleBusy)} onPress={() => moduleToDisable && void changeModule(moduleToDisable, "disable")}>确认停用</Button></Modal.Footer>
        </Modal.Dialog></Modal.Container></Modal.Backdrop>
      </Modal>
    </main>
  );
}
