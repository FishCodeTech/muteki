"use client";

import { useEffect, useState } from "react";
import { CopyButton } from "@muteki/ui";
import {
  formatBytes,
  shortWorkspaceLabel,
  summarizeContextWindow,
} from "@/lib/runInspectorMetrics";
import { formatTokens } from "@/lib/usageFormat";
import type { RunUsageSnapshot } from "@/lib/useRunUsageLedger";
import {
  fetchLocalPiInspector,
  type LocalPiInspectorSnapshot,
  type LocalPiWorkspaceFile,
} from "@/lib/useRun";
import { RunUsageSummaryCard } from "./RunUsageSummaryCard";

type Snapshot = LocalPiInspectorSnapshot;

export function useLocalPiInspector(
  runId: string | undefined,
  enabled: boolean,
  running: boolean,
  refreshKey: unknown,
) {
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    if (!enabled || !runId) {
      setSnapshot(null);
      setError(null);
      return;
    }
    let cancelled = false;
    const load = () => {
      fetchLocalPiInspector(runId)
        .then((snap) => {
          if (cancelled) return;
          setSnapshot(snap);
          setError(null);
        })
        .catch((err: unknown) => {
          if (!cancelled) setError(err instanceof Error ? err.message : String(err));
        });
    };
    load();
    if (!running) {
      return () => {
        cancelled = true;
      };
    }
    const timer = window.setInterval(load, 2500);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
  }, [enabled, runId, running, refreshKey]);
  return { snapshot, error };
}

export function localPiSessionStateLabel(state: string | undefined): string {
  switch (state) {
    case "running": return "执行中";
    case "idle": return "待命";
    case "starting": return "启动中";
    case "stopped": return "已停止";
    case "failed": return "失败";
    case "resume_failed": return "续跑失败";
    case "interrupted": return "已中断";
    case "awaiting_operator": return "等待追问";
    default: return state || "未知";
  }
}

export function localPiSessionMeta(snapshot: Snapshot, solved = false, en = false): string {
  const info = snapshot.tools_and_skills;
  const turns = en ? `${info.turn_count} turns` : `${info.turn_count} 轮`;
  if (info.turn_active || info.session_state === "running" || info.session_state === "starting") {
    return en ? `Running · ${turns}` : `执行中 · ${turns}`;
  }
  if (!solved && (info.session_state === "failed" || info.session_state === "resume_failed")) {
    return en ? `Failed · ${turns}` : `失败 · ${turns}`;
  }
  if (!solved && info.session_state === "interrupted") {
    return en ? `Interrupted · ${turns}` : `已中断 · ${turns}`;
  }
  return turns;
}

export function LocalPiOverviewDetails({
  snapshot,
  usage,
  usageError = false,
  en = false,
}: {
  snapshot: Snapshot;
  usage?: RunUsageSnapshot | null;
  usageError?: boolean;
  en?: boolean;
}) {
  const info = snapshot.tools_and_skills;
  const resume = info.resume_status;
  const model = info.model ?? {};
  const lastError = info.last_error;
  const missingSkills = info.loaded_skills.filter((s) => s.exists === false);
  const ctx = summarizeContextWindow(
    info.context?.used_tokens,
    info.context?.limit ?? model.context_window,
  );

  let ctxText: string;
  if (ctx.used != null && ctx.limit != null && ctx.percent != null) {
    ctxText = `${formatTokens(ctx.used)} / ${formatTokens(ctx.limit)} · ${ctx.percent}%`;
  } else if (ctx.limit != null) {
    ctxText = `${en ? "Limit" : "上限"} ${formatTokens(ctx.limit)}`;
  } else if (ctx.used != null) {
    ctxText = `${formatTokens(ctx.used)} ${en ? "used" : "已用"}`;
  } else {
    ctxText = en ? "No data" : "暂无数据";
  }

  return (
    <div className="rv-pi-overview">
      <div className="rv-pi-model">
        <span className="rv-pi-model-name" title={model.id || "default"}>
          {model.id || (en ? "Pi default" : "Pi 默认")}
        </span>
        {model.effort ? <span className="rv-pi-effort">{model.effort}</span> : null}
      </div>

      <div className="rv-ctx">
        <div className="rv-ctx-head">
          <span className="rv-ctx-label">{en ? "Context Window" : "上下文窗口"}</span>
          <span className="rv-ctx-val">{ctxText}</span>
        </div>
        <div
          className="rv-ctx-track"
          role="progressbar"
          aria-valuenow={ctx.percent ?? 0}
          aria-valuemin={0}
          aria-valuemax={100}
        >
          <div
            className="rv-ctx-fill"
            data-tone={ctx.tone}
            style={{ width: `${ctx.percent ?? 0}%` }}
          />
        </div>
      </div>

      <RunUsageSummaryCard
        ledger={usage?.totals}
        solve={usage?.buckets?.solve}
        auxiliary={usage?.buckets?.auxiliary}
        error={usageError}
        fallbackUsage={info.token_usage ?? info.last_turn_usage}
        en={en}
      />

      {!resume.resumable && resume.continuation_row_present ? (
        <div className="rv-alert" role="alert">
          {en
            ? `Cannot resume: ${resume.error || "session file missing"}`
            : `无法续跑：${resume.error || "会话文件缺失或损坏"}`}
        </div>
      ) : null}

      {lastError ? (
        <div className="rv-alert" role="alert">
          {typeof lastError === "string"
            ? lastError
            : `${lastError.code || "error"}：${lastError.message || ""}`}
        </div>
      ) : null}

      {missingSkills.length > 0 ? (
        <div className="rv-alert" role="alert">
          {en ? "Missing skills: " : "缺失 Skill 文件："}
          {missingSkills.map((s) => s.name || s.id).join(", ")}
        </div>
      ) : null}
    </div>
  );
}

function fileRow(file: LocalPiWorkspaceFile, en: boolean, showRelPath = false) {
  const broken = !file.exists
    ? en
      ? "Missing"
      : "缺失"
    : !file.readable
      ? en
        ? "Unreadable"
        : "不可读"
      : null;
  const label = showRelPath ? (file.rel_path || file.name) : (file.name || file.rel_path);
  return (
    <div className="rv-file-row" key={file.rel_path} title={file.rel_path}>
      <span className="rv-file-name">
        {label}
        {broken ? <span className="rv-file-bad"> · {broken}</span> : null}
      </span>
      <span className="rv-file-meta">{formatBytes(file.size_bytes)}</span>
    </div>
  );
}

function artifactShortLabel(relPath: string, seq?: number): string {
  if (typeof seq === "number" && Number.isFinite(seq)) return `#${seq}`;
  const parts = relPath.split("/");
  return parts[parts.length - 1] || relPath;
}

export function localPiMaterialsMeta(snapshot: Snapshot, en = false): string {
  const files = snapshot.materials_and_artifacts;
  return en
    ? `${files.uploads.length} uploads · ${files.tool_artifacts.length} artifacts`
    : `${files.uploads.length} 附件 · ${files.tool_artifacts.length} 产物`;
}

export function LocalPiMaterialsDetails({
  snapshot,
  en = false,
}: {
  snapshot: Snapshot;
  en?: boolean;
}) {
  const [showAllFiles, setShowAllFiles] = useState(false);
  const [showAllArtifacts, setShowAllArtifacts] = useState(false);
  const files = snapshot.materials_and_artifacts;

  const uploadNames = new Set(files.uploads.map((u) => u.name).filter(Boolean));
  const workspaceFiles = files.workspace_files.filter(
    (f) => !uploadNames.has(f.rel_path),
  );
  const visibleWorkspaceFiles = showAllFiles
    ? workspaceFiles
    : workspaceFiles.slice(0, 4);
  const sortedArtifacts = files.tool_artifacts
    .slice()
    .sort((a, b) => (b.seq ?? -1) - (a.seq ?? -1));
  const visibleArtifacts = showAllArtifacts ? sortedArtifacts : sortedArtifacts.slice(0, 5);
  const hasAny =
    files.uploads.length > 0 ||
    workspaceFiles.length > 0 ||
    sortedArtifacts.length > 0;

  return (
    <div className="rv-files">
      <div className="rv-files-head">
        <code className="rv-files-ws" title={files.workspace_root}>
          {shortWorkspaceLabel(files.workspace_root)}
        </code>
        <CopyButton
          text={files.workspace_root}
          label={en ? "Copy workspace path" : "复制工作区路径"}
          size="sm"
        />
      </div>

      {!hasAny ? (
        <div className="rv-empty">{en ? "No files or artifacts yet" : "暂无文件或产物"}</div>
      ) : (
        <>
          {files.uploads.length > 0 ? (
            <div className="rv-files-group">
              <div className="rv-files-label">{en ? "Attachments" : "题目附件"}</div>
              {files.uploads.map((f) => fileRow(f, en))}
            </div>
          ) : null}

          {workspaceFiles.length > 0 ? (
            <div className="rv-files-group">
              <div className="rv-files-label">{en ? "Workspace Files" : "生成文件"}</div>
              {visibleWorkspaceFiles.map((f) => fileRow(f, en, true))}
              {workspaceFiles.length > 4 ? (
                <button
                  type="button"
                  className="rv-files-more"
                  onClick={() => setShowAllFiles((prev) => !prev)}
                >
                  {showAllFiles
                    ? en
                      ? "Collapse"
                      : "收起"
                    : en
                      ? `Show all ${workspaceFiles.length}`
                      : `展开全部 ${workspaceFiles.length} 个`}
                </button>
              ) : null}
            </div>
          ) : null}

          {sortedArtifacts.length > 0 ? (
            <div className="rv-files-group">
              <div className="rv-files-label">{en ? "Recent Artifacts" : "最近产物"}</div>
              {visibleArtifacts.map((item) => (
                <div
                  className="rv-file-row"
                  key={item.artifact_id}
                  title={item.preview || item.rel_path}
                >
                  <span className="rv-file-name">
                    <span className="rv-file-tool">{item.tool_name || "tool"}</span>
                    <span>{artifactShortLabel(item.rel_path, item.seq)}</span>
                    {item.status !== "ok" ? (
                      <span className="rv-file-bad">
                        {" · "}
                        {item.status === "missing"
                          ? en
                            ? "Missing"
                            : "缺失"
                          : en
                            ? "Unreadable"
                            : "不可读"}
                      </span>
                    ) : null}
                  </span>
                </div>
              ))}
              {sortedArtifacts.length > 5 ? (
                <button
                  type="button"
                  className="rv-files-more"
                  onClick={() => setShowAllArtifacts((prev) => !prev)}
                >
                  {showAllArtifacts
                    ? en
                      ? "Collapse"
                      : "收起"
                    : en
                      ? `Show all ${sortedArtifacts.length}`
                      : `展开全部 ${sortedArtifacts.length} 条`}
                </button>
              ) : null}
            </div>
          ) : null}
        </>
      )}
    </div>
  );
}
