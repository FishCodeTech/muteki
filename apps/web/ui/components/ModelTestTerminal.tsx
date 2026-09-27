"use client";

import { useState } from "react";
import { Button } from "@heroui/react";
import { Icon } from "@/components/Icon";
import type { WorkerModelTestResult } from "@/lib/useRun";

export function ModelTestTerminal({
  testing,
  result,
  compact = false,
  title = "模型测试终端",
  runningMessage = "正在启动真实 Worker 模型对话，请等待模型返回…",
}: {
  testing: boolean;
  result: WorkerModelTestResult | null;
  compact?: boolean;
  title?: string;
  runningMessage?: string;
}) {
  const [collapsed, setCollapsed] = useState(false);
  if (!testing && !result) return null;
  const stateClass = testing ? "running" : result?.ok ? "ok" : "bad";
  const stateLabel = testing ? "运行中" : result?.ok ? "已通过" : "未通过";
  if (collapsed) {
    return (
      <Button variant="ghost" className="wmodel-terminal-collapsed" onPress={() => setCollapsed(false)} aria-label={`展开${title}`}>
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
        <div><em>{stateLabel}</em><Button size="sm" variant="ghost" isIconOnly onPress={() => setCollapsed(true)} aria-label={`收起${title}`}><Icon name="chevronDown" size={13} /></Button></div>
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
      {result ? <footer><strong>{result.detail}</strong><span>{result.engine} · {result.model || "默认模型"}{typeof result.exit_code === "number" ? ` · exit ${result.exit_code}` : ""}</span></footer> : null}
    </section>
  );
}
