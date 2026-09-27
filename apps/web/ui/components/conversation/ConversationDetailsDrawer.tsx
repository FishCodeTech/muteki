"use client";

/* ─────────────────────────────────────────────────────────
 * CONVERSATION DETAILS DRAWER — on-demand inspector sheet for a tool call,
 * diff, artifact or error. Non-modal so the conversation stays interactive.
 * ───────────────────────────────────────────────────────── */

import React, { useEffect, useMemo, useRef, useState } from "react";
import type { IconName } from "@/components/Icon";
import {
  Badge,
  Callout,
  CodeBlock,
  EmptyState,
  IconButton,
  Sheet,
  Skeleton,
  useCopy,
  type Tone,
} from "@/components/chat/ui";
import { DialogSection, IconTile, MetaList, MetaRow } from "@/components/chat/dialogs/parts";
import { DiffTable } from "../ai-native/diff-table";
import { ConversationDiffViewer, diffFileKey, type DiffViewMode } from "./ConversationDiffViewer";
import { TypedResourcePreview } from "./TypedResourcePreview";
import { chatPanel } from "@/lib/chatPanelStore";
import {
  artifactHttpErrorMessage,
  fetchThreadArtifact,
  splitUnifiedDiffFiles,
  type DiffFileMeta,
} from "@/lib/conversationDiff";
import {
  previewKindFromName,
  type PreviewKind,
} from "@/lib/resourcePreview";

const ARTIFACT_PREVIEW_MAX_BYTES = 512_000;
const SHEET_WIDTH = 520;

export interface DrawerDetailPayload {
  type: "tool" | "diff" | "artifact" | "error";
  title: string;
  subtitle?: string;
  status?: string;
  toolName?: string;
  input?: unknown;
  output?: unknown;
  terminalOutput?: string;
  error?: unknown;
  diff?: {
    file: string;
    add?: number;
    del?: number;
    raw?: string;
  };
  artifact?: {
    sha256: string;
    name?: string;
    kind?: string;
    mediaType?: string;
    size?: number;
    threadId?: string;
  };
}

export interface ConversationDetailsDrawerProps {
  payload: DrawerDetailPayload | null;
  onClose: () => void;
  threadId?: string;
  className?: string;
}

function formatJson(val: unknown): string {
  if (val == null) return "";
  if (typeof val === "string") return val;
  try {
    return JSON.stringify(val, null, 2);
  } catch {
    return String(val);
  }
}

/** Pretty-prints JSON-looking payloads and guesses a highlight language for the rest. */
function codeContent(val: unknown): { code: string; language: string | null } {
  if (val == null) return { code: "", language: null };
  if (typeof val !== "string") return { code: formatJson(val), language: "json" };
  const trimmed = val.trim();
  if (/^[[{]/.test(trimmed)) {
    try {
      return { code: JSON.stringify(JSON.parse(trimmed), null, 2), language: "json" };
    } catch {
      // Not JSON after all; fall through to heuristics.
    }
  }
  if (/^(diff --git |--- a\/|@@ )/m.test(trimmed)) return { code: val, language: "diff" };
  if (/^<(!doctype|html|\?xml|svg)/i.test(trimmed)) return { code: val, language: "html" };
  return { code: val, language: null };
}

function splitCommand(input: unknown): { command: string | null; rest: unknown } {
  if (!input || typeof input !== "object" || Array.isArray(input)) return { command: null, rest: input };
  const record = input as Record<string, unknown>;
  const key = typeof record.command === "string" ? "command" : typeof record.cmd === "string" ? "cmd" : null;
  if (!key) return { command: null, rest: input };
  const { [key]: command, ...rest } = record;
  return { command: String(command), rest: Object.keys(rest).length ? rest : null };
}

function errorMessage(error: unknown): string {
  if (error == null) return "";
  if (typeof error === "string") return error;
  if (typeof error === "object") {
    const record = error as Record<string, unknown>;
    for (const key of ["message", "detail", "error", "reason"]) {
      if (typeof record[key] === "string" && record[key]) return record[key] as string;
    }
  }
  return "";
}

function statusTone(status: string | undefined, type: DrawerDetailPayload["type"]): Tone {
  if (type === "error") return "danger";
  const value = (status || "").toLowerCase();
  if (!value) return "neutral";
  if (/(fail|error|reject|denied|timeout|timed_out)/.test(value)) return "danger";
  if (/(cancel|interrupt|abort|skipped|stale|warn)/.test(value)) return "warning";
  if (/(running|progress|pending|queued|started|streaming|waiting)/.test(value)) return "running";
  if (/(complete|success|succeed|ok|done|finish|applied|approved)/.test(value)) return "success";
  return "neutral";
}

function statusLabel(status: string): string {
  const labels: Record<string, string> = {
    pending: "等待中",
    queued: "排队中",
    running: "执行中",
    completed: "已完成",
    failed: "失败",
    cancelled: "已取消",
    declined: "已拒绝",
    interrupted: "已中断",
  };
  return labels[status.toLowerCase()] || status;
}

function typeVisual(type: DrawerDetailPayload["type"]): { icon: IconName; tone: Tone; label: string } {
  switch (type) {
    case "tool":
      return { icon: "wrench", tone: "accent", label: "工具调用" };
    case "diff":
      return { icon: "fileDiff", tone: "accent", label: "文件变更" };
    case "artifact":
      return { icon: "package", tone: "neutral", label: "Artifact" };
    case "error":
      return { icon: "circleAlert", tone: "danger", label: "错误" };
    default: {
      const exhaustive: never = type;
      return exhaustive;
    }
  }
}

function formatBytes(size?: number): string {
  if (!size && size !== 0) return "未知";
  if (size < 1024) return `${size} B`;
  if (size < 1024 * 1024) return `${(size / 1024).toFixed(1)} KB`;
  return `${(size / 1024 / 1024).toFixed(2)} MB`;
}

export function ConversationDetailsDrawer({
  payload: incomingPayload,
  onClose,
  threadId = "",
  className = "",
}: ConversationDetailsDrawerProps) {
  const [artifactContent, setArtifactContent] = useState<string>("");
  const [artifactObjectUrl, setArtifactObjectUrl] = useState<string>("");
  const artifactObjectUrlRef = useRef("");
  const [artifactPreviewKind, setArtifactPreviewKind] = useState<PreviewKind | "">("");
  const [artifactLoading, setArtifactLoading] = useState(false);
  const [artifactError, setArtifactError] = useState("");
  const [retainedPayload, setRetainedPayload] = useState(incomingPayload);
  const [diffViewMode, setDiffViewMode] = useState<DiffViewMode>("unified");
  const [diffWrap, setDiffWrap] = useState(false);
  const [diffContextLines, setDiffContextLines] = useState(999);
  const [diffSelectedKey, setDiffSelectedKey] = useState("");
  const { copied, copy } = useCopy();
  const payload = incomingPayload ?? retainedPayload;

  useEffect(() => {
    if (incomingPayload) setRetainedPayload(incomingPayload);
  }, [incomingPayload]);

  // Load artifact preview if opened (typed — never dump binary as text).
  useEffect(() => {
    if (!payload?.artifact || !threadId) return;
    const sha = payload.artifact.sha256;
    const name = payload.artifact.name || sha;
    let cancelled = false;
    setArtifactLoading(true);
    setArtifactError("");
    setArtifactContent("");
    if (artifactObjectUrlRef.current) {
      URL.revokeObjectURL(artifactObjectUrlRef.current);
      artifactObjectUrlRef.current = "";
    }
    setArtifactObjectUrl("");
    setArtifactPreviewKind("");

    void (async () => {
      try {
        const res = await fetchThreadArtifact(threadId, sha);
        if (!res.ok) throw new Error(artifactHttpErrorMessage(res.status, "read"));
        const blob = await res.blob();
        const media = res.headers.get("content-type") || payload.artifact?.mediaType || "";
        let kind = previewKindFromName(name, media);
        // Diff artifacts must stay text for the Diff viewer.
        if (payload.type === "diff" || payload.artifact?.kind === "conversation.diff") {
          kind = "text";
        }
        if (cancelled) return;
        // Always keep an object URL so Download uses the authenticated bytes
        // (bare /api/... URLs cannot carry the Bearer token).
        const objectUrl = URL.createObjectURL(blob);
        artifactObjectUrlRef.current = objectUrl;
        setArtifactObjectUrl(objectUrl);
        if (kind === "image" || kind === "pdf") {
          setArtifactPreviewKind(kind);
          return;
        }
        if (kind === "binary") {
          setArtifactPreviewKind("binary");
          return;
        }
        if (blob.size > ARTIFACT_PREVIEW_MAX_BYTES) {
          setArtifactPreviewKind("too_large");
          return;
        }
        const text = await blob.text();
        if (cancelled) return;
        setArtifactContent(text);
        setArtifactPreviewKind(kind);
      } catch (err) {
        if (!cancelled) {
          setArtifactError(err instanceof Error ? err.message : "读取 Artifact 失败");
        }
      } finally {
        if (!cancelled) setArtifactLoading(false);
      }
    })();

    return () => {
      cancelled = true;
    };
  }, [payload?.artifact, payload?.type, threadId]);

  useEffect(() => () => {
    if (artifactObjectUrlRef.current) {
      URL.revokeObjectURL(artifactObjectUrlRef.current);
      artifactObjectUrlRef.current = "";
    }
  }, []);

  const drawerDiffFiles: DiffFileMeta[] = useMemo(() => {
    const raw = artifactContent || payload?.diff?.raw || "";
    if (!raw.trim()) {
      if (payload?.diff?.file) {
        return [{
          path: payload.diff.file,
          status: "M",
          staging: "artifact",
          additions: payload.diff.add,
          deletions: payload.diff.del,
          patch: payload.diff.raw || "",
        }];
      }
      return [];
    }
    return splitUnifiedDiffFiles(raw).map((file) => ({
      path: file.path,
      old_path: file.oldPath,
      status: file.status,
      staging: "artifact" as const,
      binary: file.binary,
      additions: file.additions,
      deletions: file.deletions,
      patch: file.raw,
    }));
  }, [artifactContent, payload?.diff]);

  useEffect(() => {
    if (!drawerDiffFiles.length) {
      setDiffSelectedKey("");
      return;
    }
    setDiffSelectedKey((current) => (
      current && drawerDiffFiles.some((file) => diffFileKey(file) === current)
        ? current
        : diffFileKey(drawerDiffFiles[0])
    ));
  }, [drawerDiffFiles]);

  if (!payload) return null;

  const visual = typeVisual(payload.type);
  const tone = statusTone(payload.status, payload.type);
  const { command, rest: inputRest } = splitCommand(payload.input);
  const inputBlock = codeContent(inputRest);
  const outputBlock = codeContent(payload.output);
  const errorText = errorMessage(payload.error);
  const errorBlock = typeof payload.error === "string" ? null : codeContent(payload.error);
  const hasInput = Boolean(command || inputBlock.code);
  const hasOutput = Boolean(outputBlock.code);
  const selectedDiff = drawerDiffFiles.find((file) => diffFileKey(file) === diffSelectedKey);
  const diffRaw = artifactContent || payload.diff?.raw || "";

  const handleCopyAll = () => {
    const parts: string[] = [];
    if (payload.input != null) parts.push(`# 输入\n${formatJson(payload.input)}`);
    if (payload.output != null) parts.push(`# 输出\n${formatJson(payload.output)}`);
    if (payload.terminalOutput) parts.push(`# 终端输出\n${payload.terminalOutput}`);
    if (payload.error != null) parts.push(`# 错误\n${formatJson(payload.error)}`);
    if (payload.diff) parts.push(diffRaw || payload.diff.file);
    if (payload.artifact && !payload.diff) parts.push(artifactContent || payload.artifact.sha256);
    void copy(parts.join("\n\n") || formatJson(payload));
  };

  const handleDownload = () => {
    if (!payload.artifact || !threadId) return;
    const sha = payload.artifact.sha256;
    const filename = payload.artifact.name || "artifact";
    void (async () => {
      try {
        const res = await fetchThreadArtifact(threadId, sha, { download: true });
        if (!res.ok) throw new Error(artifactHttpErrorMessage(res.status, "download"));
        const blob = await res.blob();
        const objectUrl = URL.createObjectURL(blob);
        const anchor = document.createElement("a");
        anchor.href = objectUrl;
        anchor.download = filename;
        anchor.rel = "noopener";
        anchor.click();
        URL.revokeObjectURL(objectUrl);
      } catch (err) {
        setArtifactError(err instanceof Error ? err.message : "下载 Artifact 失败");
      }
    })();
  };

  const openDiffInPanel = () => {
    if (!threadId || !payload.diff) return;
    chatPanel.openDiff(threadId, { kind: "worktree", filePath: selectedDiff?.path || payload.diff.file });
    onClose();
  };

  const headerActions = (
    <div className="flex items-center gap-0.5">
      <IconButton
        icon={copied ? "check" : "copy"}
        label={copied ? "已复制" : "复制全部内容"}
        className={copied ? "text-cx-success hover:text-cx-success" : undefined}
        onClick={handleCopyAll}
      />
      {payload.diff && threadId ? (
        <IconButton icon="panelRightOpen" label="在工作面板中查看变更" onClick={openDiffInPanel} />
      ) : null}
      {payload.artifact && threadId ? (
        <IconButton icon="download" label="下载文件" onClick={handleDownload} />
      ) : null}
    </div>
  );

  const title = (
    <span className="flex min-w-0 items-center gap-3">
      <IconTile icon={visual.icon} tone={visual.tone} />
      <span className="flex min-w-0 flex-col">
        <span className="flex min-w-0 items-center gap-2">
          <span className="truncate">{payload.title}</span>
          {payload.status ? (
            <Badge tone={tone} dot className="font-cx-mono text-[11px]">{statusLabel(payload.status)}</Badge>
          ) : null}
        </span>
        <span className="truncate font-cx-mono text-[11.5px] font-normal text-cx-fg-3">
          {payload.subtitle || payload.toolName || visual.label}
        </span>
      </span>
    </span>
  );

  const artifactPreview = payload.artifact ? (
    <DialogSection title="预览" icon="eye">
      {artifactLoading ? (
        <div className="flex flex-col gap-2 rounded-xl border border-cx-border-subtle bg-cx-code p-4" aria-busy="true" aria-label="正在读取内容">
          <Skeleton className="w-2/3" />
          <Skeleton className="w-5/6" />
          <Skeleton className="w-1/2" />
        </div>
      ) : artifactError ? (
        <Callout tone="danger" title="无法读取内容" role="alert">{artifactError}</Callout>
      ) : (
        <TypedResourcePreview
          name={payload.artifact.name || "artifact"}
          mediaType={payload.artifact.mediaType}
          previewKind={artifactPreviewKind || previewKindFromName(
            payload.artifact.name || "",
            payload.artifact.mediaType,
          )}
          content={artifactContent}
          message={
            artifactPreviewKind === "too_large"
              ? `文件超过 ${ARTIFACT_PREVIEW_MAX_BYTES / 1024} KB 预览上限。请下载原文件查看。`
              : artifactPreviewKind === "binary"
                ? "此类型暂不支持内联预览，请下载原文件（避免乱码）。"
                : null
          }
          size={payload.artifact.size}
          maxPreviewBytes={ARTIFACT_PREVIEW_MAX_BYTES}
          objectUrl={artifactObjectUrl || null}
          downloadUrl={artifactObjectUrl || null}
          testId="artifact-typed-preview"
        />
      )}
    </DialogSection>
  ) : null;

  const diffSection = payload.diff ? (
    <DialogSection
      title="变更"
      icon="fileDiff"
      meta={payload.diff.add != null || payload.diff.del != null ? (
        <span className="cx-tabular font-cx-mono text-[11.5px]">
          <span className="text-cx-add">+{payload.diff.add ?? 0}</span>{" "}
          <span className="text-cx-del">−{payload.diff.del ?? 0}</span>
        </span>
      ) : undefined}
    >
      {artifactLoading && !payload.diff.raw ? (
        <div className="flex flex-col gap-2 rounded-xl border border-cx-border-subtle bg-cx-code p-4" aria-busy="true" aria-label="正在读取变更">
          <Skeleton className="w-3/4" />
          <Skeleton className="w-1/2" />
          <Skeleton className="w-2/3" />
        </div>
      ) : drawerDiffFiles.length && diffRaw ? (
        <div className="overflow-hidden rounded-xl border border-cx-border-subtle">
          <ConversationDiffViewer
            files={drawerDiffFiles}
            selectedKey={diffSelectedKey || diffFileKey(drawerDiffFiles[0])}
            onSelectFile={setDiffSelectedKey}
            patch={selectedDiff?.patch || drawerDiffFiles[0]?.patch || diffRaw}
            path={selectedDiff?.path || drawerDiffFiles[0]?.path || payload.diff.file}
            staging="artifact"
            binary={Boolean(selectedDiff?.binary)}
            oldPath={selectedDiff?.old_path}
            viewMode={diffViewMode}
            onViewModeChange={setDiffViewMode}
            wrap={diffWrap}
            onWrapChange={setDiffWrap}
            contextLines={diffContextLines}
            onContextLinesChange={setDiffContextLines}
          />
        </div>
      ) : (
        <DiffTable
          files={[
            {
              path: payload.diff.file,
              additions: payload.diff.add,
              deletions: payload.diff.del,
              raw: payload.diff.raw || artifactContent,
            },
          ]}
        />
      )}
    </DialogSection>
  ) : null;

  const nothingToShow = payload.type !== "tool" && !hasInput && !hasOutput && !payload.terminalOutput
    && payload.error == null && !payload.diff && !payload.artifact;

  return (
    <Sheet
      open={Boolean(incomingPayload)}
      onOpenChange={(open) => { if (!open) onClose(); }}
      modal={false}
      width={SHEET_WIDTH}
      title={title}
      headerActions={headerActions}
      ariaLabel="执行详情抽屉"
      testId="conversation-details-drawer"
      className={`cx-details-sheet ${className}`.trim()}
      bodyClassName="px-4 pb-6 pt-3"
    >
      <div className="flex flex-col gap-5">
        {payload.type === "diff" ? diffSection : null}
        {payload.type === "artifact" && !payload.diff ? artifactPreview : null}

        {payload.error != null ? (
          <DialogSection title="错误" icon="circleAlert">
            <div className="flex flex-col gap-2">
              {errorText ? (
                <Callout tone="danger" role="alert">
                  <span className="whitespace-pre-wrap break-words font-cx-mono text-[12.5px]">{errorText}</span>
                </Callout>
              ) : null}
              {errorBlock?.code && (!errorText || errorBlock.code.trim() !== errorText.trim()) ? (
                <CodeBlock className="my-0" code={errorBlock.code} language={errorBlock.language} filename="error.json" collapseAfter={30} />
              ) : null}
            </div>
          </DialogSection>
        ) : null}

        {payload.type === "tool" || hasInput ? (
          <DialogSection title="输入" icon="braces">
            {hasInput ? (
              <div className="flex flex-col gap-2">
                {command ? <CodeBlock className="my-0" code={command} language="bash" filename="命令" collapseAfter={16} wrapByDefault /> : null}
                {inputBlock.code ? (
                  <CodeBlock className="my-0" code={inputBlock.code} language={inputBlock.language} filename="parameters.json" collapseAfter={30} />
                ) : null}
              </div>
            ) : (
              <p className="rounded-xl border border-dashed border-cx-border px-3 py-3 text-[12.5px] text-cx-fg-4">无结构化输入参数</p>
            )}
          </DialogSection>
        ) : null}

        {hasOutput ? (
          <DialogSection title="输出" icon="arrowDownToLine">
            <CodeBlock
              className="my-0"
              code={outputBlock.code}
              language={outputBlock.language}
              filename={outputBlock.language === "json" ? "output.json" : "output"}
              collapseAfter={40}
            />
          </DialogSection>
        ) : null}

        {payload.terminalOutput ? (
          <DialogSection title="终端输出" icon="terminal">
            <CodeBlock className="my-0" code={payload.terminalOutput} language={null} filename="terminal.log" collapseAfter={40} wrapByDefault />
          </DialogSection>
        ) : null}

        {payload.type !== "diff" ? diffSection : null}
        {payload.type !== "artifact" && !payload.diff ? artifactPreview : null}

        {payload.artifact ? (
          <DialogSection title="文件信息" icon="info">
            <MetaList>
              <MetaRow label="文件名">{payload.artifact.name || "未命名"}</MetaRow>
              <MetaRow label="类型" mono>{payload.artifact.mediaType || "未知"}</MetaRow>
              <MetaRow label="大小" mono>{formatBytes(payload.artifact.size)}</MetaRow>
              <MetaRow label="SHA-256" mono copy={payload.artifact.sha256}>{payload.artifact.sha256}</MetaRow>
            </MetaList>
          </DialogSection>
        ) : null}

        {nothingToShow ? (
          <EmptyState compact icon={visual.icon} title="暂无可展示的详情" description="该条目没有记录输入、输出或附件内容。" />
        ) : null}
      </div>
      <span className="sr-only" aria-live="polite">{copied ? "已复制到剪贴板" : ""}</span>
    </Sheet>
  );
}
