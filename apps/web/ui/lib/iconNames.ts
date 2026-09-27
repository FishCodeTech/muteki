/**
 * Semantic icon names shared by feature code and the Icon adapter. The type
 * lives in lib so registries such as runtimeTabs can type their icon field
 * without importing from components; components/Icon re-exports it.
 */
export type IconName =
  | "x" | "check" | "checkCircle" | "xCircle" | "flag" | "stop" | "crosshair" | "grid" | "pencil"
  | "radio" | "folder" | "folderOpen" | "folderPlus" | "file" | "pin" | "pause" | "paperclip"
  | "globe" | "lock" | "menu" | "panel" | "panelLeft" | "gear" | "send" | "sendUp" | "more"
  | "terminal" | "cpu" | "dot" | "loader" | "gripVertical" | "help" | "info" | "alert" | "circleAlert" | "clock" | "plug"
  | "bell" | "archive" | "trash" | "download" | "externalLink"
  | "target" | "play" | "network" | "list" | "listCollapse" | "board" | "layers"
  | "chevronDown" | "chevronUp" | "chevronLeft" | "chevronRight" | "arrowRight" | "arrowUp" | "arrowDown" | "arrowUpRight"
  | "rows" | "upload" | "code" | "copy" | "search" | "refresh" | "retry" | "plus" | "minus"
  | "gitBranch" | "gitFork" | "newChat" | "messages" | "mic" | "trophy" | "shieldAlert" | "sparkles"
  | "sun" | "moon" | "eye" | "eyeOff" | "droplet" | "star"
  | "panelBottom" | "panelLeftClose" | "panelLeftOpen" | "panelRightClose" | "panelRightOpen"
  | "maximize" | "minimize" | "columns" | "alignJustify" | "wrapText" | "foldVertical" | "unfoldVertical"
  | "chevronsUpDown" | "chevronsDownUp" | "folderTree" | "fileCode" | "fileDiff" | "filePlus" | "fileMinus"
  | "image" | "monitor" | "smartphone" | "tablet" | "rotateCw" | "arrowLeft" | "zoomIn" | "zoomOut"
  | "mousePointer" | "history" | "command" | "keyboard" | "thumbsUp" | "thumbsDown" | "edit" | "undo"
  | "link" | "hash" | "at" | "slash" | "brain" | "wrench" | "bot" | "user" | "compass" | "sliders"
  | "filter" | "sort" | "bookmark" | "share" | "pinOff" | "circleDashed" | "checkCheck" | "listTodo"
  | "messageCircle" | "quote" | "zap" | "gauge" | "database" | "package" | "shield" | "gitCompare"
  | "gitPullRequest" | "gitCommit" | "braces" | "scroll" | "stopCircle" | "arrowDownToLine" | "moreVertical"
  | "pilcrow" | "split" | "lightbulb" | "book" | "timer" | "hourglass" | "pencilLine" | "cornerDownLeft"
  | "fileSearch" | "textSearch" | "workflow" | "activity" | "asterisk" | "listChecks" | "circleDot"
  | "settings2" | "inbox";
