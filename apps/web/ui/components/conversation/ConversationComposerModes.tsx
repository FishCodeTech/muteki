"use client";

import { useEffect, useState } from "react";
import { cn } from "@/lib/cn";
import { useMediaQuery } from "@/lib/useMediaQuery";
import { isMacPlatform, matchesBinding, shortcutBinding, useShortcutBindings } from "@/lib/shortcutBindings";
import { Icon, type IconName } from "@/components/Icon";
import { Menu, MenuItem, MenuLabel, Tooltip, splitShortcut } from "@/components/chat/ui";

const EFFORT_LABELS: Record<string, string> = {
  "": "默认", none: "关闭", minimal: "极低", low: "低", medium: "中",
  high: "高", xhigh: "超高", "extra-high": "超高", max: "最大", off: "关闭", on: "开启", ultra: "极高",
};

export function effortLabelOf(effort: string): string {
  return EFFORT_LABELS[effort] || effort || EFFORT_LABELS[""];
}

const ACCESS_MODES: Record<string, { label: string; detail: string; icon: IconName }> = {
  supervised: { label: "严格监督", detail: "执行命令和修改文件前先询问", icon: "lock" },
  "auto-accept-edits": { label: "自动接受修改", detail: "自动批准文件修改，其他操作先询问", icon: "pencilLine" },
  auto: { label: "自动", detail: "常规操作自动执行，其余操作先询问", icon: "sparkles" },
  "full-access": { label: "完全访问", detail: "执行命令和修改文件时不再询问", icon: "shieldAlert" },
};

export function accessModeMeta(mode: string): { label: string; detail: string; icon: IconName } {
  return ACCESS_MODES[mode] || { label: mode, detail: "由当前 Agent 提供的访问模式", icon: "lock" };
}

const COMPACT_MQ = "(max-width: 640px)";

function isEditableTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  if (target.isContentEditable) return true;
  const tag = target.tagName;
  return tag === "TEXTAREA" || tag === "INPUT";
}


const controlClass = cn(
  "cx-press inline-flex h-8 min-w-0 shrink-0 items-center gap-1 rounded-full px-2.5 text-[13px] text-cx-fg-3",
  "hover:bg-cx-hover hover:text-cx-fg data-[state=open]:bg-cx-active data-[state=open]:text-cx-fg",
  "outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]",
);

export interface ConversationComposerModesProps {
  selectedCredentialId: string;
  selectedModel: string;
  selectedAccessMode: string;
  accessModes: string[];
  accessModeReason?: string;
  onSelect: (params: { credentialId: string; model: string; accessMode?: string }) => void;
  /** Per-thread interaction-mode preference; the toggle renders only when onInteractionModeChange is set. */
  interactionMode?: "default" | "plan";
  /** Resolved plan_mode capability of the selected runtime; false disables the toggle with a reason. */
  planModeAvailable?: boolean;
  planModeReason?: string;
  onInteractionModeChange?: (mode: "default" | "plan") => void;
  className?: string;
}

/** Plan mode and approval mode beside the send button; thinking effort lives in the model picker. */
export function ConversationComposerModes({
  selectedCredentialId, selectedModel, selectedAccessMode, accessModes, accessModeReason = "", onSelect,
  interactionMode = "default", planModeAvailable = false, planModeReason = "", onInteractionModeChange, className,
}: ConversationComposerModesProps) {
  const compact = useMediaQuery(COMPACT_MQ);
  const bindings = useShortcutBindings();
  const [accessOpen, setAccessOpen] = useState(false);
  const showAccess = accessModes.length > 0;
  const showInteractionMode = typeof onInteractionModeChange === "function";
  const planMode = interactionMode === "plan";
  const planToggleDisabled = !planMode && !planModeAvailable;
  const planSummary = planMode
    ? (planModeAvailable
      ? "规划模式：只读分析并产出计划，点击返回默认模式"
      : `规划模式：当前 Runtime 不再支持，点击返回默认模式。${planModeReason || "conversation.interaction_mode_unsupported"}`)
    : planModeAvailable
      ? "默认模式：点击进入规划模式（只读探索，先出计划再实施）"
      : `规划模式不可用：${planModeReason || "当前 Runtime 不支持规划模式"}`;
  const activeAccess = selectedAccessMode;
  const accessUnavailable = accessModes.length > 0 && !accessModes.includes(selectedAccessMode);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const mac = isMacPlatform();
      if (showAccess && matchesBinding(event, shortcutBinding("accessPicker"), mac)) {
        event.preventDefault(); setAccessOpen((value) => !value);
      } else if (
        showInteractionMode
        && !planToggleDisabled
        && matchesBinding(event, shortcutBinding("interactionMode"), mac)
        && isEditableTarget(event.target)
      ) {
        // Shift+Tab toggles plan mode only while typing in the composer;
        // elsewhere it keeps its focus-navigation meaning.
        event.preventDefault();
        onInteractionModeChange?.(interactionMode === "plan" ? "default" : "plan");
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [showAccess, showInteractionMode, planToggleDisabled, interactionMode, onInteractionModeChange]);

  if (!showAccess && !showInteractionMode) return null;

  const chooseAccess = (mode: string) => {
    if (mode === activeAccess || !accessModes.includes(mode)) return;
    onSelect({ credentialId: selectedCredentialId, model: selectedModel, accessMode: mode });
  };

  const access = accessModeMeta(activeAccess);
  const fullAccess = activeAccess === "full-access";

  return (
    <div className={cn("flex min-w-0 items-center gap-0.5", className)} data-testid="composer-mode-controls">
      {showInteractionMode ? (
        <Tooltip content={planSummary} shortcut={planToggleDisabled ? undefined : splitShortcut(bindings.interactionMode)}>
          <span className="inline-flex">
            <button
              type="button"
              aria-label={planSummary}
              aria-pressed={planMode}
              aria-keyshortcuts="Shift+Tab"
              disabled={planToggleDisabled}
              onClick={() => onInteractionModeChange?.(planMode ? "default" : "plan")}
              className={cn(
                controlClass,
                planMode && "bg-cx-active text-cx-fg",
                planToggleDisabled && "cursor-not-allowed opacity-50 hover:bg-transparent hover:text-cx-fg-3",
              )}
              data-testid="composer-interaction-mode-toggle"
              data-mode={interactionMode}
            >
              <Icon name="listChecks" size={14} className="shrink-0" />
              <span className={cn("truncate", compact && "sr-only")}>{planMode ? "规划" : "构建"}</span>
            </button>
          </span>
        </Tooltip>
      ) : null}
      {showInteractionMode && showAccess ? <span aria-hidden className="mx-0.5 h-4 w-px shrink-0 bg-cx-border-subtle" /> : null}
      {showAccess ? (
        <Tooltip content={accessUnavailable ? accessModeReason || "当前权限模式不可用，请重新选择" : access.detail} shortcut={splitShortcut(bindings.accessPicker)} disabled={accessOpen}>
          <span className="inline-flex">
            <Menu
              open={accessOpen}
              onOpenChange={setAccessOpen}
              placement="top-end"
              ariaLabel="Agent 操作权限"
              className="w-[300px]"
              trigger={(
                <button
                  type="button"
                  aria-label={accessUnavailable ? "权限不可用，请重新选择" : `权限：${access.label}`}
                  aria-keyshortcuts="Meta+Shift+A"
                  className={cn(controlClass, fullAccess && "text-cx-warning hover:bg-cx-warning-soft hover:text-cx-warning data-[state=open]:bg-cx-warning-soft data-[state=open]:text-cx-warning")}
                  data-testid="composer-access-control"
                >
                  <Icon name={access.icon} size={14} className="shrink-0" />
                  <span className={cn("truncate", compact && "sr-only")}>{accessUnavailable ? "请重新选择权限" : access.label}</span>
                  <Icon name="chevronDown" size={12} className={cn("shrink-0", fullAccess ? "text-cx-warning" : "text-cx-fg-4")} />
                </button>
              )}
            >
              <MenuLabel>权限</MenuLabel>
              {accessUnavailable ? <p role="status" className="px-3 py-2 text-xs text-cx-warning" data-testid="composer-access-mode-unavailable">{accessModeReason || "已保存的权限模式不可用，请明确选择其他模式后发送。"}</p> : null}
              {accessModes.map((mode) => {
                const meta = accessModeMeta(mode);
                return (
                  <MenuItem
                    key={mode}
                    icon={meta.icon}
                    checked={mode === activeAccess}
                    onSelect={() => chooseAccess(mode)}
                    description={meta.detail}
                    className={mode === "full-access" ? "text-cx-warning [&>svg:first-child]:text-cx-warning" : undefined}
                  >
                    {meta.label}
                  </MenuItem>
                );
              })}
            </Menu>
          </span>
        </Tooltip>
      ) : null}
    </div>
  );
}
