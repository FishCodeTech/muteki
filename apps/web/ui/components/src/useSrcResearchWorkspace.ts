"use client";

import { useMemo, useReducer } from "react";
import {
  applyOverlays, DEFAULT_ASSET_FILTER, DEFAULT_FINDING_FILTER, mergedIndex, pickPrimary, sameRef,
  type AssetFilter, type FindingFilter, type FindingMerge, type ResearchChange, type ResearchEvidence,
  type ResearchFinding, type ResearchRef, type ResearchSnapshot,
} from "@/lib/srcResearch";
import { SRC_RESEARCH_DEMO_STEPS } from "@/lib/srcResearchDemo";

export type ResearchView = "progress" | "assets" | "findings" | "conversation";
export const RESEARCH_VIEWS: ResearchView[] = ["progress", "assets", "findings", "conversation"];

export type ResearchSentMessage = { id: string; text: string; refs: ResearchRef[]; at: string };

export type ResearchWorkspaceState = {
  view: ResearchView;
  assetFilter: AssetFilter;
  assetPage: number;
  selectedAssetId: string;
  findingFilter: FindingFilter;
  findingPage: number;
  selectedFindingIds: string[];
  focusFindingId: string;
  mergeDraft: { ids: string[]; primaryId: string } | null;
  merges: FindingMerge[];
  evidence: { ref: ResearchEvidence; findingId?: string; returnView: ResearchView } | null;
  openRecord: { kind: "run" | "monitor"; id: string } | null;
  chatContext: ResearchRef[];
  sentMessages: ResearchSentMessage[];
  demoSteps: Array<{ index: number; at: string }>;
  localChanges: ResearchChange[];
};

type Action =
  | { type: "view"; view: ResearchView }
  | { type: "assetFilter"; patch: Partial<AssetFilter> }
  | { type: "assetPage"; page: number }
  | { type: "selectAsset"; assetId: string; view?: ResearchView }
  | { type: "findingFilter"; patch: Partial<FindingFilter> }
  | { type: "findingPage"; page: number }
  | { type: "toggleFinding"; findingId: string }
  | { type: "selectFindings"; ids: string[] }
  | { type: "focusFinding"; findingId: string; view?: ResearchView; assetId?: string }
  | { type: "startMerge"; ids: string[]; primaryId: string }
  | { type: "mergePrimary"; primaryId: string }
  | { type: "cancelMerge" }
  | { type: "confirmMerge"; at: string }
  | { type: "undoMerge"; mergeId: string; at: string }
  | { type: "openEvidence"; ref: ResearchEvidence; findingId?: string }
  | { type: "closeEvidence" }
  | { type: "openRecord"; record: { kind: "run" | "monitor"; id: string } | null }
  | { type: "addContext"; refs: ResearchRef[] }
  | { type: "removeContext"; ref: ResearchRef }
  | { type: "recordSend"; text: string; at: string }
  | { type: "advanceDemo"; at: string }
  | { type: "resetDemo" };

function initialState(view: ResearchView, taskRef: ResearchRef): ResearchWorkspaceState {
  return {
    view, assetFilter: DEFAULT_ASSET_FILTER, assetPage: 0, selectedAssetId: "",
    findingFilter: DEFAULT_FINDING_FILTER, findingPage: 0, selectedFindingIds: [], focusFindingId: "",
    mergeDraft: null, merges: [], evidence: null, openRecord: null, chatContext: [taskRef],
    sentMessages: [], demoSteps: [], localChanges: [],
  };
}

const refsLabel = (refs: ResearchRef[]) => refs.map(ref => ref.id).join("、");

function reduce(state: ResearchWorkspaceState, action: Action): ResearchWorkspaceState {
  switch (action.type) {
    case "view": return { ...state, view: action.view, evidence: null, openRecord: action.view === "progress" ? state.openRecord : null };
    case "assetFilter": return { ...state, assetFilter: { ...state.assetFilter, ...action.patch }, assetPage: 0 };
    case "assetPage": return { ...state, assetPage: action.page };
    case "selectAsset": return { ...state, selectedAssetId: action.assetId, view: action.view || state.view, evidence: null };
    case "findingFilter": return { ...state, findingFilter: { ...state.findingFilter, ...action.patch }, findingPage: 0 };
    case "findingPage": return { ...state, findingPage: action.page };
    case "toggleFinding": {
      const selected = state.selectedFindingIds.includes(action.findingId)
        ? state.selectedFindingIds.filter(id => id !== action.findingId)
        : [...state.selectedFindingIds, action.findingId];
      return { ...state, selectedFindingIds: selected };
    }
    case "selectFindings": return { ...state, selectedFindingIds: action.ids };
    case "focusFinding": return {
      ...state, focusFindingId: action.findingId, view: action.view || state.view, evidence: null, mergeDraft: null,
      findingFilter: action.assetId !== undefined ? { ...state.findingFilter, assetId: action.assetId } : state.findingFilter,
      findingPage: action.assetId !== undefined ? 0 : state.findingPage,
    };
    case "startMerge": return { ...state, mergeDraft: { ids: action.ids, primaryId: action.primaryId }, selectedFindingIds: action.ids, view: "findings", evidence: null };
    case "mergePrimary": return state.mergeDraft ? { ...state, mergeDraft: { ...state.mergeDraft, primaryId: action.primaryId } } : state;
    case "cancelMerge": return { ...state, mergeDraft: null };
    case "confirmMerge": {
      if (!state.mergeDraft) return state;
      const merge: FindingMerge = { id: `merge-${action.at}`, primaryId: state.mergeDraft.primaryId, memberIds: state.mergeDraft.ids, createdAt: action.at };
      const refs = merge.memberIds.map(id => ({ kind: "finding" as const, id, label: id }));
      return {
        ...state, merges: [...state.merges, merge], mergeDraft: null, selectedFindingIds: [], focusFindingId: merge.primaryId,
        localChanges: [{ id: `local-${merge.id}`, at: action.at, origin: "local", summary: `本地归并：保留 ${merge.primaryId}，归入 ${refsLabel(refs.filter(ref => ref.id !== merge.primaryId))}；原始记录未删除`, refs }, ...state.localChanges],
      };
    }
    case "undoMerge": {
      const merge = state.merges.find(row => row.id === action.mergeId);
      if (!merge) return state;
      return {
        ...state, merges: state.merges.filter(row => row.id !== action.mergeId),
        localChanges: [{ id: `local-undo-${action.mergeId}-${action.at}`, at: action.at, origin: "local", summary: `撤销本地归并：${merge.primaryId} 不再包含其他记录`, refs: [{ kind: "finding", id: merge.primaryId, label: merge.primaryId }] }, ...state.localChanges],
      };
    }
    case "openEvidence": {
      const view = state.view === "conversation" ? "findings" : state.view;
      return { ...state, view, evidence: { ref: action.ref, findingId: action.findingId, returnView: view } };
    }
    case "closeEvidence": return { ...state, view: state.evidence?.returnView || state.view, evidence: null };
    case "openRecord": return { ...state, openRecord: action.record, view: "progress", evidence: null };
    case "addContext": {
      const next = [...state.chatContext];
      action.refs.forEach(ref => { if (!next.some(row => sameRef(row, ref))) next.push(ref); });
      return { ...state, chatContext: next };
    }
    case "removeContext": return { ...state, chatContext: state.chatContext.filter(row => row.kind === "task" || !sameRef(row, action.ref)) };
    case "recordSend": {
      const refs = state.chatContext;
      const message: ResearchSentMessage = { id: `local-message-${state.sentMessages.length + 1}`, text: action.text, refs, at: action.at };
      return {
        ...state, sentMessages: [...state.sentMessages, message],
        localChanges: [{ id: `chat-${message.id}`, at: action.at, origin: "chat", summary: `对话：以 ${refs.length} 个对象为上下文追加了一条本地预览消息，未发送给 Agent`, refs }, ...state.localChanges],
      };
    }
    case "advanceDemo": return state.demoSteps.length >= SRC_RESEARCH_DEMO_STEPS.length ? state : { ...state, demoSteps: [...state.demoSteps, { index: state.demoSteps.length, at: action.at }] };
    case "resetDemo": return { ...state, demoSteps: [] };
  }
}

export function useSrcResearchWorkspace(base: ResearchSnapshot | null, initialView: ResearchView) {
  const taskRef: ResearchRef = { kind: "task", id: base?.taskId || "task", label: base?.title || "当前研究" };
  const [state, dispatch] = useReducer(reduce, undefined, () => initialState(initialView, taskRef));
  const snapshot = useMemo(() => {
    if (!base) return null;
    const overlays = base.demo ? state.demoSteps.map(step => SRC_RESEARCH_DEMO_STEPS[step.index].overlay(step.at)) : [];
    const next = applyOverlays(base, overlays);
    return { ...next, changes: [...state.localChanges, ...next.changes] };
  }, [base, state.demoSteps, state.localChanges]);
  const mergedInto = useMemo(() => mergedIndex(state.merges), [state.merges]);
  const findingsById = useMemo(() => new Map((snapshot?.findings || []).map(row => [row.id, row])), [snapshot]);
  const assetsById = useMemo(() => new Map((snapshot?.assets || []).map(row => [row.id, row])), [snapshot]);
  const actions = useMemo(() => {
    const now = () => new Date().toISOString();
    return {
      setView: (view: ResearchView) => dispatch({ type: "view", view }),
      setAssetFilter: (patch: Partial<AssetFilter>) => dispatch({ type: "assetFilter", patch }),
      setAssetPage: (page: number) => dispatch({ type: "assetPage", page }),
      selectAsset: (assetId: string, view?: ResearchView) => dispatch({ type: "selectAsset", assetId, view }),
      setFindingFilter: (patch: Partial<FindingFilter>) => dispatch({ type: "findingFilter", patch }),
      setFindingPage: (page: number) => dispatch({ type: "findingPage", page }),
      toggleFinding: (findingId: string) => dispatch({ type: "toggleFinding", findingId }),
      selectFindings: (ids: string[]) => dispatch({ type: "selectFindings", ids }),
      focusFinding: (findingId: string, options?: { view?: ResearchView; assetId?: string }) => dispatch({ type: "focusFinding", findingId, ...options }),
      startMerge: (findings: ResearchFinding[]) => {
        const primary = pickPrimary(findings);
        if (primary && findings.length > 1) dispatch({ type: "startMerge", ids: findings.map(row => row.id), primaryId: primary.id });
      },
      setMergePrimary: (primaryId: string) => dispatch({ type: "mergePrimary", primaryId }),
      cancelMerge: () => dispatch({ type: "cancelMerge" }),
      confirmMerge: () => dispatch({ type: "confirmMerge", at: now() }),
      undoMerge: (mergeId: string) => dispatch({ type: "undoMerge", mergeId, at: now() }),
      openEvidence: (ref: ResearchEvidence, findingId?: string) => dispatch({ type: "openEvidence", ref, findingId }),
      closeEvidence: () => dispatch({ type: "closeEvidence" }),
      openRecord: (record: { kind: "run" | "monitor"; id: string } | null) => dispatch({ type: "openRecord", record }),
      addContext: (...refs: ResearchRef[]) => dispatch({ type: "addContext", refs }),
      removeContext: (ref: ResearchRef) => dispatch({ type: "removeContext", ref }),
      recordSend: (text: string) => dispatch({ type: "recordSend", text, at: now() }),
      advanceDemo: () => dispatch({ type: "advanceDemo", at: now() }),
      resetDemo: () => dispatch({ type: "resetDemo" }),
    };
  }, []);
  return { state, snapshot, mergedInto, findingsById, assetsById, actions };
}

export type SrcResearchWorkspaceModel = ReturnType<typeof useSrcResearchWorkspace>;
