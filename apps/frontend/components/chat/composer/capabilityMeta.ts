import type { IconName } from "@muteki/ui";
import type {
  ComposerCapabilityItem,
  ComposerTrigger,
} from "@/lib/composerCapabilities";

export function capabilityIcon(kind: string, id = ""): IconName {
  if (kind === "component") {
    if (id.startsWith("component:computer-use:app:")) return "mousePointer";
    if (id.startsWith("component:computer-use")) return "monitor";
    if (id.startsWith("component:browser")) return "globe";
    return "board";
  }
  if (kind === "skill") return "sparkles";
  if (kind === "mcp") {
    if (id.startsWith("mcp:muteki-browser")) return "globe";
    if (id.startsWith("mcp:muteki-computer:app:")) return "mousePointer";
    if (id.startsWith("mcp:muteki-computer")) return "monitor";
    if (id.startsWith("mcp:muteki-platform")) return "target";
    return "plug";
  }
  if (kind === "plugin") return "layers";
  if (kind === "file") return "file";
  if (kind === "run") return "activity";
  if (kind === "competition") return "trophy";
  if (kind === "council") return "messages";
  if (kind === "roster") return "gitBranch";
  if (kind === "thread" || kind === "message_span") return "messages";
  if (kind === "tool_excerpt") return "wrench";
  return "terminal";
}

export function triggerLabel(trigger: ComposerTrigger): string {
  switch (trigger) {
    case "/":
      return "命令、能力与 Skill";
    case "@":
      return "添加上下文";
    case "$":
      return "Skill";
    default: {
      const exhaustive: never = trigger;
      return exhaustive;
    }
  }
}

export function triggerIcon(trigger: ComposerTrigger): IconName {
  switch (trigger) {
    case "/":
      return "slash";
    case "@":
      return "at";
    case "$":
      return "sparkles";
    default: {
      const exhaustive: never = trigger;
      return exhaustive;
    }
  }
}

export function capabilitySection(trigger: ComposerTrigger, item: ComposerCapabilityItem): string {
  if (trigger === "/") {
    if (item.kind === "command") {
      if (item.channel === "local_handler" && item.invocable !== false || item.source === "Muteki") return "聊天操作与协作";
      const labels: Record<string, string> = { claude: "Claude", codex: "Codex", cursor: "Cursor", pi: "Pi", omp: "OMP", kimi: "Kimi", grok: "Grok", opencode: "OpenCode", droid: "Droid", devin: "Devin", agy: "Antigravity" };
      return `${labels[item.engine || ""] || item.source} ${item.invocable === false ? "暂不可用" : "原生命令"}`;
    }
    if (item.kind === "mcp" && item.source.startsWith("Muteki")) return "Muteki 内置能力";
    if (item.kind === "component") return "Muteki 内置工具";
    return "Skill";
  }
  if (trigger === "$") return "Skill";
  if (item.kind === "component") return item.target ? "本机应用" : "Muteki 内置工具";
  if (item.kind === "thread") return "对话";
  if (item.kind === "run") return "平台运行 (Run)";
  if (item.kind === "competition") return "比赛 (Competition)";
  if (item.kind === "file") return "文件";
  return "插件与内置组件";
}

export function capabilityDisabled(item: ComposerCapabilityItem): boolean {
  return item.invocable === false || item.action === "inspect-runtime-capability";
}

export function capabilityLabel(item: ComposerCapabilityItem): string {
  return String(item.invocation?.wire_text || (item.kind === "command" ? `/${item.name}` : item.name));
}

export type RefStatusTone = "ok" | "stale" | "missing";

export function refStatusTone(status?: string): RefStatusTone {
  if (status === "stale") return "stale";
  if (status === "missing" || status === "forbidden") return "missing";
  return "ok";
}

export function capabilityStatusLabel(status?: string): string {
  if (!status || status === "available" || status === "runtime_reported") return "";
  if (status === "unavailable") return "暂不可用";
  if (status === "requested") return "待确认";
  if (status === "failed") return "未连接";
  return status;
}

export function refStatusLabel(status?: string): string {
  if (status === "stale") return "已变更";
  if (status === "missing") return "失效";
  if (status === "forbidden") return "无权限";
  return "";
}
