"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import { apiFetch } from "@/lib/useRun";
import { desktopChatBridge } from "@/lib/desktopChatBridge";
import { visualizationSelectionText } from "@/lib/visualizationFollowup";
import { conversationStorageKey, conversationStorageScope } from "@/lib/conversationStorageScope";
import { Button, Dialog } from "@/components/chat/ui";
import { visualizationBridge } from "./visualizationBridge";

type TweakControl = { id: number; type: "slider" | "color" | "toggle" | "select"; group: string; label: string; value: string | number | boolean; initial?: string | number | boolean; min?: number; max?: number; step?: number; unit?: string; options?: (string | { label: string; value: string })[] };
type Followup = { id: number; prompt: string; title: string; modelContent?: unknown; documentNonce: string; threadId: string; path: string; scope: string };


const CDN = "https://cdnjs.cloudflare.com https://esm.sh https://cdn.jsdelivr.net https://unpkg.com";
const CSP = `default-src 'none'; script-src 'unsafe-inline' ${CDN}; style-src 'unsafe-inline' ${CDN} https://fonts.googleapis.com https://fonts.bunny.net; font-src https://fonts.gstatic.com https://fonts.bunny.net; img-src data: blob: ${CDN}; connect-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'`;
const STYLE = `
:root { color-scheme: light dark; --background: light-dark(#fff,#18191b); --foreground: light-dark(#25262a,#e7e7e9);
--card: light-dark(#f6f6f7,#24252a); --card-foreground: var(--foreground); --muted: var(--card); --muted-foreground: light-dark(#64656b,#acadb3);
--popover: var(--card); --popover-foreground: var(--foreground); --primary: var(--foreground); --primary-foreground: var(--background);
--secondary: var(--card); --secondary-foreground: var(--foreground); --accent: var(--card); --accent-foreground: var(--foreground);
--border: light-dark(#dddde1,#3e3f45); --input: var(--border); --ring: #709be9; --destructive: #d95c5c;
--viz-series-1:#729bdb; --viz-series-2:#62ae97; --viz-series-3:#d5a354; --viz-series-4:#b18dd7; --viz-series-5:#d5839b; --viz-series-6:#76baca;
--blue:var(--viz-series-1); --green:var(--viz-series-2); --orange:var(--viz-series-3); --purple:var(--viz-series-4); --red:var(--destructive); --yellow:#d6c26e; --font-size-base:14px; }
* { box-sizing:border-box } body { margin:0; padding:12px 0; background:transparent; color:var(--foreground); font:14px/1.5 system-ui,sans-serif; overflow-wrap:anywhere }
svg,canvas,img { max-width:100% } h1,h2,h3 { font-size:16px; font-weight:500 } .card { padding:14px; background:var(--card); border-radius:10px }
.viz-row,.viz-controls { display:flex; align-items:center; flex-wrap:wrap; gap:12px } .viz-grid { display:grid; grid-template-columns:repeat(auto-fit,minmax(min(180px,100%),1fr)); gap:12px }
.text-muted { color:var(--muted-foreground) } .text-small { font-size:12px } .text-destructive { color:var(--destructive) } .text-end{text-align:right} .text-center{text-align:center} .text-nowrap{white-space:nowrap} .tabular-nums{font-variant-numeric:tabular-nums}
.btn,.form-control,.form-select { font:inherit; color:var(--foreground); background:var(--card); border:1px solid var(--border); border-radius:7px; padding:6px 10px; max-width:100% }
.btn{cursor:pointer} .btn-primary { background:var(--primary);color:var(--primary-foreground) } .btn-ghost { background:transparent;border-color:transparent } .btn-block{width:100%}
.form-label { display:block; font-size:12px } .form-range { max-width:100% } .form-check { display:flex; align-items:center; gap:7px } .nav{display:flex;gap:6px;flex-wrap:wrap}
.nav-link{font:inherit;color:var(--foreground);background:transparent;border:0;padding:7px 12px;border-radius:7px;cursor:pointer} .nav-link.active{background:var(--card)}
.table { width:100%;border-collapse:collapse } .table td,.table th{padding:8px;text-align:start;border-bottom:1px solid var(--border)} .table-responsive{overflow:auto}
.sr-only{position:absolute;width:1px;height:1px;padding:0;overflow:hidden;clip:rect(0,0,0,0)} hr{border:0;border-top:1px solid var(--border)}
`;

export function parseVisualization(text: string): { path: string; title?: string } | null {
  const match = /^(?:visualize(\{[^\n]*\})|(?:muteki-visualize|visualize)\s+(\{[^\n]*\}))$/.exec(text.trim());
  if (!match) return null;
  try {
    const value = JSON.parse(match[1] || match[2]);
    return typeof value.path === "string" ? { path: value.path, title: typeof value.title === "string" ? value.title : undefined } : null;
  } catch { return null; }
}

export function ChatVisualization({ threadId, path, title, streaming }: { threadId: string; path: string; title?: string; streaming: boolean }) {
  const frame = useRef<HTMLIFrameElement>(null);
  const [documentNonce, setDocumentNonce] = useState("");
  const [contentRevision, setContentRevision] = useState("");
  const [nativeUrl, setNativeUrl] = useState("");
  const [nativeError, setNativeError] = useState("");
  const native = Boolean(desktopChatBridge());
  const scope = conversationStorageScope();
  const currentDocument = useRef({ documentNonce, threadId, path, scope });
  currentDocument.current = { documentNonce, threadId, path, scope };
  const [theme, setTheme] = useState("dark");
  useEffect(() => {
    const sync = () => setTheme(document.documentElement.dataset.theme === "light" ? "light" : "dark");
    sync();
    const observer = new MutationObserver(sync);
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });
    return () => observer.disconnect();
  }, []);
  const [html, setHtml] = useState<string | null>(null);
  const [error, setError] = useState("");
  const [assets, setAssets] = useState<Record<string, string>>({});
  const [controls, setControls] = useState<TweakControl[]>([]);
  const [tweakOpen, setTweakOpen] = useState(false);
  const [originalPreview, setOriginalPreview] = useState(false);
  const [followup, setFollowup] = useState<Followup | null>(null);
  const [external, setExternal] = useState("");
  const [sending, setSending] = useState(false);
  const [selectionText, setSelectionText] = useState("");
  const [selectionError, setSelectionError] = useState("");
  useEffect(() => {
    try { setSelectionText(visualizationSelectionText(followup?.modelContent)); setSelectionError(""); }
    catch (failure) { setSelectionText(""); setSelectionError(failure instanceof Error ? failure.message : String(failure)); }
  }, [followup]);
  const reply = (id: number, ok: boolean, error = "") => frame.current?.contentWindow?.postMessage({ type: "muteki:viz:result", documentNonce, id, ok, error }, "*");
  const [height, setHeight] = useState(360);
  const key = conversationStorageKey(`muteki:visualization:${threadId}:${path}:${contentRevision}`);
  useEffect(() => {
    setHtml(null); setError(""); setControls([]); setFollowup(null); setExternal(""); setTweakOpen(false); setOriginalPreview(false); setDocumentNonce(""); setContentRevision(""); setNativeUrl(""); setNativeError("");
    if (streaming) return;
    const abort = new AbortController();
    apiFetch(`/api/chat-plugins/visualizations/${encodeURIComponent(threadId)}?path=${encodeURIComponent(path)}`, { signal: abort.signal })
      .then(async (r) => { const body = await r.text(); if (!r.ok) throw new Error(`图形暂不可用（HTTP ${r.status}）：${body}`); try { return JSON.parse(body); } catch { throw new Error(`visualization.protocol.invalid_json: ${body}`); } })
      .then(async (v) => {
        if (typeof v.html !== "string" || (v.assets !== undefined && (!v.assets || typeof v.assets !== "object" || Object.values(v.assets).some((value) => typeof value !== "string")))) throw new Error("visualization.protocol.invalid_document: 缺少完整图形正文或资源");
        const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(v.html + JSON.stringify(v.assets || {})));
        if (!abort.signal.aborted) {
          setContentRevision(Array.from(new Uint8Array(digest)).map((byte) => byte.toString(16).padStart(2, "0")).join(""));
          setDocumentNonce(crypto.randomUUID()); setHtml(v.html); setAssets(v.assets || {});
        }
      })
      .catch((e) => { if (!abort.signal.aborted) setError(String(e.message)); });
    return () => abort.abort();
  }, [threadId, path, streaming, scope]);
  useEffect(() => {
    const receive = (event: MessageEvent) => {
      if (event.source !== frame.current?.contentWindow || !documentNonce) return;
      const msg = event.data;
      if (msg?.documentNonce !== documentNonce) return;
      if (msg?.type === "muteki:viz:resize" && Number.isFinite(msg.height)) setHeight(Math.min(2400, Math.max(120, msg.height)));
      if (msg?.type === "muteki:viz:ready") {
        try { frame.current?.contentWindow?.postMessage({ type: "muteki:viz:state", documentNonce, value: JSON.parse(localStorage.getItem(key) || "null") }, "*"); } catch { /* Storage is optional. */ }
      }
      if (msg?.type === "muteki:viz:state-write") {
        try {
          const encoded = JSON.stringify(msg.value);
          if (!msg.value || Array.isArray(msg.value) || new TextEncoder().encode(encoded).length > 16384) throw new Error("状态过大或格式无效");
          localStorage.setItem(key, encoded); reply(msg.id, true);
        } catch { reply(msg.id, false, "无法保存交互状态"); }
      }
      if (msg?.type === "muteki:viz:tweak-add" && msg.control && Number.isFinite(msg.control.id)
        && ["slider", "color", "toggle", "select"].includes(msg.control.type)) {
        if (!["string", "number", "boolean"].includes(typeof msg.control.value)) return;
        const control = { ...msg.control, group: String(msg.control.group || "设计控件"),
          label: String(msg.control.label || "控件"),
          options: Array.isArray(msg.control.options) ? msg.control.options.flatMap((o: unknown) =>
            typeof o === "string" ? [o] : o && typeof o === "object" && typeof (o as {value?:unknown}).value === "string"
              ? [{value:String((o as {value:string}).value),label:String((o as {label?:unknown}).label || (o as {value:string}).value)}] : []) : [],
        } as TweakControl;
        setControls((old) => old.some((c) => c.id === control.id) || old.length >= 144 ? old : [...old, { ...control, initial: control.value }]);
      }
      if (msg?.type === "muteki:viz:tweak-remove" && Array.isArray(msg.ids)) setControls((old) => old.filter((c) => !msg.ids.includes(c.id)));
      if (msg?.type === "muteki:viz:followup" && typeof msg.prompt === "string" && msg.prompt.trim()) {
        if (msg.prompt.length > 16000) { reply(msg.id, false, "visualization.followup.too_large: 后续消息超过 16000 字符，未截断或发送"); return; }
        setFollowup((old) => {
          if (old) { reply(msg.id, false, "已有待确认消息"); return old; }
          return { id: msg.id, prompt: msg.prompt, title: String(msg.title || "放入输入框"), modelContent: msg.modelContent, documentNonce, threadId, path, scope };
        });
      }
      if (msg?.type === "muteki:viz:external" && typeof msg.href === "string") {
        try { const url = new URL(msg.href); if (["http:", "https:"].includes(url.protocol) && !url.username && !url.password) setExternal(url.href); } catch { /* Reject non-web destinations. */ }
      }
    };
    window.addEventListener("message", receive);
    return () => window.removeEventListener("message", receive);
  }, [key, documentNonce, threadId, path, scope]);
  const srcDocument = useMemo(() => {
    if (html === null) return "";
    const script = (source: string) => "<script>" + source.replaceAll("</script", "<\\/script") + "</script>";
    const content = assets["visualize.html"]?.replace("<!--__INLINE_VISUALIZATION_FRAGMENT__-->", html) || html;
    return `<!doctype html><html style="color-scheme:${theme}"><head><meta charset="utf-8"><meta http-equiv="Content-Security-Policy" content="${CSP}"><meta name="referrer" content="no-referrer"><style>${STYLE} ${assets["visualize.css"] || ""} :root{color-scheme:${theme}}</style><script id="codex-visualization-lucide" src="https://cdn.jsdelivr.net/npm/lucide@0.468.0/dist/umd/lucide.min.js"></script>${script(visualizationBridge(documentNonce))}${assets["calendar.js"] ? script(assets["calendar.js"]) : ""}</head><body>${content}</body></html>`;
  // Freeze a document until its content changes; theme updates use the bridge.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [html, assets, documentNonce]);
  useEffect(() => {
    frame.current?.contentWindow?.postMessage({ type: "muteki:viz:theme", documentNonce, value: theme }, "*");
  }, [theme, documentNonce]);
  useEffect(() => {
    if (!native || !srcDocument || !documentNonce) return;
    const bridge = desktopChatBridge();
    let live = true; let id = "";
    setNativeUrl(""); setNativeError("");
    if (!bridge?.createVisualization || !bridge.releaseVisualization) { setNativeError("desktop.visualization.unavailable: 桌面图形文档传输未配置"); return; }
    void bridge.createVisualization({ threadId, html: srcDocument }).then((document) => {
      if (!document?.id || !document.url) throw new Error("desktop.visualization.invalid_reply: 图形文档缺少身份或地址");
      id = document.id;
      if (!live) { void bridge.releaseVisualization!(id).catch((failure) => console.error("desktop.visualization.release_failed", failure)); return; }
      setNativeUrl(document.url);
    }).catch((failure) => { if (live) setNativeError(failure instanceof Error ? failure.message : String(failure)); });
    return () => { live = false; if (id) void bridge.releaseVisualization!(id).catch((failure) => console.error("desktop.visualization.release_failed", failure)); };
  }, [native, srcDocument, threadId, documentNonce]);
  const updateControl = (control: TweakControl, value: string | number | boolean, reset = false) => {
    frame.current?.contentWindow?.postMessage({ type: "muteki:viz:tweak-set", documentNonce, id: control.id, value, reset }, "*");
    setControls((old) => old.map((c) => c.id === control.id ? { ...c, value } : c));
  };
  const previewOriginal = (preview: boolean) => {
    setOriginalPreview(preview);
    for (const c of controls) frame.current?.contentWindow?.postMessage({ type: "muteki:viz:tweak-set", documentNonce, id: c.id, value: preview ? c.initial : c.value }, "*");
  };
  const cancelFollowup = () => { if (followup) reply(followup.id, false, "用户取消"); setFollowup(null); };
  const sendFollowup = async () => {
    if (!followup || sending) return;
    setSending(true);
    try {
      const current = currentDocument.current;
      if (followup.documentNonce !== current.documentNonce || followup.threadId !== current.threadId || followup.path !== current.path || followup.scope !== current.scope) throw new Error("visualization.followup.expired: 图形来源已改变，请重新确认");
      const selection = visualizationSelectionText(followup.modelContent);
      const request = { threadId, path, documentNonce, scope, text: followup.prompt + selection, accepted: false, error: "" };
      window.dispatchEvent(new CustomEvent("muteki:visualization-followup", { detail: request }));
      if (!request.accepted) throw new Error(request.error || "visualization.composer.unavailable: 当前聊天输入框不可用");
      reply(followup.id, true); setFollowup(null);
    } catch (failure) { reply(followup.id, false, String(failure)); setError(failure instanceof Error ? failure.message : String(failure)); }
    finally { setSending(false); }
  };
  if (error && html === null) return <p role="alert" className="whitespace-pre-wrap break-words text-xs text-cx-fg-3">{error}</p>;
  if (html === null) return <p className="text-xs text-cx-fg-3">正在准备交互图…</p>;
  return <div>
    {controls.length > 0 && <div className="mb-2 flex justify-end"><Button variant="ghost" size="sm" onClick={() => setTweakOpen(true)}>调整设计</Button></div>}
    {error || nativeError ? <p role="alert" className="mb-2 whitespace-pre-wrap break-words text-xs text-cx-warning">{error || nativeError}</p> : null}
    {native && !nativeUrl ? <p className="text-xs text-cx-fg-3">正在准备隔离的桌面图形文档…</p> : <iframe key={documentNonce} ref={frame} title={title || "交互图"} sandbox="allow-scripts" referrerPolicy="no-referrer" src={native ? nativeUrl : undefined} srcDoc={native ? undefined : srcDocument} className="w-full border-0" style={{ height }} data-testid="chat-visualization" />}
    <Dialog open={tweakOpen} onOpenChange={(open) => { if (!open && originalPreview) previewOriginal(false); setTweakOpen(open); }} title="调整设计" footer={<>
      <Button variant="ghost" onClick={() => { previewOriginal(false); controls.forEach((c) => updateControl(c, c.initial ?? c.value, true)); }}>重置</Button>
      <Button variant="ghost" onClick={() => previewOriginal(!originalPreview)}>{originalPreview ? "查看修改" : "查看原始"}</Button>
      <Button variant="outline" onClick={() => { previewOriginal(false); setTweakOpen(false); setFollowup({ id: 0, documentNonce, threadId, path, scope, title: "放入输入框", prompt: "请按这些设计控件的选择继续调整。", modelContent: controls.map((c) => ({ group: c.group, label: c.label, value: c.value })) }); }}>放入输入框</Button>
      <Button onClick={() => { previewOriginal(false); setTweakOpen(false); }}>完成</Button></>}>
      <div className="space-y-4">{controls.map((c) => <label key={c.id} className="block space-y-2 text-sm"><span>{c.group} · {c.label} {c.type === "slider" ? `${c.value}${c.unit || ""}` : ""}</span>
        {c.type === "select" ? <select aria-label={c.label} value={String(c.value)} className="w-full rounded border border-cx-border bg-cx-bg px-2 py-1" onChange={(e) => updateControl(c, e.target.value)}>{(c.options || []).map((o) => <option key={typeof o === "string" ? o : o.value} value={typeof o === "string" ? o : o.value}>{typeof o === "string" ? o : o.label}</option>)}</select>
          : <input aria-label={c.label} type={c.type === "toggle" ? "checkbox" : c.type === "slider" ? "range" : "color"} className={c.type === "slider" ? "w-full" : ""} min={c.min} max={c.max} step={c.step} checked={c.type === "toggle" ? Boolean(c.value) : undefined} value={c.type === "toggle" ? undefined : String(c.value)} onChange={(e) => updateControl(c, c.type === "toggle" ? e.target.checked : c.type === "slider" ? Number(e.target.value) : e.target.value)} />}
        {c.type === "color" && <input aria-label={`${c.label}（十六进制）`} value={String(c.value)} className="ml-3 rounded border border-cx-border bg-cx-bg px-2 py-1" onChange={(e) => updateControl(c, e.target.value)} />}
      </label>)}</div>
    </Dialog>
    <Dialog open={!!followup} onOpenChange={(open) => { if (!open && !sending) cancelFollowup(); }} title={followup?.title || "放入输入框"} description="确认后把完整内容放入当前聊天输入框，再通过普通发送按钮发送。" footer={<><Button variant="ghost" disabled={sending} onClick={cancelFollowup}>取消</Button><Button loading={sending} disabled={Boolean(selectionError)} onClick={() => void sendFollowup()}>放入输入框</Button></>}>
      <p className="whitespace-pre-wrap text-sm">{followup?.prompt}</p>
      {selectionError ? <p role="alert" className="mt-3 whitespace-pre-wrap break-words text-xs text-cx-warning">{selectionError}</p> : selectionText ? <pre className="mt-3 max-h-40 overflow-auto whitespace-pre-wrap text-xs">{selectionText}</pre> : null}
    </Dialog>
    <Dialog open={!!external} onOpenChange={(open) => { if (!open) setExternal(""); }} title="打开外部链接" footer={<><Button variant="ghost" onClick={() => setExternal("")}>取消</Button><a href={external || undefined} target="_blank" rel="noopener noreferrer" className="text-sm underline" onClick={() => setExternal("")}>打开链接</a></>}><p className="break-all text-sm">{external}</p></Dialog>
  </div>;
}
