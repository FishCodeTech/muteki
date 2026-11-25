"use client";

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { apiFetch } from "@/lib/useRun";
import { randomUuid } from "@/lib/browserCrypto";
import { desktopChatBridge } from "@/lib/desktopChatBridge";
import { visualizationSelectionText } from "@/lib/visualizationFollowup";
import { conversationStorageKey, conversationStorageScope } from "@/lib/conversationStorageScope";
import { Button, Checkbox, Dialog, Input, Label, Select, Slider, useCopy } from "@muteki/ui";
import { buildVisualizationDocument } from "./visualizationDocument";
import { useVisualizationTheme } from "./useVisualizationTheme";

type TweakControl = { id: number; type: "slider" | "color" | "toggle" | "select"; group: string; label: string; value: string | number | boolean; initial?: string | number | boolean; min?: number; max?: number; step?: number; unit?: string; options?: (string | { label: string; value: string })[] };
type Followup = { id: number; prompt: string; title: string; modelContent?: unknown; documentNonce: string; threadId: string; path: string; scope: string };


export function parseVisualization(text: string): { path: string; title?: string; inlineHtml?: string } | null {
  if (!text.includes("visualize")) return null;
  const inline = /^(`{3,}|~{3,})muteki-visualize[ \t]*\r?\n([\s\S]*?)\r?\n\1[ \t]*$/.exec(text.trim());
  if (inline) return { path: "inline", inlineHtml: inline[2].trim() };
  const match = /^(?:visualize(\{[^\n]*\})|(?:muteki-visualize|visualize)\s+(\{[^\n]*\}))$/.exec(text.trim());
  if (!match) return null;
  try {
    const value = JSON.parse(match[1] || match[2]);
    return typeof value.path === "string" ? { path: value.path, title: typeof value.title === "string" ? value.title : undefined } : null;
  } catch { return null; }
}

const FENCE_LINE = /^\s{0,3}(`{3,}|~{3,})/;
const TRAILING_MARKER = /(?<![\p{L}\p{M}\p{Nd}\p{Pc}-])(?:muteki-visualize|visualize)[ \t]*(\{[^\n]*\})[ \t]*$/u;

export type VisualizationSegment = { text: string; offset: number } | { visual: { path: string; title?: string }; offset: number };

export function splitVisualizationMarkers(block: string): VisualizationSegment[] {
  if (!block.includes("visualize")) return block.trim() ? [{ text: block, offset: 0 }] : [];
  const segments: VisualizationSegment[] = [];
  let fence: string | null = null, start = 0, offset = 0;
  for (const line of block.split("\n")) {
    const end = offset + line.length;
    const open = FENCE_LINE.exec(line)?.[1];
    if (open) {
      if (!fence) fence = open;
      else if (open[0] === fence[0] && open.length >= fence.length && !line.trim().replace(/[`~]/g, "")) fence = null;
    } else if (!fence) {
      const match = TRAILING_MARKER.exec(line);
      const visual = match ? parseVisualization(`muteki-visualize ${match[1]}`) : null;
      if (match && visual) {
        const before = block.slice(start, offset + match.index);
        if (before.trim()) segments.push({ text: before.trimEnd(), offset: start });
        segments.push({ visual, offset: offset + match.index });
        start = end + 1;
      }
    }
    offset = end + 1;
  }
  const rest = block.slice(start);
  if (rest.trim()) segments.push({ text: rest, offset: start });
  return segments;
}

type VisualizationProps = { threadId: string; messageId?: string; path: string; title?: string; inlineHtml?: string; streaming: boolean; expanded?: boolean };

export function ChatVisualization({ threadId, messageId, path, title, inlineHtml, streaming, expanded = false }: VisualizationProps) {
  const frame = useRef<HTMLIFrameElement>(null);
  const [documentNonce, setDocumentNonce] = useState("");
  const [contentRevision, setContentRevision] = useState("");
  const [nativeUrl, setNativeUrl] = useState("");
  const [nativeError, setNativeError] = useState("");
  const native = Boolean(desktopChatBridge());
  const scope = conversationStorageScope();
  const currentDocument = useRef({ documentNonce, threadId, path, scope });
  currentDocument.current = { documentNonce, threadId, path, scope };
  const theme = useVisualizationTheme();
  const [html, setHtml] = useState<string | null>(null);
  const [documentTitle, setDocumentTitle] = useState(title || "交互图");
  const [expandedOpen, setExpandedOpen] = useState(false);
  const [sourceOpen, setSourceOpen] = useState(false);
  const [downloadUrl, setDownloadUrl] = useState("");
  const { copy, copied } = useCopy();
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
  const reply = useCallback((id: number, ok: boolean, error = "") => frame.current?.contentWindow?.postMessage({ type: "muteki:viz:result", documentNonce, id, ok, error }, "*"), [documentNonce]);
  const [height, setHeight] = useState(360);
  const key = conversationStorageKey(`muteki:visualization:${threadId}:${messageId || ""}:${path}:${contentRevision}`);
  const legacyKey = conversationStorageKey(`muteki:visualization:${threadId}:${path}:${contentRevision}`);
  useEffect(() => {
    setHtml(null); setError(""); setControls([]); setFollowup(null); setExternal(""); setTweakOpen(false); setOriginalPreview(false); setDocumentNonce(""); setContentRevision(""); setNativeUrl(""); setNativeError(""); setDocumentTitle(title || "交互图");
    if (streaming) return;
    if (inlineHtml !== undefined && new TextEncoder().encode(inlineHtml).length > 1_000_000) {
      setError("visualization.too_large: 图形超过 1 MB，未截断或渲染"); return;
    }
    const abort = new AbortController();
    apiFetch(`/api/chat-plugins/visualizations/${encodeURIComponent(threadId)}?path=${encodeURIComponent(path)}${messageId ? `&message_id=${encodeURIComponent(messageId)}` : ""}`, { signal: abort.signal })
      .then(async (r) => { const body = await r.text(); if (!r.ok) throw new Error(`图形暂不可用（HTTP ${r.status}）：${body}`); try { return JSON.parse(body); } catch { throw new Error(`visualization.protocol.invalid_json: ${body}`); } })
      .then((v) => {
        if (typeof v.html !== "string" || typeof v.documentId !== "string" || (v.assets !== undefined && (!v.assets || typeof v.assets !== "object" || Object.values(v.assets).some((value) => typeof value !== "string")))) throw new Error("visualization.protocol.invalid_document: 缺少完整图形正文或资源");
        if (!abort.signal.aborted) {
          setContentRevision(v.documentId);
          setDocumentNonce(randomUuid()); setHtml(v.html); setAssets(v.assets || {});
          setDocumentTitle(title || v.title || "交互图");
        }
      })
      .catch((e) => { if (!abort.signal.aborted) setError(String(e.message)); });
    return () => abort.abort();
  }, [threadId, messageId, path, title, inlineHtml, streaming, scope]);
  useEffect(() => {
    const receive = (event: MessageEvent) => {
      if (event.source !== frame.current?.contentWindow || !documentNonce) return;
      const msg = event.data;
      if (msg?.documentNonce !== documentNonce) return;
      if (msg?.type === "muteki:viz:resize" && Number.isFinite(msg.height)) setHeight(Math.min(2000, Math.max(80, Math.ceil(msg.height))));
      if (msg?.type === "muteki:viz:ready") {
        try { frame.current?.contentWindow?.postMessage({ type: "muteki:viz:state", documentNonce, value: JSON.parse(localStorage.getItem(key) || localStorage.getItem(legacyKey) || "null") }, "*"); } catch {  }
      }
      if (msg?.type === "muteki:viz:state-write") {
        try {
          const encoded = JSON.stringify(msg.value);
          if (!msg.value || Array.isArray(msg.value) || new TextEncoder().encode(encoded).length > 16384) throw new Error("状态过大或格式无效");
          localStorage.setItem(key, encoded); reply(msg.id, true);
          window.dispatchEvent(new CustomEvent("muteki:visualization-state", { detail: { key, value: msg.value, documentNonce } }));
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
        if (document.activeElement !== frame.current || !navigator.userActivation?.isActive) return;
        try { const url = new URL(msg.href); if (["http:", "https:"].includes(url.protocol) && !url.username && !url.password) setExternal(url.href); } catch {  }
      }
    };
    const syncState = (event: Event) => {
      const detail = (event as CustomEvent).detail;
      if (detail?.key === key && detail.documentNonce !== documentNonce) {
        frame.current?.contentWindow?.postMessage({ type: "muteki:viz:state", documentNonce, value: detail.value }, "*");
      }
    };
    window.addEventListener("message", receive);
    window.addEventListener("muteki:visualization-state", syncState);
    return () => { window.removeEventListener("message", receive); window.removeEventListener("muteki:visualization-state", syncState); };
  }, [key, legacyKey, documentNonce, threadId, path, scope, reply]);
  const themeReady = theme !== null;
  const srcDocument = useMemo(() => {
    if (html === null || theme === null) return "";
    return buildVisualizationDocument(html, assets, theme, documentNonce);
  // Theme changes update the existing frame, preserving scripts and selections.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [html, assets, documentNonce, themeReady]);
  const postTheme = useCallback(() => {
    if (theme) frame.current?.contentWindow?.postMessage({ type: "muteki:viz:theme", documentNonce, value: theme.appearance, variables: theme.variables }, "*");
  }, [theme, documentNonce]);
  useEffect(postTheme, [postTheme]);
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
  useEffect(() => {
    if (html === null || !theme) { setDownloadUrl(""); return; }
    const document = buildVisualizationDocument(html, assets, theme, contentRevision, true);
    const url = URL.createObjectURL(new Blob([document], { type: "text/html;charset=utf-8" }));
    setDownloadUrl(url);
    return () => URL.revokeObjectURL(url);
  }, [html, assets, theme, contentRevision]);
  if (error && html === null) return <p role="alert" className="whitespace-pre-wrap break-words text-cx-xs text-cx-fg-3">{error}</p>;
  if (html === null) {
    return (
      <div
        className="my-3 flex items-center justify-center rounded-xl border border-cx-border-subtle bg-cx-bg-subtle/40 text-cx-xs text-cx-fg-3"
        style={{ minHeight: height }}
      >
        正在准备交互图…
      </div>
    );
  }
  return <div className={expanded ? "flex min-h-0 flex-1 flex-col" : "my-3 min-w-0"} data-testid="chat-visualization-document">
    <div className="mb-2 flex flex-wrap items-center justify-end gap-1 not-prose" role="toolbar" aria-label="图形操作">
      {controls.length > 0 && <Button variant="ghost" size="sm" onClick={() => setTweakOpen(true)}>调整设计</Button>}
      <Button variant="ghost" size="sm" onClick={() => setSourceOpen(!sourceOpen)}>{sourceOpen ? "预览" : "源码"}</Button>
      {sourceOpen && <Button variant="ghost" size="sm" onClick={() => void copy(html)}>{copied ? "已复制" : "复制源码"}</Button>}
      {downloadUrl && <a href={downloadUrl} download={(documentTitle.replace(/[<>:"/\\|?*\u0000-\u001f]/g, "-").trim() || "visualization") + ".html"} className="cx-press inline-flex h-7 items-center gap-1.5 rounded-lg px-2.5 text-cx-xs font-medium text-cx-fg-2 outline-none hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]">下载</a>}
      {!expanded && <Button variant="ghost" size="sm" onClick={() => setExpandedOpen(true)}>展开</Button>}
    </div>
    {error || nativeError ? <p role="alert" className="mb-2 whitespace-pre-wrap break-words text-cx-xs text-cx-warning">{error || nativeError}</p> : null}
    {sourceOpen && <pre tabIndex={0} aria-label="图形源码" className="min-h-0 max-h-[65vh] overflow-auto rounded-lg bg-cx-sunken p-3 font-cx-mono text-cx-xs text-cx-fg">{html}</pre>}
    {native && !nativeUrl ? <p className="text-cx-xs text-cx-fg-3">正在准备隔离的桌面图形文档…</p> : <iframe key={documentNonce} ref={frame} title={documentTitle} sandbox="allow-scripts" referrerPolicy="no-referrer" loading="lazy" onLoad={postTheme} src={native ? nativeUrl : undefined} srcDoc={native ? undefined : srcDocument} className={sourceOpen ? "hidden" : "w-full min-h-0 border-0"} style={{ height: expanded ? "100%" : height, flex: expanded ? 1 : undefined, colorScheme: theme?.appearance }} data-testid="chat-visualization" />}
    {!expanded && <Dialog open={expandedOpen} onOpenChange={setExpandedOpen} title={documentTitle} size="full" bodyClassName="flex min-h-0 flex-1 flex-col !p-3" testId="visualization-expanded">
      <ChatVisualization threadId={threadId} messageId={messageId} path={path} title={title} inlineHtml={inlineHtml} streaming={false} expanded />
    </Dialog>}
    <Dialog open={tweakOpen} onOpenChange={(open) => { if (!open && originalPreview) previewOriginal(false); setTweakOpen(open); }} title="调整设计" footer={<>
      <Button variant="ghost" onClick={() => { previewOriginal(false); controls.forEach((c) => updateControl(c, c.initial ?? c.value, true)); }}>重置</Button>
      <Button variant="ghost" onClick={() => previewOriginal(!originalPreview)}>{originalPreview ? "查看修改" : "查看原始"}</Button>
      <Button variant="secondary" onClick={() => { previewOriginal(false); setTweakOpen(false); setFollowup({ id: 0, documentNonce, threadId, path, scope, title: "放入输入框", prompt: "请按这些设计控件的选择继续调整。", modelContent: controls.map((c) => ({ group: c.group, label: c.label, value: c.value })) }); }}>放入输入框</Button>
      <Button variant="primary" onClick={() => { previewOriginal(false); setTweakOpen(false); }}>完成</Button></>}>
      <div className="space-y-4">{controls.map((c) => {
        const heading = `${c.group} · ${c.label}`;
        if (c.type === "select") {
          return (
            <div key={c.id} className="flex flex-col">
              <Label>{heading}</Label>
              <Select
                ariaLabel={c.label}
                value={String(c.value)}
                className="w-full"
                onChange={(next) => updateControl(c, next)}
                options={(c.options || []).map((o) => typeof o === "string" ? { value: o, label: o } : { value: o.value, label: o.label })}
              />
            </div>
          );
        }
        if (c.type === "toggle") {
          return (
            <div key={c.id}>
              <Checkbox
                checked={Boolean(c.value)}
                onCheckedChange={(checked) => updateControl(c, checked)}
                label={heading}
              />
            </div>
          );
        }
        if (c.type === "slider") {
          return (
            <Slider
              key={c.id}
              label={heading}
              valueLabel={`${c.value}${c.unit || ""}`}
              min={c.min}
              max={c.max}
              step={c.step}
              value={Number(c.value)}
              onValueChange={(next) => updateControl(c, next)}
            />
          );
        }
        return (
          <div key={c.id} className="flex flex-col">
            <Label>{heading}</Label>
            <div className="flex items-center gap-2.5">
              <input
                aria-label={c.label}
                type="color"
                value={String(c.value)}
                onChange={(e) => updateControl(c, e.target.value)}
                className="h-8 w-10 shrink-0 cursor-pointer rounded-lg border border-cx-border bg-cx-elevated p-0.5"
              />
              <Input
                aria-label={`${c.label}（十六进制）`}
                value={String(c.value)}
                className="min-w-0 flex-1 font-cx-mono"
                onChange={(e) => updateControl(c, e.target.value)}
              />
            </div>
          </div>
        );
      })}</div>
    </Dialog>
    <Dialog open={!!followup} onOpenChange={(open) => { if (!open && !sending) cancelFollowup(); }} title={followup?.title || "放入输入框"} description="确认后把完整内容放入当前对话输入框，再通过普通发送按钮发送。" footer={<><Button variant="ghost" disabled={sending} onClick={cancelFollowup}>取消</Button><Button variant="primary" loading={sending} disabled={Boolean(selectionError)} onClick={() => void sendFollowup()}>放入输入框</Button></>}>
      <p className="whitespace-pre-wrap text-cx-sm">{followup?.prompt}</p>
      {selectionError ? <p role="alert" className="mt-3 whitespace-pre-wrap break-words text-cx-xs text-cx-warning">{selectionError}</p> : selectionText ? <pre className="mt-3 max-h-40 overflow-auto whitespace-pre-wrap text-cx-xs">{selectionText}</pre> : null}
    </Dialog>
    <Dialog open={!!external} onOpenChange={(open) => { if (!open) setExternal(""); }} title="打开外部链接" size="sm" footer={<><Button variant="ghost" onClick={() => setExternal("")}>取消</Button><Button variant="primary" onClick={() => { window.open(external, "_blank", "noopener,noreferrer"); setExternal(""); }}>打开链接</Button></>}><p className="break-all text-cx-sm">{external}</p></Dialog>
  </div>;
}
