"use client";

import { Button } from "@heroui/react";
import { createPortal } from "react-dom";

import { useEffect, useRef, useState } from "react";
import type { CSSProperties, DragEvent as ReactDragEvent, PointerEvent as ReactPointerEvent } from "react";
import { RunSummary, RunStatus, Folder } from "@/lib/useRun";
import { useLang, useT, type Lang } from "@/lib/i18n";
import { Icon, type IconName } from "@/components/Icon";
import { clampRailWidth, railWidthMax, RAIL_WIDTH_DEFAULT, RAIL_WIDTH_MIN, RAIL_WIDTH_MAX } from "@/lib/railSizing";
import { transitionClass, useTransitionState } from "@/lib/useTransitionState";

/**
 * Left thread rail — ChatGPT/Claude-style conversation list.
 *
 * Sections: Pinned · Recent · (optional) Archived. The active draft is shown
 * separately at the top via `draftActive` (it has no backend row yet).
 *
 * Core interaction rules:
 *  - Clicking a row only SELECTS it (highlight + accent bar). It never reorders
 *    or hoists the row.
 *  - Pinning is the ONLY way a row enters the Pinned section, via a row action.
 *  - Running rows show a spinner in place; they don't auto-hoist.
 *  - Archived rows are hidden behind a toggle.
 */

type RailAction =
  | { kind: "pin"; runId: string; pinned: boolean }
  | { kind: "archive"; runId: string; archived: boolean }
  | { kind: "rename"; runId: string; name: string }
  | { kind: "delete"; runId: string }
  | { kind: "move"; runId: string; folderId: string | null }
  | { kind: "reorder"; runIds: string[]; pinned: boolean }
  | { kind: "newFolder" }
  | { kind: "renameFolder"; folderId: string; name: string }
  | { kind: "deleteFolder"; folderId: string };

export function ThreadRail({
  collapsed,
  width,
  runs,
  folders,
  activeRunId,
  draftActive,
  connected,
  onNew,
  onSelect,
  onAction,
  onResize,
  onOpenSettings,
  onClose,
  workspaceMode = "ctf",
}: {
  collapsed: boolean;
  width: number;
  runs: RunSummary[];
  folders: Folder[];
  activeRunId: string;
  /** true when the active conversation is a not-yet-dispatched local draft */
  draftActive: boolean;
  connected: boolean;
  onNew: () => void;
  onSelect: (runId: string) => void;
  // newFolder resolves to the created folder so the rail can start inline-naming
  // it immediately; every other action is fire-and-forget.
  onAction: (a: RailAction) => void | Promise<Folder | null | void>;
  onResize: (width: number) => void;
  onOpenSettings: () => void;
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
      };
      const label = labels[key];
      if (label) return label[lang === "zh" ? 0 : 1];
    }
    return baseT(key, values);
  };
  const [showArchived, setShowArchived] = useState(false);
  const activeRun = runs.find((r) => r.run_id === activeRunId);
  const activeFinished = !draftActive && !!activeRun?.finished;
  // A not-yet-dispatched draft has no backend run, so no SSE stream is opened by
  // design (see useRun). That's "idle", not "disconnected" — only show the red
  // disconnected state when a real, unfinished run has actually lost its stream.
  const footState = connected ? "online" : draftActive || activeFinished ? "idle" : "off";
  const footLabel = connected
    ? t("rail.swarmOnline")
    : activeFinished
      ? t("rail.runFinished")
      : draftActive
        ? t("rail.idle")
        : t("rail.disconnected");
  // Client-side rail search/filter over the already-loaded runs (name / category /
  // status / run_id). Mirrors the search affordance the graph + blackboard already
  // have — the run list is the one busy surface that lacked one. Empty = show all.
  const [query, setQuery] = useState("");
  const [menuFor, setMenuFor] = useState<string | null>(null);
  const [contextPosition, setContextPosition] = useState<{ runId: string; x: number; y: number } | null>(null);
  const [renaming, setRenaming] = useState<string | null>(null);
  // native HTML5 drag-and-drop: the run id being dragged + the current drop zone
  // (folder id, or "" for the top-level Recent group) for the drop highlight.
  const [dragRun, setDragRun] = useState<string | null>(null);
  const [dropZone, setDropZone] = useState<string | null>(null);
  const [rowDrop, setRowDrop] = useState<{ runId: string; side: "before" | "after" } | null>(null);
  const [collapsedFolders, setCollapsedFolders] = useState<Set<string>>(new Set());
  const [renamingFolder, setRenamingFolder] = useState<string | null>(null);
  const [resizing, setResizing] = useState(false);
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const resizeCleanup = useRef<(() => void) | null>(null);
  // preserve scroll position across run-list refreshes (poll re-renders)
  const scrollTop = useRef(0);
  useEffect(() => {
    const el = scrollRef.current;
    if (el) el.scrollTop = scrollTop.current;
  });
  useEffect(() => () => resizeCleanup.current?.(), []);

  const resizeTo = (next: number) => {
    const viewport = typeof window !== "undefined" ? window.innerWidth : undefined;
    onResize(clampRailWidth(next, viewport));
  };

  const startResize = (e: ReactPointerEvent<HTMLDivElement>) => {
    if (collapsed) return;
    e.preventDefault();
    e.stopPropagation();
    resizeCleanup.current?.();
    setResizing(true);
    document.body.classList.add("rail-resizing");
    const startX = e.clientX;
    const startWidth = width;

    const onMove = (ev: PointerEvent) => {
      ev.preventDefault();
      resizeTo(startWidth + ev.clientX - startX);
    };
    const stop = () => {
      setResizing(false);
      document.body.classList.remove("rail-resizing");
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", stop);
      window.removeEventListener("pointercancel", stop);
      resizeCleanup.current = null;
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

  // Rail-level Escape: dismiss any open ⋯ menu (row or folder) and cancel an
  // in-progress rename. This is the innermost dismiss layer — RenameInput already
  // stops propagation of its own keydown, so its Esc cancels the rename without
  // reaching here; this handles the case where focus has moved off the field
  // (e.g. menu still open) so a single Esc always clears the rail's transient UI.
  // The settings modal / artifact panel sit on top and handle Esc first (their
  // handlers stop propagation or run before this fires), so this never steals
  // their Esc.
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

  // Filter every run by the search query (name / category / localized status /
  // run_id), case-insensitive. Applied up front so Pinned / Folders / Recent /
  // Archived all narrow together. Blank query → identity (show everything).
  const q = query.trim().toLowerCase();
  const matchesQuery = (r: RunSummary): boolean => {
    if (!q) return true;
    const hay = [
      r.name,
      r.category,
      r.status,
      t(`rail.status.${r.status}`),
      relTime(r.updated_at, lang),
      r.run_id,
    ].filter(Boolean).join(" ").toLowerCase();
    return hay.includes(q);
  };
  const matched = q ? runs.filter(matchesQuery) : runs;

  const pinned = matched
    .filter((r) => r.pinned && !r.archived)
    .sort((a, b) => (b.pinned_at ?? 0) - (a.pinned_at ?? 0));
  // A manual drag must remain authoritative even while a run is active.
  const byCreationOrder = (a: RunSummary, b: RunSummary) =>
    (b.order ?? 0) - (a.order ?? 0);
  const liveRuns = matched.filter((r) => !r.pinned && !r.archived);
  const folderIds = new Set(folders.map((f) => f.id));
  // a run with a folder_id whose folder was deleted falls back to top-level.
  const inFolder = (r: RunSummary, fid: string) => r.folder_id === fid;
  const ungrouped = liveRuns
    .filter((r) => !r.folder_id || !folderIds.has(r.folder_id))
    .sort(byCreationOrder);
  const archived = matched.filter((r) => r.archived).sort(byCreationOrder);
  // When searching, surface archived hits automatically so a match isn't hidden
  // behind the collapsed Archived toggle, and detect a genuinely empty result so
  // we can show a "no matches" state instead of a bare blank rail.
  const searching = q.length > 0;
  const noMatches = searching && pinned.length === 0 && liveRuns.length === 0 && archived.length === 0;

  // drop a dragged run into a folder (fid="" → top-level / un-file).
  const dropInto = (fid: string) => {
    if (dragRun) onAction({ kind: "move", runId: dragRun, folderId: fid || null });
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
    return r.folder_id && folderIds.has(r.folder_id) ? `folder:${r.folder_id}` : "recent";
  };
  const canDropOnRow = (target: RunSummary) => {
    const source = runs.find((r) => r.run_id === dragRun);
    return !!source && source.run_id !== target.run_id && sectionOf(source) === sectionOf(target);
  };
  const sectionItems = (target: RunSummary) => {
    const section = sectionOf(target);
    return section === "pinned" ? pinned
      : section === "archived" ? archived
      : section === "recent" ? ungrouped
      : liveRuns.filter((r) => sectionOf(r) === section).sort(byCreationOrder);
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
  const reorderOnRow = (target: RunSummary, side: "before" | "after") => {
    if (!dragRun || !canDropOnRow(target)) return;
    const section = sectionOf(target);
    const items = sectionItems(target);
    const before = items.map((r) => r.run_id);
    const after = before.filter((id) => id !== dragRun);
    const at = after.indexOf(target.run_id) + (side === "after" ? 1 : 0);
    after.splice(at, 0, dragRun);
    if (after.some((id, index) => id !== before[index])) {
      onAction({ kind: "reorder", runIds: after, pinned: section === "pinned" });
    }
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
        x: Math.max(8, Math.min(x, window.innerWidth - 220)),
        y: Math.max(8, Math.min(y, window.innerHeight - 280)),
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
      setRowDrop(null);
      setDropZone(null);
    },
  });

  return (
    <nav
      className={`rail t-texts-reveal ${collapsed ? "collapsed" : ""} ${resizing ? "resizing" : ""}`}
      style={{ "--rail-width": `${width}px` } as CSSProperties}
      aria-label={t("a11y.nav")}
    >
      <div className="rail-body">
        <div className="rail-top">
          <span className="rail-title">{t("rail.navTitle")}</span>
          <Button className="rail-close" isIconOnly variant="ghost" onPress={onClose} aria-label={t("rail.close")}><Icon name="x" size={15} /></Button>
        </div>
        <Button className="newsolve" onClick={onNew}>
          <span className="newsolve-icon" aria-hidden="true"><Icon name="plus" size={15} /></span>
          <span>{t("rail.newSolve")}</span>
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
              <Button className="thread-item active is-draft t-texts-reveal" onClick={() => onSelect(activeRunId)}>
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
          const items = liveRuns.filter((r) => inFolder(r, f.id)).sort(byCreationOrder);
          // While searching, hide folders that contain no match so results aren't
          // buried under empty folder headers. (Drag-and-drop targets are moot mid-search.)
          if (searching && items.length === 0) return null;
          const open = searching || !collapsedFolders.has(f.id);
          return (
            <div
              key={f.id}
              className={`rail-folder ${dropZone === f.id ? "drop" : ""} ${menuFor === `folder:${f.id}` ? "menu-open" : ""}`}
              {...dzProps(f.id)}
            >
              <div className="rail-folder-head">
                <Button className="rail-folder-toggle" onClick={() => toggleFolder(f.id)}>
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
                  <Button className="dots" data-tooltip={t("rail.menu.moreActions")} aria-label={t("rail.menu.moreActions")}
                    onClick={(e) => { e.stopPropagation(); setMenuFor((cur) => (cur === `folder:${f.id}` ? null : `folder:${f.id}`)); }}><Icon name="more" size={15} /></Button>
                  <FolderMenu
                      open={menuFor === `folder:${f.id}`}
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

        <div className={`rail-sec rail-recent-head ${dropZone === "" ? "drop" : ""}`} {...dzProps("")}>
          <span>{t("rail.recent")}</span>
          {workspaceMode === "ctf" && <Button className="rail-newfolder" data-tooltip={t("rail.newFolderTitle")} aria-label={t("rail.newFolderTitle")}
            onClick={async () => {
              const folder = await onAction({ kind: "newFolder" });
              // immediately drop the new folder into inline-rename (auto-focused);
              // blur/Enter commits — type nothing and it just keeps the default name.
              if (folder && typeof folder === "object" && "id" in folder) setRenamingFolder(folder.id);
            }}><Icon name="folderPlus" size={15} /></Button>}
        </div>
        <div className={`thread ${dropZone === "" ? "drop" : ""}`} {...dzProps("")}>
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
          // While searching, reveal matching archived runs inline (don't bury a hit
          // behind the collapsed toggle); otherwise keep the manual show/hide.
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
          <div className="rail-foot-status">
            <span className="rail-foot-state">
              <span className={`dot ${footState === "online" ? "" : footState}`} />
              <span>{footLabel}</span>
            </span>
            <Button className="rail-settings-btn" onClick={onOpenSettings} data-tooltip={t("settings.open")} aria-label={t("settings.open")}>
              <Icon name="gear" size={14} />
            </Button>
          </div>
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
    "t-texts-reveal",
    active ? "active" : "",
    pinned ? "is-pinned" : "",
    archived ? "is-archived" : "",
    dragging ? "dragging" : "",
    rowDrop ? `drop-${rowDrop}` : "",
    menuOpen ? "menu-open" : "",
  ].filter(Boolean).join(" ");
  const suppressRowDragRef = useRef(false);
  const shouldSuppressRowDrag = (target: EventTarget | null) => {
    if (suppressRowDragRef.current) return true;
    return target instanceof HTMLElement && !!target.closest(".row-menu, .menu, button, input, .rename-input");
  };
  const markMenuPointer = () => {
    suppressRowDragRef.current = true;
  };
  const clearMenuPointerSoon = () => {
    window.setTimeout(() => { suppressRowDragRef.current = false; }, 0);
  };

  return (
    <div
      className={cls}
      role="button"
      tabIndex={0}
      // NOT draggable while renaming OR while the ⋯ menu is open — otherwise a
      // press on a menu item starts an HTML5 row-drag (drag begins on mousedown,
      // before the button's click ever fires) and the option never registers.
      draggable={!renaming && !menuOpen}
      onDragStart={(e) => {
        if (shouldSuppressRowDrag(e.target)) {
          e.preventDefault();
          e.stopPropagation();
          return;
        }
        e.dataTransfer.effectAllowed = "move";
        e.dataTransfer.setData("text/plain", run.run_id);
        onDragStart();
      }}
      onDragEnd={() => { suppressRowDragRef.current = false; onDragEnd(); }}
      onDragOver={onDragOverRow}
      onDragLeave={onDragLeaveRow}
      onDrop={onDropOnRow}
      data-tooltip={run.mode === "pentest" ? name : `${name} · ${run.category || "—"}`}
      onClick={() => !renaming && onSelect()}
      onContextMenu={(e) => {
        e.preventDefault();
        e.stopPropagation();
        if (!renaming) onOpenContextMenu(e.clientX, e.clientY);
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
              {run.mode !== "pentest" && run.category && <span className="ct">{run.category}</span>}
              <span className="st">{t(`rail.status.${run.status}`)}</span>
              {when && <span className="when" data-tooltip={absTime(run.updated_at)}>{when}</span>}
            </span>
          </>
        )}
      </div>

      {pinned && <span className="pin-badge" data-tooltip={t("rail.menu.unpin")}><Icon name="pin" size={12} /></span>}

      {!renaming && (
        // stop drag from initiating inside the menu region: HTML5 drag starts on
        // mousedown at the draggable row, so a press on the ⋯ button or any menu
        // item would begin a row-drag before the click lands. Halting mousedown +
        // dragstart here keeps presses in the menu as plain clicks.
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
  open, contextPosition, run, t, onClose, onStartRename, onAction,
}: {
  open: boolean;
  contextPosition: { x: number; y: number } | null;
  run: RunSummary;
  t: (k: string, v?: Record<string, string | number>) => string;
  onClose: () => void;
  onStartRename: () => void;
  onAction: (a: RailAction) => void;
}) {
  const ref = useRef<HTMLDivElement | null>(null);
  const presence = useTransitionState(open, { durationVar: "--dropdown-close-dur", durationMs: 150 });
  useEffect(() => {
    if (!open) return;
    const onDoc = (e: MouseEvent) => { if (ref.current && !ref.current.contains(e.target as Node)) onClose(); };
    const onEsc = (e: KeyboardEvent) => { if (e.key === "Escape") onClose(); };
    document.addEventListener("mousedown", onDoc);
    document.addEventListener("keydown", onEsc);
    return () => { document.removeEventListener("mousedown", onDoc); document.removeEventListener("keydown", onEsc); };
  }, [onClose, open]);
  // focus the first ENABLED item on open (Share/Collab are disabled placeholders)
  // so the menu is keyboard-driveable the instant it appears.
  useEffect(() => { if (open) ref.current?.querySelector<HTMLButtonElement>(".mi:not([disabled])")?.focus(); }, [open]);

  const act = (a: RailAction) => { onClose(); onAction(a); };

  if (!presence.present) return null;
  const menu = (
    <div
      className={transitionClass("menu t-dropdown", presence.phase)}
      data-origin="top-right"
      aria-hidden={!open}
      inert={!open}
      ref={ref}
      style={contextPosition ? {
        position: "fixed", left: contextPosition.x, top: contextPosition.y,
        right: "auto", width: 200, maxHeight: "calc(100vh - 16px)",
        overflowY: "auto", zIndex: 1000,
      } : undefined}
      onPointerDown={(e) => e.stopPropagation()}
      onMouseDown={(e) => e.stopPropagation()}
      onClick={(e) => e.stopPropagation()}
    >
      {/* Not yet wired (no backend) — shown disabled per scope decision */}
      <Button className="mi" isDisabled>{t("rail.menu.share")}</Button>
      <Button className="mi" isDisabled>{t("rail.menu.collab")}</Button>
      <Button className="mi" onClick={onStartRename}>{t("rail.menu.rename")}</Button>
      <div className="msep" />
      {run.pinned ? (
        <Button className="mi" onClick={() => act({ kind: "pin", runId: run.run_id, pinned: false })}>{t("rail.menu.unpin")}</Button>
      ) : (
        <Button className="mi" onClick={() => act({ kind: "pin", runId: run.run_id, pinned: true })}>{t("rail.menu.pin")}</Button>
      )}
      {run.archived ? (
        <Button className="mi" onClick={() => act({ kind: "archive", runId: run.run_id, archived: false })}>{t("rail.menu.unarchive")}</Button>
      ) : (
        <Button className="mi" onClick={() => act({ kind: "archive", runId: run.run_id, archived: true })}>{t("rail.menu.archive")}</Button>
      )}
      <Button className="mi danger" onClick={() => act({ kind: "delete", runId: run.run_id })}>{t("rail.menu.delete")}</Button>
    </div>
  );
  return contextPosition && typeof document !== "undefined"
    ? createPortal(menu, document.body)
    : menu;
}

function FolderMenu({
  open, t, onClose, onRename, onDelete,
}: {
  open: boolean;
  t: (k: string, v?: Record<string, string | number>) => string;
  onClose: () => void;
  onRename: () => void;
  onDelete: () => void;
}) {
  const ref = useRef<HTMLDivElement | null>(null);
  const presence = useTransitionState(open, { durationVar: "--dropdown-close-dur", durationMs: 150 });
  useEffect(() => {
    if (!open) return;
    const onDoc = (e: MouseEvent) => { if (ref.current && !ref.current.contains(e.target as Node)) onClose(); };
    const onEsc = (e: KeyboardEvent) => { if (e.key === "Escape") onClose(); };
    document.addEventListener("mousedown", onDoc);
    document.addEventListener("keydown", onEsc);
    return () => { document.removeEventListener("mousedown", onDoc); document.removeEventListener("keydown", onEsc); };
  }, [onClose, open]);
  // focus the first item on open so the menu is immediately keyboard-driveable
  useEffect(() => { if (open) ref.current?.querySelector<HTMLButtonElement>(".mi")?.focus(); }, [open]);
  if (!presence.present) return null;
  return (
    <div
      className={transitionClass("menu t-dropdown", presence.phase)}
      data-origin="top-right"
      aria-hidden={!open}
      inert={!open}
      ref={ref}
      onPointerDown={(e) => e.stopPropagation()}
      onMouseDown={(e) => e.stopPropagation()}
      onClick={(e) => e.stopPropagation()}
    >
      <Button className="mi" onClick={onRename}>{t("rail.menu.rename")}</Button>
      <Button className="mi danger" onClick={onDelete}>{t("rail.folderDelete")}</Button>
    </div>
  );
}

function RenameInput({ initial, onCommit, onCancel }: {
  initial: string; onCommit: (v: string) => void; onCancel: () => void;
}) {
  const [v, setV] = useState(initial);
  const ref = useRef<HTMLInputElement | null>(null);
  // Guard against a double commit: pressing Enter calls onCommit → the parent
  // unmounts this input → React fires onBlur during unmount → onCommit again.
  // (Escape→onCancel also unmounts, so the same blur would re-commit a discarded
  // edit.) committedRef makes commit/cancel one-shot.
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

/** Status glyph in front of the title — one per lifecycle state. */
function StatusIcon({ status, t, mode }: { status: RunStatus; t: (k: string) => string; mode?: "ctf" | "pentest" }) {
  if (status === "running") {
    return <span className="tk spin" aria-label={t("rail.status.running")}><span className="spinner" /></span>;
  }
  const icon: Record<Exclude<RunStatus, "running">, IconName> = {
    draft: "dot",
    paused: "pause",
    solved: mode === "pentest" ? "check" : "flag",
    finished: "stop",
    failed: "alert",
  };
  return (
    <span className={`tk st-${status}`} aria-label={t(`rail.status.${status}`)}>
      <Icon name={icon[status as Exclude<RunStatus, "running">]} size={13} />
    </span>
  );
}
