"use client";

import { useEffect, useRef, useState } from "react";
import { ChatMessage, HitlRequest } from "@/lib/events";

/**
 * ChatGPT/Claude-style conversation over the run. READ side: the agent's
 * reasoning / text / tool calls / insights / human-guidance echoes stream in as
 * bubbles. WRITE side: the operator types — plain text is a hint, or a slash
 * command (/redirect /pause /resume /focus /submit) — and it is delivered to the
 * live swarm through the HITL backend (InsightBus GUIDANCE), truly commanding the
 * agents mid-run, not just annotating a log.
 */

const ACTIONS = [
  { key: "hint", label: "Hint", hint: "weigh this highly" },
  { key: "redirect", label: "Redirect", hint: "change course now" },
  { key: "focus", label: "Focus", hint: "concentrate here" },
  { key: "pause", label: "Pause", hint: "idle the swarm" },
  { key: "resume", label: "Resume", hint: "un-idle" },
];

function bubbleClass(m: ChatMessage): string {
  if (m.role === "human") return "msg human";
  if (m.role === "system") return `msg system ${m.kind}`;
  return `msg agent ${m.kind}`;
}

function speaker(m: ChatMessage): string {
  if (m.role === "human") return "you";
  if (m.role === "system") return m.kind === "insight" ? "insight bus" : "system";
  return m.solverId ? m.solverId : "agent";
}

export function Chat({
  messages,
  hitlRequests,
  onCommand,
  solvers,
}: {
  messages: ChatMessage[];
  hitlRequests: HitlRequest[];
  onCommand: (target: string, action: string, text: string) => void;
  solvers: string[];
}) {
  const [text, setText] = useState("");
  const [target, setTarget] = useState("global");
  const endRef = useRef<HTMLDivElement>(null);
  const stick = useRef(true);

  useEffect(() => {
    if (stick.current) endRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [messages]);

  const send = (action: string, payloadOverride?: string) => {
    const raw = payloadOverride ?? text.trim();
    if (action === "pause" || action === "resume") {
      onCommand(target, action, "");
    } else {
      let a = action;
      let payload = raw;
      if (raw.startsWith("/")) {
        const [verb, ...rest] = raw.slice(1).split(" ");
        a = verb;
        payload = rest.join(" ");
      }
      if (!payload && a !== "pause" && a !== "resume") return;
      onCommand(target, a, payload);
    }
    setText("");
  };

  return (
    <div className="chat">
      <div className="chat-scroll" onScroll={(e) => {
        const el = e.currentTarget;
        stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 60;
      }}>
        {messages.length === 0 && (
          <div className="chat-empty">
            The conversation appears here as the swarm reasons. Type a hint to steer it,
            or use <code>/redirect</code>, <code>/pause</code>, <code>/resume</code>,
            <code>/focus</code>, <code>/submit flag&#123;…&#125;</code>.
          </div>
        )}
        {messages.map((m) => (
          <div key={m.id} className={bubbleClass(m)}>
            <div className="who">{speaker(m)} <span className="k">{m.kind}</span></div>
            <div className="body">{m.content}</div>
          </div>
        ))}
        {hitlRequests.map((r) => (
          <HitlCard key={r.id} req={r} onAnswer={(opt) => onCommand("global", "submit", opt)} />
        ))}
        <div ref={endRef} />
      </div>

      <div className="composer">
        <div className="composer-row">
          <select value={target} onChange={(e) => setTarget(e.target.value)} title="who to command">
            <option value="global">all solvers</option>
            {solvers.map((s) => (
              <option key={s} value={`solver:${s}`}>{s}</option>
            ))}
          </select>
          <input
            value={text}
            onChange={(e) => setText(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && !e.shiftKey && (e.preventDefault(), send("hint"))}
            placeholder="steer the swarm — hint text, or /redirect /focus /submit flag{…}"
          />
          <button className="primary" onClick={() => send("hint")}>Send</button>
        </div>
        <div className="quick">
          {ACTIONS.map((a) => (
            <button key={a.key} title={a.hint} onClick={() => send(a.key)}>{a.label}</button>
          ))}
        </div>
      </div>
    </div>
  );
}

function HitlCard({ req, onAnswer }: { req: HitlRequest; onAnswer: (opt: string) => void }) {
  const [free, setFree] = useState("");
  return (
    <div className="hitl-card">
      <div className="who">agent <span className="k">needs a decision</span></div>
      <div className="body">{req.prompt}</div>
      <div className="hitl-opts">
        {req.options.map((o) => (
          <button key={o} onClick={() => onAnswer(o)}>{o}</button>
        ))}
        <input
          value={free}
          placeholder="or type an answer…"
          onChange={(e) => setFree(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && free.trim() && onAnswer(free.trim())}
        />
      </div>
    </div>
  );
}
