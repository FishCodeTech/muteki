"use client";

import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { Button } from "@heroui/react";
import { Icon, type IconName } from "@/components/Icon";

export function SrcStatus({ children, tone = "neutral" }: { children: ReactNode; tone?: "neutral" | "accent" | "positive" | "warning" | "danger" }) {
  return <span className={`src-status src-status--${tone}`}><i aria-hidden="true" />{children}</span>;
}

export function stateTone(state: string): "neutral" | "accent" | "positive" | "warning" | "danger" {
  if (["completed", "ready", "verified", "approved", "allowed", "fixed", "GREEN"].includes(state)) return "positive";
  if (["failed", "denied", "expired", "cancelled", "outcome_unknown", "RED", "configuration_mismatch"].includes(state)) return "danger";
  if (["waiting_approval", "pending", "resolving", "partial", "stale", "YELLOW", "unverified", "unresolved"].includes(state)) return "warning";
  if (["running", "dispatched", "created"].includes(state)) return "accent";
  return "neutral";
}

export function SrcError({ error, onRetry }: { error: string; onRetry?: () => void }) {
  if (!error) return null;
  return <div className="src-notice src-notice--danger" role="alert"><Icon name="circleAlert" size={15} /><span>{error}</span>{onRetry ? <Button size="sm" variant="ghost" onPress={onRetry}>重试</Button> : null}</div>;
}

export function SrcEmpty({ children = "暂无记录" }: { children?: ReactNode }) {
  return <div className="src-empty"><Icon name="circleDashed" size={18} /><p>{children}</p></div>;
}

export function SrcLoading({ label = "正在读取…" }: { label?: string }) {
  return <div className="src-empty" role="status"><Icon name="loader" size={18} className="src-spin" /><p>{label}</p></div>;
}

export function SrcSection({ title, count, children, actions }: { title: string; count?: number; children: ReactNode; actions?: ReactNode }) {
  return <section className="src-section"><header><h3>{title}{count !== undefined ? <span className="src-count">{count}</span> : null}</h3>{actions ? <div className="src-section-actions">{actions}</div> : null}</header>{children}</section>;
}

export function SrcDefinitions({ rows }: { rows: Array<[string, ReactNode]> }) {
  return <dl className="src-definitions">{rows.map(([term, value], index) => <div key={`${term}-${index}`}><dt>{term}</dt><dd>{value ?? "—"}</dd></div>)}</dl>;
}

export function SrcJson({ value }: { value: unknown }) {
  return <pre className="src-json">{JSON.stringify(value ?? {}, null, 2)}</pre>;
}

export function SrcInspectorFrame({ title, subtitle, onClose, children, actions }: {
  title: string; subtitle?: string; onClose: () => void; children: ReactNode; actions?: ReactNode;
}) {
  return <aside className="src-inspector" aria-label={title}>
    <header className="src-inspector-header"><div><h2>{title}</h2>{subtitle ? <p>{subtitle}</p> : null}</div><div className="src-inspector-actions">{actions}<Button size="sm" variant="ghost" isIconOnly aria-label="关闭检查器" onPress={onClose}><Icon name="x" size={16} /></Button></div></header>
    <div className="src-inspector-body">{children}</div>
  </aside>;
}

export function useSrcResource<T>(loader: () => Promise<T>, dependencies: readonly unknown[], options?: { pollMs?: number; enabled?: boolean }) {
  const { pollMs = 0, enabled = true } = options || {};
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(enabled);
  const seq = useRef(0);
  const loaderRef = useRef(loader);
  loaderRef.current = loader;
  const reload = useCallback(async () => {
    const current = ++seq.current;
    if (!data) setLoading(true);
    try {
      const result = await loaderRef.current();
      if (current === seq.current) { setData(result); setError(""); }
    } catch (cause) {
      if (current === seq.current) setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      if (current === seq.current) setLoading(false);
    }
  }, [data]);
  useEffect(() => {
    if (!enabled) { setLoading(false); return; }
    setData(null); setError(""); seq.current += 1;
    void reload();
    if (!pollMs) return;
    const timer = window.setInterval(() => void reload(), pollMs);
    return () => { window.clearInterval(timer); seq.current += 1; };
    // The caller controls resource identity through dependencies.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [...dependencies, enabled, pollMs]);
  return { data, error, loading, reload, setData };
}

export function SrcIconButton({ icon, label, onPress, selected = false }: { icon: IconName; label: string; onPress: () => void; selected?: boolean }) {
  return <Button size="sm" variant={selected ? "secondary" : "ghost"} isIconOnly aria-label={label} aria-pressed={selected} onPress={onPress}><Icon name={icon} size={17} /></Button>;
}
