"use client";

import { useState } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "@muteki/ui";
import { Collapse } from "@muteki/ui";

export interface HostNotice {
  label: string;
  subject?: string;
  body: string;
  childId?: string;
}

const CHILD_STATE: Record<string, string> = {
  reported: "已汇报",
  updated: "已更新汇报",
  unreported: "未汇报就结束",
  failed: "已失败",
  completed: "已完成",
};

function shortId(id: string): string {
  return id.length > 8 ? id.slice(0, 8) : id;
}

export function parseHostNotice(text: string): HostNotice | null {
  const source = String(text || "").trim();
  if (!source) return null;

  let match = /^\[control\] child (\S+) \(([^)]*)\) (reported|unreported; final message, not a report|failed|completed)\n?([\s\S]*)$/.exec(source);
  if (match) {
    const state = match[3] === "unreported; final message, not a report" ? "unreported" : match[3];
    return { label: CHILD_STATE[state] || state, subject: match[2], body: match[4].trim(), childId: match[1] };
  }

  match = /^\[control\] child (\S+) needs (\w+)\.[^\n]*\n([\s\S]*)$/.exec(source);
  if (match) {
    return {
      label: match[2] === "approval" ? "等待审批" : "在提问",
      subject: shortId(match[1]),
      body: match[3].trim(),
      childId: match[1],
    };
  }

  match = /^\[control\] thread (\S+) \("([^"]*)"\) (\w+): ([\s\S]*)$/.exec(source);
  if (match) {
    return { label: match[3], subject: match[2], body: match[4].trim(), childId: match[1] };
  }

  match = /^\[control\] thread (\S+): ([^\n]*)\n?([\s\S]*)$/.exec(source);
  if (match) {
    return { label: match[2].trim(), subject: shortId(match[1]), body: match[3].trim(), childId: match[1] };
  }

  match = /^\[control\] grant (approved|declined): ([\s\S]*)$/.exec(source);
  if (match) {
    return { label: match[1] === "approved" ? "授权已批准" : "授权已拒绝", body: match[2].trim() };
  }

  match = /^\[orchestrate\] thread (\S+) \("([^"]*)"\) is waiting for approval: ([\s\S]*)$/.exec(source);
  if (match) {
    return { label: "等待审批", subject: match[2], body: match[3].trim(), childId: match[1] };
  }

  match = /^\[orchestrate\] thread (\S+) \("([^"]*)"\) is asking \(([\s\S]*)$/.exec(source);
  if (match) {
    return { label: "在提问", subject: match[2], body: match[3].trim(), childId: match[1] };
  }

  match = /^\[orchestrate\] thread (\S+) \("([^"]*)"\) (reported|updated|unreported|failed|completed)(?:\.\d+)?\n?([\s\S]*)$/.exec(source);
  if (match) {
    return { label: CHILD_STATE[match[3]] || match[3], subject: match[2], body: match[4].trim(), childId: match[1] };
  }

  match = /^Update on pull request #(\d+) \(([^)]*)\)[^\n]*\n([\s\S]*)$/.exec(source);
  if (match) {
    return { label: "更新", subject: `拉取请求 #${match[1]}`, body: match[3].trim() };
  }

  match = /^(?:\S+ )?stopped watching pull request #(\d+)[^\n]*\n?([\s\S]*)$/.exec(source);
  if (match) {
    return { label: "已停止关注", subject: `拉取请求 #${match[1]}`, body: match[2].trim() };
  }

  match = /^\[control\] ([\s\S]+)$/.exec(source);
  if (match) {
    const [first = "", ...rest] = match[1].split("\n");
    return { label: first.trim(), body: rest.join("\n").trim() };
  }

  return null;
}

export function HostNoticeRow({
  notice,
  onOpenThread,
}: {
  notice: HostNotice;
  onOpenThread?: (threadId: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const caption = [notice.subject, notice.label].filter(Boolean).join(" · ");
  return (
    <div className="flex w-full flex-col items-center" data-testid="host-notice">
      <button
        type="button"
        onClick={() => setOpen((value) => !value)}
        aria-expanded={open}
        className="group inline-flex max-w-full items-center gap-1.5 rounded-full px-2.5 py-1 text-cx-xs text-cx-fg-3 outline-none transition-colors hover:bg-cx-hover hover:text-cx-fg-2 focus-visible:ring-2 focus-visible:ring-cx-focus"
      >
        <Icon name="bot" size={12} className="shrink-0 text-cx-fg-4" />
        <span className="min-w-0 truncate">{caption}</span>
        {notice.childId && onOpenThread ? (
          <span
            role="link"
            tabIndex={-1}
            className="shrink-0 text-cx-accent hover:underline"
            onClick={(event) => {
              event.stopPropagation();
              onOpenThread(notice.childId!);
            }}
          >
            打开委派线程
          </span>
        ) : null}
        <span aria-hidden className={cn("cx-chevron inline-flex shrink-0 text-cx-fg-4")} data-open={open || undefined}>
          <Icon name="chevronRight" size={11} />
        </span>
      </button>
      <Collapse open={open} enterDuration={0.24} exitDuration={0.18}>
        <div className="cx-scroll mt-1 max-h-80 w-full max-w-xl overflow-y-auto whitespace-pre-wrap break-words rounded-xl border border-cx-border-subtle bg-cx-bg-subtle px-3 py-2 font-cx-mono text-cx-xs leading-5 text-cx-fg-3">
          {notice.body || "（无输出）"}
        </div>
      </Collapse>
    </div>
  );
}
