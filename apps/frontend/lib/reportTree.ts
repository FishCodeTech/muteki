import type { ReportFolder, RunReport } from "./reports";

export const REPORT_NODE_MIME = "application/x-muteki-report-node";
export const REPORTS_MAX_DEPTH = 5;
export const ORDER_STEP = 1000;

export interface DragReportNode {
  kind: "folder" | "report";
  id: string;
}

let activeReportNodeDrag: DragReportNode | null = null;

export function beginReportNodeDrag(node: DragReportNode): void {
  activeReportNodeDrag = node;
}

export function endReportNodeDrag(): void {
  activeReportNodeDrag = null;
}

export function hasReportNodeDrag(dt?: DataTransfer | null): boolean {
  return Boolean(activeReportNodeDrag) || Array.from(dt?.types ?? []).includes(REPORT_NODE_MIME);
}

export function getActiveReportNodeDrag(): DragReportNode | null {
  return activeReportNodeDrag;
}

export function readReportNodeDrag(dt?: DataTransfer | null): DragReportNode | null {
  if (dt && Array.from(dt.types ?? []).includes(REPORT_NODE_MIME)) {
    try {
      const parsed = JSON.parse(dt.getData(REPORT_NODE_MIME) || "null") as DragReportNode | null;
      if (parsed && (parsed.kind === "folder" || parsed.kind === "report") && typeof parsed.id === "string") {
        return parsed;
      }
    } catch {
    }
  }
  return activeReportNodeDrag;
}

export interface FolderNode {
  folder: ReportFolder;
  children: FolderNode[];
  reports: RunReport[];
  count: number;
  depth: number;
}

export interface ReportTree {
  roots: FolderNode[];
  rootReports: RunReport[];
}

export function foldersById(folders: ReportFolder[]): Map<string, ReportFolder> {
  return new Map(folders.map((f) => [f.id, f]));
}

export function sortFolders(folders: ReportFolder[]): ReportFolder[] {
  return [...folders].sort((a, b) => a.order - b.order || a.name.localeCompare(b.name));
}

export function sortReports(reports: RunReport[]): RunReport[] {
  return [...reports].sort((a, b) => (a.order ?? a.created_at) - (b.order ?? b.created_at) || b.updated_at - a.updated_at);
}

export function buildTree(reports: RunReport[], folders: ReportFolder[]): ReportTree {
  const nodes = new Map<string, FolderNode>();
  for (const f of folders) nodes.set(f.id, { folder: f, children: [], reports: [], count: 0, depth: 1 });
  const roots: FolderNode[] = [];
  for (const f of sortFolders(folders)) {
    const node = nodes.get(f.id)!;
    const parent = f.parent_id ? nodes.get(f.parent_id) : undefined;
    if (parent) {
      parent.children.push(node);
      node.depth = parent.depth + 1;
    } else {
      roots.push(node);
    }
  }
  const fixDepth = (node: FolderNode, depth: number) => {
    node.depth = depth;
    for (const c of node.children) fixDepth(c, depth + 1);
  };
  for (const r of roots) fixDepth(r, 1);
  const rootReports: RunReport[] = [];
  for (const r of reports) {
    const node = r.folder_id ? nodes.get(r.folder_id) : undefined;
    if (node) node.reports.push(r);
    else rootReports.push(r);
  }
  const count = (node: FolderNode): number => {
    node.reports = sortReports(node.reports);
    node.count = node.reports.length + node.children.reduce((sum, c) => sum + count(c), 0);
    return node.count;
  };
  for (const r of roots) count(r);
  return { roots, rootReports: sortReports(rootReports) };
}

export function folderDepth(folderId: string, byId: Map<string, ReportFolder>): number {
  let depth = 0;
  let cur: ReportFolder | undefined = byId.get(folderId);
  while (cur && depth < 10) {
    depth++;
    cur = cur.parent_id ? byId.get(cur.parent_id) : undefined;
  }
  return depth;
}

export function folderPath(folderId: string | null | undefined, byId: Map<string, ReportFolder>): ReportFolder[] {
  const out: ReportFolder[] = [];
  let cur = folderId ? byId.get(folderId) : undefined;
  while (cur) {
    out.unshift(cur);
    cur = cur.parent_id ? byId.get(cur.parent_id) : undefined;
  }
  return out;
}

export function pathLabel(path: ReportFolder[], rootLabel: string): string {
  return [rootLabel, ...path.map((f) => f.name)].join(" / ");
}

export function descendantFolderIds(folderId: string, folders: ReportFolder[]): Set<string> {
  const children = new Map<string | null, string[]>();
  for (const f of folders) {
    const list = children.get(f.parent_id) ?? [];
    list.push(f.id);
    children.set(f.parent_id, list);
  }
  const out = new Set<string>();
  const walk = (id: string) => {
    for (const c of children.get(id) ?? []) {
      if (!out.has(c)) {
        out.add(c);
        walk(c);
      }
    }
  };
  walk(folderId);
  return out;
}

export function subtreeHeight(folderId: string, folders: ReportFolder[]): number {
  const children = new Map<string, string[]>();
  for (const f of folders) {
    if (!f.parent_id) continue;
    const list = children.get(f.parent_id) ?? [];
    list.push(f.id);
    children.set(f.parent_id, list);
  }
  const height = (id: string, seen: Set<string>): number => {
    if (seen.has(id)) return 1;
    seen.add(id);
    const kids = children.get(id) ?? [];
    return 1 + kids.reduce((max, k) => Math.max(max, height(k, seen)), 0);
  };
  return height(folderId, new Set());
}

export function isSelfOrDescendant(target: string | null, folderId: string, folders: ReportFolder[]): boolean {
  if (!target) return false;
  if (target === folderId) return true;
  return descendantFolderIds(folderId, folders).has(target);
}

export function canReparent(folderId: string, newParent: string | null, folders: ReportFolder[]): boolean {
  const byId = foldersById(folders);
  if (isSelfOrDescendant(newParent, folderId, folders)) return false;
  return (newParent ? folderDepth(newParent, byId) : 0) + subtreeHeight(folderId, folders) <= REPORTS_MAX_DEPTH;
}

export function nextOrder(siblings: { order?: number }[]): number {
  return (siblings[siblings.length - 1]?.order ?? 0) + ORDER_STEP;
}

export function midpointOrder(before?: { order?: number } | null, after?: { order?: number } | null): number {
  const a = before?.order ?? null;
  const b = after?.order ?? null;
  if (a === null && b === null) return ORDER_STEP;
  if (a === null) return b! - ORDER_STEP;
  if (b === null) return a + ORDER_STEP;
  return (a + b) / 2;
}


export function expandedKey(mode: string): string {
  return `reports.tree.expanded.${mode}`;
}

export function loadExpanded(mode: string): Set<string> {
  try {
    const raw = localStorage.getItem(expandedKey(mode));
    return new Set(raw ? (JSON.parse(raw) as string[]) : []);
  } catch {
    return new Set();
  }
}

export function saveExpanded(mode: string, expanded: Set<string>): void {
  try {
    localStorage.setItem(expandedKey(mode), JSON.stringify([...expanded]));
  } catch {
  }
}
