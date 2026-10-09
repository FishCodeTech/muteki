"use client";

import { chatPanel, type ChatSurface } from "@/lib/chatPanelStore";
import { desktopChatBridge, type DesktopBrowserAction } from "@/lib/desktopChatBridge";
import { apiFetch } from "@/lib/useRun";

/**
 * Provider tool calls (chat_browser_*) arrive on the thread event stream as
 * `browser_request` frames. This module performs them on the right-panel
 * preview of this client and posts the outcome back to the service.
 */
export interface BrowserRequest {
  request_id: string;
  action: "open" | "navigate" | "read" | "click" | "type" | "screenshot" | "press" | "scroll" | "wait" | "evaluate" | "logs";
  arguments: Record<string, unknown>;
  deadline_ms: number;
}

type PreviewSurface = Extract<ChatSurface, { kind: "preview" }>;

class BrowserActionError extends Error {
  constructor(readonly code: string, message: string) {
    super(message);
  }
}

/** Which kind of browser this client can drive; sent when subscribing to a thread stream. */
export function browserControlHost(): "desktop" | "web" {
  return desktopChatBridge()?.browserControl ? "desktop" : "web";
}

function livePreview(threadId: string): PreviewSurface | undefined {
  const panel = chatPanel.thread(threadId);
  const previews = panel.surfaces.filter((item): item is PreviewSurface => item.kind === "preview" && Boolean(item.url));
  return previews.find((item) => item.id === panel.activeId) ?? previews.at(-1);
}

async function perform(threadId: string, request: BrowserRequest): Promise<Record<string, unknown>> {
  const deadline = Date.now() + request.deadline_ms;
  const remaining = (cap = Infinity) => Math.max(1000, Math.min(cap, deadline - Date.now()));
  const control = desktopChatBridge()?.browserControl;
  const args = request.arguments;
  const call = (input: DesktopBrowserAction, cap?: number) =>
    control!({ ...input, threadId, timeoutMs: remaining(cap) });

  const url = typeof args.url === "string" ? args.url : "";
  if (request.action === "open" || (request.action === "navigate" && url)) {
    const newTab = request.action === "open" && args.new_tab === true;
    const current = livePreview(threadId);
    if (!control) {
      chatPanel.openPreview(threadId, url, { newTab });
      return { url, host: "web", load_state: "unknown",
        note: "Web 客户端的预览是跨源 iframe，无法确认加载结果或读取页面。" };
    }
    if (current && !newTab) {
      // Navigate the existing view in place so its cookies and history survive.
      chatPanel.open(threadId, current);
      await call({ action: "wait" }, 8000);
      return call({ action: "navigate", url });
    }
    chatPanel.openPreview(threadId, url, { newTab });
    return call({ action: "wait", url });
  }

  if (!control) {
    throw new BrowserActionError("browser.host_unsupported", "此客户端不能读取或操作右侧浏览器，请在 Muteki 桌面客户端中打开此对话。");
  }
  const current = livePreview(threadId);
  if (!current) throw new BrowserActionError("browser.not_open", "右侧浏览器没有打开网页，请先调用 chat_browser_open。");
  chatPanel.open(threadId, current);
  await call({ action: "wait" }, 10000);
  switch (request.action) {
    case "navigate":
      return call({ action: "history", direction: args.action as "back" | "forward" | "reload" });
    case "read":
      return call({ action: "read", mode: args.mode as "text" | "html" | "elements",
        selector: (args.selector as string | null) ?? null, offset: Number(args.offset) || 0, limit: Number(args.limit) });
    case "click":
      return call({ action: "click", ref: args.ref as number | undefined, selector: args.selector as string | undefined,
        text: args.text as string | undefined, point: args.point as { x: number; y: number } | undefined });
    case "type":
      return call({ action: "type", ref: args.ref as number | undefined, selector: args.selector as string | undefined,
        text: String(args.text ?? ""), clear: args.clear === true, submit: args.submit === true });
    case "screenshot":
      return call({ action: "screenshot" });
    case "press":
      return call({ action: "press", ref: args.ref as number | undefined, selector: args.selector as string | undefined,
        key: String(args.key ?? ""), modifiers: Array.isArray(args.modifiers) ? args.modifiers.map(String) : [], repeat: Number(args.repeat) || 1 });
    case "scroll":
      return call({ action: "scroll", ref: args.ref as number | undefined, selector: args.selector as string | undefined,
        direction: args.direction as "up" | "down" | "left" | "right" | undefined, amount: args.amount as number | undefined,
        to: args.to as "top" | "bottom" | undefined });
    case "wait":
      return call({ action: "wait", selector: args.selector as string | undefined, text: args.text as string | undefined,
        gone: args.gone === true }, Number(args.timeout_ms) || 10000);
    case "evaluate":
      return call({ action: "evaluate", expression: String(args.expression ?? "") });
    case "logs":
      return call({ action: "logs", kind: args.kind as "console" | "network", level: (args.level as string | null) ?? null,
        failed_only: args.failed_only === true, offset: Number(args.offset) || 0, limit: Number(args.limit) || 100 });
    default:
      throw new BrowserActionError("browser.action_unknown", `未知浏览器操作 ${String(request.action)}`);
  }
}

const queues = new Map<string, Promise<void>>();

async function reply(threadId: string, request: BrowserRequest) {
  let body: Record<string, unknown>;
  try {
    body = { ok: true, result: await perform(threadId, request) };
  } catch (error) {
    const detail = error as { code?: unknown; message?: unknown; retryable?: unknown };
    body = { ok: false, error: {
      code: typeof detail?.code === "string" && detail.code ? detail.code : "browser.failed",
      message: typeof detail?.message === "string" && detail.message ? detail.message : String(error),
      retryable: detail?.retryable === true,
    } };
  }
  const res = await apiFetch(`/api/threads/${encodeURIComponent(threadId)}/browser/${encodeURIComponent(request.request_id)}`, {
    method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body),
  });
  if (!res.ok && res.status !== 404) console.error("chat.browser.reply_failed", res.status);
}

/** Runs one thread's requests in arrival order; a page action must not interleave with the next. */
export function handleBrowserRequest(threadId: string, raw: string) {
  let request: BrowserRequest;
  try {
    request = JSON.parse(raw) as BrowserRequest;
  } catch {
    console.error("chat.browser.request_invalid");
    return;
  }
  if (!request?.request_id || !request.action) return;
  const previous = queues.get(threadId) ?? Promise.resolve();
  const next = previous.then(() => reply(threadId, request)).catch((error) => console.error("chat.browser.request_failed", error));
  queues.set(threadId, next);
  void next.finally(() => { if (queues.get(threadId) === next) queues.delete(threadId); });
}
