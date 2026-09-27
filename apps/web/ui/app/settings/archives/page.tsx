"use client";

import Link from "next/link";
import { useCallback, useEffect, useMemo, useState } from "react";
import { Icon } from "@/components/Icon";
import {
  fetchConversationProjects,
  fetchConversationThreads,
  sendConversationCommand,
  waitForTerminalReceipt,
  type ConversationProject,
  type ConversationThread,
} from "@/lib/useConversation";

export default function ArchivesSettingsPage() {
  const [threads, setThreads] = useState<ConversationThread[]>([]);
  const [projects, setProjects] = useState<ConversationProject[]>([]);
  const [query, setQuery] = useState("");
  const [loading, setLoading] = useState(true);
  const [pendingId, setPendingId] = useState("");
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");

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

  useEffect(() => { void refresh(); }, [refresh]);

  const projectNames = useMemo(() => new Map(projects.map((project) => [project.project_id, project.name])), [projects]);
  const archived = useMemo(() => threads
    .filter((thread) => thread.state.status === "archived")
    .sort((a, b) => String(b.updated_at || b.created_at || "").localeCompare(String(a.updated_at || a.created_at || ""))), [threads]);
  const matching = useMemo(() => {
    const needle = query.trim().toLowerCase();
    if (!needle) return archived;
    return archived.filter((thread) => [
      thread.title,
      thread.summary,
      thread.thread_id,
      thread.project_id ? projectNames.get(thread.project_id) : "",
    ].some((value) => String(value || "").toLowerCase().includes(needle)));
  }, [archived, projectNames, query]);

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
    <section className="settings-data-page" aria-label="归档管理">
      <div className="settings-data-toolbar">
        <label className="settings-data-search">
          <Icon name="search" size={16} />
          <input value={query} onChange={(event) => setQuery(event.target.value)} placeholder="搜索标题、项目或会话 ID" aria-label="搜索已归档对话" />
        </label>
        <button type="button" className="settings-data-secondary" onClick={() => void refresh()} disabled={loading || Boolean(pendingId)}>
          <Icon name="refresh" size={15} />刷新
        </button>
      </div>
      <div className="settings-data-list-head">
        <span>已归档对话</span>
        <span>{archived.length}</span>
      </div>
      <div role="status" aria-live="polite" className="settings-data-feedback">
        {error ? <p className="is-error">{error}</p> : notice ? <p>{notice}</p> : null}
      </div>
      {loading ? <p className="settings-data-empty">正在加载归档…</p> : matching.length ? (
        <ul className="settings-archive-list">
          {matching.map((thread) => (
            <li key={thread.thread_id} className="settings-archive-row">
              <div className="settings-archive-info">
                <strong>{thread.title || "未命名对话"}</strong>
                <span>
                  {thread.project_id ? projectNames.get(thread.project_id) || "未命名项目" : "未归入项目"}
                  {thread.updated_at || thread.created_at ? ` · ${new Date(thread.updated_at || thread.created_at || "").toLocaleString()}` : ""}
                </span>
                {thread.summary ? <p>{thread.summary}</p> : null}
              </div>
              <div className="settings-archive-actions">
                <Link href={`/chat/${encodeURIComponent(thread.thread_id)}`} className="settings-data-secondary">打开</Link>
                <button type="button" className="settings-data-primary" disabled={Boolean(pendingId)} onClick={() => void restore(thread)}>
                  {pendingId === thread.thread_id ? "恢复中…" : "取消归档"}
                </button>
              </div>
            </li>
          ))}
        </ul>
      ) : <p className="settings-data-empty">{query ? "没有匹配的归档对话" : "暂无已归档对话"}</p>}
    </section>
  );
}
