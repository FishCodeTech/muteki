
import { ENGINE_HUE, currentMode, hueColor } from "./palette-engine";

const defaultColor = (): string => hueColor(250, currentMode(), 0.18);

const engineSegment = (s: string): string => {
  const segs = s.split(/[-_:]/);
  if (segs.includes("pi")) return "pi";
  if (segs.includes("omp") || segs.includes("ohmypi")) return "omp";
  if (segs.includes("agy")) return "agy";
  return "";
};

export function workerEngine(id: string, engine?: string): string {
  const s = (engine || id).toLowerCase();
  if (s === "claude_code" || s === "claude") return "Claude Code";
  if (s === "codex_cli" || s === "codex") return "Codex";
  if (s === "cursor_agent" || s === "cursor") return "Cursor";
  if (s === "kimi_code" || s === "kimi" || s === "kimi-code") return "Kimi Code";
  if (s === "grok_cli" || s === "grok") return "Grok";
  if (s === "opencode_cli" || s === "opencode") return "OpenCode";
  if (s === "devin_cli" || s === "devin") return "Devin CLI";
  if (s === "droid" || s === "droid_cli" || s === "factory_droid") return "Droid";
  if (s === "agy" || s === "agy_cli" || s === "antigravity") return "Antigravity";
  if (s === "dsh_sdk_worker" || s === "dsh" || s === "deepseek_harness") return "DeepSeek Harness";
  if (s === "pi" || s === "omp" || s === "oh_my_pi" || s === "oh-my-pi" || s === "ohmypi") {
    return s === "pi" ? "Pi" : "Oh My Pi";
  }
  const seg = engineSegment(s);
  if (seg === "pi") return "Pi";
  if (seg === "omp") return "Oh My Pi";
  if (seg === "agy") return "Antigravity";
  if (s.includes("codex")) return "Codex";
  if (s.includes("cursor")) return "Cursor";
  if (s.includes("claude")) return "Claude Code";
  if (s.includes("kimi")) return "Kimi Code";
  if (s.includes("grok")) return "Grok";
  if (s.includes("opencode")) return "OpenCode";
  if (s.includes("droid")) return "Droid";
  if (s.includes("devin")) return "Devin CLI";
  if (s.includes("antigravity")) return "Antigravity";
  if (s.includes("dsh") || s.includes("deepseek-harness")) return "DeepSeek Harness";
  if (s.includes("deepseek") || s === "reason") return "DeepSeek";
  if (s.includes("mock")) return "Mock";
  if (s.includes("verifier")) return "Verifier";
  return "Worker";
}

export function workerEngineKey(id: string, engine?: string): string {
  const s = (engine || id).toLowerCase();
  if (s === "claude_code") return "claude";
  if (s === "codex_cli") return "codex";
  if (s === "cursor_agent") return "cursor";
  const seg = engineSegment(s);
  if (seg) return seg;
  if (s === "antigravity") return "agy";
  for (const k of Object.keys(ENGINE_HUE)) {
    if (k === "pi" || k === "omp" || k === "agy") continue;
    if (s.includes(k)) return k;
  }
  return "";
}

export function workerColor(id: string, engine?: string): string {
  const key = workerEngineKey(id, engine);
  return key ? hueColor(ENGINE_HUE[key], currentMode()) : defaultColor();
}

export function workerColorVar(id: string, engine?: string): string {
  const key = workerEngineKey(id, engine);
  return key ? `var(--eng-${key}, var(--blue))` : "var(--eng-default)";
}

export function workerInitial(id: string): string {
  const tail = id.split(/[-_:]/).filter(Boolean).pop() || id;
  return tail.slice(0, 2).toUpperCase();
}

export { workerGeneration } from "./events";

function workerShortLabel(id: string, engine?: string): string {
  const raw = (id || "").toLowerCase();
  if (raw === "reason" || raw === "solver" || raw === "coordinator") return raw;
  const resolved = workerEngine(id, engine);
  const base = resolved === "Claude Code" ? "claude"
    : resolved === "Oh My Pi" ? "omp"
    : resolved === "Worker" ? (id.split(/[:]/).pop() || id).replace(/^cli-/, "")
    : resolved.toLowerCase().replace(/\s+/g, "-");
  const gen = id.match(/-g(\d+)$/)?.[1];
  const stem = gen ? id.slice(0, -(`-g${gen}`.length)) : id;
  const suffix = stem.match(/[-_:](\d+)$/)?.[1];
  const core = suffix ? `${base.replace(/-\d+$/, "")}-${suffix}` : base;
  return gen ? `${core}·g${gen}` : core;
}

export type WorkerConnectionKind = "official" | "custom_endpoint" | "system";

export type WorkerIdentity = {
  solverId: string;
  engine?: string;
  profileId?: string;
  profileLabel?: string;
  model?: string;
  accountId?: string;
  endpointHost?: string;
  connection?: WorkerConnectionKind;
  provider?: string;
};

export type WorkerDisplayName = {
  title: string;
  initial: string;
  titleAttr: string;
  model?: string;
  connectionKind?: WorkerConnectionKind;
  connectionDetail?: string;
};

function workerInitialFromLabel(label: string, fallbackId: string): string {
  const text = (label || "").trim();
  if (!text) return workerInitial(fallbackId);
  if (/[\u3400-\u9fff]/.test(text)) return text.slice(0, 2);
  const parts = text.split(/[\s\-_:/]+/).filter(Boolean);
  if (parts.length >= 2) return (parts[0][0] + parts[1][0]).toUpperCase();
  return text.slice(0, 2).toUpperCase();
}

function workerIdentityKey(lane: WorkerIdentity): string {
  return lane.profileId || lane.profileLabel || lane.solverId;
}

function workerInstanceIndex(
  id: string,
  siblings: WorkerIdentity[],
): { index: number; count: number } {
  const self = siblings.find((lane) => lane.solverId === id);
  const key = self ? workerIdentityKey(self) : id;
  const group = siblings.filter((lane) => workerIdentityKey(lane) === key);
  const index = Math.max(1, group.findIndex((lane) => lane.solverId === id) + 1);
  return { index, count: group.length };
}

export function toWorkerIdentity(
  id: string,
  lane?: {
    engine?: string;
    profileId?: string;
    profileLabel?: string;
    model?: string;
    accountId?: string;
    endpointHost?: string;
    connection?: WorkerConnectionKind;
    provider?: string;
  } | null,
): WorkerIdentity {
  return {
    solverId: id,
    engine: lane?.engine,
    profileId: lane?.profileId,
    profileLabel: lane?.profileLabel,
    model: lane?.model,
    accountId: lane?.accountId,
    endpointHost: lane?.endpointHost,
    connection: lane?.connection,
    provider: lane?.provider,
  };
}

export function workerDisplayName(
  id: string,
  lane?: WorkerIdentity | null,
  siblings: WorkerIdentity[] = [],
): WorkerDisplayName {
  const identity = lane ? { ...lane, solverId: lane.solverId || id } : { solverId: id };
  const baseTitle = (identity.profileLabel || "").trim() || workerShortLabel(id, identity.engine);
  const group = siblings.length ? siblings : [identity];
  const { index, count } = workerInstanceIndex(id, group);
  const title = count > 1 && index > 1 ? `${baseTitle} · ${index}` : baseTitle;
  const connectionDetail = identity.connection === "custom_endpoint"
    ? (identity.provider || identity.endpointHost || "")
    : identity.connection === "official"
      ? (identity.accountId || "")
      : identity.connection === "system"
        ? ""
        : (identity.provider || identity.endpointHost || identity.accountId || "");
  return {
    title,
    initial: workerInitialFromLabel(baseTitle, id),
    titleAttr: [id, identity.model, connectionDetail].filter(Boolean).join(" · "),
    model: identity.model,
    connectionKind: identity.connection,
    connectionDetail,
  };
}

export function formatWorkerSubtitle(
  display: WorkerDisplayName,
  t: (key: string) => string,
): string {
  const model = display.model || t("runtime.actor.defaultModel");
  const kind = display.connectionKind;
  let connection = display.connectionDetail || "";
  if (kind) {
    switch (kind) {
      case "custom_endpoint":
        connection = display.connectionDetail || t("runtime.actor.customEndpoint");
        break;
      case "official":
        connection = display.connectionDetail || t("runtime.actor.officialAccount");
        break;
      case "system":
        connection = t("runtime.actor.systemLogin");
        break;
      default: {
        const _exhaustive: never = kind;
        return _exhaustive;
      }
    }
  }
  return connection ? `${model} · ${connection}` : model;
}

export function actorDisplayTitle(
  id: string,
  t: (key: string) => string,
  lane?: WorkerIdentity | null,
  siblings: WorkerIdentity[] = [],
): string {
  if (id === "human" || id === "operator") return t("runtime.actor.operator");
  if (id === "system") return t("runtime.actor.system");
  if (id === "report-value") return t("runtime.actor.reportValue");
  if (id === "challenger") return t("collab.role.challenger");
  if (id === "reason" || id === "coordinator") return t("coord.title");
  return workerDisplayName(id, lane, siblings).title;
}

export function resumeCommand(engine: string, session: string): string {
  if (!session) return "";
  const resolved = workerEngine(engine, engine);
  if (resolved === "Codex") return `codex exec resume ${session}`;
  if (resolved === "Cursor") return `cursor-agent --resume ${session}`;
  if (resolved === "Pi") return `pi --session ${session}`;
  if (resolved === "Oh My Pi") return `omp --resume ${session}`;
  if (resolved === "Kimi Code") return `kimi --session ${session}`;
  if (resolved === "Grok") return `grok --resume ${session}`;
  if (resolved === "OpenCode") return `opencode run --session ${session}`;
  if (resolved === "Devin CLI") return `devin --resume ${session}`;
  if (resolved === "DeepSeek Harness") return "";
  return `claude -r ${session}`;
}
