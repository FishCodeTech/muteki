"use client";

/**
 * C32 — ExportDialog
 *
 * Exports a conversation thread as Markdown or JSONL. Shows a snapshot
 * preview (turn / message / artifact counts) and redaction options before
 * the download. A separate dialog creates scoped, expiring read-only shares.
 */

import React, { useState } from "react";
import { cn } from "@/lib/cn";
import { Icon, type IconName } from "@/components/Icon";
import { Button, Callout, Checkbox, Dialog } from "@/components/chat/ui";
import type { ConversationView } from "@/lib/useConversation";
import { apiFetch } from "@/lib/useRun";
import { ShareDialog } from "./ShareDialog";

export interface ExportDialogProps {
  open: boolean;
  onClose: () => void;
  view: ConversationView;
}

type ExportFormat = "markdown" | "jsonl";

const FORMATS: Array<{ value: ExportFormat; label: string; ext: string; icon: IconName; description: string }> = [
  {
    value: "markdown",
    label: "Markdown",
    ext: ".md",
    icon: "book",
    description: "人类可读格式，含标题、轮次正文和附件索引。",
  },
  {
    value: "jsonl",
    label: "JSONL",
    ext: ".jsonl",
    icon: "braces",
    description: "结构化换行 JSON，每行一条记录，便于程序处理。",
  },
];

export function ExportDialog({ open, onClose, view }: ExportDialogProps) {
  const [shareOpen, setShareOpen] = useState(false);
  const [format, setFormat] = useState<ExportFormat>("markdown");
  const [excludeTools, setExcludeTools] = useState(false);
  const [excludePaths, setExcludePaths] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  const stats = view.statistics;
  const threadId = view.thread.thread_id;
  const title = view.thread.title || "conversation";
  const msgCount = view.messages.length;
  const turnCount = stats?.turn_count ?? view.turns.length;
  const artifactCount = view.artifacts?.length ?? 0;
  const watermark = view.watermark;

  const handleDownload = async () => {
    setError("");
    setBusy(true);
    try {
      const params = new URLSearchParams({
        format,
        exclude_tools: excludeTools ? "1" : "0",
        exclude_paths: excludePaths ? "1" : "0",
      });
      // apiFetch carries Bearer/ticket auth when MUTEKI_WEB_PASSWORD is set; raw fetch omits it.
      const res = await apiFetch(
        `/api/threads/${encodeURIComponent(threadId)}/export?${params}`,
      );
      if (!res.ok) {
        const body = await res.json().catch(() => ({})) as Record<string, unknown>;
        const err = body.error as Record<string, unknown> | undefined;
        throw new Error((err?.message as string) || `导出失败（HTTP ${res.status}）`);
      }
      const blob = await res.blob();
      const ext = format === "jsonl" ? "jsonl" : "md";
      // Strip only filesystem-unsafe chars so CJK titles survive; the server also sends filename*.
      const safeTitle = title.replace(/[<>:"/\\|?*\x00-\x1f]/g, "").trim().slice(0, 80) || "export";
      const date = new Date().toISOString().slice(0, 10).replace(/-/g, "");
      const filename = `${safeTitle.replace(/\s+/g, "-")}-${date}.${ext}`;
      const objUrl = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = objUrl;
      a.download = filename;
      a.click();
      URL.revokeObjectURL(objUrl);
      onClose();
    } catch (exc) {
      setError(exc instanceof Error ? exc.message : "导出失败");
    } finally {
      setBusy(false);
    }
  };

  const counts: Array<[string, string]> = [
    ["轮次", String(turnCount)],
    ["消息", String(msgCount)],
    ["附件", String(artifactCount)],
    ["水位", `#${watermark}`],
  ];

  return (
    <>
    <Dialog
      open={open && !shareOpen}
      onOpenChange={(next) => { if (!next && !busy) onClose(); }}
      size="md"
      icon="download"
      tone="accent"
      title="导出会话"
      description={`快照以当前水位 #${watermark} 为基准，导出期间内容保持一致。`}
      dismissable={!busy}
      testId="conversation-export-dialog"
      footer={(
        <>
          <Button className="mr-auto" variant="ghost" disabled={busy} onClick={() => setShareOpen(true)}>分享只读快照</Button>
          <Button variant="ghost" onClick={onClose} disabled={busy}>取消</Button>
          <Button
            variant="primary"
            icon="download"
            loading={busy}
            onClick={() => void handleDownload()}
            data-testid="conversation-export-confirm"
          >
            {busy ? "导出中…" : "导出"}
          </Button>
        </>
      )}
    >
      <div className="flex flex-col gap-5 pb-2">
        <dl className="grid grid-cols-4 overflow-hidden rounded-xl border border-cx-border-subtle bg-cx-bg-subtle">
          {counts.map(([label, value]) => (
            <div key={label} className="flex flex-col gap-0.5 border-r border-cx-border-subtle px-3 py-2.5 last:border-r-0">
              <dt className="text-[11.5px] text-cx-fg-4">{label}</dt>
              <dd className="cx-tabular truncate font-cx-mono text-[14px] font-semibold text-cx-fg">{value}</dd>
            </div>
          ))}
        </dl>

        <section className="flex flex-col gap-2">
          <h3 className="text-[12.5px] font-semibold text-cx-fg-2">导出格式</h3>
          <div role="radiogroup" aria-label="导出格式" className="grid grid-cols-2 gap-2">
            {FORMATS.map((option) => {
              const selected = option.value === format;
              return (
                <button
                  key={option.value}
                  type="button"
                  role="radio"
                  aria-checked={selected}
                  onClick={() => setFormat(option.value)}
                  onKeyDown={(event) => {
                    if (!["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown"].includes(event.key)) return;
                    event.preventDefault();
                    const index = FORMATS.findIndex((item) => item.value === format);
                    const step = event.key === "ArrowLeft" || event.key === "ArrowUp" ? -1 : 1;
                    const next = FORMATS[(index + step + FORMATS.length) % FORMATS.length];
                    setFormat(next.value);
                    (event.currentTarget.parentElement?.querySelector<HTMLElement>(`[data-format="${next.value}"]`))?.focus();
                  }}
                  tabIndex={selected ? 0 : -1}
                  data-format={option.value}
                  className={cn(
                    "cx-export-format group relative flex flex-col items-start gap-2 rounded-xl border px-3 py-3 text-left transition-[border-color,background-color,box-shadow] duration-150",
                    selected
                      ? "border-cx-border-strong bg-cx-selected"
                      : "border-cx-border bg-cx-elevated hover:border-cx-border-strong hover:bg-cx-hover",
                  )}
                >
                  <span className="flex w-full items-center gap-2">
                    <span
                      className={cn(
                        "grid size-7 place-items-center rounded-lg",
                        selected ? "bg-cx-fg text-cx-bg" : "bg-cx-hover text-cx-fg-2",
                      )}
                    >
                      <Icon name={option.icon} size={14} />
                    </span>
                    <span className="min-w-0 flex-1 truncate text-[13.5px] font-semibold text-cx-fg">
                      {option.label}
                      <span className="ml-1.5 font-cx-mono text-[11.5px] font-normal text-cx-fg-4">{option.ext}</span>
                    </span>
                    <span
                      aria-hidden
                      className={cn(
                        "grid size-4 shrink-0 place-items-center rounded-full border transition-colors",
                        selected ? "border-cx-fg bg-cx-fg text-cx-bg" : "border-cx-border-strong",
                      )}
                    >
                      {selected ? <Icon name="check" size={10} /> : null}
                    </span>
                  </span>
                  <span className="text-[12px] leading-[18px] text-cx-fg-3">{option.description}</span>
                </button>
              );
            })}
          </div>
        </section>

        <section className="flex flex-col gap-3">
          <h3 className="text-[12.5px] font-semibold text-cx-fg-2">脱敏选项</h3>
          <Checkbox
            checked={excludeTools}
            onCheckedChange={setExcludeTools}
            label="排除工具调用参数"
            description="保留工具名称和摘要，移除调用参数。"
          />
          <Checkbox
            checked={excludePaths}
            onCheckedChange={setExcludePaths}
            label="脱敏绝对路径"
            description="绝对路径替换为 <path>。"
          />
        </section>

        {error ? <Callout tone="danger" role="alert" title="导出失败">{error}</Callout> : null}
      </div>
    </Dialog>
    <ShareDialog open={open && shareOpen} onClose={() => setShareOpen(false)} threadId={threadId} />
    </>
  );
}
