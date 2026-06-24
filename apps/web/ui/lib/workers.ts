/**
 * Shared worker-identity helpers — engine, deterministic color, initials.
 *
 * Colour is keyed by ENGINE (not a hash of the id) so the same engine always
 * reads the same colour across the deck, matching the command-deck mockup:
 *   claude = cyan · codex = pink · cursor = violet · reason/deepseek = amber.
 * The DeepSeek `reason` actor is the coordinator (see lib/events.ts split), so
 * it gets the amber coordinator colour rather than a worker hue.
 */

// literal hexes (mirror the :root --eng-* tokens) — used directly in inline styles
// so CSS color-mix() works without a var-in-var indirection. KEEP IN SYNC with the
// light-theme palette in app/globals.css :root (pushed deeper so they read on white).
const ENGINE_COLOR: Record<string, string> = {
  claude: "#0891b2", // --cyan
  codex: "#db2777", // --pink
  cursor: "#7c3aed", // --violet
  reason: "#b45309", // --amber (coordinator)
  deepseek: "#b45309",
  verifier: "#ca8a04", // --gold (deeper than --gold body token for chip contrast)
};
const DEFAULT_COLOR = "#2563eb"; // --blue

export function workerEngine(id: string, engine?: string): string {
  const s = (engine || id).toLowerCase();
  if (s === "claude_code" || s === "claude") return "Claude Code";
  if (s === "codex_cli" || s === "codex") return "Codex";
  if (s === "cursor_agent" || s === "cursor") return "Cursor";
  if (s.includes("codex")) return "Codex";
  if (s.includes("cursor")) return "Cursor";
  if (s.includes("claude")) return "Claude Code";
  if (s.includes("deepseek") || s === "reason") return "DeepSeek";
  if (s.includes("mock")) return "Mock";
  if (s.includes("verifier")) return "Verifier";
  return "Worker";
}

export function workerColor(id: string, engine?: string): string {
  const s = (engine || id).toLowerCase();
  if (s === "claude_code") return ENGINE_COLOR.claude;
  if (s === "codex_cli") return ENGINE_COLOR.codex;
  if (s === "cursor_agent") return ENGINE_COLOR.cursor;
  for (const key of Object.keys(ENGINE_COLOR)) {
    if (s.includes(key)) return ENGINE_COLOR[key];
  }
  return DEFAULT_COLOR;
}

export function workerInitial(id: string): string {
  const tail = id.split(/[-_:]/).filter(Boolean).pop() || id;
  return tail.slice(0, 2).toUpperCase();
}

export function workerShortLabel(id: string, engine?: string): string {
  const raw = (id || "").toLowerCase();
  if (raw === "reason" || raw === "solver" || raw === "coordinator") return raw;
  const resolved = workerEngine(id, engine);
  const base = resolved === "Claude Code" ? "claude"
    : resolved === "Worker" ? (id.split(/[:]/).pop() || id).replace(/^cli-/, "")
    : resolved.toLowerCase().replace(/\s+/g, "-");
  const suffix = id.match(/[-_:](\d+)$/)?.[1];
  return suffix ? `${base.replace(/-\d+$/, "")}-${suffix}` : base;
}

/** The resume/attach command for a live CLI session, by engine. */
export function resumeCommand(engine: string, session: string): string {
  if (!session) return "";
  const resolved = workerEngine(engine, engine);
  if (resolved === "Codex") return `codex exec resume ${session}`;
  if (resolved === "Cursor") return `cursor-agent --resume ${session}`;
  return `claude -r ${session}`;
}
