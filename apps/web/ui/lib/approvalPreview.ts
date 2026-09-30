/**
 * #123 — Derive file-change approval preview from pending_approval payload.
 *
 * Mirrors muteki.conversation.approval_queue.normalize_approval_payload field
 * names so the UI can render path list + Diff when Stream populates them, and
 * show an explicit missing state when they are absent.
 *
 * Kept free of React / useRun imports so unit scripts can load it directly.
 */

export type ApprovalFilePreview = {
  path: string;
  status?: string;
  diff?: string;
  additions?: number;
  deletions?: number;
};

export type ApprovalPreview = {
  approvalId: string;
  kind: string;
  actionLabel: string;
  command: string;
  cwd: string;
  reason: string;
  scope: string;
  expires: string;
  status: string;
  nativeOptions?: Array<{ option_id: string; kind: string; name: string }>;
  args: string;
  /** Aggregated unified diff (top-level or joined from files). */
  diff: string;
  files: ApprovalFilePreview[];
  /** Singular path when Stream sets `path` or there is exactly one file. */
  primaryPath: string;
  /** file_change with no usable path list. */
  missingPaths: boolean;
  /** file_change with no usable patch/diff. */
  missingDiff: boolean;
};

function asString(value: unknown): string {
  if (value == null || value === "") return "";
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  if (Array.isArray(value)) {
    return value
      .map((item) => (typeof item === "string" ? item : JSON.stringify(item)))
      .filter(Boolean)
      .join(" ");
  }
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

function firstString(row: Record<string, unknown>, ...keys: string[]): string {
  for (const key of keys) {
    if (!(key in row)) continue;
    const text = asString(row[key]).trim();
    if (text) return text;
  }
  return "";
}

function asRecord(value: unknown): Record<string, unknown> | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  return value as Record<string, unknown>;
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

/** Split a multi-file unified / git diff into per-path patches (local copy). */
export function splitApprovalDiffFiles(patch: string): Array<{
  path: string;
  status: string;
  additions: number;
  deletions: number;
  raw: string;
}> {
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

  // Fallback: single chunk without diff --git headers.
  if (chunks.length === 1 && !chunks[0][0]?.startsWith("diff --git ")) {
    const raw = chunks[0].join("\n");
    const path = extractPathsFromPlusPlus(raw)[0] || "change.diff";
    const { additions, deletions } = countPatchLines(raw);
    return [{ path, status: "M", additions, deletions, raw }];
  }

  return chunks.map((chunk) => {
    const header = chunk[0] || "";
    const match = /^diff --git a\/(.*?) b\/(.*?)$/.exec(header);
    let path = match?.[2] || extractPathsFromPlusPlus(chunk.join("\n"))[0] || "unknown";
    let status = "M";
    for (const line of chunk.slice(1)) {
      if (line.startsWith("new file mode")) status = "A";
      else if (line.startsWith("deleted file mode")) status = "D";
      else if (line.startsWith("rename to ")) {
        path = line.slice("rename to ".length);
        status = "R";
      }
    }
    const raw = chunk.join("\n");
    const { additions, deletions } = countPatchLines(raw);
    return { path, status, additions, deletions, raw };
  });
}

function extractPathsFromPlusPlus(diff: string): string[] {
  const paths: string[] = [];
  const seen = new Set<string>();
  for (const line of diff.split("\n")) {
    let match = /^\+\+\+\s+b\/(.+)$/.exec(line);
    if (!match) match = /^\+\+\+\s+(?!\/dev\/null)(.+)$/.exec(line);
    if (!match) continue;
    const path = match[1].trim();
    if (!path || path === "/dev/null" || seen.has(path)) continue;
    seen.add(path);
    paths.push(path);
  }
  if (paths.length) return paths;
  for (const line of diff.split("\n")) {
    const match = /^---\s+a\/(.+)$/.exec(line);
    if (!match) continue;
    const path = match[1].trim();
    if (!path || path === "/dev/null" || seen.has(path)) continue;
    seen.add(path);
    paths.push(path);
  }
  return paths;
}

function inferKind(row: Record<string, unknown>): string {
  const raw = firstString(row, "approval_kind", "kind", "type", "permission_kind")
    .toLowerCase()
    .replace(/-/g, "_");
  const aliases: Record<string, string> = {
    command: "command_execution",
    commandexecution: "command_execution",
    command_execution: "command_execution",
    shell: "command_execution",
    exec: "command_execution",
    file_change: "file_change",
    filechange: "file_change",
    apply_patch: "file_change",
    applypatch: "file_change",
    patch: "file_change",
    mcp_tool_call: "mcp_tool_call",
    mcp: "mcp_tool_call",
  };
  if (raw in aliases) return aliases[raw];
  if (firstString(row, "diff", "patch", "unified_diff", "unifiedDiff")) return "file_change";
  if (firstString(row, "command", "cmd")) return "command_execution";
  return raw || "unknown";
}

function actionLabelFor(kind: string, row: Record<string, unknown>): string {
  if (kind === "command_execution" || kind === "command") return "命令执行";
  if (kind === "file_change") return "文件变更";
  if (kind === "mcp_tool_call") return "MCP 工具调用";
  return (
    firstString(row, "title", "tool_name", "action", "name", "requester")
    || "Runtime 操作"
  );
}

function normalizeFileItem(item: unknown): ApprovalFilePreview | null {
  const row = asRecord(item);
  if (!row) {
    if (typeof item === "string" && item.trim()) return { path: item.trim() };
    return null;
  }
  const path = firstString(row, "path", "filename", "file", "file_path", "name");
  if (!path) return null;
  const status = firstString(row, "status", "change_type", "kind") || undefined;
  const diff = firstString(row, "diff", "patch", "unified_diff", "unifiedDiff", "content") || undefined;
  const out: ApprovalFilePreview = { path };
  if (status) out.status = status;
  if (diff) out.diff = diff;
  for (const key of ["additions", "deletions"] as const) {
    const val = row[key];
    if (typeof val === "number" && Number.isFinite(val)) out[key] = val;
  }
  return out;
}

function filesFromMapping(raw: Record<string, unknown>): ApprovalFilePreview[] {
  const out: ApprovalFilePreview[] = [];
  for (const [pathKey, change] of Object.entries(raw)) {
    const path = String(pathKey || "").trim();
    if (!path) continue;
    const changeRow = asRecord(change);
    if (changeRow) {
      const status = firstString(changeRow, "type", "kind", "status", "change_type") || undefined;
      const diff =
        firstString(changeRow, "diff", "patch", "unified_diff", "unifiedDiff", "content")
        || undefined;
      const row: ApprovalFilePreview = { path };
      if (status) row.status = status;
      if (diff) row.diff = diff;
      out.push(row);
    } else {
      const diff = asString(change).trim();
      out.push(diff ? { path, diff } : { path });
    }
  }
  return out;
}

function collectFiles(row: Record<string, unknown>): ApprovalFilePreview[] {
  const buckets: unknown[] = [];
  const pushSource = (source: Record<string, unknown> | null) => {
    if (!source) return;
    for (const key of ["files", "changes", "paths"]) {
      const val = source[key];
      if (Array.isArray(val)) buckets.push(...val);
    }
    for (const key of ["file_changes", "fileChanges"]) {
      const val = source[key];
      const map = asRecord(val);
      if (map) buckets.push(...filesFromMapping(map));
    }
  };
  pushSource(row);
  pushSource(asRecord(row.native));

  // Top-level singular path (#145).
  const single = firstString(row, "path", "file", "filename", "file_path");
  if (single) buckets.push({ path: single });

  const out: ApprovalFilePreview[] = [];
  const seen = new Set<string>();
  for (const item of buckets) {
    const file = normalizeFileItem(item);
    if (!file || seen.has(file.path)) continue;
    seen.add(file.path);
    out.push(file);
  }
  return out;
}

function extractDiff(row: Record<string, unknown>, files: ApprovalFilePreview[]): string {
  const direct = firstString(row, "diff", "patch", "unified_diff", "unifiedDiff");
  if (direct) return direct;
  const native = asRecord(row.native);
  if (native) {
    const nested = firstString(
      native,
      "diff",
      "patch",
      "unified_diff",
      "unifiedDiff",
      "fileDiff",
      "file_diff",
    );
    if (nested) return nested;
  }
  const chunks = files.map((f) => f.diff).filter((d): d is string => Boolean(d && d.trim()));
  if (chunks.length) return chunks.join("\n");
  return "";
}

/** Build a stable preview model for ApprovalCard from a pending approval row. */
export function buildApprovalPreview(row: Record<string, unknown>): ApprovalPreview {
  const approvalId = firstString(row, "approval_id", "request_id", "id");
  const kind = inferKind(row);

  let command = firstString(row, "command", "cmd");
  let cwd = firstString(
    row,
    "cwd",
    "working_directory",
    "workspace",
    "workdir",
    "grantRoot",
    "grant_root",
  );
  const native = asRecord(row.native);
  if (!command && native) {
    command = firstString(native, "command", "cmd") || asString(native.command).trim();
  }
  if (!cwd && native) {
    cwd = firstString(
      native,
      "cwd",
      "working_directory",
      "workdir",
      "grantRoot",
      "grant_root",
    );
  }

  let files = collectFiles(row);
  const diff = extractDiff(row, files);

  if (!files.length && diff) {
    const split = splitApprovalDiffFiles(diff);
    if (split.length) {
      files = split.map((f) => ({
        path: f.path,
        status: f.status,
        diff: f.raw,
        additions: f.additions,
        deletions: f.deletions,
      }));
    } else {
      files = extractPathsFromPlusPlus(diff).map((path) => ({ path, diff }));
    }
  }

  if (files.length === 1 && diff && !files[0].diff) {
    files = [{ ...files[0], diff }];
  }

  const isFileChange = kind === "file_change";
  const missingPaths = isFileChange && files.length === 0;
  const missingDiff = isFileChange && !diff.trim();

  let reason = firstString(row, "reason", "message", "detail");
  // Align with Stream #145: queue injects this when path/diff/files absent.
  if (isFileChange && missingPaths && missingDiff) {
    const marker = "文件变更预览暂未取得";
    if (!reason) reason = marker;
    else if (!reason.includes(marker) && !reason.includes("路径/补丁缺失")) {
      reason = `${reason}（${marker}）`;
    }
  }

  const args = firstString(row, "arguments", "args", "input", "parameters");
  const primaryPath =
    firstString(row, "path", "file", "filename", "file_path")
    || (files.length === 1 ? files[0].path : "");

  return {
    approvalId,
    kind,
    actionLabel: actionLabelFor(kind, row),
    command,
    cwd,
    reason,
    scope: firstString(row, "permission_scope", "scope", "permissions"),
    expires: firstString(row, "expires_at", "expires", "ttl_seconds"),
    status: firstString(row, "status") || "pending",
    nativeOptions: Array.isArray(row.options) ? row.options.flatMap((value) => {
      const option = asRecord(value);
      if (!option || typeof option.option_id !== "string" || typeof option.kind !== "string") return [];
      return [{ option_id: option.option_id, kind: option.kind, name: typeof option.name === "string" ? option.name : "" }];
    }) : undefined,
    args: !command ? args : "",
    diff,
    files,
    primaryPath,
    missingPaths,
    missingDiff,
  };
}

/** DiffTable-compatible file list (path + raw patch). */
export function approvalDiffFiles(
  preview: ApprovalPreview,
): Array<{ path: string; raw?: string; additions?: number; deletions?: number }> {
  const withDiff = preview.files.filter((f) => f.diff && f.diff.trim());
  if (withDiff.length) {
    return withDiff.map((f) => ({
      path: f.path,
      raw: f.diff,
      additions: f.additions,
      deletions: f.deletions,
    }));
  }
  if (preview.diff.trim()) {
    const split = splitApprovalDiffFiles(preview.diff);
    if (split.length) {
      return split.map((f) => ({
        path: f.path,
        raw: f.raw,
        additions: f.additions,
        deletions: f.deletions,
      }));
    }
    const fallbackPath = preview.files[0]?.path || "change.diff";
    return [{ path: fallbackPath, raw: preview.diff }];
  }
  return [];
}
