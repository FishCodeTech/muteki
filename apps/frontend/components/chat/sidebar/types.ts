import type { IconName } from "@muteki/ui";

export type SidebarSortMode = "updated" | "priority" | "manual";
export type SidebarGroupMode = "project" | "list";

export interface NavItem {
  id: string;
  label: string;
  subtitle?: string;
  href?: string;
  status?: string;
  running?: boolean;
  unread?: boolean;
  needsAction?: boolean;
  archived?: boolean;
  failed?: boolean;
  pinned?: boolean;
  updatedAt?: string;
  engine?: string;
  icon?: string;
  badge?: string | number;
  project?: string;
  projectId?: string;
  projectName?: string;
  projectPath?: string;
  summary?: string;
  pending?: "approval" | "input";
  pendingNote?: string;
  errorText?: string;
  queueCount?: number;
  agentsActive?: number;
  agentsTotal?: number;
  woke?: boolean;
  snoozedUntil?: number;
  settled?: boolean;
  followUp?: boolean;
  followUpNote?: string;
  parked?: boolean;
  autoSettleDisabled?: boolean;
  pullRequests?: { number: number; url: string; state?: string | null; title?: string | null; repository?: string }[];
  worktree?: boolean;
  onRename?: () => void;
  onRenameCommit?: (title: string) => void;
  onSettle?: () => void;
  onSnooze?: (until: number | null) => void;
  onNewInProject?: () => void;
  onFilterProject?: () => void;
  onFork?: () => void;
  onArchive?: () => void;
  onDelete?: () => void;
  onPin?: () => void;
  onMarkReadToggle?: () => void;
  onFollowUpToggle?: () => void;
  onParkToggle?: () => void;
  onAutoSettleChange?: (disabled: boolean) => void;
  onExport?: (format: "markdown" | "jsonl") => void;
  onMergeWorktree?: () => void;
  onRegenerateTitle?: () => void;
  onMoveToProject?: (projectId: string) => void;
  onMoveUp?: () => void;
  onMoveDown?: () => void;
}

export interface NavSection {
  id?: string;
  title: string;
  subtitle?: string;
  items: NavItem[];
  kind?: "section" | "folder" | "pinned" | "activity" | "archived" | "time" | "snoozed" | "settled";
  previewLimit?: number;
  pageSize?: number;
  pageStep?: number;
  defaultCollapsed?: boolean;
  emptyLabel?: string;
  running?: boolean;
  sortMode?: SidebarSortMode;
  onSortChange?: (mode: SidebarSortMode) => void;
  onNewChat?: () => void;
  onArchiveAll?: () => void;
  onDelete?: () => void;
  liveThreadCount?: number;
  onMoveUp?: () => void;
  onMoveDown?: () => void;
  onReorderItems?: (sourceId: string, targetId: string) => void;
  onReorderSection?: (sourceId: string, targetId: string) => void;
  onAcceptItem?: (itemId: string, beforeId: string | null) => void;
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
  showPriority: boolean;
  showRunning: boolean;
  showPinned: boolean;
  onShowChange: (key: "showPriority" | "showRunning" | "showPinned", value: boolean) => void;
  unreadCount: number;
  onMarkAllRead?: () => void;
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

export interface SidebarBulkActions {
  pin: (ids: string[]) => void;
  settle: (ids: string[]) => void;
  snooze: (ids: string[], until: number) => void;
  archive?: (ids: string[]) => void;
}

export interface SidebarLabels {
  title?: string;
  newThread?: string;
  search?: string;
  thread?: string;
  projectsTitle?: string;
  createProject?: string;
  deleteProject?: string;
  deleteEmptyProjects?: string;
  newThreadInProject?: string;
  groupByProject?: string;
  projectIcon?: IconName;
  projectOpenIcon?: IconName;
  createProjectIcon?: IconName;
}

export interface SidebarNavProps {
  labels?: SidebarLabels;
  storageNamespace?: string;
  projectFilter?: SidebarProjectFilter;
  bulkActions?: SidebarBulkActions;
  sections: NavSection[];
  activitySections?: NavSection[];
  activityView?: boolean;
  activityBadge?: number;
  onToggleActivityView?: () => void;
  activityOptions?: SidebarActivityOptions;
  activeId?: string;
  onSelect: (id: string) => void;
  onNewChat?: () => void;
  onCreateFolder?: () => void;
  onDeleteEmptyFolders?: () => void;
  emptyFolderCount?: number;
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
  loading?: boolean;
}
