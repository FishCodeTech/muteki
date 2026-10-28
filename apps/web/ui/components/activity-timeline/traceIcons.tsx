import type { ReactNode } from "react";
import { EngineLogo } from "@/components/EngineLogo";
import type { EventKind, TraceNode } from "./traceModel";

/** One 24px grid, 2px round strokes, drawn to read at 12px inside a 20px tile. */
const GLYPHS: Record<string, ReactNode> = {
  agent: (
    <>
      <circle cx="12" cy="12" r="3.2" />
      <circle cx="5" cy="5.5" r="1.9" />
      <circle cx="19" cy="5.5" r="1.9" />
      <circle cx="12" cy="20" r="1.9" />
      <path d="M6.6 6.9 9.7 9.8M17.4 6.9l-3.1 2.9M12 15.2v2.9" />
    </>
  ),
  llm: (
    <>
      <path d="M11 3.5c.6 4.2 2.6 6.2 6.8 6.8-4.2.6-6.2 2.6-6.8 6.8-.6-4.2-2.6-6.2-6.8-6.8 4.2-.6 6.2-2.6 6.8-6.8Z" fill="currentColor" stroke="none" />
      <circle cx="18.6" cy="18.4" r="1.6" fill="currentColor" stroke="none" opacity=".55" />
    </>
  ),
  read: (
    <>
      <path d="M14 3.5H7.5a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2h9a2 2 0 0 0 2-2V8Z" />
      <path d="M14 3.5V8h4.5M9 12.5h6M9 16h4" />
    </>
  ),
  edit: (
    <>
      <path d="M4.5 19.5h4l10.2-10.2a2.2 2.2 0 0 0-4-4L4.5 15.5Z" />
      <path d="m13.2 6.8 4 4" />
    </>
  ),
  search: (
    <>
      <circle cx="11" cy="11" r="6.3" />
      <path d="m15.8 15.8 4 4" />
    </>
  ),
  web: (
    <>
      <circle cx="12" cy="12" r="8.3" />
      <path d="M3.9 12h16.2M12 3.8c2.3 2.4 3.4 5.1 3.4 8.2s-1.1 5.8-3.4 8.2c-2.3-2.4-3.4-5.1-3.4-8.2s1.1-5.8 3.4-8.2Z" />
    </>
  ),
  shell: (
    <>
      <rect x="3.5" y="4.5" width="17" height="15" rx="2.6" />
      <path d="m7.5 10 2.8 2.3-2.8 2.3M12.8 14.8h3.7" />
    </>
  ),
  test: (
    <>
      <path d="M9 3.5h6M10 3.5v5.8l-5 8.6a1.9 1.9 0 0 0 1.6 2.8h10.8a1.9 1.9 0 0 0 1.6-2.8l-5-8.6V3.5" />
      <path d="M7.6 14.5h8.8" />
    </>
  ),
  git: (
    <>
      <circle cx="6.5" cy="5.5" r="2" />
      <circle cx="6.5" cy="18.5" r="2" />
      <circle cx="17.5" cy="8" r="2" />
      <path d="M6.5 7.5v9M17.5 10c0 3.8-5.2 3.4-9.6 6.8" />
    </>
  ),
  plan: (
    <>
      <path d="M10.5 6.5h9M10.5 12h9M10.5 17.5h9" />
      <path d="m4 6.6 1.5 1.5L8 5.5M4 12.1l1.5 1.5L8 11" />
      <circle cx="5.8" cy="17.5" r="1.3" />
    </>
  ),
  message: <path d="M5.5 5h13a2 2 0 0 1 2 2v7.8a2 2 0 0 1-2 2h-6.7L7.5 20v-3.2h-2a2 2 0 0 1-2-2V7a2 2 0 0 1 2-2Z" />,
  reasoning: (
    <>
      <path d="M4 7.5h8M4 12h5.5M4 16.5h7.5" />
      <path d="M17.5 6c.3 2.2 1.4 3.3 3.6 3.6-2.2.3-3.3 1.4-3.6 3.6-.3-2.2-1.4-3.3-3.6-3.6 2.2-.3 3.3-1.4 3.6-3.6Z" fill="currentColor" stroke="none" />
    </>
  ),
  finding: (
    <>
      <path d="M9.3 17.5h5.4M10.2 20.5h3.6" />
      <path d="M12 3.5a5.8 5.8 0 0 0-3.4 10.5c.6.5 1 1.3 1 2.1v.4h4.8v-.4c0-.8.4-1.6 1-2.1A5.8 5.8 0 0 0 12 3.5Z" />
    </>
  ),
  approval: (
    <>
      <path d="M12 3.5 18.8 6v5c0 4.3-2.8 7.8-6.8 9.5-4-1.7-6.8-5.2-6.8-9.5V6Z" />
      <path d="m9.2 12 2 2 3.8-3.9" />
    </>
  ),
  result: (
    <>
      <circle cx="12" cy="12" r="8.3" />
      <path d="m8.6 12.2 2.3 2.3 4.6-4.7" />
    </>
  ),
  input: (
    <>
      <circle cx="12" cy="8" r="3.6" />
      <path d="M5 20c.8-3.6 3.6-5.6 7-5.6s6.2 2 7 5.6" />
    </>
  ),
  system: (
    <>
      <rect x="3.5" y="4.5" width="17" height="12" rx="2.2" />
      <path d="M9 20.5h6M12 16.5v4M8 10.5h2M12.5 10.5h3.5" />
    </>
  ),
  progress: (
    <>
      <path d="M4 18.5h4v-4h4v-4h4v-4h4" />
      <circle cx="20" cy="6.5" r="1.4" fill="currentColor" stroke="none" />
    </>
  ),
  guidance: (
    <>
      <circle cx="12" cy="12" r="8.3" />
      <path d="m15.6 8.4-2.2 5-5 2.2 2.2-5Z" />
    </>
  ),
  group: (
    <>
      <path d="m12 4 8 4-8 4-8-4Z" />
      <path d="m4 12 8 4 8-4M4 16l8 4 8-4" />
    </>
  ),
  alert: (
    <>
      <path d="M10.3 4.6 3.2 17.2a2 2 0 0 0 1.7 3h14.2a2 2 0 0 0 1.7-3L13.7 4.6a2 2 0 0 0-3.4 0Z" />
      <path d="M12 9.5v4M12 16.8v.1" />
    </>
  ),
  chevron: <path d="m7 9.5 5 5 5-5" />,
  close: <path d="M7 7l10 10M17 7 7 17" />,
  play: <path d="M8 5.5v13l10.5-6.5Z" fill="currentColor" />,
  pause: <path d="M8.5 5.5v13M15.5 5.5v13" strokeWidth="3" />,
  live: (
    <>
      <circle cx="12" cy="12" r="2.6" fill="currentColor" stroke="none" />
      <path d="M7.6 7.6a6.2 6.2 0 0 0 0 8.8M16.4 7.6a6.2 6.2 0 0 1 0 8.8" />
    </>
  ),
  prevError: <path d="M14.5 6 8.5 12l6 6M5.5 6v12" />,
  nextError: <path d="m9.5 6 6 6-6 6M18.5 6v12" />,
  stack: (
    <>
      <rect x="4" y="4.5" width="16" height="6" rx="1.6" />
      <rect x="4" y="13.5" width="16" height="6" rx="1.6" />
    </>
  ),
  side: (
    <>
      <rect x="4" y="4.5" width="7" height="15" rx="1.6" />
      <rect x="13" y="4.5" width="7" height="15" rx="1.6" />
    </>
  ),
  fit: <path d="M4.5 9V5.5a1 1 0 0 1 1-1H9M15 4.5h3.5a1 1 0 0 1 1 1V9M19.5 15v3.5a1 1 0 0 1-1 1H15M9 19.5H5.5a1 1 0 0 1-1-1V15" />,
  spark: (
    <>
      <path d="M11 3c.5 3.8 2.3 5.6 6.1 6.1-3.8.5-5.6 2.3-6.1 6.1-.5-3.8-2.3-5.6-6.1-6.1C8.7 8.6 10.5 6.8 11 3Z" fill="currentColor" stroke="none" />
      <circle cx="18.5" cy="18" r="1.7" fill="currentColor" stroke="none" opacity=".55" />
    </>
  ),
};

export type GlyphName = keyof typeof GLYPHS;

export function TraceGlyph({ name, size = 12, className }: { name: GlyphName | string; size?: number; className?: string }) {
  return (
    <svg
      className={className}
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={2}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      focusable="false"
    >
      {GLYPHS[name] ?? GLYPHS.agent}
    </svg>
  );
}

const SPAN_GLYPH: Record<TraceNode["spec"]["kind"], GlyphName> = {
  agent: "agent",
  worker: "agent",
  group: "group",
  llm: "llm",
  tool: "shell",
  event: "alert",
};

export function SpanIcon({ node, size = 20 }: { node: TraceNode; size?: number }) {
  const { spec } = node;
  const kind = spec.kind === "worker" || spec.kind === "group" ? "agent" : spec.kind;
  const status = node.status === "failed" ? "failed" : undefined;
  const engine = (spec.kind === "agent" || spec.kind === "worker") && spec.engine;
  return (
    <span className="rt-tile" data-kind={kind} data-status={status} data-logo={engine ? "" : undefined} style={{ width: size, height: size }}>
      {engine ? (
        <EngineLogo engine={engine} size={Math.round(size * 0.6)} />
      ) : (
        <TraceGlyph name={spec.kind === "tool" ? spec.tool ?? "shell" : SPAN_GLYPH[spec.kind]} size={Math.round(size * 0.6)} />
      )}
    </span>
  );
}

const EVENT_GLYPH: Record<Exclude<EventKind, "call">, GlyphName> = {
  message: "message",
  reasoning: "reasoning",
  finding: "finding",
  approval: "approval",
  result: "result",
  input: "input",
  system: "system",
  progress: "progress",
  guidance: "guidance",
};

export function EventIcon({ kind, size = 20 }: { kind: EventKind; size?: number }) {
  return (
    <span className="rt-tile" data-event={kind} style={{ width: size, height: size }}>
      <TraceGlyph name={kind === "call" ? "shell" : EVENT_GLYPH[kind]} size={Math.round(size * 0.6)} />
    </span>
  );
}
