"use client";

import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { CSSProperties, ReactElement, RefObject, DragEvent as ReactDragEvent, PointerEvent as ReactPointerEvent } from "react";
import { RunSummary, RunStatus, Folder, type OperatorTriageLabel } from "@/lib/useRun";
import { useLang, useT, type Lang } from "@/lib/i18n";
import { Button, Icon, Menu, MenuItem, MenuSeparator, shouldUseNativeContextMenu, type IconName } from "@muteki/ui";
import { createDragGhost, type DragGhost } from "@/components/chat/sidebar/dragGhost";
import { setPaneResizing } from "@/lib/paneResize";
import { clampRailWidth, railWidthMax, RAIL_WIDTH_DEFAULT, RAIL_WIDTH_MIN, RAIL_WIDTH_MAX } from "@/lib/railSizing";
import { beginRunDrag, emitRunPointerDrag, endRunDrag, RUN_DRAG_MIME } from "@/lib/reports";


type RailAction =
  | { kind: "pin"; runId: string; pinned: boolean }
  | { kind: "archive"; runId: string; archived: boolean }
  | { kind: "rename"; runId: string; name: string }
  | { kind: "delete"; runId: string }
  | { kind: "move"; runId: string; folderId: string | null }
  | { kind: "reorder"; runIds: string[]; pinned: boolean }
  | { kind: "newFolder" }
  | { kind: "renameFolder"; folderId: string; name: string }
  | { kind: "deleteFolder"; folderId: string }
  | { kind: "operatorLabel"; runId: string; label: OperatorTriageLabel | null };

function operatorLabelText(label: OperatorTriageLabel | null | undefined, lang: Lang): string {
  switch (label) {
    case "pending_followup":
      return lang === "zh" ? "待跟进" : "Follow-up";
    case "processed":
      return lang === "zh" ? "已处理" : "Processed";
    case "ignored":
      return lang === "zh" ? "已忽略" : "Ignored";
    default:
      return "";
  }
}

export function ThreadRail({
  collapsed,
  width,
  runs,
  folders,
  activeRunId,
  draftActive,
  onNew,
  onSelect,
  onAction,
  onResize,
  onOpenSettings,
  onOpenReports,
  reportsActive = false,
  onClose,
  workspaceMode = "ctf",
}: {
  collapsed: boolean;
  width: number;
  runs: RunSummary[];
  folders: Folder[];
  activeRunId: string;
  draftActive: boolean;
  onNew: () => void;
  onSelect: (runId: string) => void;
  onAction: (a: RailAction) => void | Promise<Folder | null | void>;
  onResize: (width: number) => void;
  onOpenSettings: () => void;
  onOpenReports: () => void;
  reportsActive?: boolean;
  onClose: () => void;
  workspaceMode?: "ctf" | "pentest";
}) {
  const baseT = useT();
  const { lang } = useLang();
  const t = (key: string, values?: Record<string, string | number>): string => {
    if (workspaceMode === "pentest") {
      const labels: Record<string, [string, string]> = {
        "a11y.nav": ["渗透测试列表导航", "Pentest run navigation"],
        "rail.navTitle": ["渗透测试", "Pentest runs"],
        "rail.newSolve": ["新测试", "New test"],
        "rail.newSolveItem": ["新测试", "New test"],
        "rail.search": ["搜索测试…", "Search tests…"],
        "rail.empty": ["暂无测试记录", "No tests yet"],
        "rail.emptyHint": ["在下方输入目标并开始测试。", "Describe a target to start testing."],
        "rail.status.solved": ["已证实", "Proven"],
        "rail.reports": ["测试报告", "Reports"],
      };
      const label = labels[key];
      if (label) return label[lang === "zh" ? 0 : 1];
    }
    return baseT(key, values);
  };
  const [showArchived, setShowArchived] = useState(false);
  const [query, setQuery] = useState("");
  const [menuFor, setMenuFor] = useState<string | null>(null);
  const [contextPosition, setContextPosition] = useState<{ runId: string; x: number; y: number } | null>(null);
  const [renaming, setRenaming] = useState<string | null>(null);
  const [dragRun, setDragRun] = useState<string | null>(null);
  const [dropZone, setDropZone] = useState<string | null>(null);
  const [rowDrop, setRowDrop] = useState<{ runId: string; side: "before" | "after" } | null>(null);
  const [collapsedFolders, setCollapsedFolders] = useState<Set<string>>(new Set());
  const [renamingFolder, setRenamingFolder] = useState<string | null>(null);
  const [resizing, setResizing] = useState(false);
  const navRef = useRef<HTMLElement | null>(null);
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const resizeCleanup = useRef<(() => void) | null>(null);
  const pointerDragCleanup = useRef<(() => void) | null>(null);
  const didDragRef = useRef(false);
  const scrollTop = useRef(0);
  useEffect(() => {
    const el = scrollRef.current;
    if (el) el.scrollTop = scrollTop.current;
  });
  useEffect(() => () => {
    resizeCleanup.current?.();
    pointerDragCleanup.current?.();
  }, []);

  useLayoutEffect(() => {
    if (resizing) return;
    const nav = navRef.current;
    if (!nav) return;
    nav.style.width = "";
    const body = nav.querySelector<HTMLElement>(":scope > .rail-body");
    if (body) {
      body.style.removeProperty("width");
      body.style.removeProperty("opacity");
      body.style.removeProperty("transform");
    }
  }, [resizing, width, collapsed]);

  const resizeTo = (next: number) => {
    const viewport = typeof window !== "undefined" ? window.innerWidth : undefined;
    onResize(clampRailWidth(next, viewport));
  };

  const startResize = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (collapsed) return;
    e.preventDefault();
    e.stopPropagation();
    e.currentTarget.setPointerCapture?.(e.pointerId);
    resizeCleanup.current?.();
    setResizing(true);
    setPaneResizing(true);
    const startX = e.clientX;
    const startWidth = width;
    const maxW = railWidthMax(typeof window !== "undefined" ? window.innerWidth : undefined);
    let pendingRaw: number | null = null;
    let lastRaw = startWidth;
    let frame = 0;

    const flush = () => {
      frame = 0;
      if (pendingRaw == null) return;
      const raw = pendingRaw;
      pendingRaw = null;
      lastRaw = raw;
      const nav = navRef.current;
      const body = nav?.querySelector<HTMLElement>(":scope > .rail-body");
      const shown = Math.max(0, Math.min(maxW, raw));
      if (nav) nav.style.width = `${shown}px`;
      if (body) {
        if (shown < RAIL_WIDTH_MIN) {
          const p = Math.max(0, Math.min(1, (shown - 88) / (RAIL_WIDTH_MIN - 88)));
          body.style.width = `${Math.max(RAIL_WIDTH_MIN, startWidth)}px`;
          body.style.opacity = p.toFixed(3);
          body.style.transform = `translateX(${((1 - p) * -8).toFixed(2)}px)`;
        } else {
          body.style.width = `${shown}px`;
          body.style.opacity = "";
          body.style.transform = "";
        }
      }
    };

    const onMove = (ev: PointerEvent) => {
      ev.preventDefault();
      pendingRaw = Math.round(startWidth + ev.clientX - startX);
      if (!frame) frame = requestAnimationFrame(flush);
    };
    const stop = () => {
      resizeCleanup.current = null;
      cancelAnimationFrame(frame);
      flush();
      setResizing(false);
      setPaneResizing(false);
      if (lastRaw < 140) onClose();
      else resizeTo(lastRaw);
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", stop);
      window.removeEventListener("pointercancel", stop);
    };

    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", stop);
    window.addEventListener("pointercancel", stop);
    resizeCleanup.current = stop;
  };

  const onResizeKey = (e: React.KeyboardEvent<HTMLDivElement>) => {
    if (collapsed) return;
    if (e.key === "ArrowLeft") {
      e.preventDefault();
      resizeTo(width - (e.shiftKey ? 32 : 12));
    } else if (e.key === "ArrowRight") {
      e.preventDefault();
      resizeTo(width + (e.shiftKey ? 32 : 12));
    } else if (e.key === "Home") {
      e.preventDefault();
      resizeTo(RAIL_WIDTH_MIN);
    } else if (e.key === "End") {
      e.preventDefault();
      resizeTo(railWidthMax(typeof window !== "undefined" ? window.innerWidth : undefined));
    } else if (e.key === "Enter") {
      e.preventDefault();
      resizeTo(RAIL_WIDTH_DEFAULT);
    }
  };

  const hasTransient = menuFor !== null || renaming !== null || renamingFolder !== null;
  useEffect(() => {
    if (!hasTransient) return;
    const onEsc = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      setMenuFor(null);
      setRenaming(null);
      setRenamingFolder(null);
    };
    window.addEventListener("keydown", onEsc);
    return () => window.removeEventListener("keydown", onEsc);
  }, [hasTransient]);

  const q = query.trim().toLowerCase();
  const matchesQuery = (r: RunSummary): boolean => {
    if (!q) return true;
    const hay = [
      r.name,
      r.category,
      r.kind === "local_pi" ? "pi local_pi 本地pi" : "",
      r.operator_label,
      operatorLabelText(r.operator_label, lang),
      r.status,
      t(`rail.status.${r.status}`),
      relTime(r.updated_at, lang),
      r.run_id,
    ].filter(Boolean).join(" ").toLowerCase();
    return hay.includes(q);
  };
  const matched = q ? runs.filter(matchesQuery) : runs;

  const { pinned, archived, ungrouped, folderRuns } = useMemo(() => {
    const pinned: RunSummary[] = [];
    const archived: RunSummary[] = [];
    const ungrouped: RunSummary[] = [];
    const folderRuns = new Map<string, RunSummary[]>(folders.map((folder) => [folder.id, []]));
    for (const run of matched) {
      if (run.archived) {
        archived.push(run);
      } else if (run.pinned) {
        pinned.push(run);
      } else {
        const items = run.folder_id ? folderRuns.get(run.folder_id) : undefined;
        (items ?? ungrouped).push(run);
      }
    }
    const maxExplicitOrder = matched.reduce(
      (max, run) => (typeof run.order === "number" && Number.isFinite(run.order) ? Math.max(max, run.order) : max),
      0,
    );
    const maxOrderedSeq = matched.reduce(
      (max, run) => (typeof run.order === "number" && Number.isFinite(run.order) ? Math.max(max, run.created_seq ?? 0) : max),
      0,
    );
    const orderRanks = new Map<string, number>();
    for (const run of matched) {
      if (typeof run.order === "number" && Number.isFinite(run.order)) {
        orderRanks.set(run.run_id, run.order);
      } else if ((run.created_seq ?? 0) > maxOrderedSeq) {
        orderRanks.set(run.run_id, maxExplicitOrder + 1 + ((run.created_seq ?? 0) - maxOrderedSeq));
      } else {
        orderRanks.set(run.run_id, 0);
      }
    }
    const byCreationOrder = (a: RunSummary, b: RunSummary) =>
      (orderRanks.get(b.run_id) ?? 0) - (orderRanks.get(a.run_id) ?? 0)
      || (b.created_seq ?? 0) - (a.created_seq ?? 0)
      || (b.ts ?? b.updated ?? 0) - (a.ts ?? a.updated ?? 0);
    pinned.sort((a, b) => (b.pinned_at ?? 0) - (a.pinned_at ?? 0));
    archived.sort(byCreationOrder);
    ungrouped.sort(byCreationOrder);
    for (const items of folderRuns.values()) items.sort(byCreationOrder);
    return { pinned, archived, ungrouped, folderRuns };
  }, [matched, folders]);
  const searching = q.length > 0;
  const noMatches = searching && matched.length === 0;

  const dropInto = (fid: string) => {
    if (dragRun) onAction({ kind: "move", runId: dragRun, folderId: fid || null });
    endRunDrag();
    setDragRun(null);
    setDropZone(null);
    setRowDrop(null);
  };
  const dzProps = (zone: string) => ({
    onDragOver: (e: ReactDragEvent) => {
      if (!dragRun) return;
      e.preventDefault();
      if (dropZone !== zone) setDropZone(zone);
      setRowDrop(null);
    },
    onDragLeave: () => setDropZone((z) => (z === zone ? null : z)),
    onDrop: (e: ReactDragEvent) => { if (dragRun) { e.preventDefault(); dropInto(zone); } },
  });
  const sectionOf = (r: RunSummary) => {
    if (r.archived) return "archived";
    if (r.pinned) return "pinned";
    return r.folder_id && folderRuns.has(r.folder_id) ? `folder:${r.folder_id}` : "recent";
  };
  const canDropOnRow = (target: RunSummary) => {
    const source = runs.find((r) => r.run_id === dragRun);
    return !!source && source.run_id !== target.run_id && sectionOf(source) === sectionOf(target);
  };
  const sectionItems = (target: RunSummary) => {
    if (target.archived) return archived;
    if (target.pinned) return pinned;
    const items = target.folder_id ? folderRuns.get(target.folder_id) : undefined;
    return items ?? ungrouped;
  };
  const sideForRowDrop = (
    target: RunSummary, e: ReactDragEvent<HTMLDivElement>,
  ): "before" | "after" => {
    const rect = e.currentTarget.getBoundingClientRect();
    const offset = e.clientY - rect.top;
    if (offset < rect.height * 0.3) return "before";
    if (offset > rect.height * 0.7) return "after";
    const items = sectionItems(target);
    const sourceIndex = items.findIndex((r) => r.run_id === dragRun);
    const targetIndex = items.findIndex((r) => r.run_id === target.run_id);
    return sourceIndex < targetIndex ? "after" : "before";
  };
  const reorderOnRowById = (
    sourceId: string,
    target: RunSummary,
    side: "before" | "after",
  ) => {
    const source = runs.find((r) => r.run_id === sourceId);
    if (!source || source.run_id === target.run_id || sectionOf(source) !== sectionOf(target)) return;
    const section = sectionOf(target);
    const items = sectionItems(target);
    const before = items.map((r) => r.run_id);
    const after = before.filter((id) => id !== sourceId);
    const at = after.indexOf(target.run_id) + (side === "after" ? 1 : 0);
    after.splice(at, 0, sourceId);
    if (after.some((id, index) => id !== before[index])) {
      onAction({ kind: "reorder", runIds: after, pinned: section === "pinned" });
    }
  };
  const reorderOnRow = (target: RunSummary, side: "before" | "after") => {
    if (!dragRun) return;
    reorderOnRowById(dragRun, target, side);
  };
  const latestDragStateRef = useRef({
    runs,
    pinned,
    archived,
    ungrouped,
    folderRuns,
    onAction,
  });
  latestDragStateRef.current = {
    runs,
    pinned,
    archived,
    ungrouped,
    folderRuns,
    onAction,
  };
  const startRowPointerDrag = (sourceRun: RunSummary, event: ReactPointerEvent<HTMLDivElement>) => {
    if (event.button !== 0 || event.pointerType === "touch") return;
    pointerDragCleanup.current?.();
    const runId = sourceRun.run_id;
    const startX = event.clientX;
    const startY = event.clientY;
    const sourceEl = event.currentTarget;
    let active = false;
    let ghost: DragGhost | null = null;
    let didDragTimer: number | null = null;

    const sectionOfLatest = (r: RunSummary) => {
      if (r.archived) return "archived";
      if (r.pinned) return "pinned";
      return r.folder_id && latestDragStateRef.current.folderRuns.has(r.folder_id) ? `folder:${r.folder_id}` : "recent";
    };
    const itemsForLatest = (target: RunSummary) => {
      const s = latestDragStateRef.current;
      if (target.archived) return s.archived;
      if (target.pinned) return s.pinned;
      const items = target.folder_id ? s.folderRuns.get(target.folder_id) : undefined;
      return items ?? s.ungrouped;
    };
    const sideAt = (target: RunSummary, el: HTMLElement, clientY: number): "before" | "after" => {
      const rect = el.getBoundingClientRect();
      const offset = clientY - rect.top;
      if (offset < rect.height * 0.3) return "before";
      if (offset > rect.height * 0.7) return "after";
      const items = itemsForLatest(target);
      const sourceIndex = items.findIndex((r) => r.run_id === runId);
      const targetIndex = items.findIndex((r) => r.run_id === target.run_id);
      return sourceIndex < targetIndex ? "after" : "before";
    };

    const cleanup = (cancelled: boolean) => {
      pointerDragCleanup.current = null;
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
      window.removeEventListener("pointercancel", onCancel);
      window.removeEventListener("keydown", onEsc);
      if (active) {
        delete document.documentElement.dataset.cxSidebarDragging;
        endRunDrag(runId);
        setDragRun(null);
        setDropZone(null);
        setRowDrop(null);
        if (cancelled) {
          ghost?.cancel();
          emitRunPointerDrag({ phase: "cancel", runId, x: 0, y: 0 });
        }
        if (didDragTimer !== null) window.clearTimeout(didDragTimer);
        didDragTimer = window.setTimeout(() => {
          didDragRef.current = false;
        }, 0);
      }
    };

    const onMove = (ev: PointerEvent) => {
      if (!active) {
        if (Math.hypot(ev.clientX - startX, ev.clientY - startY) < 5) return;
        active = true;
        didDragRef.current = true;
        setDragRun(runId);
        setRowDrop(null);
        setDropZone(null);
        beginRunDrag(runId);
        const reduced = document.documentElement.dataset.motion === "reduce"
          || Boolean(window.matchMedia?.("(prefers-reduced-motion: reduce)").matches);
        ghost = createDragGhost(sourceEl, startX, startY, reduced);
        document.documentElement.dataset.cxSidebarDragging = "";
        emitRunPointerDrag({ phase: "start", runId, x: ev.clientX, y: ev.clientY });
      }
      ev.preventDefault();
      ghost?.move(ev.clientX, ev.clientY);
      const hit = document.elementFromPoint(ev.clientX, ev.clientY);
      if (hit && navRef.current?.contains(hit)) {
        const rowEl = hit.closest<HTMLElement>("[data-rail-row]");
        const targetId = rowEl?.dataset.railRow;
        if (rowEl && targetId) {
          const latest = latestDragStateRef.current;
          const src = latest.runs.find((r) => r.run_id === runId);
          const dst = latest.runs.find((r) => r.run_id === targetId);
          if (src && dst && src.run_id !== dst.run_id && sectionOfLatest(src) === sectionOfLatest(dst)) {
            const side = sideAt(dst, rowEl, ev.clientY);
            setDropZone(null);
            setRowDrop((cur) => (cur?.runId === dst.run_id && cur.side === side ? cur : { runId: dst.run_id, side }));
          } else {
            setRowDrop(null);
            const folderEl = hit.closest<HTMLElement>("[data-rail-folder]");
            setDropZone(folderEl?.dataset.railFolder ?? (hit.closest("[data-rail-recent]") ? "" : null));
          }
        } else {
          setRowDrop(null);
          const folderEl = hit.closest<HTMLElement>("[data-rail-folder]");
          if (folderEl?.dataset.railFolder) setDropZone(folderEl.dataset.railFolder);
          else if (hit.closest("[data-rail-recent]")) setDropZone("");
          else setDropZone(null);
        }
      } else {
        setRowDrop(null);
        setDropZone(null);
      }
      emitRunPointerDrag({ phase: "move", runId, x: ev.clientX, y: ev.clientY });
    };

    const onUp = (ev: PointerEvent) => {
      const wasActive = active;
      let moved = false;
      if (wasActive) {
        ev.preventDefault();
        ghost?.move(ev.clientX, ev.clientY);
        const hit = document.elementFromPoint(ev.clientX, ev.clientY);
        if (hit && navRef.current?.contains(hit)) {
          const latest = latestDragStateRef.current;
          const rowEl = hit.closest<HTMLElement>("[data-rail-row]");
          const targetId = rowEl?.dataset.railRow;
          const src = latest.runs.find((r) => r.run_id === runId);
          const dst = targetId ? latest.runs.find((r) => r.run_id === targetId) : undefined;
          if (rowEl && src && dst && src.run_id !== dst.run_id && sectionOfLatest(src) === sectionOfLatest(dst)) {
            const side = sideAt(dst, rowEl, ev.clientY);
            const section = sectionOfLatest(dst);
            const items = itemsForLatest(dst);
            const before = items.map((r) => r.run_id);
            const after = before.filter((id) => id !== runId);
            const at = after.indexOf(dst.run_id) + (side === "after" ? 1 : 0);
            after.splice(at, 0, runId);
            if (after.some((id, index) => id !== before[index])) {
              latest.onAction({ kind: "reorder", runIds: after, pinned: section === "pinned" });
              moved = true;
            }
          } else {
            const folderEl = hit.closest<HTMLElement>("[data-rail-folder]");
            const folderId = folderEl?.dataset.railFolder;
            if (folderId) {
              latest.onAction({ kind: "move", runId, folderId });
              moved = true;
            } else if (hit.closest("[data-rail-recent]")) {
              latest.onAction({ kind: "move", runId, folderId: null });
              moved = true;
            }
          }
          emitRunPointerDrag({ phase: "cancel", runId, x: 0, y: 0 });
        } else {
          moved = emitRunPointerDrag({ phase: "drop", runId, x: ev.clientX, y: ev.clientY });
        }
        if (moved) ghost?.drop();
        else ghost?.cancel();
      }
      cleanup(false);
    };

    const onCancel = () => cleanup(true);
    const onEsc = (ev: KeyboardEvent) => {
      if (ev.key === "Escape") cleanup(true);
    };

    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
    window.addEventListener("pointercancel", onCancel);
    window.addEventListener("keydown", onEsc);
    pointerDragCleanup.current = () => {
      if (didDragTimer !== null) window.clearTimeout(didDragTimer);
      cleanup(true);
    };
  };
  const toggleFolder = (fid: string) =>
    setCollapsedFolders((prev) => {
      const next = new Set(prev);
      if (next.has(fid)) next.delete(fid);
      else next.add(fid);
      return next;
    });

  const rowProps = (r: RunSummary) => ({
    run: r,
    active: r.run_id === activeRunId,
    menuOpen: menuFor === r.run_id,
    renaming: renaming === r.run_id,
    onSelect: () => onSelect(r.run_id),
    onToggleMenu: () => {
      if (menuFor === r.run_id) setMenuFor(null);
      else { setContextPosition(null); setMenuFor(r.run_id); }
    },
    onOpenContextMenu: (x: number, y: number) => {
      setContextPosition({
        runId: r.run_id,
        x,
        y,
      });
      setMenuFor(r.run_id);
    },
    contextPosition: contextPosition?.runId === r.run_id ? contextPosition : null,
    onCloseMenu: () => setMenuFor(null),
    onStartRename: () => { setMenuFor(null); setRenaming(r.run_id); },
    onCommitRename: (name: string) => {
      setRenaming(null);
      const trimmed = name.trim();
      if (trimmed && trimmed !== r.name) onAction({ kind: "rename", runId: r.run_id, name: trimmed });
    },
    onCancelRename: () => setRenaming(null),
    onAction,
    dragging: dragRun === r.run_id,
    rowDrop: rowDrop?.runId === r.run_id ? rowDrop.side : null,
    didDragRef,
    onRowPointerDown: (e: ReactPointerEvent<HTMLDivElement>) => startRowPointerDrag(r, e),
    onDragStart: () => { setDragRun(r.run_id); setRowDrop(null); },
    onDragEnd: () => { setDragRun(null); setDropZone(null); setRowDrop(null); },
    onDragOverRow: (e: ReactDragEvent<HTMLDivElement>) => {
      if (!dragRun) return;
      e.stopPropagation();
      if (!canDropOnRow(r)) { setRowDrop(null); return; }
      e.preventDefault();
      e.dataTransfer.dropEffect = "move";
      const side = sideForRowDrop(r, e);
      setDropZone(null);
      setRowDrop((current) => current?.runId === r.run_id && current.side === side
        ? current : { runId: r.run_id, side });
    },
    onDragLeaveRow: (e: ReactDragEvent<HTMLDivElement>) => {
      if (!e.currentTarget.contains(e.relatedTarget as Node | null)) {
        setRowDrop((current) => current?.runId === r.run_id ? null : current);
      }
    },
    onDropOnRow: (e: ReactDragEvent<HTMLDivElement>) => {
      if (!dragRun) return;
      e.preventDefault();
      e.stopPropagation();
      if (canDropOnRow(r)) {
        reorderOnRow(r, sideForRowDrop(r, e));
      }
      endRunDrag();
      setRowDrop(null);
      setDropZone(null);
    },
  });

  return (
    <nav
      ref={navRef}
      className={`rail ${collapsed ? "collapsed" : ""} ${resizing ? "resizing" : ""}`}
      style={{ "--rail-width": `${width}px` } as CSSProperties}
      aria-label={t("a11y.nav")}
    >
      <div className="rail-body">
        <div className="rail-top">
          <span className="rail-title">{t("rail.navTitle")}</span>
          <Button className="rail-close" iconOnly variant="ghost" onClick={onClose} aria-label={t("rail.close")}><Icon name="x" size={15} /></Button>
        </div>
        <Button className="newsolve" onClick={onNew}>
          <span className="newsolve-icon" aria-hidden="true"><Icon name="plus" size={15} /></span>
          <span>{t("rail.newSolve")}</span>
        </Button>
        <Button className={`newsolve railreports ${reportsActive ? "active" : ""}`} onClick={onOpenReports} aria-current={reportsActive ? "page" : undefined}>
          <span className="newsolve-icon" aria-hidden="true"><Icon name="file" size={15} /></span>
          <span>{t("rail.reports")}</span>
        </Button>

        <div className="rail-search">
          <span className="rail-search-ico" aria-hidden="true"><Icon name="search" size={14} /></span>
          <input
            className="rail-search-input"
            type="search"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => { if (e.key === "Escape" && query) { e.stopPropagation(); setQuery(""); } }}
            placeholder={t("rail.search")}
            aria-label={t("rail.search")}
          />
          {query && (
            <Button
              className="rail-search-clear"
              onClick={() => setQuery("")}
              data-tooltip={t("rail.searchClear")}
              aria-label={t("rail.searchClear")}
            ><Icon name="x" size={13} /></Button>
          )}
        </div>

        <div
          className="rail-scroll"
          ref={scrollRef}
          onScroll={(e) => { scrollTop.current = (e.target as HTMLDivElement).scrollTop; }}
        >
        {draftActive && (
          <>
            <div className="rail-sec">{t("rail.active")}</div>
            <div className="thread">
              <Button className="thread-item active is-draft" onClick={() => onSelect(activeRunId)}>
                <StatusIcon status="draft" t={t} mode={workspaceMode} />
                <span className="nm">{t("rail.newSolveItem")}</span>
              </Button>
            </div>
          </>
        )}

        {pinned.length > 0 && (
          <>
            <div className="rail-sec">{t("rail.pinned")}</div>
            <div className="thread">
              {pinned.map((r) => <RailRow key={r.run_id} pinned {...rowProps(r)} t={t} lang={lang} />)}
            </div>
          </>
        )}

        {workspaceMode === "ctf" && folders.map((f) => {
          const items = folderRuns.get(f.id)!;
          if (searching && items.length === 0) return null;
          const open = searching || !collapsedFolders.has(f.id);
          return (
            <div
              key={f.id}
              data-rail-folder={f.id}
              className={`rail-folder ${dropZone === f.id ? "drop" : ""} ${menuFor === `folder:${f.id}` ? "menu-open" : ""}`}
              {...dzProps(f.id)}
            >
              <div className="rail-folder-head" onContextMenu={(event) => {
                if (renamingFolder === f.id || event.defaultPrevented || shouldUseNativeContextMenu(event.target)) return;
                event.preventDefault();
                event.stopPropagation();
                setContextPosition({ runId: `folder:${f.id}`, x: event.clientX, y: event.clientY });
                setMenuFor(`folder:${f.id}`);
              }}>
                <Button className="rail-folder-toggle" onClick={() => toggleFolder(f.id)} aria-keyshortcuts="Shift+F10" onKeyDown={(event) => {
                  if (renamingFolder === f.id || event.target !== event.currentTarget) return;
                  if (event.key !== "ContextMenu" && !(event.shiftKey && event.key === "F10")) return;
                  event.preventDefault();
                  const rect = event.currentTarget.getBoundingClientRect();
                  setContextPosition({ runId: `folder:${f.id}`, x: rect.left, y: rect.bottom });
                  setMenuFor(`folder:${f.id}`);
                }}>
                  <span className="rail-folder-caret">{open ? "▾" : "▸"}</span>
                  {renamingFolder === f.id ? (
                    <RenameInput
                      initial={f.name}
                      onCommit={(name) => { setRenamingFolder(null); const v = name.trim(); if (v && v !== f.name) onAction({ kind: "renameFolder", folderId: f.id, name: v }); }}
                      onCancel={() => setRenamingFolder(null)}
                    />
                  ) : (
                    <span className="rail-folder-name"><Icon name="folder" size={13} /> {f.name}</span>
                  )}
                  <span className="rail-folder-count">{items.length}</span>
                </Button>
                <div className={`row-menu ${menuFor === `folder:${f.id}` ? "menu-open" : ""}`}>
                  <FolderMenu
                      trigger={<Button className="dots" data-tooltip={t("rail.menu.moreActions")} aria-label={t("rail.menu.moreActions")} onClick={(event) => event.stopPropagation()}><Icon name="more" size={15} /></Button>}
                      open={menuFor === `folder:${f.id}`}
                      contextPosition={contextPosition?.runId === `folder:${f.id}` ? contextPosition : null}
                      onOpen={() => { setContextPosition(null); setMenuFor(`folder:${f.id}`); }}
                      t={t}
                      onClose={() => setMenuFor(null)}
                      onRename={() => { setMenuFor(null); setRenamingFolder(f.id); }}
                      onDelete={() => { setMenuFor(null); onAction({ kind: "deleteFolder", folderId: f.id }); }}
                    />
                </div>
              </div>
              {open && (
                <div className="thread">
                  {items.length === 0
                    ? <div className="rail-folder-empty">{t("rail.folderEmpty")}</div>
                    : items.map((r) => <RailRow key={r.run_id} {...rowProps(r)} t={t} lang={lang} />)}
                </div>
              )}
            </div>
          );
        })}

        <div data-rail-recent="true" className={`rail-sec rail-recent-head ${dropZone === "" ? "drop" : ""}`} {...dzProps("")}>
          <span>{t("rail.recent")}</span>
          {workspaceMode === "ctf" && <Button className="rail-newfolder" data-tooltip={t("rail.newFolderTitle")} aria-label={t("rail.newFolderTitle")}
            onClick={async () => {
              const folder = await onAction({ kind: "newFolder" });
              if (folder && typeof folder === "object" && "id" in folder) setRenamingFolder(folder.id);
            }}><Icon name="folderPlus" size={15} /></Button>}
        </div>
        <div data-rail-recent="true" className={`thread ${dropZone === "" ? "drop" : ""}`} {...dzProps("")}>
          {ungrouped.length === 0 && !draftActive && !searching && (
            <div className="rail-empty">
              {t("rail.empty")}
              <span className="rail-empty-hint">{t("rail.emptyHint")}</span>
            </div>
          )}
          {ungrouped.map((r) => <RailRow key={r.run_id} {...rowProps(r)} t={t} lang={lang} />)}
        </div>

        {noMatches && (
          <div className="rail-noresult">
            <span className="rail-noresult-ico" aria-hidden="true"><Icon name="search" size={18} /></span>
            <span className="rail-noresult-title">{t("rail.searchEmpty")}</span>
            <span className="rail-noresult-hint">{t("rail.searchEmptyHint")}</span>
          </div>
        )}

        {archived.length > 0 && (
          searching ? (
            <>
              <div className="rail-sec">{t("rail.archived")}</div>
              <div className="thread">
                {archived.map((r) => <RailRow key={r.run_id} archived {...rowProps(r)} t={t} lang={lang} />)}
              </div>
            </>
          ) : (
            <>
              <Button
                className="rail-archtoggle"
                aria-label={t("rail.archivedHint")}
                onClick={() => setShowArchived((v) => !v)}
              >
                {showArchived ? t("rail.hideArchived") : `${t("rail.showArchived")} (${archived.length})`}
              </Button>
              {showArchived && (
                <>
                  <div className="rail-archhint">{t("rail.archivedHint")}</div>
                  <div className="thread">
                    {archived.map((r) => <RailRow key={r.run_id} archived {...rowProps(r)} t={t} lang={lang} />)}
                  </div>
                </>
              )}
            </>
          )
        )}
        </div>

        <div className="rail-foot">
          <Button className="rail-workers-btn" onClick={onOpenSettings} aria-label={t("settings.open")} data-tooltip={t("rail.workersHint")}>
            <Icon name="cpu" size={15} />
            <span className="rail-workers-label">{t("settings.open")}</span>
            <Icon name="chevronRight" size={13} className="rail-workers-chev" />
          </Button>
        </div>
      </div>
      {!collapsed && (
        <div
          className="rail-resizer"
          role="separator"
          tabIndex={0}
          aria-label={t("rail.resize")}
          data-tooltip={t("rail.resize")}
          aria-orientation="vertical"
          aria-valuemin={RAIL_WIDTH_MIN}
          aria-valuemax={RAIL_WIDTH_MAX}
          aria-valuenow={width}
          onPointerDown={startResize}
          onKeyDown={onResizeKey}
          onDoubleClick={() => resizeTo(RAIL_WIDTH_DEFAULT)}
        />
      )}
    </nav>
  );
}

function RailRow({
  run,
  active,
  pinned,
  archived,
  menuOpen,
  renaming,
  onSelect,
  onToggleMenu,
  onOpenContextMenu,
  contextPosition,
  onCloseMenu,
  onStartRename,
  onCommitRename,
  onCancelRename,
  onAction,
  dragging,
  rowDrop,
  didDragRef,
  onRowPointerDown,
  onDragStart,
  onDragEnd,
  onDragOverRow,
  onDragLeaveRow,
  onDropOnRow,
  t,
  lang,
}: {
  run: RunSummary;
  active: boolean;
  pinned?: boolean;
  archived?: boolean;
  menuOpen: boolean;
  renaming: boolean;
  onSelect: () => void;
  onToggleMenu: () => void;
  onOpenContextMenu: (x: number, y: number) => void;
  contextPosition: { x: number; y: number } | null;
  onCloseMenu: () => void;
  onStartRename: () => void;
  onCommitRename: (name: string) => void;
  onCancelRename: () => void;
  onAction: (a: RailAction) => void;
  dragging: boolean;
  rowDrop: "before" | "after" | null;
  didDragRef: RefObject<boolean>;
  onRowPointerDown: (e: ReactPointerEvent<HTMLDivElement>) => void;
  onDragStart: () => void;
  onDragEnd: () => void;
  onDragOverRow: (e: ReactDragEvent<HTMLDivElement>) => void;
  onDragLeaveRow: (e: ReactDragEvent<HTMLDivElement>) => void;
  onDropOnRow: (e: ReactDragEvent<HTMLDivElement>) => void;
  t: (k: string, v?: Record<string, string | number>) => string;
  lang: Lang;
}) {
  const name = run.name || (run.mode === "pentest" ? run.run_id : t("rail.newSolveItem"));
  const when = relTime(run.updated_at, lang);
  const cls = [
    "thread-item",
    active ? "active" : "",
    pinned ? "is-pinned" : "",
    archived ? "is-archived" : "",
    dragging ? "dragging" : "",
    rowDrop ? `drop-${rowDrop}` : "",
    menuOpen ? "menu-open" : "",
  ].filter(Boolean).join(" ");
  const suppressRowDragRef = useRef(false);
  const clearMenuPointerSoonRef = useRef<() => void>(() => {});
  const rowRef = useRef<HTMLDivElement | null>(null);
  const menuTriggerRef = useRef<HTMLButtonElement | null>(null);
  const shouldSuppressRowDrag = (target: EventTarget | null) => {
    if (suppressRowDragRef.current) return true;
    return target instanceof HTMLElement && !!target.closest(".row-menu, .menu, button, input, .rename-input");
  };
  useEffect(() => {
    let timer: number | null = null;
    const clear = () => {
      if (timer !== null) window.clearTimeout(timer);
      timer = window.setTimeout(() => {
        timer = null;
        suppressRowDragRef.current = false;
      }, 0);
    };
    clearMenuPointerSoonRef.current = clear;
    return () => {
      if (timer !== null) window.clearTimeout(timer);
      window.removeEventListener("pointerup", clear);
      window.removeEventListener("pointercancel", clear);
    };
  }, []);
  const clearMenuPointerSoon = () => {
    clearMenuPointerSoonRef.current();
  };
  const markMenuPointer = () => {
    suppressRowDragRef.current = true;
    window.addEventListener("pointerup", clearMenuPointerSoonRef.current, { once: true });
    window.addEventListener("pointercancel", clearMenuPointerSoonRef.current, { once: true });
  };

  return (
    <div
      ref={rowRef}
      className={cls}
      role="button"
      aria-keyshortcuts="Shift+F10"
      tabIndex={0}
      data-rail-row={run.run_id}
      data-rail-draggable={!renaming && !menuOpen ? "true" : undefined}
      draggable={false}
      onPointerDown={(e) => {
        if (renaming || menuOpen || shouldSuppressRowDrag(e.target)) return;
        onRowPointerDown(e);
      }}
      onDragStart={(e) => {
        if (shouldSuppressRowDrag(e.target)) {
          e.preventDefault();
          e.stopPropagation();
          return;
        }
        e.dataTransfer.effectAllowed = "copyMove";
        e.dataTransfer.setData("text/plain", run.run_id);
        e.dataTransfer.setData(RUN_DRAG_MIME, run.run_id);
        beginRunDrag(run.run_id);
        onDragStart();
      }}
      onDragEnd={() => { suppressRowDragRef.current = false; endRunDrag(run.run_id); onDragEnd(); }}
      onDragOver={onDragOverRow}
      onDragLeave={onDragLeaveRow}
      onDrop={onDropOnRow}
      data-tooltip={run.mode === "pentest" ? name : `${name} · ${run.category || "—"}`}
      onClick={() => {
        if (renaming || didDragRef.current) return;
        onSelect();
      }}
      onContextMenu={(e) => {
        if (renaming || e.defaultPrevented || shouldUseNativeContextMenu(e.target)) return;
        e.preventDefault();
        e.stopPropagation();
        onOpenContextMenu(e.clientX, e.clientY);
      }}
      onKeyDown={(e) => {
        if (e.target !== e.currentTarget || renaming) return;
        if (e.key === "ContextMenu" || (e.shiftKey && e.key === "F10")) {
          e.preventDefault();
          const rect = e.currentTarget.getBoundingClientRect();
          onOpenContextMenu(rect.left + 12, rect.bottom);
        } else if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onSelect();
        }
      }}
    >
      <StatusIcon status={run.status} t={t} mode={run.mode} />
      <div className="nm-wrap">
        {renaming ? (
          <RenameInput initial={run.name} onCommit={onCommitRename} onCancel={onCancelRename} />
        ) : (
          <>
            <span className="nm">{name}</span>
            <span className="sub">
              {run.kind === "local_pi" && <span className="ct">Pi</span>}
              {run.mode !== "pentest" && run.category && <span className="ct">{run.category}</span>}
              {run.operator_label && (
                <span className="ct" data-operator-label={run.operator_label}>
                  {operatorLabelText(run.operator_label, lang)}
                </span>
              )}
              <span className="st">{t(`rail.status.${run.status}`)}</span>
              {when && <span className="when" data-tooltip={absTime(run.updated_at)}>{when}</span>}
            </span>
          </>
        )}
      </div>

      {pinned && <span className="pin-badge" data-tooltip={t("rail.menu.unpin")}><Icon name="pin" size={12} /></span>}

      {!renaming && (
        <div
          className={`row-menu ${menuOpen ? "menu-open" : ""}`}
          draggable={false}
          onPointerDownCapture={markMenuPointer}
          onPointerUpCapture={clearMenuPointerSoon}
          onPointerCancelCapture={clearMenuPointerSoon}
          onMouseDown={(e) => e.stopPropagation()}
          onDragStart={(e) => { e.preventDefault(); e.stopPropagation(); }}
        >
          <div className="rail-quick-actions">
            <button
              type="button"
              aria-label={t(run.pinned ? "rail.menu.unpin" : "rail.menu.pin")}
              aria-pressed={run.pinned}
              title={t(run.pinned ? "rail.menu.unpin" : "rail.menu.pin")}
              onClick={(e) => {
                e.stopPropagation();
                onAction({ kind: "pin", runId: run.run_id, pinned: !run.pinned });
              }}
            ><Icon name="pin" size={14} /></button>
            <button
              type="button"
              aria-label={t(run.archived ? "rail.menu.unarchive" : "rail.menu.archive")}
              title={t(run.archived ? "rail.menu.unarchive" : "rail.menu.archive")}
              onClick={(e) => {
                e.stopPropagation();
                onAction({ kind: "archive", runId: run.run_id, archived: !run.archived });
              }}
            ><Icon name="archive" size={14} /></button>
          </div>
          <Button
            ref={menuTriggerRef}
            className="dots"
            data-tooltip={t("rail.menu.moreActions")}
            aria-label={t("rail.menu.moreActions")}
            aria-haspopup="menu"
            aria-expanded={menuOpen}
            onClick={(e) => { e.stopPropagation(); onToggleMenu(); clearMenuPointerSoon(); }}
          ><Icon name="more" size={15} /></Button>
          <RowMenu
              open={menuOpen}
              contextPosition={contextPosition}
              anchorRef={contextPosition ? rowRef : menuTriggerRef}
              run={run}
              t={t}
              onClose={onCloseMenu}
              onStartRename={onStartRename}
              onAction={onAction}
            />
        </div>
      )}
    </div>
  );
}

function tsMs(ts?: number): number {
  if (!ts) return 0;
  return ts < 1e12 ? ts * 1000 : ts;
}

function relTime(ts: number | undefined, lang: Lang): string {
  const ms = tsMs(ts);
  if (!ms) return "";
  const sec = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (sec < 5) return lang === "zh" ? "刚刚" : "just now";
  if (sec < 60) return lang === "zh" ? `${sec} 秒前` : `${sec}s ago`;
  const min = Math.floor(sec / 60);
  if (min < 60) return lang === "zh" ? `${min} 分钟前` : `${min}m ago`;
  const hr = Math.floor(min / 60);
  if (hr < 24) return lang === "zh" ? `${hr} 小时前` : `${hr}h ago`;
  const day = Math.floor(hr / 24);
  return lang === "zh" ? `${day} 天前` : `${day}d ago`;
}

function absTime(ts: number | undefined): string | undefined {
  const ms = tsMs(ts);
  return ms ? new Date(ms).toLocaleString() : undefined;
}

function RowMenu({
  open, contextPosition, anchorRef, run, t, onClose, onStartRename, onAction,
}: {
  open: boolean;
  contextPosition: { x: number; y: number } | null;
  anchorRef: RefObject<HTMLElement | null>;
  run: RunSummary;
  t: (k: string, v?: Record<string, string | number>) => string;
  onClose: () => void;
  onStartRename: () => void;
  onAction: (a: RailAction) => void;
}) {
  const act = (a: RailAction) => { onClose(); onAction(a); };
  return (
    <Menu open={open} onOpenChange={(next) => { if (!next) onClose(); }} anchorPoint={contextPosition} anchorRef={anchorRef} placement={contextPosition ? "bottom-start" : "bottom-end"} ariaLabel={t("rail.menu.moreActions")}>
      <MenuItem icon="pencil" onSelect={onStartRename}>{t("rail.menu.rename")}</MenuItem>
      <MenuItem icon={run.pinned ? "pinOff" : "pin"} onSelect={() => act({ kind: "pin", runId: run.run_id, pinned: !run.pinned })}>{t(run.pinned ? "rail.menu.unpin" : "rail.menu.pin")}</MenuItem>
      <MenuSeparator />
      <MenuItem checked={run.operator_label === "pending_followup"} onSelect={() => act({ kind: "operatorLabel", runId: run.run_id, label: "pending_followup" })}>标记：待跟进</MenuItem>
      <MenuItem checked={run.operator_label === "processed"} onSelect={() => act({ kind: "operatorLabel", runId: run.run_id, label: "processed" })}>标记：已处理</MenuItem>
      <MenuItem checked={run.operator_label === "ignored"} onSelect={() => act({ kind: "operatorLabel", runId: run.run_id, label: "ignored" })}>标记：已忽略</MenuItem>
      {run.operator_label ? <MenuItem onSelect={() => act({ kind: "operatorLabel", runId: run.run_id, label: null })}>清除跟进标签</MenuItem> : null}
      <MenuSeparator />
      <MenuItem icon="archive" onSelect={() => act({ kind: "archive", runId: run.run_id, archived: !run.archived })}>{t(run.archived ? "rail.menu.unarchive" : "rail.menu.archive")}</MenuItem>
      <MenuItem icon="trash" danger onSelect={() => act({ kind: "delete", runId: run.run_id })}>{t("rail.menu.delete")}</MenuItem>
    </Menu>
  );
}

function FolderMenu({
  open, trigger, contextPosition, t, onOpen, onClose, onRename, onDelete,
}: {
  open: boolean;
  trigger: ReactElement;
  contextPosition: { x: number; y: number } | null;
  t: (k: string, v?: Record<string, string | number>) => string;
  onOpen: () => void;
  onClose: () => void;
  onRename: () => void;
  onDelete: () => void;
}) {
  return (
    <Menu open={open} trigger={trigger} anchorPoint={contextPosition} placement={contextPosition ? "bottom-start" : "bottom-end"} onOpenChange={(next) => { if (next) onOpen(); else onClose(); }} ariaLabel={t("rail.menu.moreActions")}>
      <MenuItem icon="pencil" onSelect={onRename}>{t("rail.menu.rename")}</MenuItem>
      <MenuSeparator />
      <MenuItem icon="trash" danger onSelect={onDelete}>{t("rail.folderDelete")}</MenuItem>
    </Menu>
  );
}

function RenameInput({ initial, onCommit, onCancel }: {
  initial: string; onCommit: (v: string) => void; onCancel: () => void;
}) {
  const [v, setV] = useState(initial);
  const ref = useRef<HTMLInputElement | null>(null);
  const committedRef = useRef(false);
  const commit = (value: string) => {
    if (committedRef.current) return;
    committedRef.current = true;
    onCommit(value);
  };
  const cancel = () => {
    if (committedRef.current) return;
    committedRef.current = true;
    onCancel();
  };
  useEffect(() => { ref.current?.focus(); ref.current?.select(); }, []);
  return (
    <input
      ref={ref}
      className="rename-input"
      value={v}
      onChange={(e) => setV(e.target.value)}
      onClick={(e) => e.stopPropagation()}
      onKeyDown={(e) => {
        e.stopPropagation();
        if (e.key === "Enter") commit(v);
        else if (e.key === "Escape") cancel();
      }}
      onBlur={() => commit(v)}
    />
  );
}

function StatusIcon({ status, t, mode }: { status: RunStatus; t: (k: string) => string; mode?: "ctf" | "pentest" }) {
  if (status === "running") {
    return <span className="tk spin" aria-label={t("rail.status.running")}><span className="spinner" /></span>;
  }
  const icon: Record<Exclude<RunStatus, "running">, IconName> = {
    draft: "dot",
    idle: "dot",
    paused: "pause",
    stopping: "hourglass",
    interrupted: "alert",
    solved: mode === "pentest" ? "check" : "flag",
    finished: "stop",
    failed: "alert",
  };
  return (
    <span className={`tk st-${status}`} aria-label={t(`rail.status.${status}`)}>
      <Icon name={Object.hasOwn(icon, status) ? icon[status] : "help"} size={13} />
    </span>
  );
}
