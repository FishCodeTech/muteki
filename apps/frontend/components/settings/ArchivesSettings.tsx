"use client";

import { SettingsLink as Link } from "./SettingsHost";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Button, SearchInput, Select } from "@muteki/ui";
import { Icon } from "@muteki/ui";
import { SettingsEmpty, SettingsSection } from "@/components/settings/primitives";
import { isInternalCtfThread } from "@/lib/conversationInbox";
import {
  fetchConversationProjects,
  fetchConversationThreads,
  sendConversationCommand,
  waitForTerminalReceipt,
  type ConversationProject,
  type ConversationThread,
} from "@/lib/useConversation";

export default function ArchivesSettings() {
  const [threads, setThreads] = useState<ConversationThread[]>([]);
  const [projects, setProjects] = useState<ConversationProject[]>([]);
  const [query, setQuery] = useState("");
  const [loading, setLoading] = useState(true);
  const [pendingId, setPendingId] = useState("");
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [srcTasks, setSrcTasks] = useState<Array<{ task_id: string; title: string }>>([]);
  const [handoffTaskId, setHandoffTaskId] = useState("");
  const [handoffFormat, setHandoffFormat] = useState<"tar.zst" | "tar.gz">("tar.zst");
  const [handoffBusy, setHandoffBusy] = useState(false);
  const importInputRef = useRef<HTMLInputElement | null>(null);

  const refresh = useCallback(async () => {
    try {
      const [nextThreads, nextProjects] = await Promise.all([
        fetchConversationThreads(),
        fetchConversationProjects().catch(() => [] as ConversationProject[]),
      ]);
      setThreads(nextThreads);
      setProjects(nextProjects);
      setError("");
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "加载归档失败");
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh();
    void fetch("/api/src/tasks").then((r) => (r.ok ? r.json() : null)).then((d: Array<{ task_id: string; title: string }> | { tasks?: Array<{ task_id: string; title: string }> } | null) => {
      const list = Array.isArray(d) ? d : (d?.tasks ?? []);
      setSrcTasks(list);
      if (list[0]?.task_id) setHandoffTaskId(list[0].task_id);
    }).catch(() => {});
  }, [refresh]);

  const projectNames = useMemo(() => new Map(projects.map((project) => [project.project_id, project.name])), [projects]);
  const archived = useMemo(() => threads
    .filter((thread) => thread.mode !== "src" && !isInternalCtfThread(thread) && thread.state.status === "archived")
    .sort((a, b) => String(b.updated_at || b.created_at || "").localeCompare(String(a.updated_at || a.created_at || ""))), [threads]);
  const matchingIds = useMemo(() => {
    const needle = query.trim().toLowerCase();
    if (!needle) return null;
    return new Set(archived.filter((thread) => [
      thread.title,
      thread.summary,
      thread.thread_id,
      thread.project_id ? projectNames.get(thread.project_id) : "",
    ].some((value) => String(value || "").toLowerCase().includes(needle))).map((thread) => thread.thread_id));
  }, [archived, projectNames, query]);
  const hasMatchingThreads = (matchingIds?.size ?? archived.length) > 0;

  const restore = async (thread: ConversationThread) => {
    if (pendingId) return;
    setPendingId(thread.thread_id);
    setError("");
    setNotice("");
    try {
      const initial = await sendConversationCommand(thread.thread_id, "conversation.thread.unarchive");
      const receipt = await waitForTerminalReceipt(initial, "取消归档");
      if (receipt.state !== "completed") {
        throw new Error(receipt.error?.message || "恢复尚未完成，请刷新后核对状态");
      }
      await refresh();
      setNotice(`已恢复「${thread.title || "未命名对话"}」；执行不会自动继续。`);
    } catch (cause) {
      await refresh();
      setError(cause instanceof Error ? cause.message : "恢复失败");
    } finally {
      setPendingId("");
    }
  };

  return (
    <section className="cx-settings-data flex flex-col gap-6" aria-label="归档管理">
      <div className="cx-data-toolbar">
        <SearchInput size="md" className="cx-data-search" value={query} onValueChange={setQuery} placeholder="搜索标题、项目或对话 ID" aria-label="搜索已归档对话" />
        <button type="button" className="cx-data-btn" onClick={() => void refresh()} disabled={loading || Boolean(pendingId)}>
          <Icon name="refresh" size={15} />刷新
        </button>
      </div>
      <div role="status" aria-live="polite" className="cx-data-feedback">
        {error ? <p className="is-error">{error}</p> : notice ? <p>{notice}</p> : null}
      </div>
      <SettingsSection title={<>已归档对话<span className="cx-data-count"> · {archived.length}</span></>}>
        {loading ? <SettingsEmpty>正在加载归档…</SettingsEmpty> : <>
          <ul className="cx-archive-list" hidden={!hasMatchingThreads}>
            {archived.map((thread) => (
              <li key={thread.thread_id} className="cx-archive-row" hidden={matchingIds !== null && !matchingIds.has(thread.thread_id)}>
                <div className="cx-archive-info">
                  <strong>{thread.title || "未命名对话"}</strong>
                  <span>
                    {thread.project_id ? projectNames.get(thread.project_id) || "未命名项目" : "未归入项目"}
                    {thread.updated_at || thread.created_at ? ` · ${new Date(thread.updated_at || thread.created_at || "").toLocaleString()}` : ""}
                  </span>
                  {thread.summary ? <p>{thread.summary}</p> : null}
                </div>
                <div className="cx-archive-actions">
                  <Link href={`/chat/${encodeURIComponent(thread.thread_id)}`} className="cx-data-btn">打开</Link>
                  <button type="button" className="cx-data-btn" disabled={Boolean(pendingId)} onClick={() => void restore(thread)}>
                    {pendingId === thread.thread_id ? "恢复中…" : "取消归档"}
                  </button>
                </div>
              </li>
            ))}
          </ul>
          {!hasMatchingThreads && <SettingsEmpty>{query ? "没有匹配的归档对话" : "暂无已归档对话"}</SettingsEmpty>}
        </>}
      </SettingsSection>

      <SettingsSection title="SRC 团队脱密移交归档（.tar.zst / .tar.gz）" anchor="archives-src-handoff">
        <div className="flex flex-wrap items-center gap-2 px-5 py-4">
          <Select<string>
            value={srcTasks.length ? handoffTaskId : null}
            onChange={setHandoffTaskId}
            ariaLabel="选择移交 SRC 任务"
            placeholder="暂无 SRC 任务"
            disabled={!srcTasks.length || handoffBusy}
            searchable={srcTasks.length > 8}
            className="h-8 w-full max-w-[280px] sm:w-auto sm:min-w-[180px]"
            options={srcTasks.map((t) => ({ value: t.task_id, label: t.title || t.task_id }))}
          />
          <Select<"tar.zst" | "tar.gz">
            value={handoffFormat}
            onChange={setHandoffFormat}
            ariaLabel="归档压缩格式"
            disabled={handoffBusy}
            className="h-8"
            options={[
              { value: "tar.zst", label: ".tar.zst (Zstd)" },
              { value: "tar.gz", label: ".tar.gz (Gzip)" },
            ]}
          />
          <Button
            variant="secondary"
            size="md"
            disabled={!handoffTaskId || handoffBusy}
            onClick={async () => {
              setHandoffBusy(true);
              setError("");
              try {
                const resp = await fetch(`/api/src/tasks/${encodeURIComponent(handoffTaskId)}/commands/src.archive.export`, {
                  method: "POST", headers: { "Content-Type": "application/json", "x-muteki-csrf": "1" }, body: JSON.stringify({ format: handoffFormat }),
                });
                const d = await resp.json().catch(() => ({})) as { state?: string; error?: { message?: string }; output?: { archive_b64?: string; filename?: string; sha256?: string; media_type?: string }; result?: { archive_b64?: string; filename?: string; sha256?: string; media_type?: string } };
                const out = d.output ?? d.result;
                if (!resp.ok || d.state === "failed" || !out?.archive_b64) throw new Error(d.error?.message ?? `HTTP ${resp.status}`);
                const bin = atob(out.archive_b64);
                const buf = new Uint8Array(bin.length);
                for (let i = 0; i < bin.length; i++) buf[i] = bin.charCodeAt(i);
                const url = URL.createObjectURL(new Blob([buf], { type: out.media_type || (handoffFormat === "tar.zst" ? "application/zstd" : "application/gzip") }));
                const a = document.createElement("a");
                a.href = url;
                a.download = out.filename || `src-handoff-${handoffTaskId}.${handoffFormat}`;
                document.body.appendChild(a);
                a.click();
                a.remove();
                window.setTimeout(() => URL.revokeObjectURL(url), 0);
                setNotice(`已导出脱密移交包（${String(out.sha256 || "").slice(0, 12)}）`);
              } catch (err) {
                setError(err instanceof Error ? err.message : String(err));
              } finally {
                setHandoffBusy(false);
              }
            }}
          >
            导出 {handoffFormat}
          </Button>
          <Button
            variant="secondary"
            size="md"
            disabled={!handoffTaskId || handoffBusy}
            onClick={() => importInputRef.current?.click()}
          >
            导入归档
          </Button>
          <input
            ref={importInputRef}
            type="file"
            accept=".tar.zst,.zst,.tar.gz,.tgz,application/zstd,application/gzip"
            className="hidden"
            disabled={!handoffTaskId || handoffBusy}
            onChange={(e) => {
              const file = e.target.files?.[0];
              e.target.value = "";
              if (!file || !handoffTaskId) return;
              setHandoffBusy(true);
              setError("");
              void file.arrayBuffer().then(async (buf) => {
                const bytes = new Uint8Array(buf);
                let b64 = "";
                for (let i = 0; i < bytes.length; i += 0x8000) {
                  b64 += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
                }
                const resp = await fetch(`/api/src/tasks/${encodeURIComponent(handoffTaskId)}/commands/src.archive.import`, {
                  method: "POST", headers: { "Content-Type": "application/json", "x-muteki-csrf": "1" }, body: JSON.stringify({ archive_b64: btoa(b64) }),
                });
                const d = await resp.json().catch(() => ({})) as { state?: string; error?: { message?: string }; output?: { total_imported?: number; compression?: string; phase?: string }; result?: { total_imported?: number; compression?: string; phase?: string } };
                const out = d.output ?? d.result;
                if (!resp.ok || d.state === "failed") throw new Error(d.error?.message ?? `HTTP ${resp.status}`);
                setNotice(`已导入 ${out?.total_imported ?? 0} 条记录 （${out?.compression ?? ""} · ${out?.phase ?? "committed"}）`);
              }).catch((err) => {
                setError(err instanceof Error ? err.message : String(err));
              }).finally(() => setHandoffBusy(false));
            }}
          />
        </div>
      </SettingsSection>
    </section>
  );
}
