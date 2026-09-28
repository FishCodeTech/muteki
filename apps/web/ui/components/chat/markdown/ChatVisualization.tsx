"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import { apiFetch } from "@/lib/useRun";
import { sendConversationCommand } from "@/lib/useConversation";
import { Button, Dialog } from "@/components/chat/ui";
import { VISUALIZATION_BRIDGE } from "./visualizationBridge";

type TweakControl = { id: number; type: "slider" | "color" | "toggle" | "select"; group: string; label: string; value: string | number | boolean; initial?: string | number | boolean; min?: number; max?: number; step?: number; unit?: string; options?: (string | { label: string; value: string })[] };
type Followup = { id: number; prompt: string; title: string; modelContent?: unknown };


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
  const reply = (id: number, ok: boolean, error = "") => frame.current?.contentWindow?.postMessage({ type: "muteki:viz:result", id, ok, error }, "*");
  const [height, setHeight] = useState(360);
  const key = `muteki:visualization:${threadId}:${path}`;
  useEffect(() => {
    if (streaming) return;
    const abort = new AbortController();
    setHtml(null); setError(""); setControls([]);
    apiFetch(`/api/chat-plugins/visualizations/${encodeURIComponent(threadId)}?path=${encodeURIComponent(path)}`, { signal: abort.signal })
      .then(async (r) => { if (!r.ok) throw new Error("图形暂不可用，请确认文件已生成在当前对话目录。"); return r.json(); })
      .then((v) => { if (!abort.signal.aborted) { setHtml(v.html); setAssets(v.assets || {}); } })
      .catch((e) => { if (!abort.signal.aborted) setError(String(e.message)); });
    return () => abort.abort();
  }, [threadId, path, streaming]);
  useEffect(() => {
    const receive = (event: MessageEvent) => {
      if (event.source !== frame.current?.contentWindow) return;
      const msg = event.data;
      if (msg?.type === "muteki:viz:resize" && Number.isFinite(msg.height)) setHeight(Math.min(2400, Math.max(120, msg.height)));
      if (msg?.type === "muteki:viz:ready") {
        try { frame.current?.contentWindow?.postMessage({ type: "muteki:viz:state", value: JSON.parse(localStorage.getItem(key) || "null") }, "*"); } catch { /* Storage is optional. */ }
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
        const control = { ...msg.control, group: String(msg.control.group || "设计控件").slice(0,120),
          label: String(msg.control.label || "控件").slice(0,120),
          options: Array.isArray(msg.control.options) ? msg.control.options.slice(0,12).flatMap((o: unknown) =>
            typeof o === "string" ? [o] : o && typeof o === "object" && typeof (o as {value?:unknown}).value === "string"
              ? [{value:String((o as {value:string}).value),label:String((o as {label?:unknown}).label || (o as {value:string}).value)}] : []) : [],
        } as TweakControl;
        setControls((old) => old.some((c) => c.id === control.id) || old.length >= 144 ? old : [...old, { ...control, initial: control.value }]);
      }
      if (msg?.type === "muteki:viz:tweak-remove" && Array.isArray(msg.ids)) setControls((old) => old.filter((c) => !msg.ids.includes(c.id)));
      if (msg?.type === "muteki:viz:followup" && typeof msg.prompt === "string" && msg.prompt.trim() && msg.prompt.length <= 16000) {
        setFollowup((old) => {
          if (old) { reply(msg.id, false, "已有待确认消息"); return old; }
          return { id: msg.id, prompt: msg.prompt, title: String(msg.title || "发送后续消息").slice(0,250), modelContent: msg.modelContent };
        });
      }
      if (msg?.type === "muteki:viz:external" && typeof msg.href === "string") {
        try { const url = new URL(msg.href); if (["http:", "https:"].includes(url.protocol) && !url.username && !url.password) setExternal(url.href); } catch { /* Reject non-web destinations. */ }
      }
    };
    window.addEventListener("message", receive);
    return () => window.removeEventListener("message", receive);
  }, [key]);
  const srcDocument = useMemo(() => {
    if (html === null) return "";
    const script = (source: string) => "<script>" + source.replaceAll("</script", "<\\/script") + "</script>";
    const content = assets["visualize.html"]?.replace("<!--__INLINE_VISUALIZATION_FRAGMENT__-->", html) || html;
    return `<!doctype html><html style="color-scheme:${theme}"><head><meta charset="utf-8"><meta http-equiv="Content-Security-Policy" content="${CSP}"><meta name="referrer" content="no-referrer"><style>${STYLE} ${assets["visualize.css"] || ""} :root{color-scheme:${theme}}</style><script id="codex-visualization-lucide" src="https://cdn.jsdelivr.net/npm/lucide@0.468.0/dist/umd/lucide.min.js"></script>${script(VISUALIZATION_BRIDGE)}${assets["calendar.js"] ? script(assets["calendar.js"]) : ""}</head><body>${content}</body></html>`;
  }, [html, theme, assets]);
  const updateControl = (control: TweakControl, value: string | number | boolean, reset = false) => {
    frame.current?.contentWindow?.postMessage({ type: "muteki:viz:tweak-set", id: control.id, value, reset }, "*");
    setControls((old) => old.map((c) => c.id === control.id ? { ...c, value } : c));
  };
  const previewOriginal = (preview: boolean) => {
    setOriginalPreview(preview);
    for (const c of controls) frame.current?.contentWindow?.postMessage({ type: "muteki:viz:tweak-set", id: c.id, value: preview ? c.initial : c.value }, "*");
  };
  const cancelFollowup = () => { if (followup) reply(followup.id, false, "用户取消"); setFollowup(null); };
  const sendFollowup = async () => {
    if (!followup || sending) return;
    setSending(true);
    try {
      const selection = followup.modelContent == null ? "" : "\n\n[交互图当前选择]\n" + JSON.stringify(followup.modelContent).slice(0, 16000);
      await sendConversationCommand(threadId, "conversation.turn.send", { text: followup.prompt + selection });
      reply(followup.id, true); setFollowup(null);
    } catch (e) { reply(followup.id, false, String(e)); setError(e instanceof Error ? e.message : String(e)); }
    finally { setSending(false); }
  };
  if (error) return <p role="status" className="text-xs text-cx-fg-3">{error}</p>;
  if (html === null) return <p className="text-xs text-cx-fg-3">正在准备交互图…</p>;
  return <div>
    {controls.length > 0 && <div className="mb-2 flex justify-end"><Button variant="ghost" size="sm" onClick={() => setTweakOpen(true)}>调整设计</Button></div>}
    <iframe ref={frame} title={title || "交互图"} sandbox="allow-scripts" referrerPolicy="no-referrer" srcDoc={srcDocument} className="w-full border-0" style={{ height }} data-testid="chat-visualization" />
    <Dialog open={tweakOpen} onOpenChange={(open) => { if (!open && originalPreview) previewOriginal(false); setTweakOpen(open); }} title="调整设计" footer={<>
      <Button variant="ghost" onClick={() => { previewOriginal(false); controls.forEach((c) => updateControl(c, c.initial ?? c.value, true)); }}>重置</Button>
      <Button variant="ghost" onClick={() => previewOriginal(!originalPreview)}>{originalPreview ? "查看修改" : "查看原始"}</Button>
      <Button variant="outline" onClick={() => { previewOriginal(false); setTweakOpen(false); setFollowup({ id: 0, title: "发送设计调整", prompt: "请按这些设计控件的选择继续调整。", modelContent: controls.map((c) => ({ group: c.group, label: c.label, value: c.value })) }); }}>发送修改</Button>
      <Button onClick={() => { previewOriginal(false); setTweakOpen(false); }}>完成</Button></>}>
      <div className="space-y-4">{controls.map((c) => <label key={c.id} className="block space-y-2 text-sm"><span>{c.group} · {c.label} {c.type === "slider" ? `${c.value}${c.unit || ""}` : ""}</span>
        {c.type === "select" ? <select aria-label={c.label} value={String(c.value)} className="w-full rounded border border-cx-border bg-cx-bg px-2 py-1" onChange={(e) => updateControl(c, e.target.value)}>{(c.options || []).map((o) => <option key={typeof o === "string" ? o : o.value} value={typeof o === "string" ? o : o.value}>{typeof o === "string" ? o : o.label}</option>)}</select>
          : <input aria-label={c.label} type={c.type === "toggle" ? "checkbox" : c.type === "slider" ? "range" : "color"} className={c.type === "slider" ? "w-full" : ""} min={c.min} max={c.max} step={c.step} checked={c.type === "toggle" ? Boolean(c.value) : undefined} value={c.type === "toggle" ? undefined : String(c.value)} onChange={(e) => updateControl(c, c.type === "toggle" ? e.target.checked : c.type === "slider" ? Number(e.target.value) : e.target.value)} />}
        {c.type === "color" && <input aria-label={`${c.label}（十六进制）`} value={String(c.value)} className="ml-3 rounded border border-cx-border bg-cx-bg px-2 py-1" onChange={(e) => updateControl(c, e.target.value)} />}
      </label>)}</div>
    </Dialog>
    <Dialog open={!!followup} onOpenChange={(open) => { if (!open && !sending) cancelFollowup(); }} title={followup?.title || "发送后续消息"} footer={<><Button variant="ghost" disabled={sending} onClick={cancelFollowup}>取消</Button><Button loading={sending} onClick={() => void sendFollowup()}>发送</Button></>}>
      <p className="whitespace-pre-wrap text-sm">{followup?.prompt}</p>
      {followup?.modelContent != null && <pre className="mt-3 max-h-40 overflow-auto whitespace-pre-wrap text-xs">{JSON.stringify(followup.modelContent, null, 2)}</pre>}
    </Dialog>
    <Dialog open={!!external} onOpenChange={(open) => { if (!open) setExternal(""); }} title="打开外部链接" footer={<><Button variant="ghost" onClick={() => setExternal("")}>取消</Button><a href={external || undefined} target="_blank" rel="noopener noreferrer" className="text-sm underline" onClick={() => setExternal("")}>打开链接</a></>}><p className="break-all text-sm">{external}</p></Dialog>
  </div>;
}
