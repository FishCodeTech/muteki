"use client";
import { useCallback, useEffect, useState } from "react";
import { Button, Callout, Checkbox, Dialog } from "@/components/chat/ui";
import { ShareSnapshotView } from "./ShareSnapshotView";
import { shareDate, shareJson, sharePost, type SharePreview, type ShareRecord } from "@/lib/conversationShares";

const SELECT = "w-full rounded-lg border border-cx-border bg-cx-elevated p-2 text-sm text-cx-fg";
export function ShareDialog({ open, onClose, threadId }: { open: boolean; onClose: () => void; threadId: string }) {
  const [preview, setPreview] = useState<SharePreview | null>(null);
  const [choices, setChoices] = useState<SharePreview["choices"]>({ messages: [], attachments: [] });
  const [pageOffset, setPageOffset] = useState(0);
  const [totalMessages, setTotalMessages] = useState(0);
  const [hasOlder, setHasOlder] = useState(false);
  const [messageIds, setMessageIds] = useState<string[] | null>(null);
  const [attachmentIds, setAttachmentIds] = useState<string[]>([]);
  const [redactPaths, setRedactPaths] = useState(true);
  const [includeTools, setIncludeTools] = useState(false);
  const [access, setAccess] = useState<"" | "authenticated" | "link">("");
  const [ttl, setTtl] = useState(86400);
  const [reviewed, setReviewed] = useState(false);
  const [dirty, setDirty] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [link, setLink] = useState("");
  const [created, setCreated] = useState<ShareRecord | null>(null);
  const [shares, setShares] = useState<ShareRecord[]>([]);
  const base = `/api/threads/${encodeURIComponent(threadId)}/shares`;
  const refreshList = useCallback(async () => {
    const result = await shareJson<{ shares: ShareRecord[] }>(base);
    setShares(result.shares);
  }, [base]);
  const resetReview = () => { setDirty(true); setReviewed(false); setLink(""); setCreated(null); setError(""); };
  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    setPreview(null); setPageOffset(0); setTotalMessages(0); setHasOlder(false); setChoices({ messages: [], attachments: [] }); setMessageIds(null); setAttachmentIds([]);
    setRedactPaths(true); setIncludeTools(false); setAccess(""); setReviewed(false); setDirty(false);
    setLink(""); setCreated(null); setError(""); setNotice(""); setBusy(true);
    void Promise.allSettled([
      shareJson<{ choices: SharePreview["choices"]; initial_message_ids: string[] | null; total_message_count: number; has_older: boolean }>(`${base}/source`),
      shareJson<{ shares: ShareRecord[] }>(base),
    ]).then(async ([sourceResult, listResult]) => {
      if (cancelled) return;
      if (listResult.status === "fulfilled") setShares(listResult.value.shares);
      else setError("已有分享列表读取失败，请使用下方刷新列表重试");
      if (sourceResult.status !== "fulfilled") throw sourceResult.reason;
      const source = sourceResult.value;
      setChoices(source.choices); setMessageIds(source.initial_message_ids); setTotalMessages(source.total_message_count); setHasOlder(source.has_older);
      if (source.initial_message_ids) setNotice(`会话共 ${source.total_message_count} 条正文；每次最多分享 500 条，已选最近 500 条，可继续减少范围。`);
      const data = await shareJson<SharePreview>(`${base}/preview`, sharePost({ redact_paths: true, message_ids: source.initial_message_ids }));
      if (!cancelled) setPreview(data);
    }).catch((exc) => { if (!cancelled) setError(String(exc.message || exc)); })
      .finally(() => { if (!cancelled) setBusy(false); });
    return () => { cancelled = true; };
  }, [open, base]);
  const loadMessagePage = async (offset: number) => {
    setBusy(true); setError("");
    try {
      const source = await shareJson<{ choices: SharePreview["choices"]; total_message_count: number; has_older: boolean }>(`${base}/source?offset=${offset}`);
      setChoices((previous) => ({ ...previous, messages: source.choices.messages }));
      setPageOffset(offset); setTotalMessages(source.total_message_count); setHasOlder(source.has_older);
    } catch (exc) { setError(exc instanceof Error ? exc.message : "消息范围读取失败"); }
    finally { setBusy(false); }
  };
  const updatePreview = async () => {
    setBusy(true); setError(""); setReviewed(false);
    try {
      const data = await shareJson<SharePreview>(`${base}/preview`, sharePost({ message_ids: messageIds, attachment_ids: attachmentIds, redact_paths: redactPaths, include_tool_summaries: includeTools }));
      setPreview(data); setChoices((previous) => ({ ...previous, attachments: data.choices.attachments })); setDirty(false);
    } catch (exc) { setError(exc instanceof Error ? exc.message : "预览失败"); }
    finally { setBusy(false); }
  };
  const create = async () => {
    if (!preview || dirty || !access || !reviewed) return;
    setBusy(true); setError("");
    try {
      const result = await shareJson<ShareRecord & { token: string }>(base, sharePost({ preview_id: preview.preview_id, access_mode: access, expires_in_seconds: ttl }));
      setCreated(result);
      setLink(`${window.location.origin}/share/${result.share_id}#${result.token}`);
      setNotice("只读分享已创建。请复制链接；关闭后仍可在此撤销，重新分享需要生成新链接。");
      await refreshList();
    } catch (exc) { setError(exc instanceof Error ? exc.message : "创建失败，请刷新预览后重试"); }
    finally { setBusy(false); }
  };
  const copyLink = async () => {
    try {
      if (!navigator.clipboard) throw new Error("clipboard unavailable");
      await navigator.clipboard.writeText(link);
      setNotice("链接已复制");
    } catch { setError("复制失败，请选中上方链接手动复制"); }
  };
  const revoke = async (id: string) => {
    setBusy(true); setError("");
    try { await shareJson(`${base}/${encodeURIComponent(id)}`, { method: "DELETE" }); if (created?.share_id === id) { setLink(""); setCreated(null); setDirty(true); } setNotice("分享已撤销，后续正文与附件访问均已失效。"); await refreshList(); }
    catch (exc) { setError(exc instanceof Error ? exc.message : "撤销失败"); }
    finally { setBusy(false); }
  };
  return <Dialog open={open} onOpenChange={(next) => { if (!next && !busy) onClose(); }} dismissable={!busy} size="xl" title="分享只读快照" description="先选择并检查完整内容，再明确访问范围和有效期。" testId="conversation-share-dialog" footer={<><Button variant="ghost" disabled={busy} onClick={onClose}>关闭</Button><Button variant="secondary" disabled={busy || Boolean(created)} onClick={() => void updatePreview()}>更新预览</Button><Button variant="primary" loading={busy} disabled={busy || !preview || dirty || !reviewed || !access || Boolean(created)} onClick={() => void create()}>创建只读分享</Button></>}>
    <div className="min-w-0 space-y-5">
      {error ? <Callout tone="danger" role="alert" title="操作未完成">{error}</Callout> : null}
      {notice ? <p role="status" className="text-sm text-cx-fg-2">{notice}</p> : null}
      <fieldset disabled={busy || Boolean(created)} className="space-y-3"><legend className="mb-2 font-semibold">正文范围</legend>
        <label className="block text-sm">选择范围<select className={SELECT} value={messageIds === null ? "all" : "selected"} onChange={(e) => { setMessageIds(e.target.value === "all" ? null : []); setAttachmentIds([]); resetReview(); }}><option value="all" disabled={totalMessages > 500}>当前有效分支的全部正文{totalMessages > 500 ? "（超过单次上限，请分批选择）" : ""}</option><option value="selected">手动选择消息（上下文需自行勾选）</option></select></label>
        {messageIds !== null ? <><p className="text-xs text-cx-fg-3">请同时勾选需要分享的问题、回复及其他上下文。分享仅包含已勾选的消息，系统不会自动添加同轮问题。</p><div className="flex flex-wrap items-center gap-2 text-xs"><span>已选 {messageIds.length} / 500 条；共 {totalMessages} 条有效正文</span><Button size="sm" onClick={() => { setMessageIds([]); setAttachmentIds([]); resetReview(); }}>清空选择</Button><Button size="sm" disabled={busy || pageOffset === 0} onClick={() => void loadMessagePage(Math.max(0, pageOffset - 500))}>较新消息</Button><Button size="sm" disabled={busy || !hasOlder} onClick={() => void loadMessagePage(pageOffset + 500)}>更早消息</Button></div><div className="max-h-56 space-y-2 overflow-y-auto rounded-lg border border-cx-border p-3" role="group" aria-label="选择正文消息">{choices.messages.map((m) => <label className="flex items-start gap-2 text-sm" key={m.id}><input type="checkbox" disabled={!messageIds.includes(m.id) && messageIds.length >= 500} checked={messageIds.includes(m.id)} onChange={(e) => { setMessageIds(e.target.checked ? [...messageIds, m.id] : messageIds.filter((id) => id !== m.id)); setAttachmentIds([]); resetReview(); }} /><span className="min-w-0 break-words">{m.role === "user" ? "用户" : "助手"}：{m.preview || "（仅附件）"}</span></label>)}</div></> : null}
        <Checkbox checked={redactPaths} onCheckedChange={(v) => { setRedactPaths(v); resetReview(); }} label="脱敏绝对路径" description="处理正文、工具名称及所选文本附件；工具调用参数和输出始终排除。" />
        <Checkbox checked={includeTools} onCheckedChange={(v) => { setIncludeTools(v); resetReview(); }} label="包含工具名称与状态摘要" />
        <div role="group" aria-label="选择分享附件"><h3 className="mb-2 text-sm font-semibold">附件默认排除（最多 10 个，仅限 256 KiB 内 UTF-8 文本）</h3>{choices.attachments.length ? choices.attachments.map((a) => <label key={a.id} className="mb-2 flex items-start gap-2 text-sm"><input type="checkbox" disabled={Boolean(a.unavailable_reason)} checked={attachmentIds.includes(a.id)} onChange={(e) => { setAttachmentIds(e.target.checked ? [...attachmentIds, a.id] : attachmentIds.filter((id) => id !== a.id)); resetReview(); }} /><span className="break-words">{a.name} · {a.size} 字节{a.unavailable_reason ? ` · ${a.unavailable_reason}` : ""}</span></label>) : <p className="text-sm text-cx-fg-3">所选内容暂无可分享附件。</p>}</div>
      </fieldset>
      {dirty ? <Callout tone="warning" title="范围已改变">请更新预览，再检查最终可见内容。</Callout> : null}
      {preview ? <ShareSnapshotView snapshot={preview.snapshot} /> : <p role="status">{busy ? "正在准备预览…" : "请更新预览"}</p>}
      <fieldset disabled={busy || Boolean(created)} className="grid gap-3 sm:grid-cols-2"><legend className="mb-2 font-semibold">访问与到期</legend><label className="text-sm">访问范围<select value={access} className={SELECT} onChange={(e) => setAccess(e.target.value as typeof access)}><option value="">请明确选择</option><option value="authenticated">仅已登录的工作台用户</option><option value="link">任何持有完整链接的人</option></select></label><label className="text-sm">有效期<select className={SELECT} value={ttl} onChange={(e) => setTtl(Number(e.target.value))}><option value={3600}>1 小时</option><option value={86400}>1 天</option><option value={604800}>7 天</option></select></label></fieldset>
      <p className="text-xs text-cx-fg-3">{created ? `准确到期时间：${shareDate(created.expires_at)}` : `预计到期：${shareDate(Date.now() / 1000 + ttl)}；以创建后的准确时间为准。`} 分享仅授予当前快照读取权限，不能操作原会话或工作区。路径脱敏不能识别全部敏感信息，请检查正文与附件。撤销无法追回对方已保存的副本。</p>
      <Checkbox checked={reviewed} disabled={dirty || !preview || busy || Boolean(created)} onCheckedChange={setReviewed} label="我已检查上方完整正文和附件，确认分享范围" />
      {link ? <div className="space-y-2"><label className="block text-sm">新建分享链接<input readOnly className={SELECT} value={link} onFocus={(e) => e.currentTarget.select()} /></label><Button onClick={() => void copyLink()}>复制分享链接</Button></div> : null}
      <section aria-label="已有分享"><div className="mb-2 flex items-center justify-between gap-2"><h3 className="font-semibold">本会话的分享</h3><Button size="sm" disabled={busy} onClick={() => void refreshList().catch(() => setError("分享列表读取失败，请重试"))}>刷新列表</Button></div><div className="space-y-2">{shares.length ? shares.map((s) => <div key={s.share_id} className="flex flex-wrap items-center gap-2 rounded-lg border border-cx-border p-3 text-xs"><span className="min-w-0 flex-1">{s.message_count} 条正文 · 水位 #{s.watermark} · {s.access_mode === "link" ? "链接持有人" : "需登录"}<br />创建于 {shareDate(s.created_at)}<br />到期于 {shareDate(s.expires_at)} · {{ active: "有效", revoked: "已撤销", expired: "已过期" }[s.status || "expired"]}</span>{s.status === "active" ? <Button size="sm" disabled={busy} onClick={() => void revoke(s.share_id)}>立即撤销</Button> : null}</div>) : <p className="text-sm text-cx-fg-3">还没有分享。</p>}</div></section>
    </div>
  </Dialog>;
}
