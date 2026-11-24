"use client";

import Link from "next/link";
import { useEffect, useState } from "react";
import { Badge } from "@muteki/ui";
import { SettingsEmpty, SettingsNote } from "@/components/settings/primitives";
import { apiFetch } from "@/lib/useRun";

type ReviewRecord = {
  id: string;
  thread_id: string;
  recorded_at: number;
  coverage: string;
  items: Array<{ thread_id: string; group: string; confidence: string; evidence: string[]; suggestion?: string }>;
};

const GROUPS: { [key: string]: { label: string; tone: "neutral" | "accent" | "success" | "warning" } } = {
  continue: { label: "继续处理", tone: "accent" },
  needs_decision: { label: "需要决定", tone: "warning" },
  waiting_on_others: { label: "等待其他人", tone: "neutral" },
  can_settle: { label: "可以收尾", tone: "success" },
  parked_or_superseded: { label: "已搁置或被取代", tone: "neutral" },
  unsure: { label: "不确定", tone: "neutral" },
};
const CONFIDENCE: { [key: string]: string } = { clear_evidence: "证据明确", inference: "推断", unsure: "不确定" };

export function ControlReviewRecords({ hasOtherReviews }: { hasOtherReviews: boolean }) {
  const [records, setRecords] = useState<ReviewRecord[] | null>(null);
  const [error, setError] = useState("");
  useEffect(() => {
    let live = true;
    apiFetch("/api/control/review-records")
      .then(async (response) => {
        const data = await response.json().catch(() => ({}));
        if (!response.ok) throw new Error(data?.error?.message || `读取审阅记录失败（HTTP ${response.status}）`);
        if (live) setRecords(data.records ?? []);
      })
      .catch((cause) => {
        if (live) setError(cause instanceof Error ? cause.message : "读取审阅记录失败");
      });
    return () => {
      live = false;
    };
  }, []);

  if (error) return <div className="border-b border-cx-border-subtle px-5 py-3 last:border-b-0"><SettingsNote tone="danger">{error}</SettingsNote></div>;
  if (records === null) return hasOtherReviews ? null : <SettingsEmpty>正在读取审阅记录…</SettingsEmpty>;
  if (records.length === 0) return hasOtherReviews ? null : <SettingsEmpty>还没有审阅记录。线程用 muteki-collab 审阅其他线程后，结论会出现在这里。</SettingsEmpty>;
  return (
    <>
      {records.map((record) => (
        <div key={record.id} className="flex flex-col gap-3 border-b border-cx-border-subtle px-5 py-4 last:border-b-0">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <span className="flex min-w-0 items-center gap-2">
              <Link
                className="truncate text-cx-sm font-medium text-cx-fg hover:text-cx-accent hover:underline"
                href={`/chat/${encodeURIComponent(record.thread_id)}`}
              >
                对话审阅
              </Link>
              <Badge tone="neutral">{record.items.length} 个线程</Badge>
            </span>
            <time className="cx-tabular text-cx-xs text-cx-fg-3" dateTime={new Date(record.recorded_at * 1000).toISOString()}>
              {new Date(record.recorded_at * 1000).toLocaleString()}
            </time>
          </div>
          {record.coverage ? <p className="whitespace-pre-wrap text-cx-sm leading-5 text-cx-fg-2">{record.coverage}</p> : null}
          {record.items.length > 0 ? (
            <ul className="flex flex-col gap-2">
              {record.items.map((item, index) => {
                const group = GROUPS[item.group];
                return (
                  <li key={`${item.thread_id}:${index}`} className="flex flex-col gap-1.5 rounded-xl bg-cx-bg-subtle px-3.5 py-3">
                    <div className="flex min-w-0 flex-wrap items-center gap-2">
                      <Link
                        className="min-w-0 truncate font-mono text-cx-xs text-cx-fg-2 hover:text-cx-accent hover:underline"
                        href={`/chat/${encodeURIComponent(item.thread_id)}`}
                        title={item.thread_id}
                      >
                        {item.thread_id}
                      </Link>
                      <Badge tone={group?.tone ?? "neutral"}>{group?.label ?? item.group}</Badge>
                      <span className="text-cx-xs text-cx-fg-3">{CONFIDENCE[item.confidence] ?? item.confidence}</span>
                    </div>
                    {item.evidence.map((evidence, evidenceIndex) => (
                      <p key={evidenceIndex} className="whitespace-pre-wrap break-words text-cx-sm leading-5 text-cx-fg-2">{evidence}</p>
                    ))}
                    {item.suggestion ? (
                      <p className="whitespace-pre-wrap break-words text-cx-sm leading-5 text-cx-fg">
                        <span className="text-cx-fg-3">建议：</span>{item.suggestion}
                      </p>
                    ) : null}
                  </li>
                );
              })}
            </ul>
          ) : null}
        </div>
      ))}
    </>
  );
}
