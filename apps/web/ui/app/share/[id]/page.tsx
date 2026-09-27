"use client";
import { useCallback, useEffect, useRef, useState } from "react";
import { useParams } from "next/navigation";
import { ShareSnapshotView } from "@/components/conversation/ShareSnapshotView";
import { shareDate, shareJson, type ShareView } from "@/lib/conversationShares";
import { apiFetch, login } from "@/lib/useRun";

export default function SharedConversationPage() {
  const params = useParams<{ id: string }>();
  const id = String(params.id || "");
  const [token, setToken] = useState("");
  const requestSequence = useRef(0);
  const [view, setView] = useState<ShareView | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [needsLogin, setNeedsLogin] = useState(false);
  const [password, setPassword] = useState("");
  const load = useCallback(async (shareToken: string) => {
    const sequence = ++requestSequence.current;
    if (!/^[A-Za-z0-9_-]{24}$/.test(id) || !/^[A-Za-z0-9_-]{43}$/.test(shareToken)) { setView(null); setBusy(false); setNeedsLogin(false); setError("分享链接不完整，请向创建者索取完整链接。"); return; }
    setBusy(true); setError("");
    try {
      const result = await shareJson<ShareView>(`/api/conversation-shares/${id}`, { headers: { "X-Muteki-Share-Token": shareToken }, referrerPolicy: "no-referrer" });
      if (sequence !== requestSequence.current) return;
      setView(result); setNeedsLogin(false);
    } catch (exc) {
      if (sequence !== requestSequence.current) return;
      setView(null); setNeedsLogin((exc as { status?: number }).status === 401);
      setError(exc instanceof Error ? exc.message : "读取分享失败");
    } finally { if (sequence === requestSequence.current) setBusy(false); }
  }, [id]);
  useEffect(() => {
    const readFragment = () => { const value = window.location.hash.slice(1); setToken(value); setView(null); void load(value); };
    readFragment();
    window.addEventListener("hashchange", readFragment);
    return () => { requestSequence.current += 1; window.removeEventListener("hashchange", readFragment); };
  }, [load]);
  useEffect(() => {
    if (!view) return;
    const ms = Math.max(0, view.expires_at * 1000 - Date.now());
    const timer = window.setTimeout(() => { setView(null); setError("此分享已过期"); }, ms);
    return () => window.clearTimeout(timer);
  }, [view]);
  const download = async (attachmentId: string) => {
    const response = await apiFetch(`/api/conversation-shares/${id}/attachments/${encodeURIComponent(attachmentId)}`, { headers: { "X-Muteki-Share-Token": token }, cache: "no-store", referrerPolicy: "no-referrer" });
    if (!response.ok) { await load(token); throw new Error("附件不可用，分享可能已过期或撤销"); }
    const blob = await response.blob();
    const url = URL.createObjectURL(blob);
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = view?.snapshot.attachments.find((a) => a.id === attachmentId)?.name || "attachment.txt";
    anchor.click();
    window.setTimeout(() => URL.revokeObjectURL(url), 60_000);
  };
  return <main className="min-h-dvh overflow-x-hidden bg-cx-bg px-4 py-8 text-cx-fg sm:px-8" data-testid="conversation-share-page"><div className="mx-auto max-w-3xl space-y-6">
    <header><p className="text-xs font-semibold tracking-wide text-cx-fg-3">MUTEKI · 只读分享</p><h1 className="mt-2 text-2xl font-semibold">会话成果快照</h1><p className="mt-2 text-sm text-cx-fg-3">此页面只包含创建者选定的固定内容，不提供原对话、文件或终端操作权限。</p></header>
    {busy ? <p role="status">正在读取…</p> : null}
    {error ? <div role="alert" className="rounded-xl border border-cx-border p-4 text-sm">{error}</div> : null}
    {needsLogin ? <form className="space-y-3 rounded-xl border border-cx-border p-4" onSubmit={(event) => { event.preventDefault(); setBusy(true); void login(password).then(async (result) => { setPassword(""); if (!result.ok) throw new Error("登录失败，请检查密码"); await load(token); }).catch((exc) => setError(exc.message)).finally(() => setBusy(false)); }}><label className="block text-sm">工作台密码<input autoComplete="current-password" type="password" value={password} onChange={(e) => setPassword(e.target.value)} className="mt-2 block w-full rounded-lg border border-cx-border bg-cx-elevated p-2" /></label><button className="rounded-lg border border-cx-border px-4 py-2 text-sm" disabled={busy || !password}>登录并查看</button></form> : null}
    {!busy ? <button type="button" onClick={() => void load(token)} className="rounded-lg border border-cx-border px-3 py-2 text-sm">重新检查分享状态</button> : null}
    {view ? <><p className="text-xs text-cx-fg-3">访问：{view.access_mode === "link" ? "完整链接持有人" : "已登录的工作台用户"} · 到期：{shareDate(view.expires_at)}</p><ShareSnapshotView snapshot={view.snapshot} onDownload={(attachmentId) => void download(attachmentId).catch((exc) => setError(exc.message))} /></> : null}
  </div></main>;
}
