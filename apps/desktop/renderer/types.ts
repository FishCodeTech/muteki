export type DesktopCommand = 'back' | 'forward' | 'reload' | 'sidebar' | 'search' | 'theme' | 'new-chat' | 'settings' | 'shortcuts';
export type DesktopWindowAction = 'minimize' | 'maximize' | 'close';
import type { DesktopSpeechInput, DesktopSpeechEvent } from '../../web/ui/lib/desktopChatBridge';
export interface DesktopNotificationScope { connectionVersion: number; serviceId: string; identityId: string }
export interface DesktopNotificationEvent extends DesktopNotificationScope {
  id: string; threadId: string; eventId: string; requestedEventId?: string; dedupeKey: string; seq: number;
  status: 'submitted' | 'awaiting_show' | 'shown' | 'failed' | 'outcome_unknown' | 'clicked' | 'closed';
  shown: boolean; code?: string; message?: string;
  history: Array<{status: DesktopNotificationEvent['status']; at: string; code?: string; message?: string}>;
}
export interface DesktopNotificationStatus extends DesktopNotificationScope {
  permission: NotificationPermission | 'unsupported'; workspacePermission: NotificationPermission;
  host: 'desktop-client'; systemPermission: 'unknown' | 'default' | 'granted' | 'denied' | 'provisional' | 'ephemeral'; code: string;
  systemNotificationSettings?: { authorizationStatus: string; authorizationStatusRaw: number; alertSettingRaw?: number; soundSettingRaw?: number };
  systemPermissionError?: { code: string; message: string; detail?: string };
  delivery?: DesktopNotificationEvent;
}
export type DesktopState = {
  environment?: { id: string; channel: 'stable' | 'dev' | 'candidate'; name: string };
  managedService?: { state: 'stopped' | 'starting' | 'maintenance' | 'ready' | 'failed'; error?: string; origin?: string; portChanged?: boolean; logs: string };
  notificationOwner?: boolean; notificationWorkspaceKey?: string; notificationVisibleThreadIds?: string[];
  authSessionVersion?: number;
  anchorTarget?: {hash: string; id: number};
  serviceVersion?: string; serviceFeatures?: Record<string, number>; desktopVersion?: string; remoteUiBuild?: string; remoteAppearanceContract?: string;
  origin: string; transportOrigin: string; status: 'idle' | 'connecting' | 'connected' | 'error';
  message: string; configuring: boolean; platform: string; route: string; connectionVersion?: number;
  serviceId?: string; identityId?: string; persistenceWarning?: string;
  draftRecovery?: boolean;
  canGoBack?: boolean; canGoForward?: boolean;
  noticeVersion?: number;
  capabilities?: {version: number; host: string; entries?: Record<string, {supported: boolean; host: 'desktop-client' | 'service'; code?: string; reason?: string}>; [key: string]: unknown};
};
export interface DesktopBridge {
  connectLocal(): Promise<DesktopState>;
  consumeAnchor(id: number): Promise<void>;
  getState(): Promise<DesktopState>; connect(origin: string): Promise<DesktopState>;
  configure(): Promise<void>; resume(): Promise<void>; navigate(href: string, mode?: 'push' | 'replace'): Promise<void>;
  action(name: DesktopCommand): Promise<void>; setLocale(lang: 'zh' | 'en'): Promise<void>;
  syncAppearance(input: {preference: 'system' | 'light' | 'dark'; resolvedTheme: 'light' | 'dark'; selection: {kind: 'preset'; id: string} | {kind: 'custom'; hue: number}}): Promise<void>;
  windowAction(name: DesktopWindowAction): Promise<void>; newWindow(route: string): Promise<{windowId: number}>;
  openExternal(url: string): Promise<void>;
  openPreview(input: {surfaceId: string; url: string; reload?: boolean; rect: {x: number; y: number; width: number; height: number}; threadId: string; persistent?: boolean}): Promise<{id: string}>; closePreview(input?: {id?: string; surfaceId?: string; hide?: boolean}): Promise<void>;
  previewAction(input: {id: string; action: 'back' | 'forward' | 'reload' | 'stop' | 'pick' | 'pick-cancel' | 'reload-hard' | 'devtools' | 'clear-data' | 'screenshot' | 'zoom' | 'color-scheme'; factor?: number; scheme?: string}): Promise<any>; // eslint-disable-line @typescript-eslint/no-explicit-any
  browserControl(input: {threadId: string; action: string; timeoutMs: number; [key: string]: unknown}): Promise<Record<string, unknown>>;
  createVisualization(input: {threadId: string; html: string}): Promise<{id: string; url: string}>;
  releaseVisualization(id: string): Promise<void>;
  onPreview(callback: (event: {id: string; threadId: string; url: string; title?: string; status: 'loading' | 'loaded' | 'error'; message?: string; canGoBack?: boolean; canGoForward?: boolean}) => void): () => void;
  cacheAttachment(input: {data: ArrayBuffer; name: string; type: string; lastModified: number; draftId: string}): Promise<{id: string; name: string; type: string; size: number; sha256: string}>;
  restoreAttachment(input: {id: string; draftId: string}): Promise<{data: ArrayBuffer; name: string; type: string; lastModified: number}>;
  attachmentCacheUsage(): Promise<number>;
  removeAttachment(input: {id: string; draftId: string}): Promise<void>;
  selectPath(input: {kind: 'directory' | 'file'}): Promise<{id: string; path: string; name: string; host: 'desktop-client'; serverMapped: false} | null>;
  openPath(input: {id: string; action: 'reveal' | 'open'}): Promise<void>;
  selectWorkspaceRoot(input: {threadId: string; workspaceId: string; serviceId: string; identityId: string; serviceRoot: string}): Promise<{grantId: string; threadId: string; workspaceId: string; clientRoot: string; serviceRoot: string; host: 'desktop-client'; mapping: 'user-selected'} | null>;
  openWorkspaceFile(input: {grantId: string; threadId: string; workspaceId: string; serviceId: string; identityId: string; relativePath: string; action: 'reveal' | 'open'}): Promise<void>;
  requestMicrophone(): Promise<{granted: boolean; status?: string; code?: string}>;
  startSpeech(input: DesktopSpeechInput): Promise<{id: string}>;
  finishSpeech(input: {id: string}): Promise<void>; cancelSpeech(input: {id: string}): Promise<void>;
  onSpeech(callback: (event: DesktopSpeechEvent) => void): () => void;
  playNotificationSound(input: DesktopNotificationScope & {threadId: string; eventId: string; dedupeKey: string}): Promise<DesktopNotificationScope & {threadId: string; eventId: string; dedupeKey: string; status: "sound-requested" | "already-requested" | "suppressed"; host: "desktop-client"}>;
  notificationStatus(input: DesktopNotificationScope): Promise<DesktopNotificationStatus>;
  requestNotifications(input: DesktopNotificationScope): Promise<DesktopNotificationStatus>;
  sendNotification(input: DesktopNotificationScope & {threadId: string; eventId: string; dedupeKey: string; title: string; body: string; wantSound?: boolean}): Promise<DesktopNotificationEvent>;
  onNotification(callback: (event: DesktopNotificationEvent) => void): () => void;
  openPermissionSettings(name: 'microphone' | 'speechRecognition' | 'notifications'): Promise<void>;
  acknowledgeClose(input: {token: string; persisted: boolean; error?: string}): Promise<void>;
  terminalOpen(input: {threadId: string; path: string}): Promise<{id: string}>;
  terminalSend(id: string, data: string): Promise<void>; terminalClose(id: string): Promise<void>;
  onTerminal(callback: (event: {id: string; type: 'open' | 'message' | 'error' | 'close'; data?: string; code?: string | number; message?: string}) => void): () => void;
  onBeforeClose(callback: (event: {token: string}) => void): () => void;
  onCommand(callback: (command: {name: DesktopCommand}) => void): () => void;
  onFocus(callback: () => void): () => void;
  onState(callback: (state: DesktopState) => void): () => void;
}
declare global { interface Window { mutekiDesktop: DesktopBridge } }
