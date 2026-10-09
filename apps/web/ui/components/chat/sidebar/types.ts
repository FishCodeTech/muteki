export type SidebarSortMode = "updated" | "priority" | "manual";
export type SidebarGroupMode = "project" | "list";

export interface NavItem {
  id: string;
  label: string;
  subtitle?: string;
  /** Route for copy-link / open-in-new-tab / modifier clicks. */
  href?: string;
  status?: string;
  running?: boolean;
  unread?: boolean;
  /** 待审批或等待用户输入 */
  needsAction?: boolean;
  archived?: boolean;
  failed?: boolean;
  pinned?: boolean;
  updatedAt?: string;
  icon?: string;
  badge?: string | number;
  project?: string;
  projectId?: string;
  projectName?: string;
  projectPath?: string;
  summary?: string;
  /** What the thread is waiting on: a tool approval or an answer from the user. */
  pending?: "approval" | "input";
  /** Set when `pending` belongs to a subagent of this thread, e.g. "子代理 X 等待审批". */
  pendingNote?: string;
  errorText?: string;
  queueCount?: number;
  /** Snooze ended (or ended early) and the thread has not been opened since. */
  woke?: boolean;
  snoozedUntil?: number;
  settled?: boolean;
  onRename?: () => void;
  /** Commit an inline rename from the sidebar row. */
  onRenameCommit?: (title: string) => void;
  onSettle?: () => void;
  /** `null` wakes a snoozed thread immediately. */
  onSnooze?: (until: number | null) => void;
  onNewInProject?: () => void;
  onFilterProject?: () => void;
  onFork?: () => void;
  onArchive?: () => void;
  onDelete?: () => void;
  onPin?: () => void;
  onMoveUp?: () => void;
  onMoveDown?: () => void;
}

export interface NavSection {
  id?: string;
  title: string;
  subtitle?: string;
  items: NavItem[];
  /** `time` = recency bucket (今天 / 昨天 / …); `section` = plain list. */
  kind?: "section" | "folder" | "pinned" | "activity" | "archived" | "time" | "snoozed" | "settled";
  /** Default number of rows shown before an inline expand control appears. */
  previewLimit?: number;
  /** Rows shown first, then revealed in `pageStep` increments. */
  pageSize?: number;
  pageStep?: number;
  /** Collapsed until the user expands it (the choice is remembered). */
  defaultCollapsed?: boolean;
  emptyLabel?: string;
  running?: boolean;
  sortMode?: SidebarSortMode;
  onSortChange?: (mode: SidebarSortMode) => void;
  onNewChat?: () => void;
  onArchiveAll?: () => void;
  onMoveUp?: () => void;
  onMoveDown?: () => void;
  onReorderItems?: (sourceId: string, targetId: string) => void;
  onReorderSection?: (sourceId: string, targetId: string) => void;
}

export interface SidebarBodyHit {
  threadId: string;
  messageId: string;
  title: string;
  snippet: string;
  role: string;
  archived?: boolean;
  superseded?: boolean;
}

export interface SidebarActivityOptions {
  /** Keep priority chats above time buckets, including entries already viewed. */
  showPriority: boolean;
  showRunning: boolean;
  /** Group pinned chats separately; hiding the group keeps chats in time buckets. */
  showPinned: boolean;
  onShowChange: (key: "showPriority" | "showRunning" | "showPinned", value: boolean) => void;
  unreadCount: number;
  onMarkAllRead?: () => void;
  /** Listed chats with nothing pending, removable via "清除已读对话". */
  readCount: number;
  onClearRead: () => void;
  onRestoreDefaults: () => void;
}

export interface SidebarProjectOption {
  id: string;
  name: string;
  path?: string;
  count: number;
}

export interface SidebarProjectFilter {
  options: SidebarProjectOption[];
  value: string | null;
  onChange: (projectId: string | null) => void;
}

/** Actions applied to a multi-selection of thread ids. */
export interface SidebarBulkActions {
  pin: (ids: string[]) => void;
  settle: (ids: string[]) => void;
  snooze: (ids: string[], until: number) => void;
  archive?: (ids: string[]) => void;
}

export interface SidebarNavProps {
  projectFilter?: SidebarProjectFilter;
  bulkActions?: SidebarBulkActions;
  sections: NavSection[];
  activitySections?: NavSection[];
  activityView?: boolean;
  /** Pending/unread attention count shown on the activity bell. */
  activityBadge?: number;
  onToggleActivityView?: () => void;
  activityOptions?: SidebarActivityOptions;
  activeId?: string;
  onSelect: (id: string) => void;
  onNewChat?: () => void;
  onCreateFolder?: () => void;
  searchQuery?: string;
  onSearchChange?: (q: string) => void;
  sortMode?: SidebarSortMode;
  onSortChange?: (mode: SidebarSortMode) => void;
  groupMode?: SidebarGroupMode;
  onGroupChange?: (mode: SidebarGroupMode) => void;
  collapsed?: boolean;
  className?: string;
  bodyHits?: SidebarBodyHit[];
  bodyHitsLoading?: boolean;
  bodyHitsLoadingMore?: boolean;
  bodyHitsError?: string;
  bodyHitsHasMore?: boolean;
  onLoadMoreBodyHits?: () => void;
  onRetryBodyHits?: () => void;
  includeSuperseded?: boolean;
  onIncludeSupersededChange?: (value: boolean) => void;
  onSelectBodyHit?: (hit: { threadId: string; messageId: string }) => void;
  /** Thread list is still loading for the first time. */
  loading?: boolean;
}
