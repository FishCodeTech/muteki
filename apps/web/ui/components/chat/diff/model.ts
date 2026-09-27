import type { DiffFileMeta, DiffLineAnnotation } from "@/lib/conversationDiff";

export type DiffViewMode = "unified" | "split";
export type DiffSide = "old" | "new";

export interface DiffLine {
  kind: "ctx" | "add" | "del";
  text: string;
  oldLine?: number;
  newLine?: number;
  /** Index into the reconstructed old-side text (ctx/del). */
  oldIndex?: number;
  /** Index into the reconstructed new-side text (ctx/add). */
  newIndex?: number;
  /** Index-aligned counterpart inside the same change block (for word diff). */
  pair?: DiffLine;
  noNewline?: boolean;
}

export type HunkSegment =
  | { kind: "ctx"; lines: DiffLine[] }
  | { kind: "change"; dels: DiffLine[]; adds: DiffLine[] };

export interface DiffHunk {
  header: string;
  context: string;
  oldStart: number;
  oldCount: number;
  newStart: number;
  newCount: number;
  segments: HunkSegment[];
}

export interface ParsedPatch {
  hunks: DiffHunk[];
  oldText: string;
  newText: string;
  additions: number;
  deletions: number;
  changed: number;
  maxCols: number;
  maxLine: number;
  binary: boolean;
  truncated: boolean;
  /** False when the patch has body text but no recognizable hunks. */
  parseable: boolean;
  raw: string;
}

export interface DiffListFile {
  key: string;
  meta: DiffFileMeta;
  parsed: ParsedPatch;
}

/** Files with more changed lines than this render collapsed behind a "load" notice and skip highlighting. */
export const LARGE_FILE_LINES = 2000;
export const MAX_SIZER_COLS = 800;

const HUNK_RE = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@ ?(.*)$/;
const TRUNCATED_MARK = "/* truncated: file exceeds";

function isWide(code: number): boolean {
  return (
    (code >= 0x1100 && code <= 0x115f)
    || (code >= 0x2e80 && code <= 0xa4cf)
    || (code >= 0xac00 && code <= 0xd7a3)
    || (code >= 0xf900 && code <= 0xfaff)
    || (code >= 0xfe30 && code <= 0xfe4f)
    || (code >= 0xff00 && code <= 0xff60)
    || (code >= 0xffe0 && code <= 0xffe6)
    || (code >= 0x1f300 && code <= 0x1faff)
    || code >= 0x20000
  );
}

export function visualWidth(text: string): number {
  if (text.length > 4000) return text.length;
  let width = 0;
  for (const ch of text) {
    if (ch === "\t") width += 4 - (width % 4);
    else width += isWide(ch.codePointAt(0) ?? 0) ? 2 : 1;
  }
  return width;
}

interface HunkBuilder {
  hunk: DiffHunk;
  remainingOld: number;
  remainingNew: number;
  oldNum: number;
  newNum: number;
}

export function parsePatch(raw: string, options: { binary?: boolean } = {}): ParsedPatch {
  const text = raw || "";
  const lines = text.split("\n");
  const hunks: DiffHunk[] = [];
  const oldSide: string[] = [];
  const newSide: string[] = [];
  let binary = Boolean(options.binary);
  let additions = 0;
  let deletions = 0;
  let maxCols = 0;
  let maxLine = 0;
  let headerDone = false;
  let bodyLines = 0;
  const cursor: { hunk: HunkBuilder | null; last: DiffLine | null } = { hunk: null, last: null };

  const startHunk = (header: string, oldStart: number, oldCount: number, newStart: number, newCount: number, context: string) => {
    const hunk: DiffHunk = { header, context, oldStart, oldCount, newStart, newCount, segments: [] };
    hunks.push(hunk);
    cursor.hunk = { hunk, remainingOld: oldCount, remainingNew: newCount, oldNum: oldStart, newNum: newStart };
    cursor.last = null;
  };

  const push = (builder: HunkBuilder, line: DiffLine) => {
    const segments = builder.hunk.segments;
    const tail = segments[segments.length - 1];
    if (line.kind === "ctx") {
      if (tail?.kind === "ctx") tail.lines.push(line);
      else segments.push({ kind: "ctx", lines: [line] });
    } else if (line.kind === "del") {
      if (tail?.kind === "change" && tail.adds.length === 0) tail.dels.push(line);
      else segments.push({ kind: "change", dels: [line], adds: [] });
    } else if (tail?.kind === "change") {
      tail.adds.push(line);
    } else {
      segments.push({ kind: "change", dels: [], adds: [line] });
    }
    const cols = visualWidth(line.text);
    if (cols > maxCols) maxCols = cols;
    cursor.last = line;
  };

  for (const rawLine of lines) {
    const line = rawLine.endsWith("\r") ? rawLine.slice(0, -1) : rawLine;
    if (line.startsWith("diff --git ")) {
      cursor.hunk = null;
      headerDone = false;
      continue;
    }
    if (line.startsWith("@@")) {
      const match = HUNK_RE.exec(line);
      if (match) {
        startHunk(
          line,
          Number(match[1]),
          match[2] === undefined ? 1 : Number(match[2]),
          Number(match[3]),
          match[4] === undefined ? 1 : Number(match[4]),
          match[5] || "",
        );
      } else {
        startHunk(line, 1, Number.POSITIVE_INFINITY, 1, Number.POSITIVE_INFINITY, "");
      }
      headerDone = true;
      continue;
    }
    if (!cursor.hunk) {
      if (/^Binary files .+ differ$/.test(line) || line.startsWith("GIT binary patch")) {
        binary = true;
        continue;
      }
      if (line.startsWith("+++ ")) {
        headerDone = true;
        continue;
      }
      // Synthetic untracked patches carry "+" bodies without an @@ header.
      if (headerDone && /^[+\- ]/.test(line)) {
        startHunk("", 0, Number.POSITIVE_INFINITY, 1, Number.POSITIVE_INFINITY, "");
      } else {
        if (headerDone && line.trim() && !line.startsWith("# ")) bodyLines += 1;
        continue;
      }
    }
    const active = cursor.hunk;
    if (!active) continue;
    const first = line[0];
    if (first === "\\") {
      if (cursor.last) cursor.last.noNewline = true;
      continue;
    }
    const expectsMore = active.remainingOld > 0 || active.remainingNew > 0;
    if (first === "+") {
      const next: DiffLine = { kind: "add", text: line.slice(1), newLine: active.newNum, newIndex: newSide.length };
      newSide.push(next.text);
      active.newNum += 1;
      active.remainingNew -= 1;
      additions += 1;
      push(active, next);
    } else if (first === "-") {
      const next: DiffLine = { kind: "del", text: line.slice(1), oldLine: active.oldNum, oldIndex: oldSide.length };
      oldSide.push(next.text);
      active.oldNum += 1;
      active.remainingOld -= 1;
      deletions += 1;
      push(active, next);
    } else if (first === " " || (line === "" && expectsMore && Number.isFinite(active.remainingOld))) {
      const body = line.slice(1);
      const next: DiffLine = {
        kind: "ctx",
        text: body,
        oldLine: active.oldNum,
        newLine: active.newNum,
        oldIndex: oldSide.length,
        newIndex: newSide.length,
      };
      oldSide.push(body);
      newSide.push(body);
      active.oldNum += 1;
      active.newNum += 1;
      active.remainingOld -= 1;
      active.remainingNew -= 1;
      push(active, next);
    } else {
      cursor.hunk = null;
      if (line.trim() && !line.startsWith("# ")) bodyLines += 1;
      continue;
    }
    if (active.oldNum > maxLine) maxLine = active.oldNum;
    if (active.newNum > maxLine) maxLine = active.newNum;
  }

  for (const hunk of hunks) {
    for (const segment of hunk.segments) {
      if (segment.kind !== "change") continue;
      const count = Math.min(segment.dels.length, segment.adds.length);
      for (let index = 0; index < count; index += 1) {
        segment.dels[index].pair = segment.adds[index];
        segment.adds[index].pair = segment.dels[index];
      }
    }
  }

  return {
    hunks,
    oldText: oldSide.join("\n"),
    newText: newSide.join("\n"),
    additions,
    deletions,
    changed: additions + deletions,
    maxCols,
    maxLine,
    binary,
    truncated: text.includes(TRUNCATED_MARK),
    parseable: hunks.length > 0 || binary || bodyLines === 0,
    raw: text,
  };
}

const parseCache = new Map<string, ParsedPatch>();

/** Identical patch text returns the identical object so downstream caches (tokens) survive refreshes. */
export function parsePatchCached(raw: string, binary = false): ParsedPatch {
  const key = `${binary ? 1 : 0}\u0000${raw}`;
  const hit = parseCache.get(key);
  if (hit) return hit;
  const parsed = parsePatch(raw, { binary });
  if (parseCache.size > 400) parseCache.delete(parseCache.keys().next().value as string);
  parseCache.set(key, parsed);
  return parsed;
}

// ── Intraline word diff ─────────────────────────────────────────────────────

export type CharRange = readonly [number, number];
export interface IntralineRanges {
  del: CharRange[];
  add: CharRange[];
}

const WORD_RE = /[\p{L}\p{N}_]+|\s+|[^\p{L}\p{N}_\s]/gu;
const MAX_LCS_CELLS = 90_000;
const intralineCache = new WeakMap<DiffLine, IntralineRanges | null>();

function tokenize(text: string): string[] {
  return text.match(WORD_RE) ?? [];
}

function changedRanges(tokens: string[], keep: boolean[]): CharRange[] {
  const ranges: Array<[number, number]> = [];
  let offset = 0;
  tokens.forEach((token, index) => {
    const end = offset + token.length;
    if (!keep[index]) {
      const last = ranges[ranges.length - 1];
      if (last && last[1] === offset) last[1] = end;
      else ranges.push([offset, end]);
    }
    offset = end;
  });
  return ranges;
}

/** A kept whitespace token wedged between two changed tokens would split one highlight into confetti. */
function absorbWhitespace(tokens: string[], keep: boolean[]) {
  for (let index = 1; index < tokens.length - 1; index += 1) {
    if (keep[index] && !keep[index - 1] && !keep[index + 1] && /^\s+$/.test(tokens[index])) keep[index] = false;
  }
}

function computeIntraline(before: string, after: string): IntralineRanges | null {
  if (before === after) return null;
  if (before.length > 1200 || after.length > 1200) return null;
  const a = tokenize(before);
  const b = tokenize(after);
  let prefix = 0;
  while (prefix < a.length && prefix < b.length && a[prefix] === b[prefix]) prefix += 1;
  let suffix = 0;
  while (
    suffix < a.length - prefix
    && suffix < b.length - prefix
    && a[a.length - 1 - suffix] === b[b.length - 1 - suffix]
  ) suffix += 1;
  const keepA = a.map((_, index) => index < prefix || index >= a.length - suffix);
  const keepB = b.map((_, index) => index < prefix || index >= b.length - suffix);
  const midA = a.slice(prefix, a.length - suffix);
  const midB = b.slice(prefix, b.length - suffix);
  if (midA.length && midB.length && midA.length * midB.length <= MAX_LCS_CELLS) {
    const n = midA.length;
    const m = midB.length;
    const dp = new Uint16Array((n + 1) * (m + 1));
    for (let i = n - 1; i >= 0; i -= 1) {
      for (let j = m - 1; j >= 0; j -= 1) {
        dp[i * (m + 1) + j] = midA[i] === midB[j]
          ? dp[(i + 1) * (m + 1) + j + 1] + 1
          : Math.max(dp[(i + 1) * (m + 1) + j], dp[i * (m + 1) + j + 1]);
      }
    }
    let i = 0;
    let j = 0;
    while (i < n && j < m) {
      if (midA[i] === midB[j]) {
        keepA[prefix + i] = true;
        keepB[prefix + j] = true;
        i += 1;
        j += 1;
      } else if (dp[(i + 1) * (m + 1) + j] >= dp[i * (m + 1) + j + 1]) {
        i += 1;
      } else {
        j += 1;
      }
    }
  }
  absorbWhitespace(a, keepA);
  absorbWhitespace(b, keepB);
  const del = changedRanges(a, keepA);
  const add = changedRanges(b, keepB);
  const changedChars = (ranges: CharRange[]) => ranges.reduce((sum, [start, end]) => sum + end - start, 0);
  const ratioA = before.length ? changedChars(del) / before.length : 1;
  const ratioB = after.length ? changedChars(add) / after.length : 1;
  if (ratioA > 0.7 && ratioB > 0.7) return null;
  if (!del.length && !add.length) return null;
  return { del, add };
}

/** Word-level ranges for an index-aligned del/add pair (memoized per line object). */
export function intralineFor(line: DiffLine): CharRange[] | undefined {
  const pair = line.pair;
  if (!pair) return undefined;
  const del = line.kind === "del" ? line : pair;
  const add = line.kind === "del" ? pair : line;
  let result = intralineCache.get(del);
  if (result === undefined) {
    result = computeIntraline(del.text, add.text);
    intralineCache.set(del, result);
  }
  if (!result) return undefined;
  return line.kind === "del" ? result.del : result.add;
}

// ── Row model ───────────────────────────────────────────────────────────────

export type NoticeKind = "binary" | "large" | "meta" | "raw" | "truncated";

export type DiffListRow =
  | { type: "file"; key: string; fi: number }
  | { type: "hunk"; key: string; fi: number; hunk: DiffHunk; hidden: number }
  | { type: "line"; key: string; fi: number; line: DiffLine }
  | { type: "pair"; key: string; fi: number; left: DiffLine | null; right: DiffLine | null }
  | { type: "fold"; key: string; fi: number; id: string; count: number }
  | { type: "comment"; key: string; fi: number; annotations: DiffLineAnnotation[] }
  | { type: "notice"; key: string; fi: number; notice: NoticeKind }
  | { type: "end"; key: string; fi: number };

export interface BuildRowsOptions {
  mode: DiffViewMode;
  contextLines: number;
  ignoreWhitespace: boolean;
  collapsed: ReadonlySet<string>;
  expandedFolds: ReadonlySet<string>;
  largeOpened: ReadonlySet<string>;
  annotations: readonly DiffLineAnnotation[];
}

export interface BuiltRows {
  rows: DiffListRow[];
  fileStart: number[];
}

function annotationMatchesFile(annotation: DiffLineAnnotation, meta: DiffFileMeta): boolean {
  if (annotation.path !== meta.path) return false;
  if (!annotation.staging || !meta.staging) return true;
  return annotation.staging === meta.staging;
}

export function annotationEnd(annotation: Pick<DiffLineAnnotation, "lineNumber" | "endLineNumber">): number {
  return Math.max(annotation.lineNumber, annotation.endLineNumber ?? annotation.lineNumber);
}

function anchorKeys(line: DiffLine): string[] {
  const keys: string[] = [];
  if (line.oldLine !== undefined && line.kind !== "add") keys.push(`o:${line.oldLine}`);
  if (line.newLine !== undefined && line.kind !== "del") keys.push(`n:${line.newLine}`);
  return keys;
}

function annotationAnchorKey(annotation: DiffLineAnnotation): string {
  return `${annotation.side === "old" ? "o" : "n"}:${annotationEnd(annotation)}`;
}

function normalizeWs(text: string): string {
  return text.replace(/\s+/g, "");
}

/** Change block → display lines; with ignoreWhitespace, pairs equal modulo whitespace become context. */
function changeLines(segment: Extract<HunkSegment, { kind: "change" }>, ignoreWhitespace: boolean): Array<DiffLine[] | { ctx: DiffLine }> {
  if (!ignoreWhitespace) return [segment.dels.concat(segment.adds)];
  const out: Array<DiffLine[] | { ctx: DiffLine }> = [];
  let dels: DiffLine[] = [];
  let adds: DiffLine[] = [];
  const flush = () => {
    if (dels.length || adds.length) out.push(dels.concat(adds));
    dels = [];
    adds = [];
  };
  const count = Math.max(segment.dels.length, segment.adds.length);
  for (let index = 0; index < count; index += 1) {
    const del = segment.dels[index];
    const add = segment.adds[index];
    if (del && add && normalizeWs(del.text) === normalizeWs(add.text)) {
      flush();
      out.push({
        ctx: {
          kind: "ctx",
          text: add.text,
          oldLine: del.oldLine,
          newLine: add.newLine,
          oldIndex: del.oldIndex,
          newIndex: add.newIndex,
        },
      });
    } else {
      if (del) dels.push(del);
      if (add) adds.push(add);
    }
  }
  flush();
  return out;
}

function splitPairs(lines: DiffLine[]): Array<[DiffLine | null, DiffLine | null]> {
  const dels = lines.filter((line) => line.kind === "del");
  const adds = lines.filter((line) => line.kind === "add");
  const pairs: Array<[DiffLine | null, DiffLine | null]> = [];
  const count = Math.max(dels.length, adds.length);
  for (let index = 0; index < count; index += 1) pairs.push([dels[index] ?? null, adds[index] ?? null]);
  return pairs;
}

function lineKey(prefix: string, fileKey: string, left?: DiffLine | null, right?: DiffLine | null): string {
  return `${prefix}:${fileKey}:${left?.oldLine ?? "-"}:${right?.newLine ?? left?.newLine ?? "-"}`;
}

export function buildRows(files: readonly DiffListFile[], options: BuildRowsOptions): BuiltRows {
  const rows: DiffListRow[] = [];
  const fileStart: number[] = [];
  const foldContext = options.contextLines > 0 && options.contextLines < 999 ? options.contextLines : 0;

  files.forEach((file, fi) => {
    const fk = file.key;
    fileStart.push(rows.length);
    rows.push({ type: "file", key: `F:${fk}`, fi });
    if (options.collapsed.has(fk)) {
      rows.push({ type: "end", key: `E:${fk}`, fi });
      return;
    }
    const { parsed } = file;
    const pending = new Map<string, DiffLineAnnotation[]>();
    for (const annotation of options.annotations) {
      if (!annotationMatchesFile(annotation, file.meta)) continue;
      const anchor = annotationAnchorKey(annotation);
      const list = pending.get(anchor);
      if (list) list.push(annotation);
      else pending.set(anchor, [annotation]);
    }
    const flushComments = (lines: Array<DiffLine | null>) => {
      if (!pending.size) return;
      for (const line of lines) {
        if (!line) continue;
        for (const anchor of anchorKeys(line)) {
          const list = pending.get(anchor);
          if (!list) continue;
          pending.delete(anchor);
          rows.push({ type: "comment", key: `C:${fk}:${anchor}`, fi, annotations: list });
        }
      }
    };
    const emitLine = (line: DiffLine) => {
      if (options.mode === "unified") rows.push({ type: "line", key: lineKey("l", fk, line.kind === "add" ? null : line, line), fi, line });
      else rows.push({ type: "pair", key: lineKey("p", fk, line, line), fi, left: line, right: line });
      flushComments([line]);
    };
    const emitChange = (lines: DiffLine[]) => {
      if (options.mode === "unified") {
        for (const line of lines) emitLine(line);
        return;
      }
      for (const [left, right] of splitPairs(lines)) {
        rows.push({ type: "pair", key: lineKey("p", fk, left, right), fi, left, right });
        flushComments([left, right]);
      }
    };

    if (parsed.truncated) rows.push({ type: "notice", key: `N:${fk}:truncated`, fi, notice: "truncated" });
    if (parsed.binary) {
      rows.push({ type: "notice", key: `N:${fk}:binary`, fi, notice: "binary" });
    } else if (!parsed.parseable) {
      rows.push({ type: "notice", key: `N:${fk}:raw`, fi, notice: "raw" });
    } else if (!parsed.hunks.length) {
      rows.push({ type: "notice", key: `N:${fk}:meta`, fi, notice: "meta" });
    } else if (parsed.changed > LARGE_FILE_LINES && !options.largeOpened.has(fk)) {
      rows.push({ type: "notice", key: `N:${fk}:large`, fi, notice: "large" });
    } else {
      let previousEnd = 1;
      parsed.hunks.forEach((hunk, hi) => {
        const hidden = hunk.header && Number.isFinite(hunk.oldCount) ? Math.max(0, hunk.oldStart - previousEnd) : 0;
        if (Number.isFinite(hunk.oldCount)) previousEnd = hunk.oldStart + hunk.oldCount;
        if (hunk.header) rows.push({ type: "hunk", key: `H:${fk}:${hi}`, fi, hunk, hidden });
        const lastSegment = hunk.segments.length - 1;
        hunk.segments.forEach((segment, si) => {
          if (segment.kind === "change") {
            for (const part of changeLines(segment, options.ignoreWhitespace)) {
              if (Array.isArray(part)) emitChange(part);
              else emitLine(part.ctx);
            }
            return;
          }
          const lines = segment.lines;
          const foldId = `${fk}:${hi}:${si}`;
          const leading = si === 0;
          const trailing = si === lastSegment;
          const head = leading ? 0 : foldContext;
          const tail = trailing ? 0 : foldContext;
          if (foldContext && lines.length > head + tail + 4 && !options.expandedFolds.has(foldId)) {
            lines.slice(0, head).forEach(emitLine);
            rows.push({ type: "fold", key: `X:${foldId}`, fi, id: foldId, count: lines.length - head - tail });
            lines.slice(lines.length - tail).forEach(emitLine);
          } else {
            lines.forEach(emitLine);
          }
        });
      });
    }
    for (const [anchor, list] of pending) {
      rows.push({ type: "comment", key: `C:${fk}:${anchor}`, fi, annotations: list });
    }
    rows.push({ type: "end", key: `E:${fk}`, fi });
  });

  return { rows, fileStart };
}

/** All lines of one side in [from, to], for comment snapshots. */
export function sideLines(parsed: ParsedPatch, side: DiffSide, from: number, to: number): DiffLine[] {
  const out: DiffLine[] = [];
  for (const hunk of parsed.hunks) {
    for (const segment of hunk.segments) {
      const lines = segment.kind === "ctx" ? segment.lines : side === "old" ? segment.dels : segment.adds;
      for (const line of lines) {
        const number = side === "old" ? line.oldLine : line.newLine;
        if (number !== undefined && number >= from && number <= to) out.push(line);
      }
    }
  }
  return out;
}

// ── Paths & file tree ───────────────────────────────────────────────────────

export function splitPath(path: string): { dir: string; name: string } {
  const index = path.lastIndexOf("/");
  if (index < 0) return { dir: "", name: path };
  return { dir: path.slice(0, index + 1), name: path.slice(index + 1) };
}

export type TreeNode =
  | { type: "dir"; id: string; name: string; children: TreeNode[]; additions: number; deletions: number }
  | { type: "file"; id: string; name: string; file: DiffListFile };

interface MutableDir {
  name: string;
  dirs: Map<string, MutableDir>;
  files: DiffListFile[];
}

export function buildFileTree(files: readonly DiffListFile[]): TreeNode[] {
  const root: MutableDir = { name: "", dirs: new Map(), files: [] };
  for (const file of files) {
    const parts = file.meta.path.split("/").filter(Boolean);
    parts.pop();
    let node = root;
    for (const part of parts) {
      let next = node.dirs.get(part);
      if (!next) {
        next = { name: part, dirs: new Map(), files: [] };
        node.dirs.set(part, next);
      }
      node = next;
    }
    node.files.push(file);
  }

  const convert = (dir: MutableDir, prefix: string): TreeNode[] => {
    const dirs = Array.from(dir.dirs.values()).sort((a, b) => a.name.localeCompare(b.name));
    const out: TreeNode[] = [];
    for (const child of dirs) {
      let name = child.name;
      let cursor = child;
      while (cursor.files.length === 0 && cursor.dirs.size === 1) {
        const only = cursor.dirs.values().next().value as MutableDir;
        name = `${name}/${only.name}`;
        cursor = only;
      }
      const id = `${prefix}${name}/`;
      const children = convert(cursor, id);
      const totals = children.reduce(
        (acc, node) => {
          if (node.type === "dir") return { additions: acc.additions + node.additions, deletions: acc.deletions + node.deletions };
          return {
            additions: acc.additions + (node.file.meta.additions ?? node.file.parsed.additions),
            deletions: acc.deletions + (node.file.meta.deletions ?? node.file.parsed.deletions),
          };
        },
        { additions: 0, deletions: 0 },
      );
      out.push({ type: "dir", id, name, children, ...totals });
    }
    const filesSorted = [...dir.files].sort((a, b) => a.meta.path.localeCompare(b.meta.path));
    for (const file of filesSorted) {
      out.push({ type: "file", id: file.key, name: splitPath(file.meta.path).name, file });
    }
    return out;
  };

  return convert(root, "");
}
