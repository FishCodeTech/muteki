"use client";

import type { ReactNode } from "react";
import { Icon } from "@/components/Icon";
import { srcFetch as apiFetch } from "@/lib/srcFetch";
import styles from "./review.module.css";

export type EvidenceReference = {
  id?: string;
  artifact_id?: string;
  exchange_id?: string;
  label?: string;
  kind?: string;
  sha256?: string;
  source_run_id?: string | null;
  source_event_seq?: number | null;
  complete?: string;
  url?: string;
};

export type OpenEvidence = (reference: EvidenceReference) => void;

export function commandUrl(taskId: string, commandType: string): string {
  return `/api/src/tasks/${encodeURIComponent(taskId)}/commands/${encodeURIComponent(commandType)}`;
}

async function responseError(response: Response, fallback: string): Promise<Error> {
  let message = `${fallback}（HTTP ${response.status}）`;
  try {
    const body = await response.json() as { error?: { message?: string; code?: string } };
    if (body.error?.message) message = body.error.message;
    if (body.error?.code) message = `${message} [${body.error.code}]`;
  } catch {
    // Keep the HTTP status when the server did not return a JSON error envelope.
  }
  return new Error(message);
}

export async function fetchJson<T>(url: string): Promise<T> {
  const response = await apiFetch(url);
  if (!response.ok) throw await responseError(response, "读取失败");
  return await response.json() as T;
}

export async function postCommand<T = Record<string, unknown>>(
  taskId: string,
  commandType: string,
  payload: Record<string, unknown>,
): Promise<T> {
  const response = await apiFetch(commandUrl(taskId, commandType), {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload),
  });
  const body = await response.json() as {
    output?: T;
    error?: { message?: string; code?: string } | null;
  };
  if (!response.ok || body.error) {
    throw new Error(body.error?.message || `操作失败（HTTP ${response.status}）`);
  }
  const output = (body.output === undefined ? body : body.output) as T & { result?: unknown };
  return (output && typeof output === "object" && "result" in output ? output.result : output) as T;
}

export function queryUrl(taskId: string, queryType: string): string {
  return `/api/src/tasks/${encodeURIComponent(taskId)}/queries/${encodeURIComponent(queryType)}`;
}

export async function querySrc<T>(taskId: string, queryType: string, payload: Record<string, unknown> = {}): Promise<T> {
  const response = await apiFetch(queryUrl(taskId, queryType), {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload),
  });
  if (!response.ok) throw await responseError(response, "查询失败");
  return await response.json() as T;
}

export function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}

export function formatDate(value?: string | number | null): string {
  if (value === undefined || value === null || value === "") return "—";
  const date = typeof value === "number" ? new Date(value * 1000) : new Date(value);
  return Number.isNaN(date.getTime()) ? String(value) : date.toLocaleString();
}

export function lines(value: string): string[] {
  return value.split("\n").map(item => item.trim()).filter(Boolean);
}

export function newIdempotencyKey(prefix: string): string {
  const id = typeof crypto !== "undefined" && crypto.randomUUID
    ? crypto.randomUUID()
    : `${Date.now()}-${Math.random().toString(16).slice(2)}`;
  return `${prefix}-${id}`;
}

export function ReviewShell({ title, subtitle, actions, children }: {
  title: string;
  subtitle?: string;
  actions?: ReactNode;
  children: ReactNode;
}) {
  return <section className={styles.shell} aria-label={title}>
    <header className={styles.header}>
      <div className={styles.heading}><h1>{title}</h1>{subtitle ? <p>{subtitle}</p> : null}</div>
      {actions ? <div className={styles.headerActions}>{actions}</div> : null}
    </header>
    <div className={styles.content}>{children}</div>
  </section>;
}

export function ReviewTabs({ items, active, onChange, label }: {
  items: Array<{ id: string; label: string; count?: number }>;
  active: string;
  onChange: (id: string) => void;
  label: string;
}) {
  return <nav className={styles.tabs} aria-label={label}>
    {items.map(item => <button
      type="button" key={item.id} aria-pressed={active === item.id}
      className={active === item.id ? styles.activeTab : ""}
      onClick={() => onChange(item.id)}
    >{item.label}{item.count !== undefined ? <span>{item.count}</span> : null}</button>)}
  </nav>;
}

export function Badge({ children, tone = "neutral" }: {
  children: ReactNode;
  tone?: "neutral" | "accent" | "positive" | "warning" | "danger";
}) {
  return <span className={`pentest-chip pentest-chip--${tone}`}>{children}</span>;
}

export function Notice({ children, tone = "info" }: {
  children: ReactNode;
  tone?: "info" | "warning" | "danger" | "success";
}) {
  const icon = tone === "success" ? "check" : tone === "info" ? "info" : "alert";
  return <p className={`pentest-notice pentest-notice--${tone === "success" ? "info" : tone}`} role={tone === "danger" ? "alert" : "status"}>
    <Icon name={icon} size={14} />{children}
  </p>;
}

export function Loading({ label = "正在读取完整记录…" }: { label?: string }) {
  return <div className={styles.centerState} role="status"><Icon name="loader" size={20} className="pentest-spin" /><p>{label}</p></div>;
}

export function Empty({ children }: { children: ReactNode }) {
  return <p className="pentest-empty">{children}</p>;
}

export function ErrorState({ message, onRetry }: { message: string; onRetry: () => void }) {
  return <div className={styles.centerState} role="alert">
    <Icon name="circleAlert" size={20} /><p>{message}</p>
    <button type="button" className={styles.textButton} onClick={onRetry}><Icon name="refresh" size={14} />重试</button>
  </div>;
}

export function Section({ title, count, children }: { title: string; count?: number; children: ReactNode }) {
  return <section className="pentest-document-section">
    <h2 className="pentest-section-title">{title}{count !== undefined ? <span className="pentest-count">{count}</span> : null}</h2>
    {children}
  </section>;
}

export function EvidenceList({ refs, onOpen, empty = "未引用原件" }: {
  refs?: EvidenceReference[];
  onOpen?: OpenEvidence;
  empty?: string;
}) {
  if (!refs?.length) return <span className={styles.muted}>{empty}</span>;
  return <ul className={styles.evidenceList}>{refs.map((ref, index) => {
    const id = ref.artifact_id || ref.exchange_id || ref.id || `evidence-${index}`;
    const label = ref.label || id;
    return <li key={`${id}-${index}`}>
      {onOpen ? <button type="button" onClick={() => onOpen(ref)} title="打开完整来源">
        <Icon name="arrowUpRight" size={12} />{label}
      </button> : <code>{label}</code>}
      {ref.source_run_id ? <small>{ref.source_run_id}{ref.source_event_seq ? ` #${ref.source_event_seq}` : ""}</small> : null}
      {ref.complete && ref.complete !== "complete" ? <Badge tone="warning">{ref.complete}</Badge> : null}
      {ref.sha256 ? <small title={ref.sha256}>sha256:{ref.sha256.slice(0, 12)}…</small> : null}
    </li>;
  })}</ul>;
}

export function KeyValues({ items }: { items: Array<[string, ReactNode]> }) {
  return <dl className="pentest-report-definition">{items.map(([key, value]) => <div key={key} className={styles.keyValue}><dt>{key}</dt><dd>{value ?? "—"}</dd></div>)}</dl>;
}

export function RawDetails({ value, label = "查看完整结构" }: { value: unknown; label?: string }) {
  return <details className={styles.rawDetails}><summary>{label}<Icon name="chevronDown" size={12} /></summary><pre>{JSON.stringify(value, null, 2)}</pre></details>;
}

export async function downloadReport(taskId: string, commandType: string, payload: Record<string, unknown>): Promise<void> {
  const result = await postCommand<unknown>(taskId, commandType, payload);
  const candidate = Array.isArray(result) ? result[0] : (result as { bytes?: unknown }).bytes ?? result;
  if (!candidate || typeof candidate !== "object") throw new Error("导出结果缺少二进制内容");
  const row = candidate as { encoding?: string; data?: string; base64?: string };
  const encoded = row.encoding === "base64" ? row.data : row.base64;
  if (!encoded) throw new Error("导出结果缺少 base64 内容");
  const bytes = Uint8Array.from(atob(encoded), char => char.charCodeAt(0));
  const container = result && typeof result === "object" ? result as { media_type?: string; filename?: string } : {};
  const mediaType = Array.isArray(result) ? String(result[1] || "application/octet-stream") : container.media_type || "application/octet-stream";
  const filename = Array.isArray(result) ? `src-assets-${taskId}.zip` : container.filename || "src-evidence.zip";
  const blobUrl = URL.createObjectURL(new Blob([bytes], { type: mediaType }));
  const anchor = document.createElement("a");
  anchor.href = blobUrl; anchor.download = filename;
  document.body.append(anchor); anchor.click(); anchor.remove();
  window.setTimeout(() => URL.revokeObjectURL(blobUrl), 1000);
}
