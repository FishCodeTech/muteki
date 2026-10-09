import type { ConversationEvent } from "@/lib/useConversation";

export interface ConversationLinkSource {
  url: string;
  host: string;
  /** Tool names (or "回答") that surfaced the link. */
  via: string[];
  firstSeq: number;
}

export interface ConversationLinkGroup {
  host: string;
  links: ConversationLinkSource[];
}

export interface ConversationMcpSource {
  server: string;
  tools: Array<{ name: string; calls: number }>;
  calls: number;
}

export interface ConversationSources {
  linkGroups: ConversationLinkGroup[];
  linkCount: number;
  mcp: ConversationMcpSource[];
  mcpCalls: number;
}

const URL_PATTERN = /https?:\/\/[^\s"'`<>()[\]{}\\]+/g;
const TRAILING_PUNCTUATION = /[.,;:!?。，；：！？、]+$/;
const MCP_NAME = /^mcp__(.+?)__(.+)$/;

function eventText(value: unknown): string {
  if (value == null) return "";
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value);
  } catch {
    return "";
  }
}

export function extractUrls(text: string): string[] {
  if (!text || !text.includes("http")) return [];
  const found: string[] = [];
  for (const match of text.matchAll(URL_PATTERN)) {
    const raw = match[0].replace(TRAILING_PUNCTUATION, "");
    try {
      const parsed = new URL(raw);
      if (parsed.protocol === "http:" || parsed.protocol === "https:") found.push(parsed.toString());
    } catch {
      // Not a URL after all (e.g. a bare scheme in prose).
    }
  }
  return found;
}

function mcpIdentity(payload: Record<string, unknown>): { server: string; tool: string } | null {
  const name = String(payload.tool || payload.name || "");
  const server = String(payload.mcp_server || "").trim();
  if (server) return { server, tool: name || "tool" };
  const match = MCP_NAME.exec(name);
  return match ? { server: match[1], tool: match[2] } : null;
}

/**
 * Sources the reader can audit: links seen in tool input/output and answers,
 * and MCP servers the agent actually called. Covers only the loaded events.
 */
export function collectConversationSources(
  events: ConversationEvent[],
  messages: Array<{ role?: string; text?: string | null; stream_seq?: number }> = [],
): ConversationSources {
  const links = new Map<string, ConversationLinkSource>();
  const mcpCalls = new Map<string, Map<string, number>>();
  const countedCalls = new Set<string>();

  const addLink = (url: string, via: string, seq: number) => {
    const existing = links.get(url);
    if (existing) {
      if (via && !existing.via.includes(via)) existing.via.push(via);
      return;
    }
    let host = url;
    try {
      host = new URL(url).hostname.replace(/^www\./, "");
    } catch {
      // extractUrls already validated the URL.
    }
    links.set(url, { url, host, via: via ? [via] : [], firstSeq: seq });
  };

  for (const event of events) {
    const type = event.event_type || "";
    if (!type.includes("tool")) continue;
    const payload = (event.payload || {}) as Record<string, unknown>;
    const tool = String(payload.tool || payload.name || "工具");
    const seq = Number(event.seq || 0);
    for (const field of [payload.input, payload.arguments, payload.output, payload.result]) {
      for (const url of extractUrls(eventText(field))) addLink(url, tool, seq);
    }
    const mcp = mcpIdentity(payload);
    const callId = String(payload.call_id || payload.tool_call_id || "");
    if (mcp && callId && !countedCalls.has(callId)) {
      countedCalls.add(callId);
      const tools = mcpCalls.get(mcp.server) || new Map<string, number>();
      tools.set(mcp.tool, (tools.get(mcp.tool) || 0) + 1);
      mcpCalls.set(mcp.server, tools);
    }
  }
  for (const message of messages) {
    if (message.role !== "assistant") continue;
    for (const url of extractUrls(String(message.text || ""))) addLink(url, "回答", Number(message.stream_seq || 0));
  }

  const groups = new Map<string, ConversationLinkSource[]>();
  for (const link of [...links.values()].sort((a, b) => a.firstSeq - b.firstSeq)) {
    const list = groups.get(link.host) || [];
    list.push(link);
    groups.set(link.host, list);
  }
  const linkGroups = [...groups.entries()]
    .map(([host, rows]) => ({ host, links: rows }))
    .sort((a, b) => b.links.length - a.links.length || a.links[0].firstSeq - b.links[0].firstSeq);

  const mcp = [...mcpCalls.entries()]
    .map(([server, tools]) => {
      const rows = [...tools.entries()].map(([name, calls]) => ({ name, calls })).sort((a, b) => b.calls - a.calls);
      return { server, tools: rows, calls: rows.reduce((sum, row) => sum + row.calls, 0) };
    })
    .sort((a, b) => b.calls - a.calls);

  return {
    linkGroups,
    linkCount: links.size,
    mcp,
    mcpCalls: mcp.reduce((sum, row) => sum + row.calls, 0),
  };
}
