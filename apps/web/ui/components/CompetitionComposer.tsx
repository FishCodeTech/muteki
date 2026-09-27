"use client";

import { Button } from "@heroui/react";

/**
 * CompetitionComposer：比赛聊天输入与结构化操作卡片流（设计 14.4）。
 *
 * - 自然语言消息经 POST /messages 只记录为聊天事件，不直接改状态；
 *   附带结构化命令时响应携带 command receipt（实际操作记录）。
 * - 卡片流渲染 reducer 产出的 ActivityCard：连接测试、同步差异、
 *   调度 / 准入、实例、子 Run（run_id 链接 /run/[id]）、提交回执、
 *   失败分类与 Advisor 建议。
 * - 快捷按钮：同步、启动 / 暂停调度（消息 + 结构化命令一步完成）。
 */

import { useEffect, useRef, useState } from "react";
import type { CSSProperties } from "react";
import Link from "next/link";
import type { ActivityCard, ChatMessage } from "@/lib/competition-events";

const muted: CSSProperties = { color: "var(--muted)", fontSize: 11 };
const mono: CSSProperties = { fontFamily: "var(--font-mono)", fontSize: 10.5 };
const btn: CSSProperties = {
  height: 30,
  padding: "0 12px",
  border: "1px solid var(--line2)",
  borderRadius: 8,
  background: "var(--panel2)",
  color: "var(--text)",
  fontSize: 12,
  fontWeight: 650,
  cursor: "pointer",
};

const TONE_COLORS: Record<string, string> = {
  info: "var(--blue)",
  ok: "var(--green)",
  warn: "var(--amber)",
  error: "var(--red)",
};

function CardView({ card }: { card: ActivityCard }) {
  const color = TONE_COLORS[card.tone] ?? "var(--muted)";
  return (
    <div
      style={{
        border: `1px solid color-mix(in srgb, ${color} 32%, var(--line))`,
        borderRadius: 10,
        padding: "8px 10px",
        display: "grid",
        gap: 3,
        background: `color-mix(in srgb, ${color} 3%, var(--panel))`,
      }}
    >
      <div style={{ display: "flex", gap: 8, alignItems: "baseline", flexWrap: "wrap" }}>
        <span style={{ fontSize: 11.5, fontWeight: 750, color }}>
          {card.title}
        </span>
        <span style={muted}>
          {card.at ? new Date(card.at).toLocaleTimeString() : ""}
        </span>
        {card.runId ? (
          <Link
            href={`/run/${encodeURIComponent(card.runId)}`}
            style={{ ...mono, color: "var(--blue)" }}
          >
            打开子 Run →
          </Link>
        ) : null}
      </div>
      {card.lines.map((line, i) => (
        <div key={i} style={{ ...mono, color: "var(--text)" }}>
          {line}
        </div>
      ))}
      {card.commandId ? (
        <div style={{ ...mono, color: "var(--muted)" }}>
          command {card.commandId}
        </div>
      ) : null}
    </div>
  );
}

export function CompetitionComposer({
  messages,
  cards,
  busy,
  schedulerState,
  onSend,
  onSync,
  onScheduler,
}: {
  messages: ChatMessage[];
  cards: ActivityCard[];
  busy: boolean;
  schedulerState: string;
  onSend: (text: string) => void;
  onSync: () => void;
  onScheduler: (action: "start" | "pause" | "resume") => void;
}) {
  const [text, setText] = useState("");
  const feedRef = useRef<HTMLDivElement | null>(null);

  // 合并消息与卡片为一条时间线。
  const timeline = [
    ...messages.map((m) => ({ kind: "msg" as const, at: m.at, seq: m.seq, m })),
    ...cards.map((c) => ({ kind: "card" as const, at: c.at, seq: c.seq, c })),
  ].sort((a, b) => (a.seq - b.seq) || String(a.at).localeCompare(String(b.at)));

  useEffect(() => {
    const el = feedRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [timeline.length]);

  return (
    <div style={{ display: "grid", gridTemplateRows: "auto 1fr auto", gap: 8, minHeight: 0, height: "100%" }}>
      <div style={{ display: "flex", gap: 6, flexWrap: "wrap" }}>
        <Button style={btn} isDisabled={busy} onClick={onSync}>
          同步比赛
        </Button>
        {schedulerState === "stopped" && (
          <Button style={btn} isDisabled={busy} onClick={() => onScheduler("start")}>
            启动调度
          </Button>
        )}
        {schedulerState === "running" && (
          <Button style={btn} isDisabled={busy} onClick={() => onScheduler("pause")}>
            暂停调度
          </Button>
        )}
        {schedulerState === "paused" && (
          <Button style={btn} isDisabled={busy} onClick={() => onScheduler("resume")}>
            恢复调度
          </Button>
        )}
      </div>

      <div
        ref={feedRef}
        style={{
          minHeight: 0,
          overflowY: "auto",
          display: "grid",
          gap: 8,
          alignContent: "start",
          padding: 2,
        }}
      >
        {!timeline.length ? (
          <div style={{ ...muted, padding: 12 }}>
            还没有消息。自然语言消息只记录为聊天事件；状态变化都通过结构化命令
            与回执完成。
          </div>
        ) : (
          timeline.map((item) =>
            item.kind === "msg" ? (
              <div
                key={item.m.key}
                style={{
                  justifySelf: "start",
                  maxWidth: "92%",
                  border: "1px solid var(--line)",
                  borderRadius: 10,
                  padding: "7px 10px",
                  background: item.m.local ? "var(--panel2)" : "var(--panel)",
                  display: "grid",
                  gap: 3,
                }}
              >
                <div style={{ display: "flex", gap: 8, ...muted }}>
                  <span>{item.m.author}</span>
                  <span>{new Date(item.m.at).toLocaleTimeString()}</span>
                  {item.m.local ? <span>发送中…</span> : null}
                </div>
                <div style={{ fontSize: 12.5, whiteSpace: "pre-wrap" }}>
                  {item.m.text}
                </div>
                {item.m.commandId ? (
                  <div style={{ ...mono, color: "var(--muted)" }}>
                    附结构化命令 {item.m.commandId}
                    {item.m.receiptState ? `（${item.m.receiptState}）` : ""}
                  </div>
                ) : null}
              </div>
            ) : (
              <CardView key={item.c.key} card={item.c} />
            ),
          )
        )}
      </div>

      <form
        noValidate
        style={{ display: "flex", gap: 6 }}
        onSubmit={(e) => {
          e.preventDefault();
          const t = text.trim();
          if (!t) return;
          onSend(t);
          setText("");
        }}
      >
        <label className="sr-only" htmlFor="competition-message">比赛消息</label>
        <input
          id="competition-message"
          style={{
            flex: 1,
            height: 34,
            padding: "0 12px",
            border: "1px solid var(--line2)",
            borderRadius: 9,
            background: "var(--panel2)",
            color: "var(--bright)",
            fontSize: 12.5,
            minWidth: 0,
          }}
          placeholder="给比赛发送消息（不直接改状态）…"
          value={text}
          onChange={(e) => setText(e.target.value)}
          disabled={busy}
        />
        <Button style={btn} type="submit" isDisabled={busy || !text.trim()}>
          发送
        </Button>
      </form>
    </div>
  );
}
