"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { Button, Callout, Checkbox, Dialog, TextField } from "@/components/chat/ui";
import { fetchConversationSearch, queryLooksSearchable, type ConversationSearchHit, type ConversationThread } from "@/lib/useConversation";
import { conversationStorageScope } from "@/lib/conversationStorageScope";
import { useConversationLocalPreview } from "@/lib/conversationPreviewMode";

export function ConversationSearchDialog({ open, query, onQueryChange, onClose, threads, onSelectThread, onSelectMessage, restrictSearchToThreads = false, threadLabel = "聊天", searchLabel = "搜索聊天" }: {
  open: boolean; query: string; onQueryChange: (query: string) => void; onClose: () => void;
  restrictSearchToThreads?: boolean;
  threadLabel?: string;
  searchLabel?: string;
  threads: ConversationThread[]; onSelectThread: (threadId: string) => void; onSelectMessage: (threadId: string, messageId: string) => void;
}) {
  const [allHits, setHits] = useState<ConversationSearchHit[]>([]);
  const [includeArchived, setIncludeArchived] = useState(false);
  const [includeSuperseded, setIncludeSuperseded] = useState(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const [nextOffset, setNextOffset] = useState<number | null>(null);
  const [retry, setRetry] = useState(0);
  const generation = useRef(0);
  const controller = useRef<AbortController | null>(null);
  const scope = conversationStorageScope();
  const load = useCallback(async (offset = 0) => {
    controller.current?.abort();
    const request = new AbortController(); controller.current = request;
    const version = ++generation.current;
    setLoading(true); setError("");
    try {
      const page = await fetchConversationSearch(query, { includeArchived, includeSuperseded, offset, limit: 30, signal: request.signal });
      if (request.signal.aborted || generation.current !== version || conversationStorageScope() !== scope) return;
      setHits((current) => offset ? [...new Map([...current, ...page.hits].map((hit) => [hit.message_id, hit])).values()] : page.hits);
      setNextOffset(page.has_more && typeof page.next_offset === "number" ? page.next_offset : null);
    } catch (failure) {
      if (request.signal.aborted || generation.current !== version) return;
      setError(failure instanceof Error ? failure.message : String(failure));
    } finally { if (generation.current === version && !request.signal.aborted) setLoading(false); }
  }, [includeArchived, includeSuperseded, query, scope]);
  const localPreview = useConversationLocalPreview();
  useEffect(() => {
    controller.current?.abort(); ++generation.current;
    setHits([]); setNextOffset(null); setError(""); setLoading(false);
    // Local previews only match the titles of their own threads.
    if (!open || !queryLooksSearchable(query) || localPreview) return;
    setLoading(true);
    const timer = window.setTimeout(() => void load(), 250);
    return () => { window.clearTimeout(timer); controller.current?.abort(); };
  }, [load, localPreview, open, query, retry]);
  const titles = threads.filter((thread) => (thread.state.status !== "archived" || includeArchived) && thread.title.toLocaleLowerCase().includes(query.toLocaleLowerCase()));
  const allowedIds = restrictSearchToThreads ? new Set(threads.map((thread) => thread.thread_id)) : null;
  const hits = allowedIds ? allHits.filter((hit) => allowedIds.has(hit.thread_id)) : allHits;
  return <Dialog open={open} onOpenChange={(next) => { if (!next) onClose(); }} title={searchLabel} description={`按标题或消息正文查找${threadLabel}；已替代消息作为审计记录单独打开。`} size="lg">
    <TextField label={`搜索${threadLabel}标题或消息正文`} value={query} onChange={(event) => onQueryChange(event.target.value)} data-autofocus />
    <div className="mt-3 flex flex-wrap gap-4"><Checkbox checked={includeArchived} onCheckedChange={setIncludeArchived} label={`包含已归档${threadLabel}`} /><Checkbox checked={includeSuperseded} onCheckedChange={setIncludeSuperseded} label="包含已替代审计消息" /></div>
    <div className="mt-3 max-h-[60vh] space-y-3 overflow-auto">
      {titles.length ? <section><p className="mb-1 text-xs text-cx-fg-4">标题匹配</p>{titles.map((thread) => <button key={thread.thread_id} type="button" className="block w-full rounded-lg px-3 py-2 text-left text-sm hover:bg-cx-hover" onClick={() => { onSelectThread(thread.thread_id); onClose(); }}>{thread.title || `未命名${threadLabel}`}{thread.state.status === "archived" ? " · 已归档" : ""}</button>)}</section> : null}
      {queryLooksSearchable(query) ? <section><p className="mb-1 text-xs text-cx-fg-4">消息正文{loading ? " · 正在加载…" : ` · 已加载 ${hits.length} 条`}</p>{hits.map((hit) => <button key={`${hit.thread_id}:${hit.message_id}`} type="button" className="block w-full rounded-lg px-3 py-2 text-left hover:bg-cx-hover" onClick={() => { onSelectMessage(hit.thread_id, hit.message_id); onClose(); }}><span className="block text-xs font-medium text-cx-fg-2">{hit.thread_title || `未命名${threadLabel}`} · {hit.role}{hit.archived ? " · 已归档" : ""}{hit.superseded ? " · 已替代审计记录" : ""}</span><span className="mt-1 block whitespace-pre-wrap break-words text-sm text-cx-fg-3">{hit.snippet}</span></button>)}{!loading && !error && !hits.length ? <p className="p-3 text-sm text-cx-fg-4">没有匹配的消息正文</p> : null}</section> : <p className="text-xs text-cx-fg-4">消息正文至少输入一个中文字符或两个其他字符。</p>}
      {error ? <Callout tone="danger" role="alert" title="搜索未完成"><pre className="whitespace-pre-wrap break-words text-xs">{error}</pre><Button size="xs" onClick={() => setRetry((value) => value + 1)}>重新搜索</Button></Callout> : null}
      {nextOffset !== null ? <Button size="sm" variant="secondary" loading={loading} disabled={loading} onClick={() => void load(nextOffset)}>加载更多消息结果</Button> : null}
    </div>
  </Dialog>;
}
