"use client";

import { useEffect, useMemo, useRef, useState, type DragEvent, type KeyboardEvent, type PointerEvent as ReactPointerEvent, type ReactNode } from "react";
import { useT } from "@/lib/i18n";
import { cn } from "@/lib/cn";
import { createDragGhost, type DragGhost } from "@/components/chat/sidebar/dragGhost";
import {
  beginReportNodeDrag,
  buildTree,
  canReparent,
  endReportNodeDrag,
  folderPath,
  foldersById,
  getActiveReportNodeDrag,
  hasReportNodeDrag,
  isSelfOrDescendant,
  midpointOrder,
  pathLabel,
  readReportNodeDrag,
  REPORT_NODE_MIME,
  sortFolders,
  sortReports,
  type FolderNode,
} from "@/lib/reportTree";
import {
  createReportFolder,
  deleteReportFolder,
  endRunDrag,
  generationActive,
  hasRunDrag,
  readRunDragId,
  subscribeRunPointerDrag,
  updateReport,
  updateReportFolder,
  type ReportFolder,
  type RunReport,
} from "@/lib/reports";
import { reportErrorText } from "@/lib/reportErrors";
import { Icon, type IconName } from "@muteki/ui";
import { Button, Dialog, IconButton, Menu, MenuSeparator, shouldUseNativeContextMenu, toast } from "@muteki/ui";
import { fmtElapsed, useElapsedSeconds } from "./GenerationBanner";

export interface TreeActions {
  onSelect: (report: RunReport) => void;
  onRefresh: () => void;
  onDeleteReport: (report: RunReport) => void;
  onGenerate: (report: RunReport) => void;
  onCancelGeneration: (report: RunReport) => void;
  onNewDocument: (folderId: string | null) => void;
  onNewLinked?: (folderId: string | null) => void;
  onEditReport?: (report: RunReport) => void;
  onToggleStatus?: (report: RunReport) => void;
  onRenameReport?: (report: RunReport, title: string) => Promise<void> | void;
  onCopyMarkdown?: (report: RunReport) => void;
  onDownloadReport?: (report: RunReport) => void;
  onOpenRun?: (runId: string) => void;
  onOrganize?: () => void;
  onExpandAll?: () => void;
  onCollapseAll?: () => void;
  onRunDrop: (runId: string, folderId: string | null) => void;
  runName: (runId: string | null) => string;
  runCategory: (runId: string | null) => string;
  runSolved: (runId: string | null) => boolean;
}

interface Props extends TreeActions {
  mode: "ctf" | "pentest";
  reports: RunReport[];
  folders: ReportFolder[];
  selectedId: string;
  expanded: Set<string>;
  onToggleExpand: (id: string) => void;
  visibleReports: RunReport[] | null;
  rootLabel: string;
  solvedLabel: string;
  createFolderNonce?: number;
  checkedIds: Set<string>;
  onCheck: (ids: string[], checked: boolean) => void;
  empty?: ReactNode;
}

type DropPos = "before" | "after" | "into";

interface CtxMenu {
  point?: { x: number; y: number };
  kind: "folder" | "report" | "root";
  id: string;
}

export function ReportTree({
  mode,
  reports,
  folders,
  selectedId,
  expanded,
  onToggleExpand,
  visibleReports,
  rootLabel,
  solvedLabel,
  createFolderNonce,
  checkedIds,
  onCheck,
  empty,
  onSelect,
  onRefresh,
  onDeleteReport,
  onGenerate,
  onCancelGeneration,
  onNewDocument,
  onNewLinked,
  onEditReport,
  onToggleStatus,
  onRenameReport,
  onCopyMarkdown,
  onDownloadReport,
  onOpenRun,
  onOrganize,
  onExpandAll,
  onCollapseAll,
  onRunDrop,
  runName,
  runCategory,
  runSolved,
}: Props) {
  const t = useT();
  const byId = useMemo(() => foldersById(folders), [folders]);
  const reportById = useMemo(() => new Map(reports.map(report => [report.id, report])), [reports]);
  const flat = visibleReports !== null;
  const tree = useMemo(() => buildTree(reports, folders), [reports, folders]);
  const [menu, setMenu] = useState<CtxMenu | null>(null);
  const menuAnchor = useRef<HTMLElement | null>(null);
  const [renaming, setRenaming] = useState<string | null>(null);
  const [renameDraft, setRenameDraft] = useState("");
  const [moveTarget, setMoveTarget] = useState<{ kind: "folder" | "report"; id: string } | null>(null);
  const [deleteFolder, setDeleteFolder] = useState<ReportFolder | null>(null);
  const [dropHint, setDropHint] = useState<{ id: string; pos: DropPos } | null>(null);
  const [rootRunDragOver, setRootRunDragOver] = useState(false);
  const [rootNodeDragOver, setRootNodeDragOver] = useState(false);
  const [draggingId, setDraggingId] = useState<string | null>(null);
  const expandTimer = useRef<number>(0);
  const expandTargetRef = useRef<string | null>(null);
  const didNodeDragRef = useRef(false);
  const nodeDragCleanupRef = useRef<(() => void) | null>(null);
  const containerRef = useRef<HTMLDivElement>(null);
  const [focusId, setFocusId] = useState("");

  const clearAutoExpand = () => {
    window.clearTimeout(expandTimer.current);
    expandTargetRef.current = null;
  };

  const scheduleAutoExpand = (folderId: string) => {
    if (latestTreeRef.current.expanded.has(folderId)) {
      clearAutoExpand();
      return;
    }
    if (expandTargetRef.current === folderId) return;
    clearAutoExpand();
    expandTargetRef.current = folderId;
    expandTimer.current = window.setTimeout(() => {
      expandTargetRef.current = null;
      if (!latestTreeRef.current.expanded.has(folderId)) {
        latestTreeRef.current.onToggleExpand(folderId);
      }
    }, 600);
  };

  useEffect(() => {
    const clearDragState = () => {
      window.clearTimeout(expandTimer.current);
      expandTargetRef.current = null;
      setDropHint(null);
      setRootRunDragOver(false);
      setRootNodeDragOver(false);
      setDraggingId(null);
    };
    window.addEventListener("dragend", clearDragState);
    return () => {
      window.clearTimeout(expandTimer.current);
      window.removeEventListener("dragend", clearDragState);
      nodeDragCleanupRef.current?.();
    };
  }, []);

  const counts = useMemo(() => {
    const map = new Map<string, { folders: number; reports: number }>();
    const walk = (n: FolderNode): { folders: number; reports: number } => {
      const f = n.children.reduce((s: number, c: FolderNode) => s + walk(c).folders + 1, 0);
      map.set(n.folder.id, { folders: f, reports: n.count });
      return { folders: f, reports: n.count };
    };
    tree.roots.forEach(walk);
    return map;
  }, [tree]);


  const moveReportTo = async (reportId: string, folderId: string | null, order?: number) => {
    try {
      await updateReport(reportId, { folder_id: folderId, ...(order !== undefined ? { order } : {}) });
      onRefresh();
    } catch (e) {
      toast({ tone: "danger", title: reportErrorText(t, e) });
      onRefresh();
    }
  };

  const moveFolderTo = async (folderId: string, parentId: string | null, order?: number) => {
    const folder = byId.get(folderId);
    if (!folder) return;
    if (!canReparent(folderId, parentId, folders)) {
      toast({ tone: "warning", title: t("reports.tree.moveInvalid") });
      return;
    }
    try {
      await updateReportFolder(folderId, { parent_id: parentId, ...(order !== undefined ? { order } : {}), expected_revision: folder.revision });
      onRefresh();
    } catch (e) {
      toast({ tone: "danger", title: reportErrorText(t, e) });
      onRefresh();
    }
  };

  const latestTreeRef = useRef({
    expanded,
    onToggleExpand,
    onRunDrop,
    flat,
    folders,
    reports,
    byId,
    reportById,
    moveReportTo,
    moveFolderTo,
  });
  latestTreeRef.current = {
    expanded,
    onToggleExpand,
    onRunDrop,
    flat,
    folders,
    reports,
    byId,
    reportById,
    moveReportTo,
    moveFolderTo,
  };

  useEffect(() => {
    const resolveRunDropTarget = (x: number, y: number): { kind: "folder" | "row" | "root"; folderId: string | null } | null => {
      const hit = document.elementFromPoint(x, y);
      if (!hit || !containerRef.current?.contains(hit)) return null;
      const emptyEl = hit.closest<HTMLElement>("[data-report-empty-folder]");
      if (emptyEl?.dataset.reportEmptyFolder) {
        return { kind: "folder", folderId: emptyEl.dataset.reportEmptyFolder };
      }
      const folderEl = hit.closest<HTMLElement>("[data-report-folder-row]");
      if (folderEl?.dataset.reportFolderRow) {
        return { kind: "folder", folderId: folderEl.dataset.reportFolderRow };
      }
      const rowEl = hit.closest<HTMLElement>("[data-report-row]");
      if (rowEl) {
        return { kind: "row", folderId: rowEl.dataset.reportParentFolder || null };
      }
      return { kind: "root", folderId: null };
    };

    return subscribeRunPointerDrag((ev) => {
      if (ev.phase === "start") return;
      if (ev.phase === "cancel") {
        window.clearTimeout(expandTimer.current);
        expandTargetRef.current = null;
        setDropHint(null);
        setRootRunDragOver(false);
        return;
      }
      if (ev.phase === "move") {
        const target = resolveRunDropTarget(ev.x, ev.y);
        if (!target) {
          window.clearTimeout(expandTimer.current);
          expandTargetRef.current = null;
          setDropHint(null);
          setRootRunDragOver(false);
          return;
        }
        if (target.kind === "folder" && target.folderId) {
          const folderId = target.folderId;
          setRootRunDragOver(false);
          setDropHint((h) => (h?.id === folderId && h.pos === "into" ? h : { id: folderId, pos: "into" }));
          if (latestTreeRef.current.expanded.has(folderId)) {
            window.clearTimeout(expandTimer.current);
            expandTargetRef.current = null;
          } else if (expandTargetRef.current !== folderId) {
            window.clearTimeout(expandTimer.current);
            expandTargetRef.current = folderId;
            expandTimer.current = window.setTimeout(() => {
              expandTargetRef.current = null;
              if (!latestTreeRef.current.expanded.has(folderId)) {
                latestTreeRef.current.onToggleExpand(folderId);
              }
            }, 600);
          }
          return;
        }
        window.clearTimeout(expandTimer.current);
        expandTargetRef.current = null;
        if (target.kind === "row" && target.folderId) {
          const folderId = target.folderId;
          setRootRunDragOver(false);
          setDropHint((h) => (h?.id === folderId && h.pos === "into" ? h : { id: folderId, pos: "into" }));
        } else {
          setDropHint(null);
          setRootRunDragOver(true);
        }
        return;
      }
      if (ev.phase === "drop") {
        const target = resolveRunDropTarget(ev.x, ev.y);
        window.clearTimeout(expandTimer.current);
        expandTargetRef.current = null;
        setDropHint(null);
        setRootRunDragOver(false);
        if (!target) return false;
        latestTreeRef.current.onRunDrop(ev.runId, target.folderId);
        return true;
      }
    });
  }, []);

  const renameFolder = async (id: string, name: string) => {
    const folder = byId.get(id);
    const trimmed = name.trim();
    setRenaming(null);
    if (!folder || !trimmed || trimmed === folder.name) return;
    try {
      await updateReportFolder(id, { name: trimmed, expected_revision: folder.revision });
      onRefresh();
    } catch (e) {
      toast({ tone: "danger", title: reportErrorText(t, e) });
    }
  };

  const renameReport = async (id: string, title: string) => {
    const report = reports.find((r) => r.id === id);
    const trimmed = title.trim();
    setRenaming(null);
    if (!report || !trimmed || trimmed === report.title) return;
    if (onRenameReport) {
      await onRenameReport(report, trimmed);
      return;
    }
    try {
      await updateReport(id, { title: trimmed, expected_revision: report.revision });
      onRefresh();
    } catch (e) {
      toast({ tone: "danger", title: reportErrorText(t, e) });
    }
  };

  const newSubfolder = async (parentId: string | null) => {
    try {
      const made = await createReportFolder({ mode, name: t("reports.tree.newFolder"), parent_id: parentId });
      if (parentId && !expanded.has(parentId)) onToggleExpand(parentId);
      onRefresh();
      setRenaming(`folder:${made.id}`);
      setRenameDraft(made.name);
    } catch (e) {
      toast({ tone: "danger", title: reportErrorText(t, e) });
    }
  };

  const lastFolderNonceRef = useRef(createFolderNonce ?? 0);
  useEffect(() => {
    const next = createFolderNonce ?? 0;
    if (next > lastFolderNonceRef.current) {
      lastFolderNonceRef.current = next;
      void newSubfolder(null);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [createFolderNonce]);

  const doDeleteFolder = async (folder: ReportFolder, strategy: "move_up" | "delete_all") => {
    try {
      const res = await deleteReportFolder(folder.id, strategy);
      toast({ tone: "success", title: t("reports.tree.deleted", { reports: res.deleted_reports, folders: res.deleted_folders }) });
      setDeleteFolder(null);
      onRefresh();
    } catch (e) {
      toast({ tone: "danger", title: reportErrorText(t, e) });
    }
  };


  const nodeDrag = (e: DragEvent, kind: "folder" | "report", id: string) => {
    if (e.target instanceof HTMLElement && e.target.closest("input, button")) {
      e.preventDefault();
      e.stopPropagation();
      return;
    }
    e.dataTransfer.setData("text/plain", id);
    e.dataTransfer.setData(REPORT_NODE_MIME, JSON.stringify({ kind, id }));
    e.dataTransfer.effectAllowed = "move";
    beginReportNodeDrag({ kind, id });
    setDraggingId(id);
  };

  const dragPayload = (e: DragEvent) => readReportNodeDrag(e.dataTransfer);

  const dropPosFor = (
    e: DragEvent<HTMLElement>,
    isFolder: boolean,
    draggedKind: "folder" | "report" | null,
  ): DropPos => {
    if (isFolder && draggedKind !== "folder") return "into";
    const rect = e.currentTarget.getBoundingClientRect();
    const y = e.clientY - rect.top;
    if (isFolder) {
      if (y >= rect.height * 0.22 && y <= rect.height * 0.78) return "into";
      return y < rect.height / 2 ? "before" : "after";
    }
    return y < rect.height / 2 ? "before" : "after";
  };

  const onRowDragOver = (
    e: DragEvent<HTMLElement>,
    id: string,
    isFolder: boolean,
    parentOf: string | null,
  ) => {
    const hasNode = !flat && hasReportNodeDrag(e.dataTransfer);
    const hasRun = hasRunDrag(e.dataTransfer);
    if (!hasNode && !hasRun) return;
    if (hasRun) {
      e.preventDefault();
      e.stopPropagation();
      e.dataTransfer.dropEffect = "copy";
      if (isFolder) {
        setRootRunDragOver(false);
        setDropHint((h) => (h?.id === id && h.pos === "into" ? h : { id, pos: "into" }));
        scheduleAutoExpand(id);
      } else if (parentOf) {
        clearAutoExpand();
        setRootRunDragOver(false);
        setDropHint((h) => (h?.id === parentOf && h.pos === "into" ? h : { id: parentOf, pos: "into" }));
      } else {
        clearAutoExpand();
        setDropHint(null);
        setRootRunDragOver(true);
      }
      return;
    }
    const activeNode = getActiveReportNodeDrag();
    if (
      activeNode?.id === id
      || (activeNode?.kind === "folder" && isFolder && isSelfOrDescendant(id, activeNode.id, folders))
    ) {
      e.stopPropagation();
      return;
    }
    e.preventDefault();
    e.stopPropagation();
    const pos = dropPosFor(e, isFolder, activeNode?.kind ?? null);
    e.dataTransfer.dropEffect = "move";
    setDropHint((h) => (h?.id === id && h.pos === pos ? h : { id, pos }));
    if (isFolder && pos === "into") {
      scheduleAutoExpand(id);
    } else {
      clearAutoExpand();
    }
  };

  const onEmptyFolderDragOver = (e: DragEvent<HTMLElement>, folderId: string) => {
    const hasNode = !flat && hasReportNodeDrag(e.dataTransfer);
    const hasRun = hasRunDrag(e.dataTransfer);
    if (!hasNode && !hasRun) return;
    if (!hasRun) {
      const activeNode = getActiveReportNodeDrag();
      if (activeNode?.kind === "folder" && (activeNode.id === folderId || isSelfOrDescendant(folderId, activeNode.id, folders))) {
        e.stopPropagation();
        return;
      }
    }
    e.preventDefault();
    e.stopPropagation();
    e.dataTransfer.dropEffect = hasRun ? "copy" : "move";
    setRootRunDragOver(false);
    setDropHint((h) => (h?.id === folderId && h.pos === "into" ? h : { id: folderId, pos: "into" }));
  };

  const onRowDrop = (
    e: DragEvent<HTMLElement>,
    id: string,
    parentOf: string | null,
    siblings: { id: string; order?: number }[],
    isFolder: boolean,
  ) => {
    const hasRun = hasRunDrag(e.dataTransfer);
    const payload = flat ? null : dragPayload(e);
    if (!hasRun && !payload) return;
    e.preventDefault();
    e.stopPropagation();
    clearAutoExpand();
    const pos = dropHint?.id === id ? dropHint.pos : "into";
    setDropHint(null);
    setRootRunDragOver(false);
    setDraggingId(null);
    if (hasRun) {
      const runId = readRunDragId(e.dataTransfer);
      endRunDrag();
      if (runId) onRunDrop(runId, isFolder ? id : parentOf);
      return;
    }
    endReportNodeDrag();
    if (!payload || payload.id === id) return;
    if (payload.kind === "report") {
      if (isFolder || pos === "into") {
        if (!expanded.has(id)) onToggleExpand(id);
        void moveReportTo(payload.id, id);
      } else {
        const idx = siblings.findIndex((s) => s.id === id);
        const before = pos === "before" ? siblings[idx - 1] : siblings[idx];
        const after = pos === "before" ? siblings[idx] : siblings[idx + 1];
        const order = midpointOrder(before, after);
        void moveReportTo(payload.id, parentOf, order);
      }
      return;
    }
    if (isFolder && pos === "into") {
      if (!expanded.has(id)) onToggleExpand(id);
      void moveFolderTo(payload.id, id);
    } else if (isFolder) {
      const idx = siblings.findIndex((s) => s.id === id);
      const before = pos === "before" ? siblings[idx - 1] : siblings[idx];
      const after = pos === "before" ? siblings[idx] : siblings[idx + 1];
      const order = midpointOrder(before, after);
      void moveFolderTo(payload.id, parentOf, order);
    } else {
      void moveFolderTo(payload.id, parentOf);
    }
  };

  const onRootDrop = (e: DragEvent<HTMLElement>) => {
    clearAutoExpand();
    setDropHint(null);
    setRootRunDragOver(false);
    setDraggingId(null);
    if (hasRunDrag(e.dataTransfer)) {
      e.preventDefault();
      e.stopPropagation();
      const runId = readRunDragId(e.dataTransfer);
      endRunDrag();
      if (runId) onRunDrop(runId, null);
      return;
    }
    const payload = flat ? null : dragPayload(e);
    if (!payload) return;
    e.preventDefault();
    e.stopPropagation();
    endReportNodeDrag();
    if (payload.kind === "report") void moveReportTo(payload.id, null);
    else void moveFolderTo(payload.id, null);
  };

  const startNodePointerDrag = (
    e: ReactPointerEvent<HTMLElement>,
    kind: "folder" | "report",
    id: string,
  ) => {
    if (e.button !== 0 || e.pointerType === "touch") return;
    if (latestTreeRef.current.flat) return;
    if (e.target instanceof HTMLElement && e.target.closest("input, button")) return;
    nodeDragCleanupRef.current?.();
    const startX = e.clientX;
    const startY = e.clientY;
    const sourceEl = e.currentTarget;
    let active = false;
    let ghost: DragGhost | null = null;
    let didDragTimer: number | null = null;

    const dropPosAt = (el: HTMLElement, isFolder: boolean, clientY: number): DropPos => {
      if (isFolder && kind !== "folder") return "into";
      const rect = el.getBoundingClientRect();
      const y = clientY - rect.top;
      if (isFolder) {
        if (y >= rect.height * 0.22 && y <= rect.height * 0.78) return "into";
        return y < rect.height / 2 ? "before" : "after";
      }
      return y < rect.height / 2 ? "before" : "after";
    };

    const cleanup = (cancelled: boolean) => {
      nodeDragCleanupRef.current = null;
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      window.removeEventListener("pointercancel", onCancel);
      window.removeEventListener("keydown", onEsc);
      if (active) {
        delete document.documentElement.dataset.cxSidebarDragging;
        clearAutoExpand();
        endReportNodeDrag();
        setDropHint(null);
        setRootNodeDragOver(false);
        setDraggingId(null);
        if (cancelled) ghost?.cancel();
        if (didDragTimer !== null) window.clearTimeout(didDragTimer);
        didDragTimer = window.setTimeout(() => {
          didNodeDragRef.current = false;
        }, 0);
      }
    };

    const onMove = (ev: PointerEvent) => {
      if (!active) {
        if (Math.hypot(ev.clientX - startX, ev.clientY - startY) < 5) return;
        active = true;
        didNodeDragRef.current = true;
        setDraggingId(id);
        beginReportNodeDrag({ kind, id });
        const reduced = document.documentElement.dataset.motion === "reduce"
          || Boolean(window.matchMedia?.("(prefers-reduced-motion: reduce)").matches);
        ghost = createDragGhost(sourceEl, startX, startY, reduced);
        document.documentElement.dataset.cxSidebarDragging = "";
      }
      ev.preventDefault();
      ghost?.move(ev.clientX, ev.clientY);
      const hit = document.elementFromPoint(ev.clientX, ev.clientY);
      const latest = latestTreeRef.current;
      const emptyEl = hit?.closest<HTMLElement>("[data-report-empty-folder]");
      if (emptyEl?.dataset.reportEmptyFolder) {
        const folderId = emptyEl.dataset.reportEmptyFolder;
        if (kind === "folder" && (id === folderId || isSelfOrDescendant(folderId, id, latest.folders))) {
          clearAutoExpand();
          setDropHint(null);
          setRootNodeDragOver(false);
        } else {
          clearAutoExpand();
          setRootNodeDragOver(false);
          setDropHint((h) => (h?.id === folderId && h.pos === "into" ? h : { id: folderId, pos: "into" }));
        }
        return;
      }
      const folderEl = hit?.closest<HTMLElement>("[data-report-folder-row]");
      if (folderEl?.dataset.reportFolderRow) {
        const folderId = folderEl.dataset.reportFolderRow;
        if (id === folderId || (kind === "folder" && isSelfOrDescendant(folderId, id, latest.folders))) {
          clearAutoExpand();
          setDropHint(null);
          setRootNodeDragOver(false);
        } else {
          const pos = dropPosAt(folderEl, true, ev.clientY);
          setRootNodeDragOver(false);
          setDropHint((h) => (h?.id === folderId && h.pos === pos ? h : { id: folderId, pos }));
          if (pos === "into") scheduleAutoExpand(folderId);
          else clearAutoExpand();
        }
        return;
      }
      const rowEl = hit?.closest<HTMLElement>("[data-report-row]");
      if (rowEl?.dataset.reportRow) {
        const reportId = rowEl.dataset.reportRow;
        clearAutoExpand();
        setRootNodeDragOver(false);
        if (reportId === id) {
          setDropHint(null);
        } else {
          const pos = dropPosAt(rowEl, false, ev.clientY);
          setDropHint((h) => (h?.id === reportId && h.pos === pos ? h : { id: reportId, pos }));
        }
        return;
      }
      clearAutoExpand();
      setDropHint(null);
      setRootNodeDragOver(Boolean(hit?.closest("[data-report-drop-tree]")));
    };

    const onUp = (ev: PointerEvent) => {
      const wasActive = active;
      let moved = false;
      if (wasActive) {
        ev.preventDefault();
        ghost?.move(ev.clientX, ev.clientY);
        const hit = document.elementFromPoint(ev.clientX, ev.clientY);
        const latest = latestTreeRef.current;
        const emptyEl = hit?.closest<HTMLElement>("[data-report-empty-folder]");
        const folderEl = hit?.closest<HTMLElement>("[data-report-folder-row]");
        const rowEl = hit?.closest<HTMLElement>("[data-report-row]");
        if (emptyEl?.dataset.reportEmptyFolder) {
          const folderId = emptyEl.dataset.reportEmptyFolder;
          if (!(kind === "folder" && (id === folderId || isSelfOrDescendant(folderId, id, latest.folders)))) {
            if (!latest.expanded.has(folderId)) latest.onToggleExpand(folderId);
            if (kind === "report") void latest.moveReportTo(id, folderId);
            else void latest.moveFolderTo(id, folderId);
            moved = true;
          }
        } else if (folderEl?.dataset.reportFolderRow) {
          const folderId = folderEl.dataset.reportFolderRow;
          if (!(id === folderId || (kind === "folder" && isSelfOrDescendant(folderId, id, latest.folders)))) {
            const pos = dropPosAt(folderEl, true, ev.clientY);
            if (kind === "report" || pos === "into") {
              if (!latest.expanded.has(folderId)) latest.onToggleExpand(folderId);
              if (kind === "report") void latest.moveReportTo(id, folderId);
              else void latest.moveFolderTo(id, folderId);
              moved = true;
            } else {
              const parentOf = latest.byId.get(folderId)?.parent_id ?? null;
              const siblings = sortFolders(latest.folders.filter((x) => x.parent_id === parentOf));
              const idx = siblings.findIndex((s) => s.id === folderId);
              const before = pos === "before" ? siblings[idx - 1] : siblings[idx];
              const after = pos === "before" ? siblings[idx] : siblings[idx + 1];
              void latest.moveFolderTo(id, parentOf, midpointOrder(before, after));
              moved = true;
            }
          }
        } else if (rowEl?.dataset.reportRow) {
          const reportId = rowEl.dataset.reportRow;
          if (reportId !== id) {
            const targetReport = latest.reportById.get(reportId);
            const parentOf = targetReport?.folder_id ?? null;
            if (kind === "report") {
              const pos = dropPosAt(rowEl, false, ev.clientY);
              const siblings = sortReports(latest.reports.filter((x) => (x.folder_id ?? null) === parentOf));
              const idx = siblings.findIndex((s) => s.id === reportId);
              const before = pos === "before" ? siblings[idx - 1] : siblings[idx];
              const after = pos === "before" ? siblings[idx] : siblings[idx + 1];
              void latest.moveReportTo(id, parentOf, midpointOrder(before, after));
              moved = true;
            } else {
              void latest.moveFolderTo(id, parentOf);
              moved = true;
            }
          }
        } else if (hit?.closest("[data-report-drop-tree]")) {
          if (kind === "report") void latest.moveReportTo(id, null);
          else void latest.moveFolderTo(id, null);
          moved = true;
        }
        if (moved) ghost?.drop();
        else ghost?.cancel();
      }
      cleanup(false);
    };

    const onCancel = () => cleanup(true);
    const onEsc = (ev: globalThis.KeyboardEvent) => {
      if (ev.key === "Escape") cleanup(true);
    };

    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
    window.addEventListener("pointercancel", onCancel);
    window.addEventListener("keydown", onEsc);
    nodeDragCleanupRef.current = () => {
      if (didDragTimer !== null) window.clearTimeout(didDragTimer);
      cleanup(true);
    };
  };


  const openMenu = (event: React.MouseEvent<HTMLElement>, kind: CtxMenu["kind"], id: string) => {
    menuAnchor.current = event.currentTarget;
    const point = event.type === "contextmenu" ? { x: event.clientX, y: event.clientY } : undefined;
    setMenu({ kind, id, point });
  };

  const menuFolder = menu?.kind === "folder" ? byId.get(menu.id) : undefined;
  const menuReport = menu?.kind === "report" ? reportById.get(menu.id) : undefined;


  interface Row {
    kind: "folder" | "report";
    id: string;
    depth: number;
    parentId: string | null;
  }
  const flatRows = useMemo(() => {
    if (visibleReports) return visibleReports.map((r): Row => ({ kind: "report", id: r.id, depth: 1, parentId: null }));
    const out: Row[] = [];
    const walk = (nodes: FolderNode[]) => {
      for (const n of nodes) {
        out.push({ kind: "folder", id: n.folder.id, depth: n.depth, parentId: n.folder.parent_id });
        if (expanded.has(n.folder.id)) {
          walk(n.children);
          for (const r of n.reports) out.push({ kind: "report", id: r.id, depth: n.depth + 1, parentId: n.folder.id });
        }
      }
    };
    walk(tree.roots);
    for (const r of tree.rootReports) out.push({ kind: "report", id: r.id, depth: 1, parentId: null });
    return out;
  }, [tree, expanded, visibleReports]);

  const rowIndexById = useMemo(() => new Map(flatRows.map((row, index) => [row.id, index])), [flatRows]);
  const lastCheckedId = useRef("");
  const toggleCheck = (id: string, range: boolean) => {
    const order = flatRows.filter((row) => row.kind === "report").map((row) => row.id);
    const from = order.indexOf(lastCheckedId.current);
    const to = order.indexOf(id);
    lastCheckedId.current = id;
    if (range && from >= 0 && to >= 0) {
      onCheck(order.slice(Math.min(from, to), Math.max(from, to) + 1), true);
      return;
    }
    onCheck([id], !checkedIds.has(id));
  };

  const onTreeKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (!e.currentTarget.contains(e.target as Node)) return;
    const idx = rowIndexById.get(focusId) ?? 0;
    const row = flatRows[idx];
    if (!row) return;
    const focus = (i: number) => {
      const next = flatRows[i];
      if (!next) return;
      setFocusId(next.id);
      containerRef.current?.querySelector<HTMLElement>(`[data-tree-id="${next.id}"]`)?.focus();
    };
    switch (e.key) {
      case "ArrowDown": e.preventDefault(); focus(idx + 1); break;
      case "ArrowUp": e.preventDefault(); focus(idx - 1); break;
      case "ArrowRight":
        e.preventDefault();
        if (row.kind === "folder" && !expanded.has(row.id)) onToggleExpand(row.id);
        else focus(idx + 1);
        break;
      case "ArrowLeft":
        e.preventDefault();
        if (row.kind === "folder" && expanded.has(row.id)) onToggleExpand(row.id);
        else if (row.parentId) focus(rowIndexById.get(row.parentId) ?? -1);
        break;
      case "Enter": {
        if (e.target instanceof Element && e.target.closest("button")) break;
        e.preventDefault();
        if (row.kind === "report") {
          const r = reportById.get(row.id);
          if (r) onSelect(r);
        }
        break;
      }
      case "F2":
        if (row.kind === "folder") {
          e.preventDefault();
          setRenaming(`folder:${row.id}`);
          setRenameDraft(byId.get(row.id)?.name ?? "");
        } else if (row.kind === "report") {
          e.preventDefault();
          setRenaming(`report:${row.id}`);
          setRenameDraft(reports.find((x) => x.id === row.id)?.title ?? "");
        }
        break;
      case "Delete": {
        e.preventDefault();
        if (row.kind === "folder") {
          const f = byId.get(row.id);
          if (f) setDeleteFolder(f);
        } else {
          const r = reportById.get(row.id);
          if (r) onDeleteReport(r);
        }
        break;
      }
      case "ContextMenu":
      case "F10":
        if (e.key === "F10" && !e.shiftKey) break;
        e.preventDefault();
        menuAnchor.current = (e.target as HTMLElement).closest<HTMLElement>("[data-tree-id]");
        setMenu({ kind: row.kind, id: row.id });
        break;
    }
  };


  const selecting = checkedIds.size > 0;
  const reportRow = (r: RunReport, depth: number, siblings: { id: string; order?: number }[]) => {
    const gen = r.generation;
    const isRenamingReport = renaming === `report:${r.id}`;
    const isSelected = r.id === selectedId;
    const isChecked = checkedIds.has(r.id);
    const name = r.run_id ? runName(r.run_id) : "";
    const meta = [
      name && name !== r.title ? name : "",
      runCategory(r.run_id),
      flat && r.folder_id ? pathLabel(folderPath(r.folder_id, byId), rootLabel) : "",
    ].filter(Boolean).join(" · ");
    const solved = runSolved(r.run_id);
    const hasMarks = solved || r.status === "final" || Boolean(gen) || Boolean(r.pending_update);
    return (
      <div
        key={r.id}
        role="treeitem"
        aria-level={depth}
        aria-selected={isSelected}
        data-tree-id={r.id}
        data-report-row={r.id}
        data-report-parent-folder={flat ? "" : (r.folder_id ?? "")}
        tabIndex={focusId === r.id || (!focusId && isSelected) ? 0 : -1}
        draggable={false}
        onPointerDown={!flat && !isRenamingReport ? (e) => startNodePointerDrag(e, "report", r.id) : undefined}
        onDragStart={(e) => nodeDrag(e, "report", r.id)}
        onDragEnd={() => { clearAutoExpand(); endReportNodeDrag(); setDropHint(null); setRootRunDragOver(false); setDraggingId(null); }}
        onDragOver={(e) => onRowDragOver(e, r.id, false, flat ? null : (r.folder_id ?? null))}
        onDragLeave={(e) => {
          if (!e.currentTarget.contains(e.relatedTarget as Node | null)) {
            setDropHint((h) => (h?.id === r.id ? null : h));
          }
        }}
        onDrop={(e) => onRowDrop(e, r.id, flat ? null : (r.folder_id ?? null), siblings, false)}
        onClick={() => {
          if (didNodeDragRef.current) return;
          onSelect(r);
        }}
        onContextMenu={(e) => {
          if (shouldUseNativeContextMenu(e.target)) return;
          e.preventDefault();
          e.stopPropagation();
          openMenu(e, "report", r.id);
        }}
        onFocus={() => setFocusId(r.id)}
        className={cn(
          "group relative flex cursor-pointer items-start gap-2 rounded-xl py-2 pr-2.5 outline-none transition-colors focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]",
          !isRenamingReport && "select-none",
          draggingId === r.id && "opacity-40",
          isChecked ? "bg-cx-accent-soft text-cx-fg" : isSelected ? "bg-cx-active text-cx-fg" : "hover:bg-cx-hover",
          dropHint?.id === r.id && "bg-cx-accent-soft",
        )}
        style={{ paddingLeft: `${(depth - 1) * 16 + (selecting ? 8 : 10)}px` }}
      >
        {dropHint?.id === r.id && dropHint.pos !== "into" && (
          <span className={cn("pointer-events-none absolute inset-x-2 h-0.5 rounded bg-cx-accent", dropHint.pos === "before" ? "top-0" : "bottom-0")} />
        )}
        {selecting && (
          <span className="flex h-5 w-4 shrink-0 items-center justify-center">
            <input
              type="checkbox"
              checked={isChecked}
              onClick={(e) => e.stopPropagation()}
              onChange={(e) => toggleCheck(r.id, (e.nativeEvent as MouseEvent).shiftKey)}
              aria-label={t("reports.selectReport", { title: r.title })}
              className="size-3.5 cursor-pointer accent-[var(--cx-accent)]"
            />
          </span>
        )}
        <span className="flex min-w-0 flex-1 flex-col gap-0.5">
          <span className="flex min-w-0 items-baseline gap-2">
            {isRenamingReport ? (
              <input
                autoFocus
                value={renameDraft}
                onClick={(e) => e.stopPropagation()}
                onChange={(e) => setRenameDraft(e.target.value)}
                onBlur={() => void renameReport(r.id, renameDraft)}
                onKeyDown={(e) => {
                  e.stopPropagation();
                  if (e.key === "Enter") void renameReport(r.id, renameDraft);
                  if (e.key === "Escape") setRenaming(null);
                }}
                aria-label={t("reports.tree.rename")}
                className="h-6 min-w-0 flex-1 rounded border border-cx-accent-line bg-cx-bg px-1 text-cx-sm outline-none"
              />
            ) : (
              <span className={cn("min-w-0 flex-1 truncate text-cx-sm leading-5 text-cx-fg", isSelected ? "font-medium" : "font-normal")}>{r.title}</span>
            )}
            <time
              className="shrink-0 text-cx-2xs tabular-nums text-cx-fg-4 transition-opacity group-hover:opacity-0 group-focus-within:opacity-0"
              title={r.updated_at ? new Date(r.updated_at * 1000).toLocaleString() : undefined}
            >
              {fmtDay(r.updated_at)}
            </time>
          </span>
          {(meta || hasMarks) && (
            <span className="flex min-w-0 items-center gap-2 text-cx-xs leading-4 text-cx-fg-3">
              {meta && <span className="min-w-0 truncate">{meta}</span>}
              {solved && (
                <span className="inline-flex shrink-0 items-center gap-0.5 text-cx-success">
                  <Icon name="check" size={11} />
                  {solvedLabel}
                </span>
              )}
              {r.status === "final" && <span className="shrink-0 text-cx-accent">{t("reports.filter.final")}</span>}
              {gen?.state === "running" && <RunningMark startedAt={gen.started_at} />}
              {gen?.state === "queued" && (
                <span className="shrink-0 text-cx-accent">{t("reports.gen.queuedBadge", { n: gen.queue_position ?? 1 })}</span>
              )}
              {gen?.state === "failed" && <span className="shrink-0 text-cx-danger">{t("reports.gen.failed")}</span>}
              {r.pending_update && <span className="shrink-0 text-cx-accent">{t("reports.pending.badge")}</span>}
            </span>
          )}
        </span>
        <span className="absolute right-1.5 top-1.5 flex items-center gap-0.5 opacity-0 transition-opacity group-hover:opacity-100 group-focus-within:opacity-100">
          {!selecting && (
            <span className="grid size-6 place-items-center">
              <input
                type="checkbox"
                checked={isChecked}
                onClick={(e) => e.stopPropagation()}
                onChange={(e) => toggleCheck(r.id, (e.nativeEvent as MouseEvent).shiftKey)}
                aria-label={t("reports.selectReport", { title: r.title })}
                className="size-3.5 cursor-pointer accent-[var(--cx-accent)]"
              />
            </span>
          )}
          <IconButton
            icon="more"
            label={t("reports.more")}
            size="xs"
            onClick={(e) => { e.stopPropagation(); openMenu(e, "report", r.id); }}
          />
        </span>
      </div>
    );
  };
  const folderRow = (node: FolderNode, siblings: { id: string; order?: number }[]) => {
    const f = node.folder;
    const open = expanded.has(f.id);
    const childFolders = open ? node.children.map(({ folder }) => folder) : [];
    const isRenaming = renaming === `folder:${f.id}`;
    return (
      <div key={f.id} role="none">
        <div
          role="treeitem"
          aria-level={node.depth}
          aria-expanded={open}
          aria-selected={false}
          data-tree-id={f.id}
          data-report-folder-row={f.id}
          tabIndex={focusId === f.id ? 0 : -1}
          draggable={false}
          onPointerDown={!isRenaming ? (e) => startNodePointerDrag(e, "folder", f.id) : undefined}
          onDragStart={(e) => nodeDrag(e, "folder", f.id)}
          onDragEnd={() => { clearAutoExpand(); endReportNodeDrag(); setDropHint(null); setRootRunDragOver(false); setDraggingId(null); }}
          onDragOver={(e) => onRowDragOver(e, f.id, true, f.parent_id)}
          onDragLeave={(e) => {
            if (!e.currentTarget.contains(e.relatedTarget as Node | null)) {
              if (expandTargetRef.current === f.id) clearAutoExpand();
              setDropHint((h) => (h?.id === f.id ? null : h));
            }
          }}
          onDrop={(e) => onRowDrop(e, f.id, f.parent_id, siblings, true)}
          onClick={() => {
            if (didNodeDragRef.current) return;
            onToggleExpand(f.id);
          }}
          onContextMenu={(e) => {
            if (shouldUseNativeContextMenu(e.target)) return;
            e.preventDefault();
            e.stopPropagation();
            openMenu(e, "folder", f.id);
          }}
          onFocus={() => setFocusId(f.id)}
          className={cn(
            "group relative flex h-8 cursor-pointer items-center gap-2 rounded-lg pr-2 text-cx-xs font-medium text-cx-fg-3 outline-none transition-colors hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]",
            !isRenaming && "select-none",
            draggingId === f.id && "opacity-40",
            dropHint?.id === f.id && dropHint.pos === "into" && "bg-cx-accent-soft text-cx-fg ring-1 ring-inset ring-cx-accent/60",
          )}
          style={{ paddingLeft: `${(node.depth - 1) * 16 + 6}px` }}
        >
          {dropHint?.id === f.id && dropHint.pos !== "into" && (
            <span className={cn("pointer-events-none absolute inset-x-2 h-0.5 rounded bg-cx-accent", dropHint.pos === "before" ? "top-0" : "bottom-0")} />
          )}
          <span className="flex w-4 shrink-0 justify-center">
            <Icon name="chevronRight" size={12} className={cn("text-cx-fg-4 transition-transform", open && "rotate-90")} />
          </span>
          {isRenaming ? (
            <input
              autoFocus
              value={renameDraft}
              onClick={(e) => e.stopPropagation()}
              onChange={(e) => setRenameDraft(e.target.value)}
              onBlur={() => void renameFolder(f.id, renameDraft)}
              onKeyDown={(e) => {
                if (e.key === "Enter") void renameFolder(f.id, renameDraft);
                if (e.key === "Escape") setRenaming(null);
              }}
              aria-label={t("reports.tree.rename")}
              className="h-6 min-w-0 flex-1 rounded border border-cx-accent-line bg-cx-bg px-1 text-cx-sm outline-none"
            />
          ) : (
            <span className="min-w-0 flex-1 truncate">{f.name}</span>
          )}
          {node.count > 0 && (
            <span className="shrink-0 text-cx-2xs font-normal tabular-nums text-cx-fg-4 group-hover:opacity-0 group-focus-within:opacity-0">{node.count}</span>
          )}
          <IconButton
            icon="more"
            label={t("reports.more")}
            size="xs"
            className="absolute right-1 opacity-0 group-hover:opacity-100 group-focus-within:opacity-100"
            onClick={(e) => { e.stopPropagation(); openMenu(e, "folder", f.id); }}
          />
        </div>
        {open && (
          <div role="group">
            {node.children.map((child) => folderRow(child, childFolders))}
            {node.reports.map((report) => reportRow(report, node.depth + 1, node.reports))}
            {!node.children.length && !node.reports.length && (
              <div
                data-report-empty-folder={f.id}
                onDragOver={(e) => onEmptyFolderDragOver(e, f.id)}
                onDragLeave={(e) => {
                  if (!e.currentTarget.contains(e.relatedTarget as Node | null)) {
                    setDropHint((h) => (h?.id === f.id ? null : h));
                  }
                }}
                onDrop={(e) => onRowDrop(e, f.id, f.parent_id, siblings, true)}
                className={cn(
                  "flex min-h-7 items-center rounded-lg py-1 text-cx-xs text-cx-fg-4 transition-colors",
                  dropHint?.id === f.id && dropHint.pos === "into" && "border border-dashed border-cx-accent/60 bg-cx-accent-soft/70 text-cx-fg-2",
                )}
                style={{ paddingLeft: `${node.depth * 16 + 30}px` }}
              >
                {t("reports.tree.empty")}
              </div>
            )}
          </div>
        )}
      </div>
    );
  };


  const moveOptions = useMemo(() => {
    if (!moveTarget) return [];
    const out: { value: string; label: string; disabled?: boolean }[] = [{ value: "__root__", label: rootLabel }];
    for (const f of folders) {
      const path = pathLabel(folderPath(f.id, byId), rootLabel);
      let disabled = false;
      if (moveTarget.kind === "folder") {
        disabled = isSelfOrDescendant(f.id, moveTarget.id, folders) || !canReparent(moveTarget.id, f.id, folders);
      }
      out.push({ value: f.id, label: path, disabled });
    }
    return out;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [moveTarget, folders, rootLabel]);

  const deleteCounts = deleteFolder ? counts.get(deleteFolder.id) : undefined;
  const rootFolders = useMemo(() => tree.roots.map(({ folder }) => folder), [tree.roots]);

  return (
    <div
      ref={containerRef}
      role="tree"
      data-report-drop-tree="true"
      aria-label={t("reports.title")}
      aria-multiselectable={false}
      className={cn(
        "cx-scroll flex min-h-0 flex-1 flex-col gap-px overflow-y-auto px-2 pb-3 transition-colors",
        (rootRunDragOver || rootNodeDragOver) && !dropHint && "rounded-lg bg-cx-accent-soft/40",
      )}
      onKeyDown={onTreeKeyDown}
      onDragOver={(e) => {
        const hasNode = !flat && hasReportNodeDrag(e.dataTransfer);
        const hasRun = hasRunDrag(e.dataTransfer);
        if (!hasNode && !hasRun) return;
        e.preventDefault();
        e.dataTransfer.dropEffect = hasRun ? "copy" : "move";
        clearAutoExpand();
        setDropHint(null);
        if (hasRun) setRootRunDragOver(true);
      }}
      onDragLeave={(e) => {
        if (!e.currentTarget.contains(e.relatedTarget as Node | null)) {
          setDropHint(null);
          setRootRunDragOver(false);
        }
      }}
      onDrop={onRootDrop}
      onClick={() => setMenu(null)}
      onContextMenu={(e) => {
        if (shouldUseNativeContextMenu(e.target)) return;
        e.preventDefault();
        openMenu(e, "root", "");
      }}
    >
      {visibleReports ? (
        visibleReports.map((report) => reportRow(report, 1, []))
      ) : (
        <>
          {tree.roots.map((node) => folderRow(node, rootFolders))}
          {tree.rootReports.map((report) => reportRow(report, 1, tree.rootReports))}
        </>
      )}
      {!flatRows.length && empty}
      {flatRows.length > 0 && <div className="min-h-8 flex-1" />}

      {menu && (
        <Menu
          open
          anchorRef={menuAnchor}
          anchorPoint={menu.point}
          onOpenChange={(open) => { if (!open) setMenu(null); }}
          ariaLabel={t("reports.more")}
          className="min-w-52"
        >
          {menu.kind === "root" && (
            <>
              <MenuBtn icon="folderPlus" label={t("reports.tree.newFolder")} onClick={() => { setMenu(null); void newSubfolder(null); }} />
              {onNewLinked && (
                <MenuBtn icon="filePlus" label={t("reports.new")} onClick={() => { setMenu(null); onNewLinked(null); }} />
              )}
              <MenuBtn icon="pencilLine" label={t("reports.standalone.new")} onClick={() => { setMenu(null); onNewDocument(null); }} />
              <MenuSeparator />
              {onOrganize && (
                <MenuBtn icon="folderTree" label={t("reports.organize.button")} onClick={() => { setMenu(null); onOrganize(); }} />
              )}
              {onExpandAll && folders.length > 0 && (
                <MenuBtn icon="unfoldVertical" label={t("reports.tree.expandAll")} onClick={() => { setMenu(null); onExpandAll(); }} />
              )}
              {onCollapseAll && folders.length > 0 && (
                <MenuBtn icon="foldVertical" label={t("reports.tree.collapseAll")} onClick={() => { setMenu(null); onCollapseAll(); }} />
              )}
              <MenuBtn icon="refresh" label={t("reports.reload")} onClick={() => { setMenu(null); onRefresh(); }} />
            </>
          )}
          {menuFolder && (
            <>
              {onNewLinked && (
                <MenuBtn icon="filePlus" label={t("reports.tree.newLinkedIn")} onClick={() => { setMenu(null); onNewLinked(menuFolder.id); }} />
              )}
              <MenuBtn icon="pencilLine" label={t("reports.tree.newIn")} onClick={() => { setMenu(null); onNewDocument(menuFolder.id); }} />
              <MenuBtn icon="folderPlus" label={t("reports.tree.newSub")} onClick={() => { setMenu(null); void newSubfolder(menuFolder.id); }} />
              <MenuBtn icon="folder" label={t("reports.tree.newSibling")} onClick={() => { setMenu(null); void newSubfolder(menuFolder.parent_id); }} />
              <MenuSeparator />
              <MenuBtn icon="pencil" label={t("reports.tree.rename")} onClick={() => { setMenu(null); setRenaming(`folder:${menuFolder.id}`); setRenameDraft(menuFolder.name); }} />
              <MenuBtn icon="folderOpen" label={t("reports.tree.moveTo")} onClick={() => { setMenu(null); setMoveTarget({ kind: "folder", id: menuFolder.id }); }} />
              <MenuSeparator />
              <MenuBtn icon="trash" label={t("reports.delete")} danger onClick={() => { setMenu(null); setDeleteFolder(menuFolder); }} />
            </>
          )}
          {menuReport && (
            <>
              <MenuBtn icon="file" label={t("reports.tree.open")} onClick={() => { setMenu(null); onSelect(menuReport); }} />
              {onEditReport && (
                <MenuBtn icon="pencil" label={t("reports.editBody")} onClick={() => { setMenu(null); onEditReport(menuReport); }} />
              )}
              <MenuBtn icon="pencilLine" label={t("reports.tree.rename")} onClick={() => { setMenu(null); setRenaming(`report:${menuReport.id}`); setRenameDraft(menuReport.title); }} />
              <MenuSeparator />
              {menuReport.run_id && menuReport.mode === "ctf" && !generationActive(menuReport) && (
                <MenuBtn icon="refresh" label={t("reports.ai.generate")} onClick={() => { setMenu(null); onGenerate(menuReport); }} />
              )}
              {generationActive(menuReport) && (
                <MenuBtn icon="stop" label={t("reports.ai.cancel")} onClick={() => { setMenu(null); onCancelGeneration(menuReport); }} />
              )}
              {onToggleStatus && (
                <MenuBtn
                  icon="check"
                  label={menuReport.status === "final" ? t("reports.markDraft") : t("reports.markFinal")}
                  onClick={() => { setMenu(null); onToggleStatus(menuReport); }}
                />
              )}
              <MenuSeparator />
              {onCopyMarkdown && (
                <MenuBtn icon="copy" label={t("reports.copy")} onClick={() => { setMenu(null); onCopyMarkdown(menuReport); }} />
              )}
              {onDownloadReport && (
                <MenuBtn icon="download" label={t("reports.download")} onClick={() => { setMenu(null); onDownloadReport(menuReport); }} />
              )}
              {onOpenRun && menuReport.run_id && (
                <MenuBtn icon="arrowUpRight" label={t("reports.openRun")} onClick={() => { setMenu(null); onOpenRun(menuReport.run_id!); }} />
              )}
              <MenuSeparator />
              <MenuBtn icon="folderPlus" label={t("reports.tree.newFolderHere")} onClick={() => { setMenu(null); void newSubfolder(menuReport.folder_id ?? null); }} />
              <MenuBtn icon="folderOpen" label={t("reports.tree.moveTo")} onClick={() => { setMenu(null); setMoveTarget({ kind: "report", id: menuReport.id }); }} />
              <MenuSeparator />
              <MenuBtn icon="trash" label={t("reports.delete")} danger onClick={() => { setMenu(null); onDeleteReport(menuReport); }} />
            </>
          )}
        </Menu>
      )}

      <Dialog
        open={Boolean(moveTarget)}
        onOpenChange={(o) => !o && setMoveTarget(null)}
        title={t("reports.tree.moveTo")}
        icon="folder"
        footer={<Button variant="ghost" onClick={() => setMoveTarget(null)}>{t("dialog.cancel")}</Button>}
      >
        <div className="flex max-h-[50vh] flex-col gap-0.5 overflow-y-auto">
          {moveOptions.map((o) => (
            <button
              key={o.value}
              type="button"
              disabled={o.disabled}
              onClick={() => {
                const target = moveTarget;
                setMoveTarget(null);
                if (!target) return;
                const fid = o.value === "__root__" ? null : o.value;
                if (target.kind === "report") void moveReportTo(target.id, fid);
                else void moveFolderTo(target.id, fid);
              }}
              className={cn(
                "flex h-8 items-center gap-2 rounded-lg px-2 text-left text-cx-sm text-cx-fg-2 hover:bg-cx-hover disabled:cursor-not-allowed disabled:opacity-40",
              )}
            >
              <Icon name="folder" size={13} className="text-cx-fg-4" />
              <span className="truncate">{o.label}</span>
            </button>
          ))}
        </div>
      </Dialog>

      <DeleteFolderDialog
        folder={deleteFolder}
        counts={deleteCounts}
        onClose={() => setDeleteFolder(null)}
        onConfirm={(strategy) => deleteFolder && void doDeleteFolder(deleteFolder, strategy)}
      />
    </div>
  );
}

function fmtDay(secs: number): string {
  if (!secs) return "";
  return new Date(secs * 1000).toLocaleDateString(undefined, { month: "numeric", day: "numeric" });
}

function RunningMark({ startedAt }: { startedAt: number }) {
  const t = useT();
  const elapsed = useElapsedSeconds(startedAt);
  return (
    <span className="inline-flex shrink-0 items-center gap-1 text-cx-accent">
      <Icon name="loader" size={11} className="cx-spin" />
      {t("reports.gen.runningBadge")}
      <span className="tabular-nums">{fmtElapsed(elapsed)}</span>
    </span>
  );
}

function MenuBtn({ icon, label, danger, onClick }: { icon?: IconName; label: string; danger?: boolean; onClick: () => void }) {
  return (
    <button
      type="button"
      role="menuitem"
      onClick={onClick}
      className={cn(
        "flex h-8 w-full items-center gap-2 rounded-lg px-2.5 text-left text-cx-sm outline-none focus-visible:bg-cx-hover",
        danger ? "text-cx-danger hover:bg-cx-danger-soft" : "text-cx-fg-2 hover:bg-cx-hover",
      )}
    >
      {icon ? <Icon name={icon} size={14} className={cn("shrink-0", danger ? "text-cx-danger" : "text-cx-fg-3")} /> : <span className="w-3.5 shrink-0" />}
      <span className="truncate">{label}</span>
    </button>
  );
}

function DeleteFolderDialog({
  folder,
  counts,
  onClose,
  onConfirm,
}: {
  folder: ReportFolder | null;
  counts?: { folders: number; reports: number };
  onClose: () => void;
  onConfirm: (strategy: "move_up" | "delete_all") => void;
}) {
  const t = useT();
  const [strategy, setStrategy] = useState<"move_up" | "delete_all">("move_up");
  useEffect(() => {
    if (folder) setStrategy("move_up");
  }, [folder]);
  if (!folder) return null;
  return (
    <Dialog
      open
      onOpenChange={(o) => !o && onClose()}
      title={t("reports.tree.deleteTitle", { name: folder.name })}
      description={t("reports.tree.deleteDesc", { name: folder.name, folders: counts?.folders ?? 0, reports: counts?.reports ?? 0 })}
      icon="trash"
      tone="danger"
      footer={
        <>
          <Button variant="ghost" onClick={onClose}>{t("dialog.cancel")}</Button>
          <Button variant="danger" onClick={() => onConfirm(strategy)}>{t("reports.delete")}</Button>
        </>
      }
    >
      <div className="flex flex-col gap-2" role="radiogroup" aria-label={t("reports.tree.deleteTitle", { name: folder.name })}>
        <label className="flex cursor-pointer items-start gap-2 rounded-lg border border-cx-border p-2.5 has-[:checked]:border-cx-accent-line has-[:checked]:bg-cx-accent-soft">
          <input type="radio" name="folder-del" checked={strategy === "move_up"} onChange={() => setStrategy("move_up")} className="mt-0.5 accent-[var(--cx-accent)]" />
          <span>
            <span className="block text-cx-sm font-medium text-cx-fg">{t("reports.tree.delMoveUp")}</span>
            <span className="block text-cx-xs text-cx-fg-3">{t("reports.tree.delMoveUpHint")}</span>
          </span>
        </label>
        <label className="flex cursor-pointer items-start gap-2 rounded-lg border border-cx-border p-2.5 has-[:checked]:border-cx-danger has-[:checked]:bg-cx-danger-soft">
          <input type="radio" name="folder-del" checked={strategy === "delete_all"} onChange={() => setStrategy("delete_all")} className="mt-0.5 accent-[var(--cx-danger)]" />
          <span>
            <span className="block text-cx-sm font-medium text-cx-danger">{t("reports.tree.delAll")}</span>
            <span className="block text-cx-xs text-cx-fg-3">{t("reports.tree.delAllHint")}</span>
          </span>
        </label>
      </div>
    </Dialog>
  );
}
