import {
  imageFileToDataUrl,
  joinReportAssets,
  newAssetId,
  reportByteSize,
  type ReportAssets,
} from "./reportMarkdown";
import type { RunReport } from "./reports";

export const IMPORT_MAX_BYTES = 1_800_000;
export const MARKDOWN_EXT = /\.(md|markdown)$/i;

export type ImportStatus =
  | "pending"
  | "exists"
  | "too_large"
  | "importing"
  | "imported"
  | "failed"
  | "skipped";

export interface ImportRow {
  key: string;
  file: File;
  path: string;
  dirParts: string[];
  title: string;
  markdown: string;
  sha256: string;
  linkedRunId: string | null;
  suggested: boolean;
  folderId: string | null;
  missingImages: string[];
  imageFiles: Map<string, File>;
  status: ImportStatus;
  checked: boolean;
  embedImages: boolean;
  warning?: string;
  error?: string;
  detail?: string;
}

export function parseTitle(text: string): string | null {
  let fence = false;
  for (const line of text.split("\n")) {
    if (/^\s*```/.test(line)) fence = !fence;
    if (fence) continue;
    const m = /^#\s+(.+?)\s*$/.exec(line);
    if (m) return m[1];
  }
  return null;
}

export async function sha256Hex(text: string): Promise<string> {
  const buf = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return Array.from(new Uint8Array(buf), (b) => b.toString(16).padStart(2, "0")).join("");
}

export function normalizeName(s: string): string {
  return s.toLowerCase().replace(/[^a-z0-9\u4e00-\u9fff]+/g, "");
}

export function stemOf(path: string): string {
  const base = path.split("/").pop() ?? path;
  return base.replace(/\.[^.]+$/, "");
}

export function suggestRun(title: string, stem: string, runs: { run_id: string; name?: string }[]): string | null {
  const cand = [normalizeName(stem), normalizeName(title)].filter(Boolean);
  let hit: string | null = null;
  for (const run of runs) {
    const name = normalizeName(run.name || "");
    if (!name) continue;
    const match = cand.some(
      (c) => c === name || (name.length >= 3 && c.includes(name)) || (c.length >= 3 && name.includes(c)),
    );
    if (!match) continue;
    if (hit && hit !== run.run_id) return null;
    hit = run.run_id;
  }
  return hit;
}

interface FileLike {
  file: File;
  path: string;
}

export async function filesFromInput(files: FileList | File[]): Promise<FileLike[]> {
  const out: FileLike[] = [];
  for (const file of Array.from(files)) {
    const rel = (file as File & { webkitRelativePath?: string }).webkitRelativePath || file.name;
    const parts = rel.split("/").filter(Boolean);
    const path = parts.length > 1 ? parts.slice(1).join("/") : parts.join("/");
    if (file.name.startsWith(".")) continue;
    if (!MARKDOWN_EXT.test(file.name)) continue;
    out.push({ file, path });
  }
  return out;
}

export async function filesFromEntry(entry: FileSystemEntry, prefix = ""): Promise<FileLike[]> {
  if (entry.isFile) {
    const file = await new Promise<File>((resolve, reject) =>
      (entry as FileSystemFileEntry).file(resolve, reject),
    );
    if (file.name.startsWith(".")) return [];
    return [{ file, path: prefix + file.name }];
  }
  if (!entry.isDirectory || entry.name.startsWith(".")) return [];
  const reader = (entry as FileSystemDirectoryEntry).createReader();
  const all: FileSystemEntry[] = [];
  for (;;) {
    const batch = await new Promise<FileSystemEntry[]>((resolve, reject) =>
      reader.readEntries(resolve, reject),
    );
    if (!batch.length) break;
    all.push(...batch);
  }
  const out: FileLike[] = [];
  for (const child of all) {
    out.push(...(await filesFromEntry(child, `${prefix}${entry.name}/`)));
  }
  return out;
}

const IMAGE_LINK = /!\[([^\]]*)\]\(([^)\s]+)(?:\s+"[^"]*")?\)/g;

function outsideFences(text: string, index: number): boolean {
  let fence = false;
  for (let i = 0; i < index; i++) {
    if (text.startsWith("```", i)) {
      fence = !fence;
      i += 2;
    }
  }
  return !fence;
}

function normalizeRelPath(dir: string, rel: string): string | null {
  if (/^(?:https?:|data:|mailto:)/i.test(rel)) return null;
  const decoded = decodeURIComponent(rel.split("#")[0].split("?")[0]);
  const parts = (dir ? `${dir}/` : "").split("/").filter(Boolean);
  for (const seg of decoded.split("/")) {
    if (!seg || seg === ".") continue;
    if (seg === "..") parts.pop();
    else parts.push(seg);
  }
  return parts.join("/");
}

export async function buildImportRows(
  picked: FileLike[],
  opts: {
    reports: RunReport[];
    runs: { run_id: string; name?: string }[];
    existingFolderId: string | null;
    explicit?: boolean;
    nonMarkdown?: FileLike[];
  },
): Promise<ImportRow[]> {
  const byPath = new Map<string, File>();
  for (const p of picked) byPath.set(p.path.toLowerCase(), p.file);
  for (const p of opts.nonMarkdown ?? []) byPath.set(p.path.toLowerCase(), p.file);
  const seen = new Set<string>();
  const rows: ImportRow[] = [];

  const list = opts.explicit && opts.nonMarkdown ? [...picked, ...opts.nonMarkdown] : picked;
  for (const { file, path } of list) {
    const dir = path.split("/").slice(0, -1).join("/");
    const dirParts = dir ? dir.split("/") : [];
    const stem = stemOf(path);
    const key = path.toLowerCase();
    if (seen.has(key)) continue;
    seen.add(key);

    if (!MARKDOWN_EXT.test(file.name)) {
      if (opts.explicit) {
        rows.push({
          key, file, path, dirParts, title: stem, markdown: "", sha256: "",
          linkedRunId: null, suggested: false, folderId: opts.existingFolderId,
          missingImages: [], imageFiles: new Map(), status: "skipped",
          checked: false, embedImages: true, detail: "not-markdown",
        });
      }
      continue;
    }

    const text = await file.text();
    const sha = await sha256Hex(text);
    const title = parseTitle(text) ?? stem;
    const row: ImportRow = {
      key, file, path, dirParts, title, markdown: text, sha256: sha,
      linkedRunId: null, suggested: false, folderId: opts.existingFolderId,
      missingImages: [], imageFiles: new Map(), status: "pending",
      checked: true, embedImages: true,
    };
    const suggested = suggestRun(title, stem, opts.runs);
    if (suggested) {
      row.linkedRunId = suggested;
      row.suggested = true;
    }
    if (reportByteSize(text) > IMPORT_MAX_BYTES) {
      row.status = "too_large";
      row.error = `${(reportByteSize(text) / 1e6).toFixed(1)} MB`;
      row.checked = false;
    } else if (opts.reports.some((r) => r.import_source && r.import_source.path === path && r.import_source.sha256 === sha)) {
      row.status = "exists";
      row.checked = false;
    } else {
      const refs = new Set<string>();
      for (const m of text.matchAll(IMAGE_LINK)) {
        if (!outsideFences(text, m.index ?? 0)) continue;
        const resolved = normalizeRelPath(dir, m[2]);
        if (resolved) refs.add(resolved);
      }
      for (const ref of refs) {
        const img = byPath.get(ref.toLowerCase());
        if (img) row.imageFiles.set(ref, img);
        else row.missingImages.push(ref);
      }
    }
    rows.push(row);
  }
  return rows;
}

export async function rowMarkdown(row: ImportRow, embed: boolean): Promise<string> {
  if (!embed || !row.imageFiles.size) return row.markdown;
  const dir = row.path.split("/").slice(0, -1).join("/");
  const assets: ReportAssets = {};
  const edits: { start: number; end: number; text: string }[] = [];
  for (const m of row.markdown.matchAll(IMAGE_LINK)) {
    const index = m.index ?? 0;
    if (!outsideFences(row.markdown, index)) continue;
    const resolved = normalizeRelPath(dir, m[2]);
    const img = resolved ? row.imageFiles.get(resolved) : undefined;
    if (!img) continue;
    const id = newAssetId();
    assets[id] = await imageFileToDataUrl(img);
    edits.push({ start: index, end: index + m[0].length, text: `![${m[1]}][${id}]` });
  }
  let body = row.markdown;
  for (const e of edits.sort((a, b) => b.start - a.start)) {
    body = body.slice(0, e.start) + e.text + body.slice(e.end);
  }
  return joinReportAssets(body, assets);
}
