"use client";

import { useCallback, useEffect, useMemo, useRef, useState, type CSSProperties, type DragEvent, type KeyboardEvent } from "react";
import { useRouter, useSearchParams } from "next/navigation";
import { cn } from "@/lib/cn";
import { useT } from "@/lib/i18n";
import { apiFetch, type RunSummary } from "@/lib/useRun";
import {
  cancelReportGeneration,
  createReport,
  createReportFolder,
  deleteReport,
  endRunDrag,
  generateReport,
  generationActive,
  getGenerationSettings,
  hasRunDrag,
  listReports,
  readRunDragId,
  renderWriteupTemplate,
  ReportError,
  resolvePendingUpdate,
  subscribeRunPointerDrag,
  updateReport,
  type ReportFolder,
  type ReportGenerationSettings,
  type ReportRunSummary,
  type RunReport,
  type WriteupTemplate,
} from "@/lib/reports";
import { reportErrorInfo, reportErrorText } from "@/lib/reportErrors";
import { GenerationBanner } from "./GenerationBanner";
import { PendingUpdateDialog } from "./PendingUpdateDialog";
import { BatchGenerateDialog } from "./BatchGenerateDialog";
import { WriteupSettingsDialog } from "./WriteupSettingsDialog";
import { ReportTree } from "./ReportTree";
import { ImportDialog } from "./ImportDialog";
import {
  folderPath,
  foldersById,
  hasReportNodeDrag,
  loadExpanded,
  pathLabel,
  saveExpanded,
} from "@/lib/reportTree";
import {
  countWords,
  inlineReportAssets,
  joinReportAssets,
  MAX_REPORT_BYTES,
  reportByteSize,
  searchableMarkdown,
  splitReportAssets,
  type ReportAssets,
} from "@/lib/reportMarkdown";
import { Icon } from "@muteki/ui";
import {
  Button,
  Callout,
  Dialog,
  IconButton,
  Label,
  Menu,
  MenuItem,
  MenuLabel,
  MenuSeparator,
  MenuSub,
  ResizeHandle,
  SearchInput,
  SegmentedControl,
  Select,
  Skeleton,
  TextField,
  toast,
  type ListOption,
} from "@muteki/ui";
import { ChatMarkdown } from "@/components/chat/markdown/ChatMarkdown";
import { MarkdownEditor, type EditorView } from "./MarkdownEditor";
import type { ReportEditorInput } from "./ReportTextEditor";
import { FilterChip } from "./FilterChip";

type StatusFilter = "all" | "draft" | "final";
type SolveFilter = "all" | "solved" | "unsolved";

function fmtTime(secs: number): string {
  if (!secs) return "—";
  return new Date(secs * 1000).toLocaleString(undefined, { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit" });
}

function fmtFullTime(secs: number): string {
  return secs ? new Date(secs * 1000).toLocaleString() : "—";
}

function downloadText(name: string, text: string) {
  const url = URL.createObjectURL(new Blob([text], { type: "text/markdown;charset=utf-8" }));
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = name;
  anchor.click();
  window.setTimeout(() => URL.revokeObjectURL(url), 1000);
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

class SourceMissing extends Error {}

async function fetchSourceMarkdown(mode: "ctf" | "pentest", runId: string): Promise<{ markdown: string; ref: string | null }> {
  if (mode === "pentest") {
    const res = await apiFetch(`/api/runs/${encodeURIComponent(runId)}/pentest-report.md`);
    if (!res.ok) {
      const data = await res.json().catch(() => ({}));
      throw new Error(data?.error?.message || data?.detail || `HTTP ${res.status}`);
    }
    return { markdown: await res.text(), ref: null };
  }
  const res = await apiFetch(`/api/runs/${encodeURIComponent(runId)}/writeup`);
  const data = await res.json().catch(() => ({}));
  if (res.status === 404 && String(data?.error?.code ?? "").endsWith("writeup_not_found")) throw new SourceMissing();
  if (!res.ok || data.ok === false) {
    throw new Error(data?.error?.message || data?.detail || `HTTP ${res.status}`);
  }
  return { markdown: data.markdown ?? "", ref: typeof data.job_id === "string" ? data.job_id : null };
}

const REPORTS_LIST_WIDTH_DEFAULT = 280;
const REPORTS_LIST_WIDTH_MIN = 220;
const REPORTS_LIST_WIDTH_MAX = 480;

function reportsListWidthKey(mode: "ctf" | "pentest"): string {
  return `muteki.reportsList.width.${mode}`;
}

function reportsListWidthMax(viewportWidth?: number): number {
  if (!viewportWidth || viewportWidth <= 0) return REPORTS_LIST_WIDTH_MAX;
  return Math.max(REPORTS_LIST_WIDTH_MIN, Math.min(REPORTS_LIST_WIDTH_MAX, Math.round(viewportWidth * 0.45)));
}

function clampListWidth(width: number, viewportWidth?: number): number {
  const next = Number.isFinite(width) ? width : REPORTS_LIST_WIDTH_DEFAULT;
  return Math.round(Math.min(reportsListWidthMax(viewportWidth), Math.max(REPORTS_LIST_WIDTH_MIN, next)));
}

function stripLeadingTitle(markdown: string, title: string): string {
  const match = /^\s*#[ \t]+(.+?)[ \t#]*(?:\r?\n|$)/.exec(markdown);
  if (!match || match[1].trim() !== title.trim()) return markdown;
  return markdown.slice(match[0].length);
}

function TagInput({ tags, onChange, placeholder, addLabel, removeLabel }: {
  tags: string[];
  onChange: (tags: string[]) => void;
  placeholder: string;
  addLabel: string;
  removeLabel: (tag: string) => string;
}) {
  const [draft, setDraft] = useState("");
  const [open, setOpen] = useState(false);
  const commit = () => {
    const tag = draft.trim();
    setDraft("");
    if (tag && !tags.includes(tag)) onChange([...tags, tag]);
  };
  const onKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.nativeEvent.isComposing || event.keyCode === 229) return;
    if (event.key === "Enter" || event.key === "," || event.key === "，") {
      event.preventDefault();
      commit();
    } else if (event.key === "Backspace" && !draft && tags.length) {
      onChange(tags.slice(0, -1));
    }
  };
  if (!tags.length && !open) {
    return (
      <div className="flex min-h-6 items-center">
        <button
          type="button"
          onClick={() => setOpen(true)}
          className="-ml-1.5 inline-flex h-6 items-center gap-1 rounded-md px-1.5 text-cx-xs text-cx-fg-4 transition-colors hover:bg-cx-hover hover:text-cx-fg-2"
        >
          <Icon name="hash" size={11} />
          {addLabel}
        </button>
      </div>
    );
  }
  return (
    <div className="flex min-h-6 flex-wrap items-center gap-1.5">
      {tags.map((tag) => (
        <span key={tag} className="inline-flex h-6 items-center gap-1 rounded-md bg-cx-hover pl-2 pr-1 text-cx-xs text-cx-fg-2">
          {tag}
          <button
            type="button"
            aria-label={removeLabel(tag)}
            onClick={() => onChange(tags.filter((item) => item !== tag))}
            className="grid size-4 place-items-center rounded text-cx-fg-4 hover:bg-cx-active hover:text-cx-fg"
          >
            <Icon name="x" size={11} />
          </button>
        </span>
      ))}
      <input
        value={draft}
        onChange={(event) => setDraft(event.target.value)}
        onKeyDown={onKeyDown}
        onBlur={() => { commit(); if (!tags.length) setOpen(false); }}
        autoFocus={open && tags.length === 0}
        placeholder={tags.length ? "" : placeholder}
        aria-label={placeholder}
        className="h-6 min-w-[8rem] flex-1 bg-transparent text-cx-xs text-cx-fg outline-none placeholder:text-cx-fg-4"
      />
    </div>
  );
}

function pickRunReport(list: RunReport[], targetRunId: string): RunReport | undefined {
  const matches = list.filter((r) => r.run_id === targetRunId);
  if (!matches.length) return undefined;
  matches.sort((a, b) => {
    const aAi = a.source.kind === "ai_writeup" ? 0 : 1;
    const bAi = b.source.kind === "ai_writeup" ? 0 : 1;
    if (aAi !== bAi) return aAi - bAi;
    return b.updated_at - a.updated_at;
  });
  return matches[0];
}

export function ReportsPage({ workspaceMode, runs, onOpenRun, active = true }: {
  workspaceMode: "ctf" | "pentest";
  runs: RunSummary[];
  onOpenRun: (runId: string) => void;
  active?: boolean;
}) {
  const t = useT();
  const router = useRouter();
  const searchParams = useSearchParams();
  const runFilter = searchParams.get("run") || "";
  const requestedReportId = searchParams.get("report") || "";
  const focusRun = searchParams.get("focusRun") || "";

  const [reports, setReports] = useState<RunReport[]>([]);
  const [runSummaries, setRunSummaries] = useState<Record<string, ReportRunSummary>>({});
  const [folders, setFolders] = useState<ReportFolder[]>([]);
  const [expanded, setExpanded] = useState<Set<string>>(() => loadExpanded(workspaceMode));
  const [importOpen, setImportOpen] = useState(false);
  const [importSeed, setImportSeed] = useState<{ files: { file: File; path: string }[]; entries: FileSystemEntry[] }>({ files: [], entries: [] });
  const [createFolderId, setCreateFolderId] = useState<string | null>(null);
  const [createFolderNonce, setCreateFolderNonce] = useState(0);
  const [genSettings, setGenSettings] = useState<ReportGenerationSettings | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [settingsTab, setSettingsTab] = useState<"templates" | "prompt">("templates");
  const [pendingTemplate, setPendingTemplate] = useState<WriteupTemplate | null>(null);
  const [moveTarget, setMoveTarget] = useState<RunReport | null>(null);
  const [organizeOpen, setOrganizeOpen] = useState(false);
  const [organizeBusy, setOrganizeBusy] = useState(false);
  const [challengeFolders, setChallengeFolders] = useState<{ id: string; name: string; order: number }[]>([]);
  const [fileDropActive, setFileDropActive] = useState(false);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState("");
  const [selectedId, setSelectedId] = useState("");

  const [query, setQuery] = useState("");
  const [statusFilter, setStatusFilter] = useState<StatusFilter>("all");
  const [categoryFilter, setCategoryFilter] = useState("all");
  const [solveFilter, setSolveFilter] = useState<SolveFilter>("all");

  const [editing, setEditing] = useState(false);
  const [editTitle, setEditTitle] = useState("");
  const [editTags, setEditTags] = useState<string[]>([]);
  const [editBody, setEditBody] = useState("");
  const [editAssets, setEditAssets] = useState<ReportAssets>({});
  const [editView, setEditView] = useState<EditorView>("split");
  const [actionError, setActionError] = useState("");
  const [conflict, setConflict] = useState(false);
  const [busy, setBusy] = useState(false);

  const [createOpen, setCreateOpen] = useState(false);
  const [createRunId, setCreateRunId] = useState("");
  const [createTitle, setCreateTitle] = useState("");
  const [createFrom, setCreateFrom] = useState<"blank" | "template" | "source">("blank");
  const [createTemplateId, setCreateTemplateId] = useState("builtin-standard");
  const [createError, setCreateError] = useState("");
  const [createBusy, setCreateBusy] = useState(false);
  const [deleteTarget, setDeleteTarget] = useState<RunReport | null>(null);
  const [deleteBusy, setDeleteBusy] = useState(false);
  const [discardAction, setDiscardAction] = useState<(() => void) | null>(null);
  const [aiConfirm, setAiConfirm] = useState<"use" | "generate" | null>(null);
  const [pendingDiffOpen, setPendingDiffOpen] = useState(false);
  const [batchOpen, setBatchOpen] = useState(false);
  const [checkedIds, setCheckedIds] = useState<Set<string>>(new Set());
  const [bulkDelete, setBulkDelete] = useState(false);
  const [serverChanged, setServerChanged] = useState(false);

  const [dragActive, setDragActive] = useState(false);
  const [dragHover, setDragHover] = useState(false);
  const [listDragHover, setListDragHover] = useState(false);
  const [dropBusy, setDropBusy] = useState(false);
  const [listWidth, setListWidth] = useState(REPORTS_LIST_WIDTH_DEFAULT);
  const [maxListWidth, setMaxListWidth] = useState(REPORTS_LIST_WIDTH_MAX);
  const gridRef = useRef<HTMLDivElement | null>(null);
  const listPaneRef = useRef<HTMLElement | null>(null);
  const detailPaneRef = useRef<HTMLElement | null>(null);
  const listResizingRef = useRef(false);
  const draftListWidthRef = useRef<number | null>(null);
  const pendingEdit = useRef("");
  const pendingFocusTitle = useRef(false);
  const titleInputRef = useRef<HTMLInputElement | null>(null);
  const editorRef = useRef<ReportEditorInput | null>(null);

  useEffect(() => {
    const syncViewportMax = () => {
      const vw = window.innerWidth;
      setMaxListWidth(reportsListWidthMax(vw));
      setListWidth((prev) => clampListWidth(prev, vw));
    };
    let initial = REPORTS_LIST_WIDTH_DEFAULT;
    try {
      const saved = Number(window.localStorage.getItem(reportsListWidthKey(workspaceMode)));
      if (Number.isFinite(saved) && saved > 0) initial = saved;
    } catch {
    }
    const vw = window.innerWidth;
    setMaxListWidth(reportsListWidthMax(vw));
    setListWidth(clampListWidth(initial, vw));
    window.addEventListener("resize", syncViewportMax);
    return () => window.removeEventListener("resize", syncViewportMax);
  }, [workspaceMode]);

  const commitListWidth = useCallback((next: number) => {
    const clamped = clampListWidth(next, typeof window !== "undefined" ? window.innerWidth : undefined);
    setListWidth(clamped);
    gridRef.current?.style.setProperty("--reports-list-width", `${clamped}px`);
    try {
      window.localStorage.setItem(reportsListWidthKey(workspaceMode), String(clamped));
    } catch {
    }
  }, [workspaceMode]);

  const onListWidthChange = useCallback((next: number) => {
    const clamped = clampListWidth(next, typeof window !== "undefined" ? window.innerWidth : undefined);
    if (listResizingRef.current) {
      draftListWidthRef.current = clamped;
      gridRef.current?.style.setProperty("--reports-list-width", `${clamped}px`);
      return;
    }
    commitListWidth(clamped);
  }, [commitListWidth]);

  const onListResizeDrag = useCallback((dragging: boolean) => {
    listResizingRef.current = dragging;
    if (!dragging) {
      const next = draftListWidthRef.current;
      draftListWidthRef.current = null;
      if (next !== null) commitListWidth(next);
    }
  }, [commitListWidth]);

  const eligibleRuns = useMemo(
    () => runs.filter((r) => r.status !== "draft" && !r.archived),
    [runs],
  );
  const quickRuns = useMemo(
    () => [...eligibleRuns]
      .sort((a, b) => {
        if (Boolean(a.solved) !== Boolean(b.solved)) return a.solved ? -1 : 1;
        return (b.updated_at ?? 0) - (a.updated_at ?? 0);
      })
      .slice(0, 4),
    [eligibleRuns],
  );

  const runName = useCallback(
    (runId: string | null) =>
      runId ? runSummaries[runId]?.name || runs.find((r) => r.run_id === runId)?.name || runId : "—",
    [runSummaries, runs],
  );
  const runCategory = useCallback(
    (runId: string | null) =>
      (runId && (runSummaries[runId]?.category || runs.find((r) => r.run_id === runId)?.category)) || "",
    [runSummaries, runs],
  );
  const runSolved = useCallback(
    (runId: string | null) =>
      (runId && (runSummaries[runId]?.solved ?? runs.find((r) => r.run_id === runId)?.solved)) || false,
    [runSummaries, runs],
  );

  const clearFilters = useCallback(() => {
    setQuery("");
    setStatusFilter("all");
    setCategoryFilter("all");
    setSolveFilter("all");
  }, []);

  const expandReportAncestors = useCallback(
    (folderId: string | null | undefined, folderList: ReportFolder[]) => {
      if (!folderId) return;
      const ids = folderPath(folderId, foldersById(folderList)).map((f) => f.id);
      if (!ids.length) return;
      setExpanded((prev) => {
        const next = new Set([...prev, ...ids]);
        saveExpanded(workspaceMode, next);
        return next;
      });
    },
    [workspaceMode],
  );

  const replaceFocusWithReport = useCallback(
    (reportId?: string) => {
      if (typeof window === "undefined") return;
      const params = new URLSearchParams(window.location.search);
      const hadFocus = params.has("focusRun");
      params.delete("focusRun");
      if (reportId) params.set("report", reportId);
      else if (!hadFocus) return;
      const qs = params.toString();
      const targetUrl = `${window.location.pathname}${qs ? `?${qs}` : ""}`;
      if (`${window.location.pathname}${window.location.search}` !== targetUrl) {
        router.replace(targetUrl);
      }
    },
    [router],
  );

  const runsRef = useRef(runs);
  runsRef.current = runs;
  const activeRef = useRef(active);
  activeRef.current = active;
  const latestTargetRef = useRef({ requestedReportId, focusRun });
  latestTargetRef.current = { requestedReportId, focusRun };
  const appliedTargetRef = useRef("");
  const lastFetchedTargetRef = useRef("");
  const focusImportingRef = useRef<string | null>(null);
  const loadSeqRef = useRef(0);
  const loadInFlightRef = useRef(false);
  const wasActiveRef = useRef(active);

  const load = useCallback(async (keepSelection = true) => {
    const seq = ++loadSeqRef.current;
    loadInFlightRef.current = true;
    setLoadError("");
    try {
      const data = await listReports(workspaceMode);
      if (seq !== loadSeqRef.current) return;
      setReports(data.reports);
      setRunSummaries(data.runs);
      setFolders(data.folders);

      const { requestedReportId: wantedReport, focusRun: wantedRun } = latestTargetRef.current;
      let target: RunReport | undefined;

      if (wantedReport && appliedTargetRef.current !== `report:${wantedReport}`) {
        target = data.reports.find((r) => r.id === wantedReport);
      }
      if (!target && wantedRun && appliedTargetRef.current !== `report:${wantedReport}`) {
        const matched = pickRunReport(data.reports, wantedRun);
        if (matched) {
          target = matched;
          replaceFocusWithReport(matched.id);
        } else if (activeRef.current && focusImportingRef.current !== wantedRun) {
          focusImportingRef.current = wantedRun;
          void (async () => {
            try {
              const fetched = await fetchSourceMarkdown(workspaceMode, wantedRun);
              if (!fetched.markdown.trim()) throw new SourceMissing();
              const summary = data.runs[wantedRun];
              const railRun = runsRef.current.find((r) => r.run_id === wantedRun);
              const title = summary?.name || railRun?.name || wantedRun;
              const created = await createReport({
                mode: workspaceMode,
                run_id: wantedRun,
                title,
                markdown: fetched.markdown,
                source: { kind: workspaceMode === "pentest" ? "pentest_report" : "ai_writeup", ref: fetched.ref },
                folder_id: null,
              });
              setReports((prev) => (prev.some((r) => r.id === created.id) ? prev : [created, ...prev]));
              appliedTargetRef.current = `report:${created.id}`;
              clearFilters();
              setSelectedId(created.id);
              replaceFocusWithReport(created.id);
            } catch (error) {
              replaceFocusWithReport();
              if (error instanceof SourceMissing) {
                toast({ tone: "warning", title: "该题目尚未生成 Writeup" });
              } else {
                toast({ tone: "danger", title: reportErrorText(t, error) });
              }
            } finally {
              focusImportingRef.current = null;
            }
          })();
        }
      }

      if (target) {
        appliedTargetRef.current = `report:${target.id}`;
        expandReportAncestors(target.folder_id, data.folders);
        clearFilters();
        setSelectedId(target.id);
        return;
      }

      setSelectedId((prev) => {
        if (keepSelection && prev && data.reports.some((r) => r.id === prev)) return prev;
        if (wantedReport && data.reports.some((r) => r.id === wantedReport)) return wantedReport;
        return data.reports[0]?.id ?? "";
      });
    } catch (error) {
      if (seq === loadSeqRef.current) setLoadError(errorText(error));
    } finally {
      if (seq === loadSeqRef.current) {
        loadInFlightRef.current = false;
        setLoading(false);
      }
    }
  }, [workspaceMode, clearFilters, expandReportAncestors, replaceFocusWithReport, t]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    if (workspaceMode !== "ctf") return;
    void getGenerationSettings().then(
      (s) => {
        setGenSettings(s);
        const defId = s.default_template_id || s.templates[0]?.id || "builtin-standard";
        setCreateTemplateId(defId);
      },
      () => {},
    );
  }, [workspaceMode]);

  const anyActive = reports.some(generationActive);
  useEffect(() => {
    if (!anyActive) return;
    const refresh = () => {
      if (!document.hidden) void load();
    };
    const interval = window.setInterval(refresh, 3000);
    const onFocus = () => void load();
    const onVis = () => {
      if (!document.hidden) void load();
    };
    window.addEventListener("focus", onFocus);
    document.addEventListener("visibilitychange", onVis);
    return () => {
      window.clearInterval(interval);
      window.removeEventListener("focus", onFocus);
      document.removeEventListener("visibilitychange", onVis);
    };
  }, [anyActive, load]);

  const foldersByIdMap = useMemo(() => foldersById(folders), [folders]);
  const toggleExpand = useCallback(
    (id: string) => {
      setExpanded((prev) => {
        const next = new Set(prev);
        if (next.has(id)) next.delete(id);
        else next.add(id);
        saveExpanded(workspaceMode, next);
        return next;
      });
    },
    [workspaceMode],
  );
  const expandPath = useCallback(
    (folderId: string | null | undefined) => {
      if (!folderId) return;
      const ids = folderPath(folderId, foldersByIdMap).map((f) => f.id);
      setExpanded((prev) => {
        const next = new Set([...prev, ...ids]);
        saveExpanded(workspaceMode, next);
        return next;
      });
    },
    [workspaceMode, foldersByIdMap],
  );
  const categories = useMemo(() => {
    const set = new Set<string>();
    for (const report of reports) {
      const category = runCategory(report.run_id);
      if (category) set.add(category);
    }
    return [...set].sort();
  }, [reports, runCategory]);

  const activeFilterCount = Number(statusFilter !== "all") + Number(categoryFilter !== "all") + Number(solveFilter !== "all");
  const filtersActive = Boolean(query.trim()) || activeFilterCount > 0;
  const listFiltered = filtersActive || Boolean(runFilter);

  const filtered = useMemo(() => {
    const needle = query.trim().toLowerCase();
    return reports.filter((report) => {
      if (runFilter && report.run_id !== runFilter) return false;
      if (statusFilter !== "all" && report.status !== statusFilter) return false;
      if (categoryFilter !== "all" && runCategory(report.run_id) !== categoryFilter) return false;
      if (solveFilter !== "all" && (runSolved(report.run_id) ? "solved" : "unsolved") !== solveFilter) return false;
      if (needle) {
        const folderPathStr = report.folder_id
          ? folderPath(report.folder_id, foldersByIdMap).map((f) => f.name).join("/")
          : "";
        const hay = `${report.title}\n${searchableMarkdown(report.markdown)}\n${runName(report.run_id)}\n${report.tags.join(" ")}\n${folderPathStr}`.toLowerCase();
        if (!hay.includes(needle)) return false;
      }
      return true;
    });
  }, [reports, query, statusFilter, categoryFilter, solveFilter, runFilter, runCategory, runName, runSolved, foldersByIdMap]);

  const selected = reports.find((r) => r.id === selectedId) ?? null;

  const [bodyPristine, setBodyPristine] = useState(true);
  useEffect(() => {
    let dead = false;
    if (!selected || !selected.markdown.trim()) {
      setBodyPristine(true);
      return;
    }
    if (!selected.ai_sha256) {
      setBodyPristine(false);
      return;
    }
    void crypto.subtle.digest("SHA-256", new TextEncoder().encode(selected.markdown)).then((buf) => {
      const hex = Array.from(new Uint8Array(buf), (b) => b.toString(16).padStart(2, "0")).join("");
      if (!dead) setBodyPristine(hex === selected.ai_sha256);
    });
    return () => {
      dead = true;
    };
  }, [selected?.id, selected?.revision]); // eslint-disable-line react-hooks/exhaustive-deps
  const keepHint = Boolean(selected && selected.markdown.trim() && !bodyPristine);

  const editSession = useRef(0);
  const currentSelection = useRef(selectedId);
  currentSelection.current = selectedId;
  const latest = useRef({ editing: false, editBody: "" });
  latest.current = { editing, editBody };
  const selectedRun: ReportRunSummary | undefined = useMemo(() => {
    if (!selected || !selected.run_id) return undefined;
    if (runSummaries[selected.run_id]) return runSummaries[selected.run_id];
    const run = runs.find((r) => r.run_id === selected.run_id);
    return run ? { name: run.name, category: run.category, mode: run.mode, status: run.status, solved: run.solved, finished: run.finished, flag: run.flag, folder_id: run.folder_id } : undefined;
  }, [selected, runSummaries, runs]);
  const selectedParts = useMemo(
    () => splitReportAssets(selected?.markdown ?? ""),
    [selected?.markdown],
  );
  const readBody = useMemo(() => stripLeadingTitle(selectedParts.body, selected?.title ?? ""), [selectedParts, selected?.title]);
  const viewText = useMemo(() => inlineReportAssets(readBody, selectedParts.assets), [readBody, selectedParts]);
  const savedEditBody = useMemo(
    () => editing ? joinReportAssets(selectedParts.body, selectedParts.assets) : "",
    [editing, selectedParts],
  );

  const dirty = useMemo(() => {
    if (!editing || !selected) return false;
    return editTitle !== selected.title
      || editTags.join("\u0000") !== selected.tags.join("\u0000")
      || joinReportAssets(editBody, editAssets) !== savedEditBody;
  }, [editing, selected, savedEditBody, editTitle, editBody, editAssets, editTags]);
  const editStats = useMemo(() => ({
    words: editing ? countWords(editBody) : 0,
  }), [editing, editBody]);

  const guard = useCallback((action: () => void) => {
    if (dirty) setDiscardAction(() => action);
    else action();
  }, [dirty]);

  useEffect(() => {
    if (!dirty) return;
    const warn = (event: BeforeUnloadEvent) => { event.preventDefault(); };
    window.addEventListener("beforeunload", warn);
    return () => window.removeEventListener("beforeunload", warn);
  }, [dirty]);

  const editBaseRevision = useRef(0);
  const startEdit = useCallback((report: RunReport) => {
    const parts = splitReportAssets(report.markdown);
    setEditTitle(report.title);
    setEditTags(report.tags);
    editSession.current += 1;
    editBaseRevision.current = report.revision;
    setEditBody(parts.body);
    setEditAssets(parts.assets);
    setEditView(parts.body.trim() ? "split" : "edit");
    setActionError("");
    setConflict(false);
    setServerChanged(false);
    setEditing(true);
  }, []);

  useEffect(() => {
    if (!editing || !selected) {
      setServerChanged(false);
      return;
    }
    if (selected.revision === editBaseRevision.current) return;
    if (dirty) {
      setServerChanged(true);
      return;
    }
    const parts = splitReportAssets(selected.markdown);
    editSession.current += 1;
    editBaseRevision.current = selected.revision;
    setEditTitle(selected.title);
    setEditTags(selected.tags);
    setEditBody(parts.body);
    setEditAssets(parts.assets);
    setServerChanged(false);
  }, [selected, editing, dirty]);

  const previousSelection = useRef<string | null>(null);
  useEffect(() => {
    if (previousSelection.current === selectedId) return;
    previousSelection.current = selectedId;
    editSession.current += 1;
    setEditing(false);
    setActionError("");
    setConflict(false);
    if (pendingEdit.current && pendingEdit.current === selectedId) {
      const report = reports.find((r) => r.id === selectedId);
      pendingEdit.current = "";
      const focusTitle = pendingFocusTitle.current;
      pendingFocusTitle.current = false;
      if (report) {
        startEdit(report);
        if (focusTitle) {
          window.requestAnimationFrame(() => {
            titleInputRef.current?.focus();
            titleInputRef.current?.select();
          });
        }
      }
    }
    // Only a selection change resets editing; report list refreshes must not.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedId]);

  const selectReport = useCallback((id: string) => {
    if (id === currentSelection.current) return;
    guard(() => setSelectedId(id));
  }, [guard]);

  useEffect(() => {
    const becameActive = !wasActiveRef.current && active;
    wasActiveRef.current = active;
    if (!active) return;
    if (becameActive) {
      if (requestedReportId || focusRun) {
        appliedTargetRef.current = "";
        lastFetchedTargetRef.current = "";
      }
      void load(true);
      return;
    }
    if (loading || loadInFlightRef.current) return;
    if (requestedReportId && appliedTargetRef.current !== `report:${requestedReportId}`) {
      const found = reports.find((r) => r.id === requestedReportId);
      if (found) {
        appliedTargetRef.current = `report:${found.id}`;
        expandReportAncestors(found.folder_id, folders);
        clearFilters();
        selectReport(found.id);
      } else if (lastFetchedTargetRef.current !== `report:${requestedReportId}`) {
        lastFetchedTargetRef.current = `report:${requestedReportId}`;
        void load(true);
      }
      return;
    }
    if (focusRun && appliedTargetRef.current !== `focusRun:${focusRun}`) {
      const found = pickRunReport(reports, focusRun);
      if (found) {
        appliedTargetRef.current = `report:${found.id}`;
        expandReportAncestors(found.folder_id, folders);
        clearFilters();
        selectReport(found.id);
        replaceFocusWithReport(found.id);
      } else if (lastFetchedTargetRef.current !== `focusRun:${focusRun}`) {
        lastFetchedTargetRef.current = `focusRun:${focusRun}`;
        void load(true);
      }
    }
  }, [active, loading, requestedReportId, focusRun, reports, folders, load, expandReportAncestors, clearFilters, selectReport, replaceFocusWithReport]);

  const editReport = (report: RunReport) => {
    if (report.id === selectedId) {
      if (!editing) startEdit(report);
      return;
    }
    guard(() => {
      pendingEdit.current = report.id;
      setSelectedId(report.id);
    });
  };

  const applyUpdate = async (patch: { title?: string; markdown?: string; status?: "draft" | "final"; tags?: string[] }) => {
    if (!selected) return;
    setBusy(true);
    setActionError("");
    try {
      const updated = await updateReport(selected.id, {
        ...patch,
        expected_revision: selected.revision,
      });
      setReports((prev) => prev.map((r) => (r.id === updated.id ? updated : r)));
      setEditing(false);
      setConflict(false);
      toast({ tone: "success", title: t("reports.savedToast") });
    } catch (error) {
      if (error instanceof ReportError && error.code === "run.report.revision_conflict") {
        setConflict(true);
        setActionError(t("reports.conflict"));
        if (error.report) {
          setReports((prev) => prev.map((r) => (r.id === error.report!.id ? error.report! : r)));
        }
      } else {
        setActionError(errorText(error));
      }
    } finally {
      setBusy(false);
    }
  };

  const saveEdit = () => {
    if (busy) return;
    const markdown = joinReportAssets(editBody, editAssets);
    const size = reportByteSize(markdown);
    if (size > MAX_REPORT_BYTES) {
      setActionError(t("reports.tooLarge", { size: `${(size / 1e6).toFixed(1)} MB`, max: `${(MAX_REPORT_BYTES / 1e6).toFixed(1)} MB` }));
      return;
    }
    void applyUpdate({ title: editTitle, markdown, tags: editTags });
  };

  const cancelEdit = () => guard(() => { setEditing(false); setActionError(""); setConflict(false); });

  const applyMarkdown = (markdown: string) => {
    const parts = splitReportAssets(markdown);
    setEditBody(parts.body);
    setEditAssets(parts.assets);
  };

  const fillFromSource = async () => {
    if (!selected?.run_id) return;
    const session = editSession.current;
    const reportId = selected.id;
    setAiConfirm(null);
    setBusy(true);
    setActionError("");
    try {
      const source = await fetchSourceMarkdown(workspaceMode, selected.run_id);
      if (currentSelection.current === reportId && editSession.current === session) applyMarkdown(source.markdown);
    } catch (error) {
      setActionError(error instanceof SourceMissing ? t("reports.ai.noneYet") : errorText(error));
    } finally {
      setBusy(false);
    }
  };

  const requestGenerate = async (report: RunReport, templateId?: string) => {
    if (!report.run_id || report.mode !== "ctf" || generationActive(report)) return;
    setAiConfirm(null);
    setActionError("");
    try {
      const { report: updated } = await generateReport(report.id, templateId ? { templateId } : undefined);
      setReports((prev) => prev.map((r) => (r.id === updated.id ? updated : r)));
      void load();
    } catch (error) {
      const info = reportErrorInfo(error);
      if (info?.soft) {
        void load();
        return;
      }
      toast({ tone: "danger", title: reportErrorText(t, error), description: info?.detail });
    }
  };

  const renameReportMeta = async (report: RunReport, title: string) => {
    const trimmed = title.trim();
    if (!trimmed || trimmed === report.title) return;
    try {
      const updated = await updateReport(report.id, { title: trimmed, expected_revision: report.revision });
      setReports((prev) => prev.map((r) => (r.id === updated.id ? updated : r)));
      if (editing && selectedId === updated.id) setEditTitle(updated.title);
      toast({ tone: "success", title: t("reports.savedToast") });
    } catch (error) {
      toast({ tone: "danger", title: reportErrorText(t, error) });
      void load();
    }
  };

  const toggleReportStatus = async (report: RunReport) => {
    const nextStatus = report.status === "final" ? "draft" : "final";
    try {
      const updated = await updateReport(report.id, { status: nextStatus, expected_revision: report.revision });
      setReports((prev) => prev.map((r) => (r.id === updated.id ? updated : r)));
      toast({ tone: "success", title: t("reports.savedToast") });
    } catch (error) {
      toast({ tone: "danger", title: reportErrorText(t, error) });
      void load();
    }
  };

  const copyReportMarkdown = (report: RunReport) => {
    void navigator.clipboard.writeText(report.markdown).then(
      () => toast({ tone: "success", title: t("reports.copied") }),
      (error) => toast({ tone: "danger", title: errorText(error) }),
    );
  };

  const downloadReportMarkdown = (report: RunReport) => {
    downloadText(`${report.title || report.id}.md`, report.markdown);
  };

  const moveListReportToFolder = async (report: RunReport, folderId: string | null) => {
    try {
      const updated = await updateReport(report.id, { folder_id: folderId });
      setReports((prev) => prev.map((r) => (r.id === updated.id ? updated : r)));
      if (folderId) expandPath(folderId);
      toast({ tone: "success", title: t("reports.savedToast") });
    } catch (error) {
      toast({ tone: "danger", title: reportErrorText(t, error) });
      void load();
    }
  };

  const expandAllFolders = () => {
    const next = new Set(folders.map((f) => f.id));
    setExpanded(next);
    saveExpanded(workspaceMode, next);
  };

  const collapseAllFolders = () => {
    const next = new Set<string>();
    setExpanded(next);
    saveExpanded(workspaceMode, next);
  };

  const triggerNewFolder = () => {
    if (!filtersActive) {
      setCreateFolderNonce((n) => n + 1);
      return;
    }
    clearFilters();
    window.requestAnimationFrame(() => setCreateFolderNonce((n) => n + 1));
  };

  const revealFolder = (folderId: string) => {
    if (filtersActive) clearFilters();
    expandPath(folderId);
    window.requestAnimationFrame(() => document.querySelector(`[data-tree-id="${folderId}"]`)?.scrollIntoView({ block: "center" }));
  };

  const renderTemplateFor = (templateId: string, vars: { title?: string; category?: string; flag?: string | null }) => {
    const list = genSettings?.templates ?? [];
    const tpl = list.find((item) => item.id === templateId) ?? list[0];
    if (!tpl) return "";
    return renderWriteupTemplate(tpl.markdown, vars);
  };

  const applyTemplateNow = (tpl: WriteupTemplate) => {
    setPendingTemplate(null);
    const rendered = renderWriteupTemplate(tpl.markdown, {
      title: editTitle || selected?.title || runName(selected?.run_id ?? null),
      category: runCategory(selected?.run_id ?? null),
      flag: selectedRun?.flag ?? null,
    });
    applyMarkdown(rendered);
  };

  const handleApplyTemplate = (tpl: WriteupTemplate) => {
    if (latest.current.editBody.trim()) setPendingTemplate(tpl);
    else applyTemplateNow(tpl);
  };

  const dismissGeneration = async (report: RunReport) => {
    setBusy(true);
    try {
      const updated = await cancelReportGeneration(report.id);
      setReports((prev) => prev.map((r) => (r.id === updated.id ? updated : r)));
      void load();
    } catch (error) {
      toast({ tone: "danger", title: reportErrorText(t, error) });
    } finally {
      setBusy(false);
    }
  };

  const applyPendingUpdate = async () => {
    if (!selected?.pending_update) return;
    if (dirty) {
      setDiscardAction(() => () => void applyPendingUpdate());
      return;
    }
    setBusy(true);
    try {
      const updated = await resolvePendingUpdate(selected.id, "apply", selected.revision);
      setReports((prev) => prev.map((r) => (r.id === updated.id ? updated : r)));
      if (editing) startEdit(updated);
      setPendingDiffOpen(false);
      toast({ tone: "success", title: t("reports.pending.applied") });
    } catch (error) {
      if (error instanceof ReportError && error.code === "run.report.revision_conflict") {
        setConflict(true);
        setActionError(t("reports.conflict"));
      } else {
        toast({ tone: "danger", title: reportErrorText(t, error) });
      }
    } finally {
      setBusy(false);
    }
  };

  const dismissPendingUpdate = async () => {
    if (!selected?.pending_update) return;
    setBusy(true);
    try {
      const updated = await resolvePendingUpdate(selected.id, "dismiss", selected.revision);
      setReports((prev) => prev.map((r) => (r.id === updated.id ? updated : r)));
      setPendingDiffOpen(false);
    } catch (error) {
      toast({ tone: "danger", title: reportErrorText(t, error) });
    } finally {
      setBusy(false);
    }
  };

  const requestAi = (kind: "use" | "generate") => {
    if (kind === "generate") {
      if (selected) void requestGenerate(selected);
      return;
    }
    if (latest.current.editBody.trim()) setAiConfirm(kind);
    else void fillFromSource();
  };

  const confirmDelete = async () => {
    if (!deleteTarget || deleteBusy) return;
    setDeleteBusy(true);
    try {
      await deleteReport(deleteTarget.id);
      const idx = reports.findIndex((r) => r.id === deleteTarget.id);
      const remaining = reports.filter((r) => r.id !== deleteTarget.id);
      setReports(remaining);
      if (selectedId === deleteTarget.id) {
        const nextId = remaining[Math.min(Math.max(0, idx), remaining.length - 1)]?.id ?? "";
        setSelectedId(nextId);
      }
      toast({ tone: "success", title: t("reports.deletedToast") });
    } catch (error) {
      setActionError(errorText(error));
    } finally {
      setDeleteBusy(false);
      setDeleteTarget(null);
    }
  };

  const exportFiltered = () => {
    const body = filtered
      .map((report) => `# ${report.title}\n\n- ${t("reports.create.run")}: ${runName(report.run_id)} (${report.run_id})\n- ${t("reports.filter.status")}: ${report.status}\n- ${t("reports.filter.category")}: ${runCategory(report.run_id) || "—"}\n- ${t("reports.tags")}: ${report.tags.join(", ") || "—"}\n\n${report.markdown}`)
      .join("\n\n---\n\n");
    downloadText(`reports-${workspaceMode}-${new Date().toISOString().slice(0, 10)}.md`, body);
  };

  const openCreate = (folderId: string | null = null) => {
    const first = eligibleRuns.find((r) => r.run_id === runFilter) ?? eligibleRuns[0];
    setCreateRunId(first?.run_id ?? "");
    setCreateTitle(first?.name ?? "");
    setCreateFrom("source");
    setCreateTemplateId(genSettings?.default_template_id || genSettings?.templates[0]?.id || "builtin-standard");
    setCreateError("");
    setCreateFolderId(folderId);
    setCreateOpen(true);
  };

  const quickCreateBlank = (folderId: string | null) => {
    if (createBusy) return;
    guard(() => {
      void (async () => {
        setCreateBusy(true);
        try {
          const report = await createReport({
            mode: workspaceMode,
            title: t("reports.standalone.untitled"),
            markdown: "",
            source: { kind: "manual", ref: null },
            folder_id: folderId,
          });
          setReports((prev) => [report, ...prev]);
          expandPath(folderId);
          if (filtersActive) clearFilters();
          pendingEdit.current = report.id;
          pendingFocusTitle.current = true;
          setSelectedId(report.id);
        } catch (error) {
          toast({ tone: "danger", title: reportErrorText(t, error) });
        } finally {
          setCreateBusy(false);
        }
      })();
    });
  };

  const quickCreateFromRun = (run: RunSummary, autoGenerate: boolean) => {
    if (createBusy) return;
    guard(() => {
      void (async () => {
        setCreateBusy(true);
        try {
          const { report, missing } = await makeReport(run.run_id, run.name, "source", false, null);
          if (filtersActive) clearFilters();
          setSelectedId(report.id);
          if (missing && autoGenerate && workspaceMode === "ctf") {
            void requestGenerate(report);
          }
        } catch (error) {
          toast({ tone: "danger", title: reportErrorText(t, error) });
        } finally {
          setCreateBusy(false);
        }
      })();
    });
  };

  const makeReport = async (
    runId: string,
    title: string,
    from: "blank" | "template" | "source",
    soft: boolean,
    folderId?: string | null,
    templateId?: string,
  ) => {
    let markdown = "";
    let warning = "";
    let missing = false;
    let source: { kind: "manual" | "ai_writeup" | "pentest_report"; ref: string | null } = { kind: "manual", ref: null };
    if (from === "template") {
      const summary = runSummaries[runId];
      const run = runs.find((r) => r.run_id === runId);
      markdown = renderTemplateFor(templateId || createTemplateId, {
        title: title.trim() || runName(runId),
        category: summary?.category || run?.category || "",
        flag: summary?.flag ?? run?.flag ?? null,
      });
    } else if (from === "source") {
      try {
        const fetched = await fetchSourceMarkdown(workspaceMode, runId);
        markdown = fetched.markdown;
        if (markdown.trim()) source = { kind: workspaceMode === "pentest" ? "pentest_report" : "ai_writeup", ref: fetched.ref };
      } catch (error) {
        if (error instanceof SourceMissing) missing = true;
        else if (!soft) throw error;
        else warning = errorText(error);
      }
    }
    const report = await createReport({ run_id: runId, title: title.trim() || runName(runId), markdown, source, folder_id: folderId ?? null });
    setReports((prev) => [report, ...prev]);
    return { report, warning, missing };
  };

  const submitCreate = async () => {
    if (!createRunId || createBusy) return;
    setCreateBusy(true);
    setCreateError("");
    try {
      const { report, missing } = await makeReport(createRunId, createTitle, createFrom, false, createFolderId, createTemplateId);
      setCreateOpen(false);
      if (createFrom === "template" && !missing) pendingEdit.current = report.id;
      expandPath(createFolderId);
      selectReport(report.id);
      if (missing) void requestGenerate(report, createTemplateId);
    } catch (error) {
      setCreateError(errorText(error));
    } finally {
      setCreateBusy(false);
    }
  };

  const dropRun = async (runId: string, folderId?: string | null) => {
    setDragActive(false);
    setDragHover(false);
    setListDragHover(false);
    const run = runs.find((r) => r.run_id === runId);
    if (!run) return toast({ tone: "danger", title: t("reports.drop.unknownRun") });
    if ((run.mode ?? "ctf") !== workspaceMode) return toast({ tone: "warning", title: t("reports.drop.wrongMode") });
    if (run.status === "draft") return toast({ tone: "warning", title: t("reports.drop.draftRun") });
    setDropBusy(true);
    try {
      const { report, warning } = await makeReport(runId, run.name, "source", true, folderId);
      if (folderId) expandPath(folderId);
      if (runFilter && runFilter !== runId) clearRunFilter();
      setQuery("");
      setStatusFilter("all");
      setCategoryFilter("all");
      setSolveFilter("all");
      selectReport(report.id);
      if (warning) toast({ tone: "warning", title: t("reports.drop.blankWarn", { reason: warning }), duration: 7000 });
      else toast({ tone: "success", title: t("reports.drop.created", { name: run.name }) });
    } catch (error) {
      toast({ tone: "danger", title: t("reports.drop.failed"), description: errorText(error) });
    } finally {
      setDropBusy(false);
    }
  };
  const dropRunRef = useRef(dropRun);
  dropRunRef.current = dropRun;

  useEffect(() => {
    const reset = () => {
      setDragActive(false);
      setDragHover(false);
      setListDragHover(false);
    };
    const startOrEnter = (event: globalThis.DragEvent) => {
      if (hasRunDrag(event.dataTransfer)) setDragActive(true);
    };
    const unsubPointerDrag = subscribeRunPointerDrag((ev) => {
      if (ev.phase === "start") {
        setDragActive(true);
        return;
      }
      if (ev.phase === "cancel") {
        reset();
        return;
      }
      if (ev.phase === "move") {
        const hit = document.elementFromPoint(ev.x, ev.y);
        setListDragHover(Boolean(hit && listPaneRef.current?.contains(hit)));
        setDragHover(Boolean(hit && detailPaneRef.current?.contains(hit)));
        return;
      }
      if (ev.phase === "drop") {
        reset();
        const hit = document.elementFromPoint(ev.x, ev.y);
        if (
          hit
          && (listPaneRef.current?.contains(hit) || detailPaneRef.current?.contains(hit))
          && !hit.closest("[data-report-drop-tree]")
        ) {
          void dropRunRef.current(ev.runId, null);
          return true;
        }
      }
    });
    window.addEventListener("dragstart", startOrEnter);
    window.addEventListener("dragenter", startOrEnter, true);
    window.addEventListener("dragend", reset, true);
    window.addEventListener("drop", reset, true);
    return () => {
      unsubPointerDrag();
      window.removeEventListener("dragstart", startOrEnter);
      window.removeEventListener("dragenter", startOrEnter, true);
      window.removeEventListener("dragend", reset, true);
      window.removeEventListener("drop", reset, true);
    };
  }, []);

  const dropProps = {
    onDragOver: (event: DragEvent<HTMLElement>) => {
      if (!hasRunDrag(event.dataTransfer)) return;
      event.preventDefault();
      event.dataTransfer.dropEffect = "copy";
    },
    onDragLeave: (event: DragEvent<HTMLElement>) => {
      if (!event.currentTarget.contains(event.relatedTarget as Node | null)) {
        setDragHover(false);
        setListDragHover(false);
      }
    },
    onDrop: (event: DragEvent<HTMLElement>) => {
      if (!hasRunDrag(event.dataTransfer)) return;
      event.preventDefault();
      setDragActive(false);
      setDragHover(false);
      setListDragHover(false);
      const runId = readRunDragId(event.dataTransfer);
      endRunDrag();
      if (runId) void dropRun(runId);
    },
  };

  const clearRunFilter = () => {
    const params = new URLSearchParams(searchParams.toString());
    params.delete("run");
    const qs = params.toString();
    router.replace(`${window.location.pathname}${qs ? `?${qs}` : ""}`);
  };

  const selectMode = checkedIds.size > 0;
  const checkedReports = useMemo(() => reports.filter((r) => checkedIds.has(r.id)), [reports, checkedIds]);
  const checkReports = useCallback((ids: string[], checked: boolean) => {
    setCheckedIds((prev) => {
      const next = new Set(prev);
      for (const id of ids) {
        if (checked) next.add(id);
        else next.delete(id);
      }
      return next;
    });
  }, []);
  const bulkGenerate = () => {
    for (const r of checkedReports) {
      if (r.run_id && r.mode === "ctf" && !generationActive(r)) void requestGenerate(r);
    }
  };
  const bulkStatus = async (status: "draft" | "final") => {
    const failures: string[] = [];
    for (const r of checkedReports) {
      if (r.status === status) continue;
      try {
        const updated = await updateReport(r.id, { status, expected_revision: r.revision });
        setReports((prev) => prev.map((x) => (x.id === updated.id ? updated : x)));
      } catch (error) {
        failures.push(`${r.title}: ${reportErrorText(t, error)}`);
      }
    }
    if (failures.length) toast({ tone: "warning", title: t("reports.bulk.partialFail"), description: failures.join(" · "), duration: 8000 });
    else toast({ tone: "success", title: t("reports.savedToast") });
  };
  const bulkDeleteConfirm = async () => {
    const targets = checkedReports;
    setBulkDelete(false);
    const failures: string[] = [];
    for (const r of targets) {
      try {
        await deleteReport(r.id);
      } catch (error) {
        failures.push(`${r.title}: ${reportErrorText(t, error)}`);
      }
    }
    setCheckedIds(new Set());
    if (targets.some((r) => r.id === selectedId)) setSelectedId("");
    if (failures.length) toast({ tone: "warning", title: t("reports.bulk.partialFail"), description: failures.join(" · "), duration: 8000 });
    else toast({ tone: "success", title: t("reports.deletedToast") });
    void load();
  };

  const genCounts = useMemo(() => {
    let running = 0;
    let queued = 0;
    let failed = 0;
    for (const r of reports) {
      const s = r.generation?.state;
      if (s === "running") running++;
      else if (s === "queued") queued++;
      else if (s === "failed") failed++;
    }
    return { running, queued, failed };
  }, [reports]);
  const retryFailed = () => {
    for (const r of reports) if (r.generation?.state === "failed") void requestGenerate(r);
  };
  const cancelActiveGenerations = () => {
    for (const r of reports) if (generationActive(r)) void dismissGeneration(r);
  };

  const organizePlan = useMemo(() => {
    if (!organizeOpen) return { folders: [] as string[], moves: [] as { report: RunReport; folderName: string }[] };
    const names = new Set<string>();
    const moves: { report: RunReport; folderName: string }[] = [];
    for (const report of reports) {
      if (!report.run_id || report.folder_id) continue;
      const run = runs.find((r) => r.run_id === report.run_id);
      const cf = run?.folder_id ? challengeFolders.find((f) => f.id === run.folder_id) : undefined;
      if (!cf) continue;
      names.add(cf.name);
      moves.push({ report, folderName: cf.name });
    }
    return { folders: [...names], moves };
  }, [organizeOpen, reports, runs, challengeFolders]);

  const openOrganize = async () => {
    try {
      const res = await apiFetch(`/api/folders`);
      const data = await res.json().catch(() => ({}));
      setChallengeFolders(data.folders ?? []);
    } catch {
      setChallengeFolders([]);
    }
    setOrganizeOpen(true);
  };

  const runOrganize = async () => {
    if (organizeBusy) return;
    setOrganizeBusy(true);
    try {
      const current = [...folders];
      const byName = new Map(current.filter((f) => !f.parent_id).map((f) => [f.name, f]));
      const folderForName = new Map<string, string>();
      for (const name of organizePlan.folders) {
        let f = byName.get(name);
        if (!f) {
          f = await createReportFolder({ mode: workspaceMode, name });
          current.push(f);
        }
        folderForName.set(name, f.id);
      }
      let moved = 0;
      for (const { report, folderName } of organizePlan.moves) {
        const fid = folderForName.get(folderName);
        if (!fid) continue;
        try {
          await updateReport(report.id, { folder_id: fid });
          moved++;
        } catch {
        }
      }
      toast({ tone: "success", title: t("reports.organize.done", { folders: folderForName.size, moved }) });
      setOrganizeOpen(false);
      void load();
    } catch (error) {
      toast({ tone: "danger", title: reportErrorText(t, error) });
    } finally {
      setOrganizeBusy(false);
    }
  };

  const isFileDrag = (event: { dataTransfer: DataTransfer | null }) => {
    const types = Array.from(event.dataTransfer?.types ?? []);
    return types.includes("Files") && !hasRunDrag(event.dataTransfer) && !hasReportNodeDrag(event.dataTransfer);
  };
  const fileDropProps = {
    onDragOver: (event: DragEvent<HTMLElement>) => {
      if (event.defaultPrevented || !isFileDrag(event)) return;
      event.preventDefault();
      event.stopPropagation();
      setFileDropActive(true);
    },
    onDragLeave: (event: DragEvent<HTMLElement>) => {
      if (!event.currentTarget.contains(event.relatedTarget as Node | null)) setFileDropActive(false);
    },
    onDrop: async (event: DragEvent<HTMLElement>) => {
      if (event.defaultPrevented || !isFileDrag(event)) return;
      event.preventDefault();
      event.stopPropagation();
      setFileDropActive(false);
      const files: { file: File; path: string }[] = [];
      const entries: FileSystemEntry[] = [];
      for (const item of Array.from(event.dataTransfer.items)) {
        const entry = item.webkitGetAsEntry?.();
        if (entry) entries.push(entry);
        else {
          const f = item.getAsFile();
          if (f) files.push({ file: f, path: f.name });
        }
      }
      setImportSeed({ files, entries });
      setImportOpen(true);
    },
  };
  const surfaceDropProps = {
    onDragOver: (event: DragEvent<HTMLElement>) => {
      if (event.defaultPrevented) return;
      dropProps.onDragOver(event);
      fileDropProps.onDragOver(event);
    },
    onDragLeave: (event: DragEvent<HTMLElement>) => {
      dropProps.onDragLeave(event);
      fileDropProps.onDragLeave(event);
    },
    onDrop: (event: DragEvent<HTMLElement>) => {
      if (event.defaultPrevented) {
        setDragActive(false);
        setDragHover(false);
        setListDragHover(false);
        setFileDropActive(false);
        return;
      }
      if (hasRunDrag(event.dataTransfer)) dropProps.onDrop(event);
      else if (isFileDrag(event)) void fileDropProps.onDrop(event);
    },
  };

  const solvedLabel = workspaceMode === "pentest" ? t("reports.provenPill") : t("reports.solvedPill");
  const statusLabels: Record<StatusFilter, string> = {
    all: t("reports.filter.all"),
    draft: t("reports.filter.draft"),
    final: t("reports.filter.final"),
  };
  const solveLabels: Record<SolveFilter, string> = {
    all: t("reports.filter.all"),
    solved: t("reports.filter.solved"),
    unsolved: t("reports.filter.unsolved"),
  };
  const statusOptions: ListOption<StatusFilter>[] = (["all", "draft", "final"] as StatusFilter[]).map((value) => ({ value, label: statusLabels[value] }));
  const solveOptions: ListOption<SolveFilter>[] = (["all", "solved", "unsolved"] as SolveFilter[]).map((value) => ({ value, label: solveLabels[value] }));
  const categoryOptions: ListOption<string>[] = [
    { value: "all", label: t("reports.filter.all") },
    ...categories.map((c) => ({ value: c, label: c })),
  ];
  const runOptions: ListOption<string>[] = eligibleRuns.map((r) => ({ value: r.run_id, label: r.name, description: r.run_id }));
  const sourceLabel = workspaceMode === "pentest" ? t("reports.create.fromPentest") : t("reports.create.fromAi");
  const folderSelectOptions: ListOption<string>[] = useMemo(
    () => [
      { value: "__root__", label: t("reports.import.root") },
      ...folders.map((f) => ({ value: f.id, label: pathLabel(folderPath(f.id, foldersByIdMap), t("reports.import.root")) })),
    ],
    [folders, foldersByIdMap, t],
  );

  return (
    <div
      {...surfaceDropProps}
      className="relative flex h-full min-h-0 w-full min-w-0 flex-1 flex-col overflow-hidden bg-cx-bg"
      aria-label={t("reports.title")}
    >
      <div
        ref={gridRef}
        style={{ "--reports-list-width": `${listWidth}px` } as CSSProperties}
        className="reports-split-grid grid min-h-0 flex-1 grid-cols-1 grid-rows-[minmax(0,2fr)_minmax(0,3fr)]"
      >
        <aside
          ref={listPaneRef}
          data-report-drop-list="true"
          onDragEnter={(event) => {
            if (hasRunDrag(event.dataTransfer)) setListDragHover(true);
          }}
          onDragOver={(event) => {
            if (!hasRunDrag(event.dataTransfer)) return;
            event.preventDefault();
            event.dataTransfer.dropEffect = "copy";
            if (!listDragHover) setListDragHover(true);
          }}
          onDragLeave={(event) => {
            if (!event.currentTarget.contains(event.relatedTarget as Node | null)) setListDragHover(false);
          }}
          onDrop={() => setListDragHover(false)}
          className={cn(
            "reports-list-pane flex min-h-0 flex-col border-b border-cx-border-subtle bg-cx-bg transition-shadow lg:border-b-0",
            listDragHover ? "ring-2 ring-inset ring-cx-accent" : dragActive && "ring-1 ring-inset ring-cx-accent-line",
          )}
          aria-label={t("reports.title")}
        >
          <div className="flex h-[var(--cx-header-h,52px)] shrink-0 items-center gap-1.5 px-3.5">
            {selectMode ? (
              <div className="flex min-w-0 flex-1 items-center gap-1" role="toolbar" aria-label={t("reports.selected", { n: checkedIds.size })}>
                <span className="min-w-0 flex-1 truncate text-cx-xs font-medium text-cx-fg">{t("reports.selected", { n: checkedIds.size })}</span>
                {workspaceMode === "ctf" && (
                  <Button size="xs" variant="ghost" onClick={bulkGenerate} disabled={!checkedReports.some((r) => r.run_id && r.mode === "ctf" && !generationActive(r))}>{t("reports.bulk.generate")}</Button>
                )}
                <Button size="xs" variant="ghost" onClick={() => void bulkStatus("final")}>{t("reports.bulk.final")}</Button>
                <IconButton icon="trash" label={t("reports.delete")} size="xs" onClick={() => setBulkDelete(true)} />
                <Menu ariaLabel={t("reports.more")} placement="bottom-end" trigger={<IconButton icon="more" label={t("reports.more")} size="xs" />}>
                  <MenuItem icon="pencilLine" onSelect={() => void bulkStatus("draft")}>{t("reports.markDraft")}</MenuItem>
                </Menu>
                <IconButton icon="x" label={t("reports.bulk.clearSel")} size="xs" onClick={() => setCheckedIds(new Set())} />
              </div>
            ) : (
              <>
                <h1 className="min-w-0 flex-1 truncate text-cx-base font-semibold tracking-[-0.01em] text-cx-fg">{t("reports.title")}</h1>
                <Menu
                  ariaLabel={t("reports.newShort")}
                  placement="bottom-end"
                  trigger={<Button variant="soft" size="sm" icon="plus" iconRight="chevronDown">{t("reports.newShort")}</Button>}
                >
                  <MenuItem icon="pencilLine" onSelect={() => quickCreateBlank(null)}>{t("reports.standalone.new")}</MenuItem>
                  <MenuItem icon="filePlus" disabled={!eligibleRuns.length} onSelect={() => openCreate(null)}>{t("reports.new")}</MenuItem>
                  <MenuSeparator />
                  <MenuItem icon="folderPlus" onSelect={triggerNewFolder}>{t("reports.tree.newFolder")}</MenuItem>
                </Menu>
                <Menu ariaLabel={t("reports.more")} placement="bottom-end" trigger={<IconButton icon="more" label={t("reports.more")} />}>
                  <MenuItem icon="fileDown" onSelect={() => { setImportSeed({ files: [], entries: [] }); setImportOpen(true); }}>{t("reports.import.button")}</MenuItem>
                  <MenuItem icon="download" disabled={!filtered.length} onSelect={exportFiltered}>
                    {listFiltered ? t("reports.exportFiltered") : t("reports.export")}
                  </MenuItem>
                  <MenuSeparator />
                  {workspaceMode === "ctf" && (
                    <MenuItem icon="layers" onSelect={() => setBatchOpen(true)}>{t("reports.batch.button")}</MenuItem>
                  )}
                  <MenuItem icon="folderTree" onSelect={() => void openOrganize()}>{t("reports.organize.button")}</MenuItem>
                  {folders.length > 0 && (
                    <>
                      <MenuItem icon="unfoldVertical" onSelect={expandAllFolders}>{t("reports.tree.expandAll")}</MenuItem>
                      <MenuItem icon="foldVertical" onSelect={collapseAllFolders}>{t("reports.tree.collapseAll")}</MenuItem>
                    </>
                  )}
                  {workspaceMode === "ctf" && (
                    <>
                      <MenuSeparator />
                      <MenuItem icon="sliders" onSelect={() => { setSettingsTab("templates"); setSettingsOpen(true); }}>{t("reports.settings")}</MenuItem>
                    </>
                  )}
                </Menu>
              </>
            )}
          </div>

          {(reports.length > 0 || listFiltered) && (
            <div className="flex shrink-0 items-center gap-1.5 px-3 pb-2 pt-0.5">
              <SearchInput
                value={query}
                onValueChange={setQuery}
                placeholder={t("reports.search")}
                aria-label={t("reports.search")}
                className="cx-field-quiet min-w-0 flex-1"
              />
              <Menu
                ariaLabel={t("reports.filter.label")}
                placement="bottom-end"
                trigger={
                  <IconButton
                    icon="filter"
                    label={t("reports.filter.label")}
                    size="md"
                    badge={activeFilterCount || undefined}
                    className={cn("size-8 rounded-xl", activeFilterCount && "text-cx-accent")}
                  />
                }
              >
                <MenuLabel>{t("reports.filter.status")}</MenuLabel>
                {statusOptions.map((o) => (
                  <MenuItem key={o.value} keepOpen checked={statusFilter === o.value} onSelect={() => setStatusFilter(o.value)}>{o.label}</MenuItem>
                ))}
                {workspaceMode === "ctf" && (
                  <>
                    <MenuLabel>{t("reports.filter.solve")}</MenuLabel>
                    {solveOptions.map((o) => (
                      <MenuItem key={o.value} keepOpen checked={solveFilter === o.value} onSelect={() => setSolveFilter(o.value)}>{o.label}</MenuItem>
                    ))}
                    {categories.length > 0 && (
                      <>
                        <MenuLabel>{t("reports.filter.category")}</MenuLabel>
                        {categoryOptions.map((o) => (
                          <MenuItem key={o.value} keepOpen checked={categoryFilter === o.value} onSelect={() => setCategoryFilter(o.value)}>{o.label}</MenuItem>
                        ))}
                      </>
                    )}
                  </>
                )}
                {activeFilterCount > 0 && (
                  <>
                    <MenuSeparator />
                    <MenuItem icon="x" onSelect={clearFilters}>{t("reports.clearFilters")}</MenuItem>
                  </>
                )}
              </Menu>
            </div>
          )}

          {listFiltered && (
            <div className="flex shrink-0 flex-wrap items-center gap-1.5 px-3 pb-1.5">
              {runFilter && (
                <FilterChip
                  label={runName(runFilter)}
                  removeLabel={t("reports.filter.remove", { label: runName(runFilter) })}
                  onRemove={clearRunFilter}
                />
              )}
              {statusFilter !== "all" && (
                <FilterChip
                  label={statusLabels[statusFilter]}
                  removeLabel={t("reports.filter.remove", { label: statusLabels[statusFilter] })}
                  onRemove={() => setStatusFilter("all")}
                />
              )}
              {solveFilter !== "all" && (
                <FilterChip
                  label={solveLabels[solveFilter]}
                  removeLabel={t("reports.filter.remove", { label: solveLabels[solveFilter] })}
                  onRemove={() => setSolveFilter("all")}
                />
              )}
              {categoryFilter !== "all" && (
                <FilterChip
                  label={categoryFilter}
                  removeLabel={t("reports.filter.remove", { label: categoryFilter })}
                  onRemove={() => setCategoryFilter("all")}
                />
              )}
              <span className="ml-auto text-cx-2xs tabular-nums text-cx-fg-4" aria-live="polite">{t("reports.count", { shown: filtered.length, total: reports.length })}</span>
            </div>
          )}

          {genCounts.running + genCounts.queued + genCounts.failed > 0 && (
            <div className="mx-3 mb-1.5 flex shrink-0 flex-wrap items-center gap-x-2 gap-y-1 text-cx-xs text-cx-fg-3" role="status">
              {genCounts.running + genCounts.queued > 0 && <Icon name="loader" size={12} className="cx-spin" />}
              {genCounts.running > 0 && <span>{t("reports.strip.running", { n: genCounts.running })}</span>}
              {genCounts.queued > 0 && <span>{t("reports.strip.queued", { n: genCounts.queued })}</span>}
              {genCounts.failed > 0 && <span className="text-cx-danger">{t("reports.strip.failed", { n: genCounts.failed })}</span>}
              {genCounts.failed > 0 && <Button size="xs" variant="link" onClick={retryFailed}>{t("reports.strip.retryFailed")}</Button>}
              {genCounts.running + genCounts.queued > 0 && <Button size="xs" variant="link" onClick={cancelActiveGenerations}>{t("reports.strip.cancelAll")}</Button>}
            </div>
          )}

          {loadError && (
            <Callout tone="danger" role="alert" className="mx-3 mb-2" action={<Button size="xs" onClick={() => void load()}>{t("reports.reload")}</Button>}>{loadError}</Callout>
          )}

          {loading ? (
            <div className="flex flex-col gap-3 px-4 py-2">
              {Array.from({ length: 4 }, (_, i) => (
                <div key={i} className="flex flex-col gap-1.5">
                  <Skeleton className="h-3.5 w-4/5" />
                  <Skeleton className="w-1/2" />
                </div>
              ))}
            </div>
          ) : (
            <ReportTree
              mode={workspaceMode}
              reports={reports}
              folders={folders}
              selectedId={selectedId}
              expanded={expanded}
              onToggleExpand={toggleExpand}
              visibleReports={listFiltered ? filtered : null}
              rootLabel={t("reports.import.root")}
              solvedLabel={solvedLabel}
              createFolderNonce={createFolderNonce}
              checkedIds={checkedIds}
              onCheck={checkReports}
              runName={runName}
              runCategory={runCategory}
              runSolved={runSolved}
              onSelect={(r) => selectReport(r.id)}
              onRefresh={() => void load()}
              onDeleteReport={setDeleteTarget}
              onGenerate={(r) => void requestGenerate(r)}
              onCancelGeneration={(r) => void dismissGeneration(r)}
              onNewDocument={(fid) => quickCreateBlank(fid)}
              onNewLinked={(fid) => openCreate(fid)}
              onEditReport={editReport}
              onToggleStatus={(r) => void toggleReportStatus(r)}
              onRenameReport={renameReportMeta}
              onCopyMarkdown={copyReportMarkdown}
              onDownloadReport={downloadReportMarkdown}
              onOpenRun={(runId) => guard(() => onOpenRun(runId))}
              onOrganize={() => void openOrganize()}
              onExpandAll={expandAllFolders}
              onCollapseAll={collapseAllFolders}
              onRunDrop={(runId, fid) => void dropRun(runId, fid)}
              empty={
                listFiltered ? (
                  <div className="flex flex-1 flex-col items-center justify-center gap-2.5 px-5 pb-[8vh] text-center">
                    <p className="text-cx-sm text-cx-fg-3">{t("reports.noMatch")}</p>
                    {filtersActive && <Button size="sm" variant="soft" onClick={clearFilters}>{t("reports.clearFilters")}</Button>}
                  </div>
                ) : (
                  <div className="flex flex-1 flex-col items-center justify-center gap-1 px-5 pb-[8vh] text-center">
                    <p className="text-cx-sm font-medium text-cx-fg-3">{t("reports.empty")}</p>
                    <p className="text-cx-xs leading-5 text-cx-fg-4">{t("reports.emptyHint")}</p>
                  </div>
                )
              }
            />
          )}
          <ResizeHandle
            side="right"
            value={listWidth}
            min={REPORTS_LIST_WIDTH_MIN}
            max={maxListWidth}
            onChange={onListWidthChange}
            onReset={() => commitListWidth(REPORTS_LIST_WIDTH_DEFAULT)}
            onDragStateChange={onListResizeDrag}
            label={t("reports.resizeList")}
            className="hidden lg:block"
          />
        </aside>

        <section
          ref={detailPaneRef}
          data-report-drop-detail="true"
          onDragEnter={(event) => {
            if (hasRunDrag(event.dataTransfer)) setDragHover(true);
          }}
          onDragOver={(event) => {
            if (!hasRunDrag(event.dataTransfer)) return;
            event.preventDefault();
            event.dataTransfer.dropEffect = "copy";
            if (!dragHover) setDragHover(true);
          }}
          onDragLeave={(event) => {
            if (!event.currentTarget.contains(event.relatedTarget as Node | null)) setDragHover(false);
          }}
          onDrop={() => setDragHover(false)}
          className="relative flex min-h-0 min-w-0 flex-col bg-cx-bg"
          aria-label={selected?.title ?? t("reports.title")}
        >
          {loading ? (
            <div className="flex flex-col gap-3 p-8"><Skeleton className="h-5 w-1/3" /><Skeleton className="w-2/3" /><Skeleton className="w-1/2" /></div>
          ) : !selected ? (
            reports.length > 0 ? (
              <div className="flex min-h-0 flex-1 flex-col items-center justify-center gap-5 px-6 pb-[8vh]">
                <div className="flex flex-col items-center gap-1.5 text-center">
                  <h2 className="text-cx-2xl font-semibold leading-9 tracking-[-0.02em] text-cx-fg">{t("reports.selectPrompt")}</h2>
                  <p className="max-w-[420px] text-cx-sm text-cx-fg-3">{t("reports.selectHint")}</p>
                </div>
                <div className="w-full max-w-[440px] divide-y divide-cx-border-subtle overflow-hidden rounded-2xl bg-cx-elevated ring-1 ring-cx-border-subtle">
                  {reports.slice(0, 4).map((report) => (
                    <button
                      key={report.id}
                      type="button"
                      onClick={() => selectReport(report.id)}
                      className="flex w-full items-center gap-3 px-4 py-3 text-left transition-colors hover:bg-cx-hover"
                    >
                      <span className="min-w-0 flex-1 truncate text-cx-sm font-medium text-cx-fg">{report.title}</span>
                      <span className="shrink-0 text-cx-xs tabular-nums text-cx-fg-4">{fmtTime(report.updated_at)}</span>
                    </button>
                  ))}
                </div>
              </div>
            ) : (
              <div className="flex min-h-0 flex-1 flex-col items-center justify-center gap-6 px-6 pb-[8vh]">
                <div className="flex flex-col items-center gap-2 text-center">
                  <h2 className="text-cx-2xl font-semibold leading-9 tracking-[-0.02em] text-cx-fg">{t("reports.hero.title")}</h2>
                  <p className="max-w-[440px] text-cx-sm text-cx-fg-3">
                    {eligibleRuns.length > 0 ? t("reports.hero.subtitleWithRuns") : t("reports.hero.subtitleEmpty")}
                  </p>
                </div>
                {quickRuns.length > 0 ? (
                  <>
                    <div className="w-full max-w-[520px] divide-y divide-cx-border-subtle overflow-hidden rounded-2xl bg-cx-elevated ring-1 ring-cx-border-subtle">
                      {quickRuns.map((run) => (
                        <div
                          key={run.run_id}
                          className="group flex items-center gap-3 px-4 py-3 transition-colors hover:bg-cx-hover"
                        >
                          <button
                            type="button"
                            disabled={createBusy}
                            onClick={() => quickCreateFromRun(run, false)}
                            className="flex min-w-0 flex-1 items-center gap-2.5 text-left"
                          >
                            <span className="min-w-0 flex-1 truncate text-cx-sm font-medium text-cx-fg">{run.name || run.run_id}</span>
                            {run.category && <span className="shrink-0 text-cx-xs text-cx-fg-3">{run.category}</span>}
                            {run.solved && <span className="shrink-0 text-cx-xs text-cx-success">{solvedLabel}</span>}
                          </button>
                          <Button
                            size="xs"
                            variant="soft"
                            disabled={createBusy}
                            onClick={() => quickCreateFromRun(run, true)}
                          >
                            {t("reports.hero.quickGenerate")}
                          </Button>
                        </div>
                      ))}
                    </div>
                    <div className="flex flex-wrap items-center justify-center gap-2">
                      <button
                        type="button"
                        disabled={createBusy}
                        onClick={() => quickCreateBlank(null)}
                        className="cx-press inline-flex h-9 items-center gap-1.5 rounded-xl bg-cx-hover px-3.5 text-cx-sm text-cx-fg-2 transition-colors hover:bg-cx-active hover:text-cx-fg disabled:opacity-50"
                      >
                        <Icon name="pencilLine" size={14} className="text-cx-fg-3" />
                        {t("reports.standalone.new")}
                      </button>
                      <button
                        type="button"
                        onClick={() => { setImportSeed({ files: [], entries: [] }); setImportOpen(true); }}
                        className="cx-press inline-flex h-9 items-center gap-1.5 rounded-xl bg-cx-hover px-3.5 text-cx-sm text-cx-fg-2 transition-colors hover:bg-cx-active hover:text-cx-fg"
                      >
                        <Icon name="fileDown" size={14} className="text-cx-fg-3" />
                        {t("reports.import.button")}
                      </button>
                    </div>
                  </>
                ) : (
                  <div className="flex flex-wrap items-center justify-center gap-2">
                    <Button variant="primary" icon="pencilLine" loading={createBusy} onClick={() => quickCreateBlank(null)}>{t("reports.standalone.new")}</Button>
                    <Button variant="soft" icon="fileDown" onClick={() => { setImportSeed({ files: [], entries: [] }); setImportOpen(true); }}>{t("reports.import.button")}</Button>
                  </div>
                )}
              </div>
            )
          ) : (
            <>
              <div className="flex h-[var(--cx-header-h,52px)] shrink-0 items-center gap-2 px-6">
                {editing ? (
                  <>
                    <div className="flex min-w-0 flex-1 items-center gap-3 text-cx-xs tabular-nums text-cx-fg-3" aria-live="polite">
                      {dirty && <span>{t("reports.unsaved")}</span>}
                      {editStats.words > 0 && <span>{t("reports.editor.words", { n: editStats.words })}</span>}
                    </div>
                    <SegmentedControl<EditorView>
                      value={editView}
                      onChange={setEditView}
                      ariaLabel={t("reports.title")}
                      options={[
                        { value: "edit", icon: "pencil", label: <span className="hidden sm:inline">{t("reports.editor.edit")}</span>, ariaLabel: t("reports.editor.edit") },
                        { value: "split", icon: "columns", label: <span className="hidden sm:inline">{t("reports.editor.split")}</span>, ariaLabel: t("reports.editor.split") },
                        { value: "preview", icon: "eye", label: <span className="hidden sm:inline">{t("reports.editor.preview")}</span>, ariaLabel: t("reports.editor.preview") },
                      ]}
                    />
                    <Button size="sm" variant="ghost" onClick={cancelEdit} disabled={busy}>{t("reports.cancel")}</Button>
                    <Button size="sm" variant="primary" shortcut="mod+S" tooltipPlacement="bottom" loading={busy} onClick={saveEdit}>{t("reports.save")}</Button>
                  </>
                ) : (
                  <>
                    <nav className="flex min-w-0 flex-1 items-center gap-0.5 text-cx-xs text-cx-fg-3" aria-label={t("reports.tree.breadcrumb")}>
                      {folderPath(selected.folder_id ?? null, foldersByIdMap).map((f, index) => (
                        <span key={f.id} className="flex min-w-0 items-center gap-0.5">
                          {index > 0 && <Icon name="chevronRight" size={10} className="shrink-0 text-cx-fg-4" />}
                          <button type="button" className="truncate rounded px-1 hover:bg-cx-hover hover:text-cx-fg" onClick={() => revealFolder(f.id)}>
                            {f.name}
                          </button>
                        </span>
                      ))}
                    </nav>
                    <Button
                      size="sm"
                      variant="ghost"
                      icon={selected.status === "final" ? "check" : undefined}
                      disabled={busy}
                      tooltip={selected.status === "final" ? t("reports.markDraft") : undefined}
                      tooltipPlacement="bottom"
                      onClick={() => void applyUpdate({ status: selected.status === "final" ? "draft" : "final" })}
                    >
                      {selected.status === "final" ? t("reports.isFinal") : t("reports.markFinal")}
                    </Button>
                    <Button size="sm" variant="soft" icon="pencil" onClick={() => startEdit(selected)}>{t("reports.edit")}</Button>
                    <Menu ariaLabel={t("reports.more")} placement="bottom-end" trigger={<IconButton icon="more" label={t("reports.more")} />}>
                      {workspaceMode === "ctf" && selected.run_id && (
                        <>
                          <MenuItem
                            icon="refresh"
                            description={keepHint ? t("reports.ai.keepHint") : undefined}
                            wrapDescription
                            disabled={generationActive(selected)}
                            onSelect={() => requestAi("generate")}
                          >
                            {readBody.trim() ? t("reports.ai.regenerate") : t("reports.ai.generate")}
                          </MenuItem>
                          {genSettings?.templates && genSettings.templates.length > 0 && (
                            <MenuSub label={t("reports.ai.generateWith")} icon="file" disabled={generationActive(selected)}>
                              {genSettings.templates.map((tpl) => (
                                <MenuItem key={tpl.id} description={tpl.description} onSelect={() => void requestGenerate(selected, tpl.id)}>{tpl.name}</MenuItem>
                              ))}
                            </MenuSub>
                          )}
                          <MenuSeparator />
                        </>
                      )}
                      <MenuItem icon="copy" onSelect={() => copyReportMarkdown(selected)}>{t("reports.copy")}</MenuItem>
                      <MenuItem icon="download" onSelect={() => downloadReportMarkdown(selected)}>{t("reports.download")}</MenuItem>
                      <MenuItem icon="folderOpen" onSelect={() => setMoveTarget(selected)}>{t("reports.tree.moveTo")}</MenuItem>
                      <MenuSeparator />
                      <MenuItem icon="trash" danger onSelect={() => setDeleteTarget(selected)}>{t("reports.delete")}</MenuItem>
                    </Menu>
                  </>
                )}
              </div>

              {actionError && (
                <Callout
                  tone="danger"
                  role="alert"
                  className={cn(!editing || editView !== "split" ? "mx-auto w-full max-w-[820px] px-6 sm:px-10" : "mx-6", "mt-2")}
                  action={conflict ? <Button size="xs" onClick={() => void load()}>{t("reports.reload")}</Button> : undefined}
                  onDismiss={() => setActionError("")}
                >
                  {actionError}
                </Callout>
              )}

              {serverChanged && (
                <Callout
                  tone="warning"
                  role="status"
                  className={cn(!editing || editView !== "split" ? "mx-auto w-full max-w-[820px] px-6 sm:px-10" : "mx-6", "mt-2")}
                  action={<Button size="xs" variant="outline" onClick={() => { if (selected) startEdit(selected); }}>{t("reports.reload")}</Button>}
                >
                  {t("reports.serverChanged")}
                </Callout>
              )}

              <GenerationBanner
                report={selected}
                busy={busy}
                className={cn(!editing || editView !== "split" ? "mx-auto w-full max-w-[820px] px-6 sm:px-10 pt-1" : "px-6 pt-1")}
                onCancel={() => void dismissGeneration(selected)}
                onRetry={() => void requestGenerate(selected)}
                onDismiss={() => void dismissGeneration(selected)}
                onApplyPending={() => void applyPendingUpdate()}
                onViewDiff={() => setPendingDiffOpen(true)}
                onIgnorePending={() => void dismissPendingUpdate()}
              />

              {editing ? (
                <>
                  <div className={cn("shrink-0 pb-1 pt-2", editView === "split" ? "px-6" : "mx-auto w-full max-w-[820px] px-6 sm:px-10")}>
                    <input
                      ref={titleInputRef}
                      value={editTitle}
                      onChange={(event) => setEditTitle(event.target.value)}
                      onKeyDown={(event) => {
                        if (event.nativeEvent.isComposing || event.keyCode === 229) return;
                        if (event.key === "Enter") {
                          event.preventDefault();
                          editorRef.current?.focus();
                        }
                      }}
                      placeholder={t("reports.titlePlaceholder")}
                      aria-label={t("reports.create.name")}
                      className="w-full rounded-md bg-transparent py-0.5 text-cx-xl font-semibold leading-8 tracking-[-0.01em] text-cx-fg outline-none placeholder:text-cx-fg-4"
                    />
                    <div className="mt-1">
                      <TagInput
                        tags={editTags}
                        onChange={setEditTags}
                        placeholder={t("reports.tagsPlaceholder")}
                        addLabel={t("reports.addTag")}
                        removeLabel={(tag) => t("reports.removeTag", { tag })}
                      />
                    </div>
                  </div>
                  <MarkdownEditor
                    value={editBody}
                    onChange={setEditBody}
                    assets={editAssets}
                    onAssetsChange={(update) => setEditAssets(update)}
                    onSave={saveEdit}
                    view={editView}
                    centered={editView !== "split"}
                    editorRef={editorRef}
                    disabled={busy}
                    templates={workspaceMode === "ctf" ? genSettings?.templates : undefined}
                    onApplyTemplate={handleApplyTemplate}
                    extraActions={!selected.run_id ? undefined : workspaceMode === "ctf" ? (
                      <Menu
                        ariaLabel={t("reports.ai.menu")}
                        placement="bottom-end"
                        trigger={
                          <Button size="xs" variant="ghost" icon={generationActive(selected) ? "loader" : undefined} iconRight="chevronDown" disabled={busy}>
                            {t("reports.editor.ai")}
                          </Button>
                        }
                      >
                        <MenuItem icon="refresh" description={keepHint ? t("reports.ai.keepHint") : t("reports.ai.generateHint")} wrapDescription disabled={generationActive(selected)} onSelect={() => requestAi("generate")}>{t("reports.ai.generate")}</MenuItem>
                        {genSettings?.templates && genSettings.templates.length > 0 && (
                          <MenuSub label={t("reports.ai.generateWith")} icon="file" disabled={generationActive(selected)}>
                            {genSettings.templates.map((tpl) => (
                              <MenuItem key={tpl.id} description={tpl.description} onSelect={() => void requestGenerate(selected, tpl.id)}>{tpl.name}</MenuItem>
                            ))}
                          </MenuSub>
                        )}
                        <MenuSeparator />
                        <MenuItem icon="fileDown" description={t("reports.ai.useLatestHint")} wrapDescription onSelect={() => requestAi("use")}>{t("reports.ai.useLatest")}</MenuItem>
                      </Menu>
                    ) : (
                      <Button size="xs" variant="ghost" icon="fileDown" disabled={busy} onClick={() => requestAi("use")}>
                        {sourceLabel}
                      </Button>
                    )}
                  />
                </>
              ) : (
                <div className="cx-scroll min-h-0 flex-1 overflow-y-auto">
                  <article className="mx-auto w-full max-w-[820px] px-6 pb-16 pt-2 sm:px-10">
                    <h2 className="text-cx-xl font-semibold leading-8 tracking-[-0.01em] text-cx-fg [overflow-wrap:anywhere]">{selected.title}</h2>
                    <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-cx-xs text-cx-fg-3">
                      {selected.run_id ? (
                        <>
                          <button
                            type="button"
                            title={selected.run_id}
                            onClick={() => guard(() => onOpenRun(selected.run_id!))}
                            className="inline-flex items-center gap-0.5 rounded font-medium text-cx-fg-2 transition-colors hover:text-cx-accent"
                          >
                            {(selectedRun?.name || runName(selected.run_id)).trim() === selected.title.trim()
                              ? t("reports.openRun")
                              : (selectedRun?.name || runName(selected.run_id))}
                            <Icon name="arrowUpRight" size={12} />
                          </button>
                          {!selectedRun && <span className="text-cx-warning">{t("reports.runMissing")}</span>}
                        </>
                      ) : (
                        <span>{t("reports.standalone.notLinked")}</span>
                      )}
                      {selectedRun?.category && <span>{selectedRun.category}</span>}
                      {selectedRun?.status === "solved" && (
                        <span className="text-cx-success">{solvedLabel}</span>
                      )}
                      {selectedRun?.flag && (
                        <button
                          type="button"
                          title={selectedRun.flag}
                          onClick={() => void navigator.clipboard.writeText(selectedRun.flag!).then(() => toast({ tone: "success", title: t("reports.copied") }))}
                          className="max-w-[18rem] truncate rounded-md bg-cx-hover px-1.5 py-0.5 font-cx-mono text-cx-2xs text-cx-fg-2 transition-colors hover:bg-cx-active"
                        >
                          {selectedRun.flag}
                        </button>
                      )}
                      <span
                        className="tabular-nums"
                        title={`${t(`reports.source.${selected.source.kind}`)}${selected.source.ref ? ` · ${selected.source.ref}` : ""} · ${fmtFullTime(selected.updated_at)}`}
                      >
                        {t("reports.updatedAt", { time: fmtTime(selected.updated_at) })}
                      </span>
                    </div>
                    {selected.tags.length > 0 && (
                      <div className="mt-2 flex flex-wrap gap-x-2 gap-y-1 text-cx-xs text-cx-fg-3">
                        {selected.tags.map((tag) => <span key={tag}>#{tag}</span>)}
                      </div>
                    )}
                    {readBody.trim() ? (
                      <ChatMarkdown text={viewText} inlineImages annotateSourceOffsets={false} className="report-prose mt-7" />
                    ) : !generationActive(selected) ? (
                      <div className="mt-7 flex flex-col items-start gap-4 border-t border-cx-border-subtle pt-8">
                        <p className="text-cx-sm text-cx-fg-3">{t("reports.ai.emptyTitle")}</p>
                        <div className="flex flex-wrap items-center gap-2">
                          {workspaceMode === "ctf" && selected.run_id && (
                            <Button size="sm" variant="primary" disabled={generationActive(selected)} onClick={() => void requestGenerate(selected)}>{t("reports.ai.generate")}</Button>
                          )}
                          <Button
                            size="sm"
                            variant={workspaceMode === "ctf" && selected.run_id ? "soft" : "primary"}
                            icon="pencil"
                            onClick={() => startEdit(selected)}
                          >
                            {t("reports.ai.manual")}
                          </Button>
                          {workspaceMode === "ctf" && genSettings?.templates && genSettings.templates.length > 0 && (
                            <Menu
                              placement="bottom-start"
                              ariaLabel={t("reports.create.template")}
                              trigger={
                                <Button size="sm" variant="ghost" icon="file" iconRight="chevronDown">
                                  {t("reports.create.template")}
                                </Button>
                              }
                            >
                              {genSettings.templates.map((tpl) => (
                                <MenuItem
                                  key={tpl.id}
                                  icon="file"
                                  description={tpl.description}
                                  onSelect={() => {
                                    const rendered = renderWriteupTemplate(tpl.markdown, {
                                      title: selected.title || runName(selected.run_id ?? null),
                                      category: runCategory(selected.run_id ?? null),
                                      flag: selectedRun?.flag ?? null,
                                    });
                                    startEdit(selected);
                                    applyMarkdown(rendered);
                                  }}
                                >
                                  {tpl.name}
                                </MenuItem>
                              ))}
                            </Menu>
                          )}
                        </div>
                      </div>
                    ) : null}
                  </article>
                </div>
              )}
            </>
          )}
          {(dragActive || dropBusy) && (
            <div
              className={cn(
                "pointer-events-none absolute inset-3 z-(--z-sticky) grid place-items-center rounded-xl border-2 border-dashed px-4 text-center text-cx-sm font-medium text-cx-fg transition-colors",
                dragHover || dropBusy ? "border-cx-accent bg-cx-accent-soft" : "border-cx-accent-line bg-[color-mix(in_srgb,var(--cx-bg)_88%,transparent)]",
              )}
              role="status"
            >
              {dropBusy ? t("reports.drop.busy") : t("reports.drop.title")}
            </div>
          )}
        </section>
      </div>
      {fileDropActive && (
        <div className="pointer-events-none absolute inset-3 z-(--z-sticky) grid place-items-center rounded-xl border-2 border-dashed border-cx-accent bg-[color-mix(in_srgb,var(--cx-bg)_88%,transparent)] px-4 text-center text-cx-sm font-medium text-cx-fg" role="status">
          {t("reports.import.dropTitle")}
        </div>
      )}
      <Dialog
        open={createOpen}
        onOpenChange={(open) => !createBusy && setCreateOpen(open)}
        title={t("reports.create.title")}
        icon="filePlus"
        dismissable={!createBusy}
        footer={
          <>
            <Button variant="ghost" onClick={() => setCreateOpen(false)} disabled={createBusy}>{t("dialog.cancel")}</Button>
            <Button variant="primary" loading={createBusy} disabled={!createRunId} onClick={() => void submitCreate()}>{t("reports.create.submit")}</Button>
          </>
        }
      >
        <div className="flex flex-col gap-4">
          {eligibleRuns.length === 0 && <Callout tone="neutral" role="status">{t("reports.create.noRuns")}</Callout>}
          <div>
            <Label>{t("reports.create.run")}</Label>
            <Select<string>
              value={createRunId || null}
              onChange={(id) => {
                setCreateRunId(id);
                const run = eligibleRuns.find((r) => r.run_id === id);
                if (run) setCreateTitle(run.name);
              }}
              options={runOptions}
              searchable
              ariaLabel={t("reports.create.run")}
              className="w-full"
            />
          </div>
          <TextField label={t("reports.create.name")} value={createTitle} onChange={(event) => setCreateTitle(event.target.value)} />
          <div>
            <Label>{t("reports.create.folder")}</Label>
            <Select<string>
              value={createFolderId ?? "__root__"}
              onChange={(v) => setCreateFolderId(v === "__root__" ? null : v)}
              options={folderSelectOptions}
              ariaLabel={t("reports.create.folder")}
              className="w-full"
            />
          </div>
          <div>
            <Label>{t("reports.create.start")}</Label>
            <SegmentedControl
              value={createFrom}
              onChange={setCreateFrom}
              ariaLabel={t("reports.create.start")}
              size="md"
              options={
                workspaceMode === "ctf"
                  ? [
                      { value: "blank", label: t("reports.create.blank"), icon: "file" },
                      { value: "template", label: t("reports.create.template"), icon: "pencilLine" },
                      { value: "source", label: sourceLabel, icon: "fileDown" },
                    ]
                  : [
                      { value: "blank", label: t("reports.create.blank"), icon: "file" },
                      { value: "source", label: sourceLabel, icon: "fileDown" },
                    ]
              }
            />
          </div>
          {workspaceMode === "ctf" && (createFrom === "template" || createFrom === "source") && genSettings?.templates && genSettings.templates.length > 0 && (
            <div>
              <div className="mb-1.5 flex items-center justify-between">
                <span className="text-cx-xs font-medium text-cx-fg-2">
                  {createFrom === "template" ? t("reports.create.pickTemplate") : t("reports.create.fallbackTemplate")}
                </span>
                <button
                  type="button"
                  onClick={() => {
                    setSettingsTab("templates");
                    setSettingsOpen(true);
                  }}
                  className="text-cx-xs text-cx-accent hover:underline"
                >
                  {t("reports.settings")}
                </button>
              </div>
              <Select<string>
                value={createTemplateId}
                onChange={setCreateTemplateId}
                ariaLabel={t("reports.create.pickTemplate")}
                className="w-full"
                options={genSettings.templates.map((tpl) => ({
                  value: tpl.id,
                  label: tpl.name,
                  description: tpl.description,
                }))}
              />
            </div>
          )}
          {createError && <Callout tone="danger" role="alert">{createError}</Callout>}
        </div>
      </Dialog>

      <Dialog
        open={Boolean(deleteTarget)}
        onOpenChange={(open) => !open && !deleteBusy && setDeleteTarget(null)}
        title={t("reports.delete")}
        description={t("reports.deleteConfirm")}
        icon="trash"
        tone="danger"
        dismissable={!deleteBusy}
        footer={
          <>
            <Button variant="ghost" onClick={() => setDeleteTarget(null)} disabled={deleteBusy}>{t("dialog.cancel")}</Button>
            <Button variant="danger" loading={deleteBusy} onClick={() => void confirmDelete()}>{t("reports.delete")}</Button>
          </>
        }
      >
        {deleteTarget ? <p className="font-medium text-cx-fg [overflow-wrap:anywhere]">{deleteTarget.title}</p> : null}
      </Dialog>

      <Dialog
        open={discardAction !== null}
        onOpenChange={(open) => !open && setDiscardAction(null)}
        title={t("reports.discard.title")}
        description={t("reports.discard.body")}
        icon="alert"
        tone="warning"
        footer={
          <>
            <Button variant="ghost" onClick={() => setDiscardAction(null)}>{t("reports.discard.keep")}</Button>
            <Button variant="danger" onClick={() => { const action = discardAction; setDiscardAction(null); action?.(); }}>{t("reports.discard.confirm")}</Button>
          </>
        }
      />

      <Dialog
        open={aiConfirm !== null}
        onOpenChange={(open) => !open && setAiConfirm(null)}
        title={aiConfirm === "generate" ? t("reports.ai.generate") : sourceLabel}
        description={t("reports.aiFillConfirm")}
        icon="fileDown"
        tone="warning"
        footer={
          <>
            <Button variant="ghost" onClick={() => setAiConfirm(null)}>{t("dialog.cancel")}</Button>
            <Button variant="primary" onClick={() => (aiConfirm === "generate" && selected ? void requestGenerate(selected) : void fillFromSource())}>{t("reports.aiFillAction")}</Button>
          </>
        }
      />

      <Dialog
        open={bulkDelete}
        onOpenChange={(open) => !open && setBulkDelete(false)}
        title={t("reports.delete")}
        description={t("reports.bulk.deleteConfirm", { n: checkedIds.size })}
        icon="trash"
        tone="danger"
        footer={
          <>
            <Button variant="ghost" onClick={() => setBulkDelete(false)}>{t("dialog.cancel")}</Button>
            <Button variant="danger" onClick={() => void bulkDeleteConfirm()}>{t("reports.delete")}</Button>
          </>
        }
      />

      {pendingDiffOpen && selected?.pending_update && (
        <PendingUpdateDialog
          report={selected}
          busy={busy}
          onClose={() => setPendingDiffOpen(false)}
          onApply={() => void applyPendingUpdate()}
          onIgnore={() => void dismissPendingUpdate()}
        />
      )}

      {workspaceMode === "ctf" && <BatchGenerateDialog open={batchOpen} onClose={() => { setBatchOpen(false); void load(); }} />}

      <Dialog
        open={organizeOpen}
        onOpenChange={(open) => !open && !organizeBusy && setOrganizeOpen(false)}
        title={t("reports.organize.title")}
        icon="folderTree"
        dismissable={!organizeBusy}
        footer={
          <>
            <Button variant="ghost" onClick={() => setOrganizeOpen(false)} disabled={organizeBusy}>{t("dialog.cancel")}</Button>
            <Button variant="primary" loading={organizeBusy} disabled={!organizePlan.moves.length} onClick={() => void runOrganize()}>{t("reports.organize.confirm")}</Button>
          </>
        }
      >
        {organizePlan.moves.length === 0 ? (
          <Callout tone="neutral" role="status">{t("reports.organize.nothing")}</Callout>
        ) : (
          <div className="flex flex-col gap-2">
            <p className="text-cx-sm text-cx-fg-2">{t("reports.organize.preview", { folders: organizePlan.folders.length, reports: organizePlan.moves.length })}</p>
            <ul className="cx-scroll max-h-48 overflow-y-auto rounded-lg border border-cx-border-subtle p-2 text-cx-xs text-cx-fg-3">
              {organizePlan.moves.map((m) => (
                <li key={m.report.id} className="flex items-center gap-2 py-0.5">
                  <Icon name="folder" size={12} className="shrink-0 text-cx-fg-4" />
                  <span className="truncate">{m.folderName}</span>
                  <span className="text-cx-fg-4">←</span>
                  <span className="truncate">{m.report.title}</span>
                </li>
              ))}
            </ul>
            <p className="text-cx-xs text-cx-fg-4">{t("reports.organize.keepHint")}</p>
          </div>
        )}
      </Dialog>

      <ImportDialog
        open={importOpen}
        mode={workspaceMode}
        runs={eligibleRuns}
        reports={reports}
        folders={folders}
        defaultFolderId={selected?.folder_id ?? null}
        seed={importSeed}
        onClose={() => setImportOpen(false)}
        onImported={() => void load()}
        onSelectReport={(r) => {
          if (filtersActive) clearFilters();
          expandPath(r.folder_id);
          setSelectedId(r.id);
        }}
      />

      <Dialog
        open={Boolean(moveTarget)}
        onOpenChange={(open) => !open && setMoveTarget(null)}
        title={t("reports.tree.moveTo")}
        icon="folder"
        footer={<Button variant="ghost" onClick={() => setMoveTarget(null)}>{t("dialog.cancel")}</Button>}
      >
        <div className="flex max-h-[50vh] flex-col gap-0.5 overflow-y-auto">
          {folderSelectOptions.map((o) => (
            <button
              key={o.value}
              type="button"
              onClick={() => {
                const target = moveTarget;
                setMoveTarget(null);
                if (target) void moveListReportToFolder(target, o.value === "__root__" ? null : o.value);
              }}
              className="flex h-8 items-center gap-2 rounded-lg px-2 text-left text-cx-sm text-cx-fg-2 hover:bg-cx-hover"
            >
              <Icon name="folder" size={13} className="text-cx-fg-4" />
              <span className="truncate">{o.label}</span>
            </button>
          ))}
        </div>
      </Dialog>

      <Dialog
        open={pendingTemplate !== null}
        onOpenChange={(open) => !open && setPendingTemplate(null)}
        title={t("reports.template.replaceTitle")}
        description={pendingTemplate ? t("reports.template.replaceBody", { name: pendingTemplate.name }) : ""}
        icon="file"
        tone="warning"
        footer={
          <>
            <Button variant="ghost" onClick={() => setPendingTemplate(null)}>{t("dialog.cancel")}</Button>
            <Button variant="primary" onClick={() => pendingTemplate && applyTemplateNow(pendingTemplate)}>{t("reports.aiFillAction")}</Button>
          </>
        }
      />

      {workspaceMode === "ctf" && (
        <WriteupSettingsDialog
          open={settingsOpen}
          initialTab={settingsTab}
          onClose={() => setSettingsOpen(false)}
          onSaved={(next) => {
            setGenSettings(next);
            const defId = next.default_template_id || next.templates[0]?.id || "builtin-standard";
            setCreateTemplateId((prev) => (next.templates.some((x) => x.id === prev) ? prev : defId));
          }}
        />
      )}
    </div>
  );
}
