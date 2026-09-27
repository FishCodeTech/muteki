/**
 * C13 Diff helpers — keep fetch/parse out of useConversation send path (#18 soft coord).
 */

import { apiFetch } from "@/lib/useRun";

export type DiffStaging = "staged" | "unstaged" | "untracked" | "both" | "artifact";

export type DiffBaselineKind = "worktree" | "turn" | "current_turn";

export interface DiffFileMeta {
  path: string;
  old_path?: string | null;
  status: string;
  staging: DiffStaging;
  binary?: boolean;
  additions?: number;
  deletions?: number;
  patch?: string;
}

export interface DiffSection {
  patch: string;
  additions: number;
  deletions: number;
}

export interface WorktreeDiffResponse {
  is_repo: boolean;
  current_branch?: string | null;
  detached_head?: boolean;
  detached_sha?: string | null;
  dirty?: boolean;
  head_sha?: string | null;
  captured_at?: string;
  baseline?: string;
  files: DiffFileMeta[];
  sections?: {
    staged?: DiffSection;
    unstaged?: DiffSection;
    untracked?: DiffSection;
  };
  patch: string;
  additions: number;
  deletions: number;
  selected_path?: string | null;
  selected_staging?: string | null;
}

export interface DiffBaseline {
  kind: DiffBaselineKind;
  label: string;
  turnId?: string;
  artifactSha256?: string;
  createdAt?: string | null;
  branch?: string | null;
  headSha?: string | null;
  capturedAt?: string | null;
}

export interface DiffArtifactRef {
  sha256: string;
  name?: string;
  kind?: string;
  media_type?: string;
  size?: number;
  turn_id?: string;
  created_at?: string;
}

export interface ParsedDiffFile {
  path: string;
  oldPath?: string | null;
  status: string;
  binary: boolean;
  additions: number;
  deletions: number;
  raw: string;
}

function countPatchLines(patch: string): { additions: number; deletions: number } {
  let additions = 0;
  let deletions = 0;
  for (const row of patch.split("\n")) {
    if (row.startsWith("+") && !row.startsWith("+++")) additions += 1;
    else if (row.startsWith("-") && !row.startsWith("---")) deletions += 1;
  }
  return { additions, deletions };
}

/** Split a multi-file unified diff into per-path patches. */
export function splitUnifiedDiffFiles(patch: string): ParsedDiffFile[] {
  const text = (patch || "").trim();
  if (!text) return [];
  const lines = text.split("\n");
  const chunks: string[][] = [];
  let current: string[] = [];
  for (const line of lines) {
    if (line.startsWith("diff --git ") && current.length) {
      chunks.push(current);
      current = [line];
    } else {
      current.push(line);
    }
  }
  if (current.length) chunks.push(current);

  return chunks.map((chunk) => {
    const header = chunk[0] || "";
    const match = /^diff --git a\/(.*?) b\/(.*?)$/.exec(header);
    const plusPath = match ? undefined : chunk.find((line) => line.startsWith("+++ b/"))?.slice("+++ b/".length);
    const minusPath = match ? undefined : chunk.find((line) => line.startsWith("--- a/"))?.slice("--- a/".length);
    let path = match?.[2] || plusPath || minusPath || "unknown";
    let oldPath: string | null = match?.[1] || minusPath || null;
    let status = "M";
    let binary = false;
    for (const line of chunk.slice(1)) {
      if (line.startsWith("new file mode")) status = "A";
      else if (line.startsWith("deleted file mode")) status = "D";
      else if (line.startsWith("rename from ")) {
        oldPath = line.slice("rename from ".length);
        status = "R";
      } else if (line.startsWith("rename to ")) {
        path = line.slice("rename to ".length);
        status = "R";
      } else if (/^Binary files .+ differ$/.test(line) || line.startsWith("GIT binary patch")) {
        binary = true;
      }
    }
    const raw = chunk.join("\n");
    const { additions, deletions } = countPatchLines(raw);
    return { path, oldPath: oldPath && oldPath !== path ? oldPath : null, status, binary, additions, deletions, raw };
  });
}

function patchesByPath(patch: string | undefined): Map<string, string> {
  const map = new Map<string, string>();
  for (const file of splitUnifiedDiffFiles(patch || "")) {
    if (!map.has(file.path)) map.set(file.path, file.raw);
  }
  return map;
}

/** Attach each worktree file's own patch from the per-section payload (one request, no lazy per-file fetch). */
export function worktreeFilesWithPatches(data: WorktreeDiffResponse): DiffFileMeta[] {
  const sections = {
    staged: patchesByPath(data.sections?.staged?.patch),
    unstaged: patchesByPath(data.sections?.unstaged?.patch),
    untracked: patchesByPath(data.sections?.untracked?.patch),
  };
  const fallback = data.sections ? null : patchesByPath(data.patch);
  return (data.files || []).map((file) => {
    const staging = (file.staging || "unstaged") as DiffStaging;
    const section = staging === "staged" || staging === "unstaged" || staging === "untracked" ? sections[staging] : null;
    return {
      ...file,
      staging,
      patch: file.patch || section?.get(file.path) || fallback?.get(file.path) || "",
    };
  });
}

export function turnIdFromDiffArtifactName(name?: string): string | undefined {
  if (!name) return undefined;
  if (name.endsWith(".diff")) return name.slice(0, -".diff".length) || undefined;
  return undefined;
}

export function isConversationDiffArtifact(artifact: DiffArtifactRef): boolean {
  return artifact.kind === "conversation.diff" || Boolean(artifact.name?.endsWith(".diff"));
}

export function listDiffArtifacts(artifacts: DiffArtifactRef[] | undefined): DiffArtifactRef[] {
  return (artifacts || []).filter(isConversationDiffArtifact);
}

export function latestDiffArtifact(
  artifacts: DiffArtifactRef[] | undefined,
  turnId?: string | null,
): DiffArtifactRef | undefined {
  const rows = listDiffArtifacts(artifacts);
  if (!rows.length) return undefined;
  if (turnId) {
    const matched = rows.find((row) => {
      if (row.turn_id && row.turn_id === turnId) return true;
      return turnIdFromDiffArtifactName(row.name) === turnId;
    });
    if (matched) return matched;
  }
  return rows[rows.length - 1];
}

export async function fetchWorktreeDiff(
  threadId: string,
  options: { path?: string; staging?: DiffStaging; signal?: AbortSignal } = {},
): Promise<WorktreeDiffResponse> {
  const params = new URLSearchParams();
  if (options.path) params.set("path", options.path);
  if (options.staging && options.staging !== "both" && options.staging !== "artifact") {
    params.set("staging", options.staging);
  }
  const query = params.toString();
  const url = `/api/threads/${encodeURIComponent(threadId)}/workspace/diff${query ? `?${query}` : ""}`;
  const response = await apiFetch(url, { signal: options.signal });
  if (!response.ok) {
    const body = await response.json().catch(() => ({} as { error?: { message?: string }; detail?: string }));
    throw new Error(body?.error?.message || body?.detail || `Diff 请求失败 (${response.status})`);
  }
  return response.json() as Promise<WorktreeDiffResponse>;
}

export async function fetchArtifactDiffText(
  threadId: string,
  sha256: string,
  signal?: AbortSignal,
): Promise<string> {
  const response = await apiFetch(
    `/api/threads/${encodeURIComponent(threadId)}/artifacts/${encodeURIComponent(sha256)}`,
    { signal },
  );
  if (!response.ok) {
    const body = await response.text();
    throw new Error(body || `Artifact Diff 读取失败 (${response.status})`);
  }
  return response.text();
}

export function languageFromPath(path: string): string {
  const ext = path.split(".").pop()?.toLowerCase() || "";
  switch (ext) {
    case "py":
      return "python";
    case "ts":
    case "tsx":
      return "typescript";
    case "js":
    case "jsx":
    case "mjs":
    case "cjs":
      return "javascript";
    case "json":
      return "json";
    case "md":
    case "markdown":
      return "markdown";
    case "css":
      return "css";
    case "html":
    case "htm":
      return "html";
    case "sh":
    case "bash":
    case "zsh":
      return "bash";
    case "rs":
      return "rust";
    case "go":
      return "go";
    case "yml":
    case "yaml":
      return "yaml";
    default:
      return "text";
  }
}

const KEYWORD_SETS: Record<string, RegExp> = {
  python: /\b(False|None|True|and|as|assert|async|await|break|class|continue|def|del|elif|else|except|finally|for|from|global|if|import|in|is|lambda|nonlocal|not|or|pass|raise|return|try|while|with|yield)\b/g,
  typescript: /\b(abstract|as|async|await|break|case|catch|class|const|continue|debugger|default|delete|do|else|enum|export|extends|false|finally|for|from|function|if|implements|import|in|instanceof|interface|let|new|null|of|package|private|protected|public|return|static|super|switch|this|throw|true|try|type|typeof|undefined|var|void|while|with|yield)\b/g,
  javascript: /\b(async|await|break|case|catch|class|const|continue|debugger|default|delete|do|else|export|extends|false|finally|for|from|function|if|import|in|instanceof|let|new|null|of|return|static|super|switch|this|throw|true|try|typeof|undefined|var|void|while|with|yield)\b/g,
};

/** Best-effort token highlight for common languages (no Shiki dependency). */
export function highlightCodeLine(content: string, language: string): string {
  const escape = (value: string) =>
    value.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  const keywordRe = KEYWORD_SETS[language];
  // Tokenize first on the raw string so keyword/number passes never touch HTML tags.
  const parts: Array<{ kind: "code" | "str" | "cmt"; text: string }> = [];
  const splitter = /("(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|#[^\n]*|\/\/[^\n]*)/g;
  let last = 0;
  let match: RegExpExecArray | null;
  while ((match = splitter.exec(content))) {
    if (match.index > last) {
      parts.push({ kind: "code", text: content.slice(last, match.index) });
    }
    const token = match[0];
    parts.push({
      kind: token.startsWith("#") || token.startsWith("//") ? "cmt" : "str",
      text: token,
    });
    last = match.index + token.length;
  }
  if (last < content.length) parts.push({ kind: "code", text: content.slice(last) });

  return parts
    .map((part) => {
      if (part.kind === "str") return `<span class="diff-tok-str">${escape(part.text)}</span>`;
      if (part.kind === "cmt") return `<span class="diff-tok-cmt">${escape(part.text)}</span>`;
      let next = escape(part.text);
      if (keywordRe) {
        next = next.replace(keywordRe, `<span class="diff-tok-kw">$1</span>`);
      }
      // Numbers after keywords so the inserted class= attributes are never re-matched.
      next = next.replace(
        /\b(0x[\da-fA-F]+|\d+(?:\.\d+)?)\b/g,
        `<span class="diff-tok-num">$1</span>`,
      );
      return next;
    })
    .join("");
}

// ── C14: Diff line annotation types ──────────────────────────────────────────

/** A user-authored feedback comment on a specific diff line. */
export interface DiffLineAnnotation {
  /** Stable client-side id. */
  id: string;
  /** File path in the diff. */
  path: string;
  /** Renamed-from path when relevant. */
  oldPath?: string;
  /** Which side of the diff the line belongs to. */
  side: "old" | "new" | "unified";
  /** 1-based line number (old-side for "old", new-side for "new"). */
  lineNumber: number;
  /** Last line of a multi-line range on the same side; absent for single lines. */
  endLineNumber?: number;
  /** Raw code text of the annotated line (snapshot). */
  snapshot: string;
  /** User-authored feedback comment. */
  comment: string;
  /** Staging area indicator carried from the diff row. */
  staging?: DiffStaging;
  /**
   * Identity of the baseline at the time the annotation was created.
   * Worktree: content-linked revision (`worktree:<head>:<digest>`).
   * Turn artifacts: immutable content SHA (`artifact:<sha256>`).
   */
  baselineId: string;
  /**
   * Set to true when the current baseline no longer matches baselineId.
   * Stale annotations cannot be sent — user must delete or re-annotate.
   */
  stale: boolean;
  createdAt: string;
}

export {
  annotationBaselineId,
  fnv1aHex,
  worktreeContentRevision,
} from "./conversationDiffBaseline";

/** HTTP error copy for artifact preview/download (access-password sessions). */
export function artifactHttpErrorMessage(
  status: number,
  action: "read" | "download" = "read",
): string {
  if (status === 401) {
    return action === "download"
      ? "登录已失效，请重新登录后下载附件"
      : "登录已失效，请重新登录后预览附件";
  }
  return action === "download"
    ? `下载失败（HTTP ${status}）`
    : `读取失败（HTTP ${status}）`;
}

/**
 * Authenticated artifact GET — same Bearer chain as upload / Diff artifact fetch.
 * Use this instead of bare `fetch` so access-password sessions do not 401 on preview.
 */
export async function fetchThreadArtifact(
  threadId: string,
  sha256: string,
  options?: { download?: boolean; signal?: AbortSignal },
): Promise<Response> {
  const path = `/api/threads/${encodeURIComponent(threadId)}/artifacts/${encodeURIComponent(sha256)}${
    options?.download ? "?download=true" : ""
  }`;
  return apiFetch(path, { cache: "no-store", signal: options?.signal });
}
