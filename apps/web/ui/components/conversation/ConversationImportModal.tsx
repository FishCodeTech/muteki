"use client";

/* ─────────────────────────────────────────────────────────
 * CONVERSATION IMPORT MODAL — C33 Provider session import.
 * Scan → Preview → Apply flow for Claude / Codex history.
 * ───────────────────────────────────────────────────────── */

import React, { useCallback, useEffect, useRef, useState } from "react";
import {
  fetchImportScan,
  applyImport,
  type ProviderSessionScan,
  type ImportApplyResult,
} from "@/lib/useConversation";
import type { ConversationProject } from "@/lib/useConversation";

type AdapterId = "claude" | "codex";
type Step = "config" | "preview" | "result";

const STATUS_LABEL: Record<string, string> = {
  continuable: "可续聊",
  read_only: "只读导入",
  missing_tools: "缺少工具记录",
};

const STATUS_COLOR: Record<string, string> = {
  continuable: "text-green",
  read_only: "text-ink-3",
  missing_tools: "text-orange",
};

export interface ConversationImportModalProps {
  projects: ConversationProject[];
  onClose: () => void;
  onImportDone: () => void;
}

export function ConversationImportModal({
  projects,
  onClose,
  onImportDone,
}: ConversationImportModalProps) {
  const [step, setStep] = useState<Step>("config");
  const [adapter, setAdapter] = useState<AdapterId>("claude");
  const [customPath, setCustomPath] = useState("");
  const [projectId, setProjectId] = useState("");
  const [scanning, setScanning] = useState(false);
  const [scanError, setScanError] = useState("");
  const [scans, setScans] = useState<ProviderSessionScan[]>([]);
  const [basePath, setBasePath] = useState("");
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [applying, setApplying] = useState(false);
  const [result, setResult] = useState<ImportApplyResult | null>(null);
  const [applyError, setApplyError] = useState("");
  const panelRef = useRef<HTMLDivElement>(null);

  useEffect(() => { panelRef.current?.focus(); }, []);
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (e.key === "Escape") { e.preventDefault(); onClose(); }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [onClose]);

  const doScan = useCallback(async () => {
    setScanning(true);
    setScanError("");
    try {
      const res = await fetchImportScan({ adapter, path: customPath || undefined, limit: 100 });
      setBasePath(res.base_path);
      if (!res.exists) {
        setScanError(`路径不存在: ${res.base_path}`);
        setScans([]);
        return;
      }
      setScans(res.scans);
      setSelected(new Set(res.scans.map((s) => s.session_id)));
      setStep("preview");
    } catch (err) {
      setScanError(err instanceof Error ? err.message : "扫描失败");
    } finally {
      setScanning(false);
    }
  }, [adapter, customPath]);

  const toggleAll = () => {
    if (selected.size === scans.length) {
      setSelected(new Set());
    } else {
      setSelected(new Set(scans.map((s) => s.session_id)));
    }
  };

  const doApply = useCallback(async () => {
    setApplying(true);
    setApplyError("");
    try {
      const res = await applyImport({
        adapter_id: adapter,
        source_path: basePath,
        sessions: [...selected],
        project_id: projectId || undefined,
      });
      setResult(res);
      setStep("result");
      onImportDone();
    } catch (err) {
      setApplyError(err instanceof Error ? err.message : "导入失败");
    } finally {
      setApplying(false);
    }
  }, [adapter, basePath, selected, projectId, onImportDone]);

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40"
      role="dialog"
      aria-modal="true"
      aria-label="导入 Provider 历史会话"
      onClick={(e) => { if (e.target === e.currentTarget) onClose(); }}
    >
      <div
        ref={panelRef}
        tabIndex={-1}
        data-cx-focus-scope=""
        className="w-full max-w-2xl rounded-card bg-surface shadow-xl border border-line mx-4 outline-none overflow-hidden"
      >
        {/* Header */}
        <div className="flex items-center justify-between border-b border-line px-5 py-3">
          <div>
            <h2 className="text-[14px] font-semibold text-ink">导入 Provider 历史会话</h2>
            <p className="text-[12px] text-ink-3">C33 · 从本地 Claude / Codex 目录读取并导入</p>
          </div>
          <button
            type="button"
            onClick={onClose}
            className="rounded p-1 text-ink-3 hover:bg-hover hover:text-ink"
            aria-label="关闭"
          >
            <svg width="14" height="14" viewBox="0 0 14 14" fill="none" aria-hidden="true">
              <path d="M1 1l12 12M13 1L1 13" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round"/>
            </svg>
          </button>
        </div>

        {/* Body */}
        <div className="px-5 py-4">
          {step === "config" && (
            <div className="flex flex-col gap-4">
              {/* Adapter picker */}
              <div>
                <label className="block text-[12px] font-medium text-ink-2 mb-1.5">Provider</label>
                <div className="flex gap-2">
                  {(["claude", "codex"] as AdapterId[]).map((a) => (
                    <button
                      key={a}
                      type="button"
                      onClick={() => setAdapter(a)}
                      className={`flex-1 rounded-control border px-3 py-2 text-[12.5px] font-medium transition-colors
                        ${adapter === a
                          ? "border-accent bg-accent/10 text-accent"
                          : "border-line bg-surface text-ink-2 hover:bg-hover"
                        }`}
                    >
                      {a === "claude" ? "Claude (claude-code)" : "Codex (openai-codex)"}
                    </button>
                  ))}
                </div>
              </div>

              {/* Custom path */}
              <div>
                <label htmlFor="import-path" className="block text-[12px] font-medium text-ink-2 mb-1.5">
                  历史目录路径 <span className="text-ink-3">（留空使用默认 ~/{adapter === "claude" ? ".claude" : ".codex"}）</span>
                </label>
                <input
                  id="import-path"
                  type="text"
                  value={customPath}
                  onChange={(e) => setCustomPath(e.target.value)}
                  placeholder={`默认: ~/${adapter === "claude" ? ".claude" : ".codex"}`}
                  className="w-full rounded-control border border-line bg-inset px-3 py-2 text-[12.5px] text-ink outline-none focus:border-cx-border-strong"
                />
              </div>

              {/* Project assignment */}
              {projects.length > 0 && (
                <div>
                  <label htmlFor="import-project" className="block text-[12px] font-medium text-ink-2 mb-1.5">
                    导入到项目 <span className="text-ink-3">（可选）</span>
                  </label>
                  <select
                    id="import-project"
                    value={projectId}
                    onChange={(e) => setProjectId(e.target.value)}
                    className="w-full rounded-control border border-line bg-inset px-3 py-2 text-[12.5px] text-ink outline-none focus:border-cx-border-strong"
                  >
                    <option value="">不归入项目</option>
                    {projects.map((p) => (
                      <option key={p.project_id} value={p.project_id}>{p.name}</option>
                    ))}
                  </select>
                </div>
              )}

              {scanError && (
                <p className="rounded-control bg-red/5 border border-red/20 px-3 py-2 text-[12px] text-red">
                  {scanError}
                </p>
              )}
            </div>
          )}

          {step === "preview" && (
            <div className="flex flex-col gap-3">
              {/* AC2: scope preview */}
              <div className="flex items-center justify-between rounded-control bg-inset px-3 py-2 text-[12px]">
                <span className="text-ink-2">
                  扫描路径 <code className="text-ink text-[11px] font-mono">{basePath}</code>
                </span>
                <span className="font-semibold text-ink">共 {scans.length} 个会话</span>
              </div>

              {scans.length === 0 ? (
                <p className="text-[12.5px] text-ink-3 py-4 text-center">未找到可导入的会话</p>
              ) : (
                <>
                  <div className="flex items-center gap-2 text-[12px]">
                    <button type="button" onClick={toggleAll} className="text-accent hover:underline">
                      {selected.size === scans.length ? "取消全选" : "全选"}
                    </button>
                    <span className="text-ink-3">已选 {selected.size} / {scans.length}</span>
                  </div>

                  <div className="max-h-72 overflow-y-auto rounded-control border border-line divide-y divide-line">
                    {scans.map((scan) => (
                      <label
                        key={scan.session_id}
                        className="flex items-start gap-3 px-3 py-2.5 hover:bg-hover cursor-pointer"
                      >
                        <input
                          type="checkbox"
                          checked={selected.has(scan.session_id)}
                          onChange={(e) => {
                            setSelected((prev) => {
                              const next = new Set(prev);
                              if (e.target.checked) next.add(scan.session_id);
                              else next.delete(scan.session_id);
                              return next;
                            });
                          }}
                          className="mt-0.5 shrink-0"
                          aria-label={`选择会话: ${scan.title}`}
                        />
                        <div className="flex-1 min-w-0">
                          <div className="flex items-center justify-between gap-2">
                            <span className="text-[12.5px] text-ink truncate">{scan.title}</span>
                            {/* AC3: status badge */}
                            <span className={`shrink-0 text-[11px] font-medium ${STATUS_COLOR[scan.import_status] || "text-ink-3"}`}>
                              {STATUS_LABEL[scan.import_status] || scan.import_status}
                            </span>
                          </div>
                          <div className="flex items-center gap-2 mt-0.5 text-[11px] text-ink-3">
                            <span>{scan.message_count} 条消息</span>
                            {scan.created_at && <span>· {scan.created_at.slice(0, 10)}</span>}
                            {scan.missing_fields.length > 0 && (
                              <span className="text-orange">· 缺少: {scan.missing_fields.join(", ")}</span>
                            )}
                          </div>
                        </div>
                      </label>
                    ))}
                  </div>
                </>
              )}

              {applyError && (
                <p className="rounded-control bg-red/5 border border-red/20 px-3 py-2 text-[12px] text-red">
                  {applyError}
                </p>
              )}
            </div>
          )}

          {step === "result" && result && (
            <div className="flex flex-col gap-3">
              <div className="grid grid-cols-3 gap-3 text-center">
                {[
                  { label: "已导入", count: result.imported.length, color: "text-green" },
                  { label: "已跳过（重复）", count: result.skipped.length, color: "text-ink-3" },
                  { label: "失败", count: result.failed.length, color: result.failed.length ? "text-red" : "text-ink-3" },
                ].map(({ label, count, color }) => (
                  <div key={label} className="rounded-control border border-line bg-inset py-3">
                    <div className={`text-[22px] font-bold ${color}`}>{count}</div>
                    <div className="text-[11px] text-ink-3 mt-0.5">{label}</div>
                  </div>
                ))}
              </div>
              {result.failed.length > 0 && (
                <details className="rounded-control border border-red/20 bg-red/5 px-3 py-2">
                  <summary className="text-[12px] text-red cursor-pointer">查看失败详情</summary>
                  <ul className="mt-2 flex flex-col gap-1">
                    {result.failed.map((f) => (
                      <li key={f.session_id} className="text-[11.5px] text-ink-2">
                        <code className="font-mono text-[11px]">{f.session_id.slice(0, 12)}</code>: {f.error}
                      </li>
                    ))}
                  </ul>
                </details>
              )}
              <p className="text-[12.5px] text-ink-2">
                原始 Provider 文件未被修改。已导入的会话标注了续聊状态，可在对话列表中查看。
              </p>
            </div>
          )}
        </div>

        {/* Footer */}
        <div className="flex items-center justify-end gap-2 border-t border-line bg-inset/40 px-5 py-3">
          {step === "config" && (
            <>
              <button
                type="button"
                onClick={onClose}
                className="h-8 rounded-control border border-line bg-surface px-4 text-[12px] font-medium text-ink hover:bg-hover"
              >
                取消
              </button>
              <button
                type="button"
                onClick={doScan}
                disabled={scanning}
                className="h-8 rounded-control bg-accent px-4 text-[12px] font-semibold text-white shadow-btn disabled:opacity-50"
              >
                {scanning ? "扫描中…" : "扫描历史"}
              </button>
            </>
          )}
          {step === "preview" && (
            <>
              <button
                type="button"
                onClick={() => { setStep("config"); setScanError(""); }}
                className="h-8 rounded-control border border-line bg-surface px-4 text-[12px] font-medium text-ink hover:bg-hover"
              >
                返回
              </button>
              <button
                type="button"
                onClick={doApply}
                disabled={applying || selected.size === 0}
                className="h-8 rounded-control bg-accent px-4 text-[12px] font-semibold text-white shadow-btn disabled:opacity-50"
              >
                {applying ? "导入中…" : `导入 ${selected.size} 个会话`}
              </button>
            </>
          )}
          {step === "result" && (
            <button
              type="button"
              onClick={onClose}
              className="h-8 rounded-control bg-accent px-4 text-[12px] font-semibold text-white shadow-btn"
            >
              完成
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
