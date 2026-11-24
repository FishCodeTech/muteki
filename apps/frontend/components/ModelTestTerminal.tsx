"use client";

import { useEffect, useState } from "react";
import { Button, Icon } from "@muteki/ui";
import type { WorkerModelTestResult } from "@/lib/useRun";

const ERROR_KIND_LABELS: Record<string, string> = {
  proxy_error: "代理错误",
  auth_error: "鉴权失败",
  upstream_error: "上游异常",
  credential_error: "凭据缺失",
  timeout: "请求超时",
  runtime_error: "运行异常",
};

export function ModelTestTerminal({
  testing,
  result,
  compact = false,
  title = "模型测试终端",
  runningMessage = "正在执行真实 Worker 模型请求，等待模型返回；完成或超时后会显示结果。",
}: {
  testing: boolean;
  result: WorkerModelTestResult | null;
  compact?: boolean;
  title?: string;
  runningMessage?: string;
}) {
  const [collapsed, setCollapsed] = useState(false);
  const [elapsedSeconds, setElapsedSeconds] = useState(0);
  useEffect(() => {
    if (!testing) return;
    const startedAt = Date.now();
    setElapsedSeconds(0);
    const timer = window.setInterval(() => setElapsedSeconds(Math.floor((Date.now() - startedAt) / 1000)), 1000);
    return () => window.clearInterval(timer);
  }, [testing]);
  if (!testing && !result) return null;
  const stateClass = testing ? "running" : result?.ok ? "ok" : "bad";
  const errorKindLabel = !testing && !result?.ok && result?.error_kind ? (ERROR_KIND_LABELS[result.error_kind] ?? result.error_kind) : "";
  const stateLabel = testing ? `运行中 · ${elapsedSeconds}s` : result?.ok ? "已通过" : (errorKindLabel ? `未通过 · ${errorKindLabel}` : "未通过");
  const proxyInfo = !testing && result?.proxy_mode ? (result.proxy_display ? `代理 ${result.proxy_display}` : `代理:${result.proxy_mode}`) : "";
  if (collapsed) {
    return (
      <Button variant="ghost" className="wmodel-terminal-collapsed" onClick={() => setCollapsed(false)} aria-label={`展开${title}`}>
        <span><i className={stateClass} />{title.replace("终端", "日志")}</span>
        <em>{stateLabel}{result?.elapsed_ms ? ` · ${(result.elapsed_ms / 1000).toFixed(1)}s` : ""}</em>
        <Icon name="chevronDown" size={13} />
      </Button>
    );
  }
  const logs = result?.logs?.length
    ? result.logs
    : result
      ? [{ stream: result.ok ? "success" as const : "error" as const, message: result.detail, elapsed_ms: result.elapsed_ms || 0 }]
      : [];
  return (
    <section className={`wmodel-terminal${compact ? " compact" : ""}`} aria-live="polite">
      <header>
        <span><i className={stateClass} />{title}</span>
        <div><em>{stateLabel}</em><Button size="sm" variant="ghost" iconOnly onClick={() => setCollapsed(true)} aria-label={`收起${title}`}><Icon name="chevronDown" size={13} /></Button></div>
      </header>
      <div className="wmodel-terminal-body">
        {testing ? <div className="system"><b>00</b><pre>{runningMessage}</pre></div> : null}
        {logs.map((log, index) => (
          <div className={log.stream} key={`${log.stream}-${index}`}>
            <b>{String(index + (testing ? 1 : 0)).padStart(2, "0")}</b>
            <pre>{log.message}</pre>
            <time>{log.elapsed_ms ? `${log.elapsed_ms}ms` : ""}</time>
          </div>
        ))}
      </div>
      {result ? <footer><strong>{result.detail}</strong><span>{[result.engine, result.model || "默认模型", proxyInfo, typeof result.exit_code === "number" ? `exit ${result.exit_code}` : ""].filter(Boolean).join(" · ")}</span></footer> : null}
    </section>
  );
}
