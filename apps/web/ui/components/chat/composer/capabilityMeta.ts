import type { IconName } from "@/components/Icon";
import type {
  ComposerCapabilityItem,
  ComposerTrigger,
} from "@/lib/composerCapabilities";

export function capabilityIcon(kind: string): IconName {
  if (kind === "skill") return "sparkles";
  if (kind === "mcp") return "plug";
  if (kind === "plugin") return "layers";
  if (kind === "file") return "file";
  if (kind === "thread" || kind === "message_span") return "messages";
  if (kind === "tool_excerpt") return "wrench";
  return "terminal";
}

export function triggerLabel(trigger: ComposerTrigger): string {
  switch (trigger) {
    case "/":
      return "命令与 Skill";
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
      if (item.channel === "local_handler" && item.invocable !== false || item.source === "Muteki") return "聊天操作";
      const labels: Record<string, string> = { claude: "Claude", codex: "Codex", cursor: "Cursor", pi: "Pi", omp: "OMP", kimi: "Kimi", grok: "Grok", opencode: "OpenCode" };
      return `${labels[item.engine || ""] || item.source} ${item.invocable === false ? "暂不可用" : "原生命令"}`;
    }
    return "Skill";
  }
  if (trigger === "$") return "Skill";
  if (item.kind === "thread") return "对话";
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

export function refStatusLabel(status?: string): string {
  if (status === "stale") return "已变更";
  if (status === "missing") return "失效";
  if (status === "forbidden") return "无权限";
  return "";
}
