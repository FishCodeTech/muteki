export interface DesktopTerminalEvent {
  id: string;
  type: "open" | "message" | "close" | "error";
  data?: string;
  code?: number | string;
  message?: string;
}

export interface DesktopPreviewEvent {
  id: string; threadId: string; url: string; title?: string;
  status?: string; message?: string; canGoBack?: boolean; canGoForward?: boolean;
}

export type NativeCapabilityId = "pathSelection" | "workspaceFileActions" | "preview" | "attachmentCache" | "terminal" | "microphone" | "notifications" | "deepLinks";
export interface NativeCapability { supported: boolean; host: "desktop-client" | "service"; code?: string; reason?: string }
export interface NativeCapabilityManifest { version: 1; host: "desktop-client"; entries: Record<NativeCapabilityId, NativeCapability> }
export interface DesktopNativeState { connectionVersion?: number; serviceId?: string; identityId?: string; origin?: string; transportOrigin?: string; capabilities?: unknown }
export interface DesktopSpeechInput extends DesktopNotificationScope { id: string; scopeKey: string; locale: string }
export interface DesktopSpeechEvent extends DesktopNotificationScope {
  id: string; scopeKey: string; type: "requesting" | "listening" | "partial" | "processing" | "result" | "error" | "cancelled";
  text?: string; locale?: string; onDevice?: boolean; code?: string; message?: string; detail?: unknown;
}
export interface DesktopNotificationScope { connectionVersion: number; serviceId: string; identityId: string }
export type DesktopNotificationStage = "submitted" | "awaiting_show" | "shown" | "failed" | "outcome_unknown" | "clicked" | "closed";
export interface DesktopNotificationEvent extends DesktopNotificationScope {
  id: string; threadId: string; eventId: string; dedupeKey: string; seq: number;
  status: DesktopNotificationStage; shown: boolean; code?: string; message?: string;
  history: Array<{ status: DesktopNotificationStage; at: string; code?: string; message?: string }>;
}
export interface DesktopNotificationInput extends DesktopNotificationScope {
  threadId: string; eventId: string; dedupeKey: string; title: string; body: string;
}
export interface DesktopNotificationStatus extends DesktopNotificationScope {
  permission: NotificationPermission | "unsupported"; workspacePermission: NotificationPermission;
  host: "desktop-client"; systemPermission: "unknown"; code: string;
  delivery?: DesktopNotificationEvent;
}
export interface DesktopSelectedPath { id: string; path: string; name: string; host: "desktop-client"; serverMapped: false }
export interface DesktopWorkspaceGrant {
  grantId: string; threadId: string; workspaceId: string; clientRoot: string; serviceRoot: string;
  host: "desktop-client"; mapping: "user-selected";
}

export interface DesktopChatBridge {
  createVisualization?: (input: { threadId: string; html: string }) => Promise<{ id: string; url: string }>;
  releaseVisualization?: (id: string) => Promise<unknown>;
  removeAttachment?: (input: { id: string; draftId: string }) => Promise<unknown>;
  getState?: () => Promise<DesktopNativeState>;
  onState?: (callback: (state: DesktopNativeState) => void) => () => void;
  selectPath?: (input: { kind: "directory" | "file" }) => Promise<DesktopSelectedPath | null>;
  openPath?: (input: { id: string; action: "reveal" | "open" }) => Promise<unknown>;
  selectWorkspaceRoot?: (input: { threadId: string; workspaceId: string; serviceId: string; identityId: string; serviceRoot: string }) => Promise<DesktopWorkspaceGrant | null>;
  openWorkspaceFile?: (input: { grantId: string; threadId: string; workspaceId: string; serviceId: string; identityId: string; relativePath: string; action: "reveal" | "open" }) => Promise<unknown>;
  openPreview?: (input: { surfaceId: string; url: string; rect: { x: number; y: number; width: number; height: number }; threadId: string; reload?: boolean }) => Promise<{ id: string }>;
  closePreview?: (input: { id?: string; surfaceId?: string; hide?: boolean }) => Promise<unknown>;
  onPreview?: (callback: (event: DesktopPreviewEvent) => void) => () => void;
  previewAction?: (input: { id: string; action: "back" | "forward" | "reload" | "stop" }) => Promise<unknown>;
  requestMicrophone?: () => Promise<{ granted: boolean; status?: string; code?: string }>;
  startSpeech?: (input: DesktopSpeechInput) => Promise<{ id: string }>;
  finishSpeech?: (input: { id: string }) => Promise<unknown>;
  cancelSpeech?: (input: { id: string }) => Promise<unknown>;
  onSpeech?: (callback: (event: DesktopSpeechEvent) => void) => () => void;
  notificationStatus?: (input: DesktopNotificationScope) => Promise<DesktopNotificationStatus>;
  requestNotifications?: (input: DesktopNotificationScope) => Promise<DesktopNotificationStatus>;
  sendNotification?: (input: DesktopNotificationInput) => Promise<DesktopNotificationEvent>;
  onNotification?: (callback: (event: DesktopNotificationEvent) => void) => () => void;
  openPermissionSettings?: (kind: "microphone" | "speechRecognition" | "notifications") => Promise<unknown>;
  cacheAttachment?: (input: { data: ArrayBuffer; name: string; type: string; lastModified: number; draftId: string }) => Promise<{ id: string; name: string; type: string; size: number; sha256: string }>;
  restoreAttachment?: (input: { id: string; draftId: string }) => Promise<{ data: ArrayBuffer; name: string; type: string; lastModified: number }>;
  terminalOpen?: (input: { threadId: string; path: string }) => Promise<{ id: string }>;
  terminalSend?: (id: string, data: string) => Promise<unknown>;
  terminalClose?: (id: string) => Promise<unknown>;
  onTerminal?: (callback: (event: DesktopTerminalEvent) => void) => () => void;
  acknowledgeClose?: (input: { token: string; persisted: boolean; error?: string }) => Promise<unknown>;
}

export function desktopChatBridge(): DesktopChatBridge | undefined {
  if (typeof window === "undefined") return undefined;
  return (window as Window & { mutekiDesktop?: DesktopChatBridge }).mutekiDesktop;
}

export type TerminalSocket = Pick<WebSocket, "send" | "close" | "onopen" | "onmessage" | "onclose" | "onerror"> & { readyState: number };

/** A renderer connection to the main process's scope-checked WebSocket.
 * Subscribing before IPC open preserves frames that arrive before its id reply.
 */
export function nativeTerminalSocket(bridge: DesktopChatBridge, threadId: string, path: string): TerminalSocket {
  if (!bridge.terminalOpen || !bridge.terminalSend || !bridge.terminalClose || !bridge.onTerminal) {
    throw new Error("desktop.terminal.unavailable: 桌面终端传输未配置");
  }
  let id = "";
  let closed = false;
  const pending: DesktopTerminalEvent[] = [];
  const reportError = (error: unknown) => socket.onerror?.call(socket as WebSocket, new ErrorEvent("error", { message: error instanceof Error ? error.message : String(error), error }));
  const socket: TerminalSocket = {
    readyState: 0,
    onopen: null, onmessage: null, onclose: null, onerror: null,
    send(data) {
      if (socket.readyState !== 1 || !id || typeof data !== "string") throw new Error("desktop.terminal.not_open: 终端尚未连接");
      void bridge.terminalSend!(id, data).catch(reportError);
    },
    close() {
      if (closed) return;
      closed = true;
      socket.readyState = 3;
      unsubscribe();
      if (id) void bridge.terminalClose!(id).catch(reportError);
    },
  };
  const deliver = (event: DesktopTerminalEvent) => {
    if (closed || event.id !== id) return;
    if (event.type === "open") {
      socket.readyState = 1;
      socket.onopen?.call(socket as WebSocket, new Event("open"));
    } else if (event.type === "message") {
      socket.onmessage?.call(socket as WebSocket, new MessageEvent("message", { data: event.data || "" }));
    } else if (event.type === "error") {
      reportError(event.message || "desktop.terminal.transport_error");
    } else {
      socket.readyState = 3;
      socket.onclose?.call(socket as WebSocket, new CloseEvent("close", { code: typeof event.code === "number" ? event.code : 1000, reason: event.message || "" }));
      closed = true;
      unsubscribe();
    }
  };
  const unsubscribe = bridge.onTerminal((event) => {
    if (!id) pending.push(event);
    else deliver(event);
  });
  void bridge.terminalOpen({ threadId, path }).then((opened) => {
    if (!opened || typeof opened.id !== "string" || !opened.id) throw new Error("desktop.terminal.invalid_reply: 终端连接缺少身份");
    id = opened.id;
    if (closed) {
      void bridge.terminalClose!(id).catch(reportError);
      return;
    }
    for (const event of pending) deliver(event);
    pending.length = 0;
  }).catch((error) => {
    socket.readyState = 3;
    reportError(error);
    unsubscribe();
  });
  return socket;
}
