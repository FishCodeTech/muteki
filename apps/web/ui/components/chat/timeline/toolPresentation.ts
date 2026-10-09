import type { IconName } from "@/components/Icon";
import type { ToolItem } from "@/components/ai-native/tool-chips";

export type ToolKind =
  | "command"
  | "read"
  | "edit"
  | "search"
  | "list"
  | "browser"
  | "think"
  | "mcp"
  | "agent"
  | "other";

export type PresentableTool = ToolItem & { isAgent?: boolean };

export interface ToolEditStat {
  additions: number;
  deletions: number;
  paths: string[];
}

const COMMAND_MARKERS = ["terminal", "shell", "bash", "exec", "command", "run_terminal", "cmd", "powershell"];
const EDIT_MARKERS = ["edit", "write", "patch", "str_replace", "create_file", "apply", "notebook", "replace", "file_change"];
const READ_MARKERS = ["read", "view", "open_file", "get_file", "cat", "fetch_file"];
const SEARCH_MARKERS = ["search", "grep", "find", "glob", "ripgrep"];
const LIST_MARKERS = ["list", "ls", "directory", "tree"];
const BROWSER_MARKERS = ["browser", "playwright", "chrome", "webfetch", "web_fetch", "websearch", "web_search", "navigate", "screenshot"];

function hasMarker(name: string, markers: string[]): boolean {
  return markers.some((marker) => name.includes(marker));
}

const ICON_HINT: Record<string, ToolKind> = {
  run: "command",
  write: "edit",
  read: "read",
  search: "search",
  think: "think",
};

export function toolKind(tool: PresentableTool): ToolKind {
  const hinted = tool.icon ? ICON_HINT[tool.icon] : undefined;
  if (hinted) return hinted;
  const name = tool.name.toLowerCase();
  if (tool.isAgent || name === "task" || name.includes("subagent") || name.endsWith("_agent") || name === "agent") {
    return "agent";
  }
  if (name.startsWith("mcp") || name.includes("mcp__") || name.startsWith("muteki_") || name.includes(".muteki_")) {
    return "mcp";
  }
  if (hasMarker(name, BROWSER_MARKERS) || name.startsWith("web")) return "browser";
  if (name.includes("think") || name.includes("reason")) return "think";
  if (hasMarker(name, COMMAND_MARKERS)) return "command";
  if (hasMarker(name, EDIT_MARKERS)) return "edit";
  if (hasMarker(name, SEARCH_MARKERS)) return "search";
  if (hasMarker(name, READ_MARKERS)) return "read";
  if (hasMarker(name, LIST_MARKERS)) return "list";
  return "other";
}

export function toolKindIcon(kind: ToolKind): IconName {
  switch (kind) {
    case "command":
      return "terminal";
    case "read":
      return "eye";
    case "edit":
      return "fileDiff";
    case "search":
      return "search";
    case "list":
      return "folderOpen";
    case "browser":
      return "globe";
    case "think":
      return "brain";
    case "mcp":
      return "plug";
    case "agent":
      return "bot";
    case "other":
      return "wrench";
    default: {
      const exhaustive: never = kind;
      return exhaustive;
    }
  }
}

export function parseToolArgs(value: string | undefined): Record<string, unknown> {
  const text = String(value || "").trim();
  if (!text) return {};
  try {
    const parsed = JSON.parse(text) as unknown;
    if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) return parsed as Record<string, unknown>;
    if (typeof parsed === "string") return { command: parsed };
    if (Array.isArray(parsed)) return { command: parsed.map(String).join(" ") };
    return {};
  } catch {
    return { command: text };
  }
}

function str(value: unknown): string {
  if (value == null) return "";
  if (typeof value === "string") return value;
  if (Array.isArray(value)) return value.map((item) => str(item)).filter(Boolean).join(" ");
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  return "";
}

function firstOf(args: Record<string, unknown>, keys: string[]): string {
  for (const key of keys) {
    const value = str(args[key]).trim();
    if (value) return value;
  }
  return "";
}

const PATH_KEYS = ["path", "file_path", "filePath", "filename", "file", "target_file", "notebook_path", "uri"];

function stripCommandWrapper(command: string): string {
  const trimmed = command.trim();
  const wrapped = /^(?:\/bin\/)?(?:ba|z)?sh\s+-l?c\s+(['"])([\s\S]*)\1$/.exec(trimmed);
  return wrapped ? wrapped[2] : trimmed;
}

function prettyMcpName(name: string): string {
  const parts = name.split("__").filter(Boolean);
  if (parts.length >= 3 && parts[0].toLowerCase() === "mcp") return `${parts[1]} · ${parts.slice(2).join("_")}`;
  return name;
}

export function toolLabel(tool: PresentableTool, kind = toolKind(tool)): string {
  const name = tool.name.toLowerCase();
  switch (kind) {
    case "command":
      return "运行";
    case "read":
      return "读取";
    case "edit":
      return name.includes("create") || (name.includes("write") && !name.includes("edit")) ? "写入" : "编辑";
    case "search":
      return "搜索";
    case "list":
      return "列出";
    case "browser":
      return name.includes("search") ? "网页搜索" : "浏览";
    case "think":
      return "思考";
    case "mcp":
      return prettyMcpName(tool.name);
    case "agent":
      return "子 Agent";
    case "other":
      return tool.name;
    default: {
      const exhaustive: never = kind;
      return exhaustive;
    }
  }
}

function compact(value: string, max = 160): string {
  const single = value.replace(/\s+/g, " ").trim();
  return single.length > max ? `${single.slice(0, max - 1)}…` : single;
}

/** One-line argument summary rendered in mono next to the tool label. */
export function toolArgSummary(tool: PresentableTool, kind = toolKind(tool)): string {
  const args = parseToolArgs(tool.argsSummary);
  switch (kind) {
    case "command":
      return compact(stripCommandWrapper(firstOf(args, ["command", "cmd", "shell_command", "script", "input"])));
    case "read":
    case "edit":
    case "list": {
      const path = firstOf(args, [...PATH_KEYS, "directory", "dir", "cwd"]);
      if (path) return compact(path);
      const changed = changeEntries(args).map((entry) => entry.path);
      if (changed.length) return compact(changed.join(", "));
      const patchPaths = patchFilePaths(firstOf(args, ["patch", "input", "diff"]));
      if (patchPaths.length) return compact(patchPaths.join(", "));
      break;
    }
    case "search": {
      const query = firstOf(args, ["query", "pattern", "regex", "search", "q", "glob_pattern", "glob"]);
      const scope = firstOf(args, ["path", "directory", "include"]);
      if (query) return compact(scope ? `${query}  ${scope}` : query);
      break;
    }
    case "browser": {
      const target = firstOf(args, ["url", "query", "search_term", "href"]);
      if (target) return compact(target);
      break;
    }
    case "agent": {
      const description = firstOf(args, ["description", "title", "prompt", "task"]);
      if (description) return compact(description);
      break;
    }
    case "think":
    case "mcp":
    case "other":
      break;
    default: {
      const exhaustive: never = kind;
      return exhaustive;
    }
  }
  if (tool.chip) return compact(tool.chip);
  const firstString = Object.values(args).map(str).find((value) => value.trim());
  if (firstString) return compact(firstString);
  return compact(tool.argsSummary || "");
}

function countLines(value: string): number {
  if (!value) return 0;
  return value.replace(/\n$/, "").split("\n").length;
}

interface ChangeEntry {
  path: string;
  kind: string;
  diff: string;
}

/** Codex-style `changes: [{ path, kind, diff | content }]` payloads (file_change). */
function changeEntries(args: Record<string, unknown>): ChangeEntry[] {
  const raw = args.changes;
  if (!Array.isArray(raw)) return [];
  const out: ChangeEntry[] = [];
  for (const item of raw) {
    if (!item || typeof item !== "object") continue;
    const row = item as Record<string, unknown>;
    const path = firstOf(row, ["path", "filename", "file"]);
    if (!path) continue;
    const kind = firstOf(row, ["kind", "status", "type", "change_type"]).toLowerCase();
    let diff = firstOf(row, ["diff", "patch", "unified_diff", "unifiedDiff", "content"]);
    if (diff && !/^[+\- @]/m.test(diff)) {
      const sign = kind === "delete" ? "-" : "+";
      diff = splitLines(diff).map((line) => `${sign}${line}`).join("\n");
    }
    out.push({ path, kind, diff });
  }
  return out;
}

function patchFilePaths(patch: string): string[] {
  if (!patch) return [];
  const paths = new Set<string>();
  for (const match of patch.matchAll(/^\*\*\* (?:Update|Add|Delete) File: (.+)$/gm)) paths.add(match[1].trim());
  for (const match of patch.matchAll(/^\+\+\+ b\/(.+)$/gm)) paths.add(match[1].trim());
  return [...paths];
}

function patchStat(patch: string): { additions: number; deletions: number } | null {
  if (!patch || !/^[+-]/m.test(patch)) return null;
  let additions = 0;
  let deletions = 0;
  for (const line of patch.split("\n")) {
    if (line.startsWith("+++") || line.startsWith("---") || line.startsWith("***")) continue;
    if (line.startsWith("+")) additions += 1;
    else if (line.startsWith("-")) deletions += 1;
  }
  return additions || deletions ? { additions, deletions } : null;
}

/** Best-effort line stats for edit tools derived from their arguments. */
export function toolEditStat(tool: PresentableTool, kind = toolKind(tool)): ToolEditStat | null {
  if (kind !== "edit") return null;
  const args = parseToolArgs(tool.argsSummary);
  const changes = changeEntries(args);
  if (changes.length) {
    let additions = 0;
    let deletions = 0;
    for (const entry of changes) {
      const stat = patchStat(entry.diff);
      additions += stat?.additions ?? 0;
      deletions += stat?.deletions ?? 0;
    }
    return { additions, deletions, paths: changes.map((entry) => entry.path) };
  }
  const path = firstOf(args, PATH_KEYS);
  const patch = firstOf(args, ["patch", "diff", "input", "command"]);
  const fromPatch = patchStat(patch);
  if (fromPatch) {
    const paths = patchFilePaths(patch);
    return { ...fromPatch, paths: paths.length ? paths : path ? [path] : [] };
  }
  const oldText = str(args.old_string ?? args.oldString ?? args.old_str);
  const newText = str(args.new_string ?? args.newString ?? args.new_str);
  if (oldText || newText) {
    return { additions: countLines(newText), deletions: countLines(oldText), paths: path ? [path] : [] };
  }
  if (Array.isArray(args.edits)) {
    let additions = 0;
    let deletions = 0;
    for (const edit of args.edits) {
      if (!edit || typeof edit !== "object") continue;
      const row = edit as Record<string, unknown>;
      additions += countLines(str(row.new_string ?? row.newString));
      deletions += countLines(str(row.old_string ?? row.oldString));
    }
    return { additions, deletions, paths: path ? [path] : [] };
  }
  const content = str(args.content ?? args.contents ?? args.text ?? args.file_text);
  if (content) return { additions: countLines(content), deletions: 0, paths: path ? [path] : [] };
  return path ? { additions: 0, deletions: 0, paths: [path] } : null;
}

function readPath(tool: PresentableTool): string {
  return firstOf(parseToolArgs(tool.argsSummary), PATH_KEYS);
}

const SUMMARY_PHRASE: Record<Exclude<ToolKind, "think">, (count: number) => string> = {
  read: (n) => `读取 ${n} 个文件`,
  edit: (n) => `编辑 ${n} 个文件`,
  command: (n) => `运行 ${n} 条命令`,
  search: (n) => `搜索 ${n} 次`,
  list: (n) => `查看 ${n} 个目录`,
  browser: (n) => `访问网页 ${n} 次`,
  mcp: (n) => `调用 ${n} 次 MCP 工具`,
  agent: (n) => `委派 ${n} 个子 Agent`,
  other: (n) => `使用 ${n} 次其他工具`,
};

/**
 * "读取 3 个文件，运行 2 条命令" in first-seen order. Reads and edits count
 * distinct paths when the arguments name them.
 */
export function summarizeTools(tools: PresentableTool[], maxPhrases = 3): string {
  const order: Exclude<ToolKind, "think">[] = [];
  const counts = new Map<Exclude<ToolKind, "think">, number>();
  const paths = new Map<"read" | "edit", Set<string>>([["read", new Set()], ["edit", new Set()]]);
  for (const tool of tools) {
    const kind = toolKind(tool);
    if (kind === "think") continue;
    if (!counts.has(kind)) order.push(kind);
    if (kind === "read" || kind === "edit") {
      const named = kind === "read" ? [readPath(tool)].filter(Boolean) : toolEditStat(tool, kind)?.paths ?? [];
      const seen = paths.get(kind)!;
      if (named.length) {
        const before = seen.size;
        named.forEach((path) => seen.add(path));
        counts.set(kind, (counts.get(kind) ?? 0) + (seen.size - before));
        continue;
      }
    }
    counts.set(kind, (counts.get(kind) ?? 0) + 1);
  }
  const phrases = order.filter((kind) => (counts.get(kind) ?? 0) > 0).map((kind) => SUMMARY_PHRASE[kind](counts.get(kind) ?? 0));
  if (phrases.length <= maxPhrases) return phrases.join("，");
  return `${phrases.slice(0, maxPhrases).join("，")}等`;
}

export interface ToolEditLine {
  id: string;
  type: "added" | "removed" | "context";
  oldLine?: number;
  newLine?: number;
  content: string;
}

const MAX_EDIT_LINES = 400;

function splitLines(value: string): string[] {
  return value ? value.replace(/\n$/, "").split("\n") : [];
}

/** Unified-diff (or apply_patch) text → FileDiff rows; headers are dropped. */
export function linesFromPatch(patch: string): ToolEditLine[] {
  const out: ToolEditLine[] = [];
  let oldNo: number | undefined;
  let newNo: number | undefined;
  for (const raw of patch.split("\n")) {
    const hunk = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(raw);
    if (hunk) {
      oldNo = Number(hunk[1]);
      newNo = Number(hunk[2]);
      continue;
    }
    if (/^(\*\*\*|diff --git|index |--- |\+\+\+ |@@)/.test(raw)) continue;
    if (raw.startsWith("+")) {
      out.push({ id: String(out.length), type: "added", newLine: newNo, content: raw.slice(1) });
      if (newNo != null) newNo += 1;
    } else if (raw.startsWith("-")) {
      out.push({ id: String(out.length), type: "removed", oldLine: oldNo, content: raw.slice(1) });
      if (oldNo != null) oldNo += 1;
    } else if (raw.startsWith(" ")) {
      out.push({ id: String(out.length), type: "context", oldLine: oldNo, newLine: newNo, content: raw.slice(1) });
      if (oldNo != null) oldNo += 1;
      if (newNo != null) newNo += 1;
    }
  }
  return out;
}

function linesFromReplace(oldText: string, newText: string, start: number): ToolEditLine[] {
  return [
    ...splitLines(oldText).map((content, index) => ({ id: String(start + index), type: "removed" as const, content })),
    ...splitLines(newText).map((content, index) => ({ id: `${start}+${index}`, type: "added" as const, content })),
  ];
}

/** Line-level preview of an edit tool's change, derived from its arguments. */
export function toolEditLines(tool: PresentableTool, kind = toolKind(tool)): ToolEditLine[] | null {
  if (kind !== "edit") return null;
  const args = parseToolArgs(tool.argsSummary);
  let lines: ToolEditLine[] = [];
  const changes = changeEntries(args);
  const patch = firstOf(args, ["patch", "diff", "input", "command"]);
  if (changes.length) {
    lines = changes.flatMap((entry, index) =>
      linesFromPatch(entry.diff).map((line) => ({ ...line, id: `${index}:${line.id}` })),
    );
  } else if (patchStat(patch)) {
    lines = linesFromPatch(patch);
  } else if (args.old_string != null || args.new_string != null || args.oldString != null || args.newString != null || args.old_str != null || args.new_str != null) {
    lines = linesFromReplace(
      str(args.old_string ?? args.oldString ?? args.old_str),
      str(args.new_string ?? args.newString ?? args.new_str),
      0,
    );
  } else if (Array.isArray(args.edits)) {
    for (const edit of args.edits) {
      if (!edit || typeof edit !== "object") continue;
      const row = edit as Record<string, unknown>;
      lines.push(...linesFromReplace(str(row.old_string ?? row.oldString), str(row.new_string ?? row.newString), lines.length));
    }
  } else {
    const content = str(args.content ?? args.contents ?? args.text ?? args.file_text);
    lines = splitLines(content).map((line, index) => ({ id: String(index), type: "added", newLine: index + 1, content: line }));
  }
  if (!lines.length) return null;
  return lines.length > MAX_EDIT_LINES ? lines.slice(0, MAX_EDIT_LINES) : lines;
}

export function formatToolDuration(ms?: number): string {
  if (ms == null || !Number.isFinite(ms) || ms <= 0) return "";
  if (ms < 1000) return `${Math.round(ms)}ms`;
  const seconds = ms / 1000;
  if (seconds < 60) return `${seconds.toFixed(seconds < 10 ? 1 : 0)}s`;
  const minutes = Math.floor(seconds / 60);
  return `${minutes}m ${Math.round(seconds % 60)}s`;
}

/** Chinese wall-clock duration used in turn summaries ("12秒", "1分20秒"). */
export function formatTurnDuration(ms: number): string {
  const totalSeconds = Math.max(0, Math.round(ms / 1000));
  if (totalSeconds < 60) return `${totalSeconds}秒`;
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;
  if (hours > 0) return `${hours}小时${minutes ? `${minutes}分` : ""}`;
  return `${minutes}分${seconds ? `${seconds}秒` : ""}`;
}

/** Compact live clock for running turns ("0:07", "1:24", "1:02:03"). */
export function formatElapsedClock(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const seconds = total % 60;
  const pad = (value: number) => String(value).padStart(2, "0");
  return hours ? `${hours}:${pad(minutes)}:${pad(seconds)}` : `${minutes}:${pad(seconds)}`;
}

export function toolOutputText(tool: PresentableTool): string {
  const raw = String(tool.outputSummary || "").trim();
  if (!raw) return "";
  if (raw.startsWith("{") || raw.startsWith("[") || raw.startsWith("\"")) {
    try {
      const parsed = JSON.parse(raw) as unknown;
      if (typeof parsed === "string") return parsed;
      if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
        const row = parsed as Record<string, unknown>;
        const text = str(row.output ?? row.stdout ?? row.result ?? row.text ?? row.content);
        const stderr = str(row.stderr);
        if (text || stderr) return [text, stderr].filter(Boolean).join("\n");
      }
    } catch {
      return raw;
    }
  }
  return raw;
}
