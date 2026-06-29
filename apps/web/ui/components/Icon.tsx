"use client";
import type { CSSProperties } from "react";

/**
 * Single monochrome SVG icon set for the whole deck — replaces every emoji /
 * Unicode-dingbat glyph that used to be rendered inline. Feather-style: 24×24
 * viewBox, `currentColor` stroke, no fill (except the few solid marks), so an
 * icon inherits the colour of its button/chip and stays crisp on the light UI.
 *
 * Usage: <Icon name="flag" />  ·  <Icon name="x" size={14} title="close" />
 */

export type IconName =
  | "x" | "check" | "xCircle" | "flag" | "crosshair" | "grid" | "pencil"
  | "radio" | "folder" | "folderPlus" | "pin" | "pause" | "paperclip"
  | "globe" | "lock" | "menu" | "panel" | "gear" | "send" | "more"
  | "terminal" | "cpu" | "dot" | "help" | "alert" | "clock" | "plug"
  | "target" | "play" | "network" | "list" | "board" | "layers"
  | "chevronDown" | "chevronRight" | "rows" | "upload" | "copy" | "search" | "refresh"
  | "sun" | "moon" | "eye" | "eyeOff";

// Each entry is the inner markup of a 24×24 icon. Stroke paths use
// currentColor; solid marks (dot) use fill.
const PATHS: Record<IconName, JSX.Element> = {
  x: <path d="M18 6 6 18M6 6l12 12" />,
  check: <path d="M20 6 9 17l-5-5" />,
  xCircle: (<><circle cx="12" cy="12" r="9" /><path d="M15 9l-6 6M9 9l6 6" /></>),
  flag: (<><path d="M4 21V4" /><path d="M4 4h13l-2.5 4L17 12H4" /></>),
  crosshair: (<><circle cx="12" cy="12" r="8" /><path d="M12 2v3M12 19v3M2 12h3M19 12h3" /></>),
  grid: (<><rect x="3" y="3" width="7" height="7" rx="1" /><rect x="14" y="3" width="7" height="7" rx="1" /><rect x="3" y="14" width="7" height="7" rx="1" /><rect x="14" y="14" width="7" height="7" rx="1" /></>),
  pencil: (<><path d="M12 20h9" /><path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z" /></>),
  radio: (<><circle cx="12" cy="12" r="2" /><path d="M7.8 7.8a6 6 0 0 0 0 8.4M16.2 16.2a6 6 0 0 0 0-8.4M4.9 4.9a10 10 0 0 0 0 14.2M19.1 19.1a10 10 0 0 0 0-14.2" /></>),
  folder: <path d="M3 7a2 2 0 0 1 2-2h4l2 2.5h8a2 2 0 0 1 2 2V18a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2Z" />,
  folderPlus: (<><path d="M3 7a2 2 0 0 1 2-2h4l2 2.5h8a2 2 0 0 1 2 2V18a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2Z" /><path d="M12 11v5M9.5 13.5h5" /></>),
  pin: (<><path d="M9 4h6l-1 6 3 3H7l3-3-1-6Z" /><path d="M12 16v4" /></>),
  pause: (<><rect x="6" y="5" width="4" height="14" rx="1" /><rect x="14" y="5" width="4" height="14" rx="1" /></>),
  paperclip: <path d="M21 9.5 12.5 18a4 4 0 0 1-5.7-5.7l8-8a2.5 2.5 0 0 1 3.6 3.6l-8 8a1 1 0 0 1-1.5-1.4l7.4-7.4" />,
  globe: (<><circle cx="12" cy="12" r="9" /><path d="M3 12h18M12 3c2.5 2.5 2.5 15 0 18M12 3c-2.5 2.5-2.5 15 0 18" /></>),
  lock: (<><rect x="5" y="11" width="14" height="9" rx="2" /><path d="M8 11V8a4 4 0 0 1 8 0v3" /></>),
  menu: <path d="M4 6h16M4 12h16M4 18h16" />,
  panel: (<><rect x="3" y="4" width="18" height="16" rx="2" /><path d="M14 4v16" /></>),
  gear: (<><circle cx="12" cy="12" r="3" /><path d="M12.2 2h-.4a2 2 0 0 0-2 2v.2a2 2 0 0 1-1 1.7l-.4.2a2 2 0 0 1-2 0l-.2-.1a2 2 0 0 0-2.7.7l-.2.4A2 2 0 0 0 4 9.8l.2.1a2 2 0 0 1 1 1.7v.8a2 2 0 0 1-1 1.7l-.2.1a2 2 0 0 0-.7 2.7l.2.4a2 2 0 0 0 2.7.7l.2-.1a2 2 0 0 1 2 0l.4.2a2 2 0 0 1 1 1.7v.2a2 2 0 0 0 2 2h.4a2 2 0 0 0 2-2v-.2a2 2 0 0 1 1-1.7l.4-.2a2 2 0 0 1 2 0l.2.1a2 2 0 0 0 2.7-.7l.2-.4a2 2 0 0 0-.7-2.7l-.2-.1a2 2 0 0 1-1-1.7v-.8a2 2 0 0 1 1-1.7l.2-.1a2 2 0 0 0 .7-2.7l-.2-.4a2 2 0 0 0-2.7-.7l-.2.1a2 2 0 0 1-2 0l-.4-.2a2 2 0 0 1-1-1.7V4a2 2 0 0 0-2-2Z" /></>),
  send: <path d="M22 2 11 13M22 2l-7 20-4-9-9-4Z" />,
  more: (<><circle cx="5" cy="12" r="1.4" /><circle cx="12" cy="12" r="1.4" /><circle cx="19" cy="12" r="1.4" /></>),
  terminal: (<><path d="M5 7l5 5-5 5" /><path d="M13 17h6" /></>),
  cpu: (<><rect x="6" y="6" width="12" height="12" rx="2" /><path d="M9 2v2M15 2v2M9 20v2M15 20v2M2 9h2M2 15h2M20 9h2M20 15h2" /></>),
  dot: <circle cx="12" cy="12" r="4" fill="currentColor" stroke="none" />,
  help: (<><circle cx="12" cy="12" r="9" /><path d="M9.5 9.5a2.5 2.5 0 1 1 3.5 2.3c-.8.4-1 .8-1 1.7" /><circle cx="12" cy="17" r="0.6" fill="currentColor" stroke="none" /></>),
  alert: (<><path d="M12 3 2 20h20L12 3Z" /><path d="M12 9v5" /><circle cx="12" cy="17.5" r="0.6" fill="currentColor" stroke="none" /></>),
  clock: (<><circle cx="12" cy="12" r="9" /><path d="M12 7v5l3 2" /></>),
  plug: (<><path d="M9 2v6M15 2v6" /><path d="M7 8h10v3a5 5 0 0 1-10 0Z" /><path d="M12 16v6" /></>),
  target: (<><circle cx="12" cy="12" r="8" /><circle cx="12" cy="12" r="4" /><circle cx="12" cy="12" r="0.6" fill="currentColor" stroke="none" /></>),
  play: <path d="M7 5l12 7-12 7Z" />,
  network: (<><circle cx="6" cy="6" r="2.4" /><circle cx="18" cy="6" r="2.4" /><circle cx="12" cy="18" r="2.4" /><path d="M7.7 7.7 10.7 16M16.3 7.7 13.3 16M8 6h8" /></>),
  list: <path d="M8 6h13M8 12h13M8 18h13M3.5 6h.01M3.5 12h.01M3.5 18h.01" />,
  board: (<><rect x="3" y="4" width="18" height="16" rx="2" /><path d="M7 8h6M7 12h10M7 16h4" /></>),
  layers: <path d="M12 3 3 8l9 5 9-5-9-5ZM3 13l9 5 9-5M3 17.5l9 5 9-5" />,
  chevronDown: <path d="M6 9l6 6 6-6" />,
  chevronRight: <path d="M9 6l6 6-6 6" />,
  rows: <path d="M4 6h16M4 10h16M4 14h16M4 18h16" />,
  upload: (<><path d="M4 16v3a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-3" /><path d="M12 16V4M7 9l5-5 5 5" /></>),
  copy: (<><rect x="9" y="9" width="11" height="11" rx="2" /><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" /></>),
  search: (<><circle cx="11" cy="11" r="7" /><path d="M21 21l-4.3-4.3" /></>),
  refresh: (<><path d="M21 12a9 9 0 1 1-2.64-6.36" /><path d="M21 4v5h-5" /></>),
  sun: (<><circle cx="12" cy="12" r="4" /><path d="M12 2v2M12 20v2M4.93 4.93l1.41 1.41M17.66 17.66l1.41 1.41M2 12h2M20 12h2M4.93 19.07l1.41-1.41M17.66 6.34l1.41-1.41" /></>),
  moon: <path d="M21 13.2A8.5 8.5 0 0 1 10.8 3 7 7 0 1 0 21 13.2Z" />,
  eye: (<><path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7-10-7-10-7Z" /><circle cx="12" cy="12" r="3" /></>),
  eyeOff: (<><path d="M9.9 4.2A10 10 0 0 1 12 4c6.5 0 10 7 10 7a18 18 0 0 1-3 3.6M6.6 6.6A18 18 0 0 0 2 11s3.5 7 10 7a10 10 0 0 0 4-.8" /><path d="M9.5 9.5a3 3 0 0 0 4.2 4.2" /><path d="M3 3l18 18" /></>),
};

export function Icon({
  name, size = 16, className, style, title,
}: {
  name: IconName; size?: number; className?: string; style?: CSSProperties; title?: string;
}) {
  return (
    <svg
      className={className}
      width={size} height={size} viewBox="0 0 24 24"
      fill="none" stroke="currentColor" strokeWidth={1.9}
      strokeLinecap="round" strokeLinejoin="round"
      aria-hidden={title ? undefined : true} role={title ? "img" : undefined}
      style={{ flex: "none", display: "inline-block", verticalAlign: "text-bottom", ...style }}
    >
      {title ? <title>{title}</title> : null}
      {PATHS[name]}
    </svg>
  );
}

export default Icon;
