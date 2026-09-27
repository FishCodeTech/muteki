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
  onRename?: () => void;
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
  items: NavItem[];
  /** `time` = recency bucket (今天 / 昨天 / …); `section` = plain list. */
  kind?: "section" | "folder" | "pinned" | "activity" | "archived" | "time";
  /** Default number of rows shown before an inline expand control appears. */
  previewLimit?: number;
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

export interface SidebarNavProps {
  sections: NavSection[];
  activitySections?: NavSection[];
  activityView?: boolean;
  /** Pending/unread attention count shown on the inbox toggle. */
  activityBadge?: number;
  onToggleActivityView?: () => void;
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
  includeSuperseded?: boolean;
  onIncludeSupersededChange?: (value: boolean) => void;
  onSelectBodyHit?: (hit: { threadId: string; messageId: string }) => void;
  /** Thread list is still loading for the first time. */
  loading?: boolean;
}
