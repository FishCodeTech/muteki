"use client";

import { IconButton, Menu, MenuItem, MenuLabel, MenuSeparator } from "@/components/chat/ui";

const STORAGE_KEY = "muteki.chat.preview-browser.v1";

export type PreviewColorScheme = "system" | "light" | "dark";

export interface PreviewBrowserPrefs {
  /** Keep cookies and storage in a profile shared by previews of this service. */
  persistent: boolean;
  pageZoom: number;
  scheme: PreviewColorScheme;
}

export const DEFAULT_BROWSER_PREFS: PreviewBrowserPrefs = { persistent: false, pageZoom: 1, scheme: "system" };

const ZOOMS = [0.5, 0.67, 0.8, 0.9, 1, 1.1, 1.25, 1.5, 2];
const SCHEMES: Array<{ value: PreviewColorScheme; label: string }> = [
  { value: "system", label: "跟随页面默认" },
  { value: "light", label: "浅色" },
  { value: "dark", label: "深色" },
];

export function readBrowserPrefs(): PreviewBrowserPrefs {
  if (typeof window === "undefined") return DEFAULT_BROWSER_PREFS;
  try {
    const parsed = JSON.parse(window.localStorage.getItem(STORAGE_KEY) || "null") as Partial<PreviewBrowserPrefs> | null;
    return {
      persistent: parsed?.persistent === true,
      pageZoom: ZOOMS.includes(Number(parsed?.pageZoom)) ? Number(parsed?.pageZoom) : 1,
      scheme: SCHEMES.some((item) => item.value === parsed?.scheme) ? (parsed!.scheme as PreviewColorScheme) : "system",
    };
  } catch {
    return DEFAULT_BROWSER_PREFS;
  }
}

export function writeBrowserPrefs(prefs: PreviewBrowserPrefs) {
  try {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(prefs));
  } catch {
    // Storage blocked: the choice applies to this session only.
  }
}

export function BrowserMenu({
  prefs,
  onChange,
  onHardReload,
  onDevTools,
  onScreenshot,
  onClearData,
  disabled,
}: {
  prefs: PreviewBrowserPrefs;
  onChange: (next: PreviewBrowserPrefs) => void;
  onHardReload: () => void;
  onDevTools: () => void;
  onScreenshot?: () => void;
  onClearData: () => void;
  disabled?: boolean;
}) {
  return (
    <Menu
      placement="bottom-end"
      ariaLabel="浏览器选项"
      className="min-w-[232px]"
      trigger={<IconButton icon="moreVertical" label="浏览器选项" disabled={disabled} />}
    >
      {onScreenshot ? <MenuItem icon="image" onSelect={onScreenshot}>截图加入输入框</MenuItem> : null}
      <MenuItem icon="refresh" hint="忽略缓存" onSelect={onHardReload}>强制刷新</MenuItem>
      <MenuItem icon="code" onSelect={onDevTools}>开发者工具</MenuItem>
      <MenuLabel>页面外观</MenuLabel>
      {SCHEMES.map((item) => (
        <MenuItem key={item.value} keepOpen checked={prefs.scheme === item.value} onSelect={() => onChange({ ...prefs, scheme: item.value })}>
          {item.label}
        </MenuItem>
      ))}
      <MenuLabel>页面缩放</MenuLabel>
      <div className="flex flex-wrap gap-1 px-2 pb-1.5" role="group" aria-label="页面缩放">
        {ZOOMS.map((value) => (
          <button
            key={value}
            type="button"
            aria-pressed={prefs.pageZoom === value}
            onClick={() => onChange({ ...prefs, pageZoom: value })}
            className={
              prefs.pageZoom === value
                ? "cx-tabular rounded-md bg-cx-accent-soft px-1.5 py-0.5 text-[12px] font-medium text-cx-accent"
                : "cx-tabular rounded-md px-1.5 py-0.5 text-[12px] text-cx-fg-3 hover:bg-cx-hover hover:text-cx-fg"
            }
          >
            {Math.round(value * 100)}%
          </button>
        ))}
      </div>
      <MenuSeparator />
      <MenuItem
        icon="lock"
        keepOpen
        checked={prefs.persistent}
        description="登录状态和本地存储在重启后保留，同一服务的预览共用"
        onSelect={() => onChange({ ...prefs, persistent: !prefs.persistent })}
      >
        保留登录状态
      </MenuItem>
      <MenuItem icon="trash" danger disabled={!prefs.persistent} onSelect={onClearData}>
        清除浏览数据
      </MenuItem>
    </Menu>
  );
}
