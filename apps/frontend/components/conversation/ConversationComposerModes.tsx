"use client";

import { useEffect, useImperativeHandle, useMemo, useState, type Ref } from "react";
import { cn } from "@/lib/cn";
import { effectiveServiceTier, modelThinkingControls, modelServiceTiers, rememberedModelEffort, rememberServiceTier, validModelEffort } from "@/lib/modelReasoning";
import { modelVariantForParameter } from "@/lib/modelVariants";
import { allCredentialModels, type ConversationCredential, type ConversationCredentialModel } from "@/lib/useConversation";
import { useMediaQuery } from "@/lib/useMediaQuery";
import { isMacPlatform, matchesBinding, shortcutBinding, useShortcutBindings } from "@/lib/shortcutBindings";
import { Icon, type IconName } from "@muteki/ui";
import { Menu, MenuItem, MenuLabel, Popover, Tooltip, splitShortcut } from "@muteki/ui";
import { ComposerEffortSlider, serviceTierSpeedOptions, variantSpeedOption, type ComposerSpeedOption } from "./ComposerEffortSlider";
import { ComposerSpeedIcon } from "./ComposerSpeedIcon";

const EFFORT_LABELS: Record<string, string> = {
  "": "默认", default: "默认", none: "关闭", "no thinking": "关闭", thinking: "开启", minimal: "极低", low: "低", medium: "中",
  high: "高", xhigh: "超高", "extra-high": "超高", "extra high": "超高", max: "最大", off: "关闭", on: "开启", ultra: "极高",
};

export function effortLabelOf(effort: string): string {
  return EFFORT_LABELS[effort.toLowerCase()] || effort || EFFORT_LABELS[""];
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
  "cx-press inline-flex h-8 min-w-0 shrink-0 items-center gap-1 rounded-xl px-2.5 text-cx-sm text-cx-fg-3",
  "[&>.truncate]:-translate-y-[0.5px]",
  "hover:bg-cx-hover hover:text-cx-fg data-[state=open]:bg-cx-active data-[state=open]:text-cx-fg",
  "outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--cx-focus)]",
);

export interface ConversationComposerModesHandle {
  openEffort: () => void;
  openAccess: () => void;
}

export interface ConversationComposerModesProps {
  selectedCredentialId: string;
  selectedModel: string;
  selectedAccessMode: string;
  selectedModelMetadata?: ConversationCredentialModel;
  selectedCredential?: ConversationCredential;
  selectedEffort?: string;
  selectedServiceTier?: string;
  accessModes: string[];
  accessModeReason?: string;
  onSelect: (params: { credentialId: string; model: string; effort?: string; serviceTier?: string; accessMode?: string }) => void;
  interactionMode?: "default" | "plan";
  planModeAvailable?: boolean;
  planModeReason?: string;
  onInteractionModeChange?: (mode: "default" | "plan") => void;
  ref?: Ref<ConversationComposerModesHandle>;
  className?: string;
}

export function ConversationComposerModes({
  selectedCredentialId, selectedModel, selectedModelMetadata, selectedCredential, selectedEffort = "", selectedServiceTier = "", selectedAccessMode, accessModes, accessModeReason = "", onSelect,
  interactionMode = "default", planModeAvailable = false, planModeReason = "", onInteractionModeChange, ref, className,
}: ConversationComposerModesProps) {
  const compact = useMediaQuery(COMPACT_MQ);
  const bindings = useShortcutBindings();
  const [accessOpen, setAccessOpen] = useState(false);
  const [effortOpen, setEffortOpen] = useState(false);
  useImperativeHandle(ref, () => ({
    openEffort() { setEffortOpen(true); setAccessOpen(false); },
    openAccess() { setAccessOpen(true); setEffortOpen(false); },
  }), []);
  const { thinking, serviceTiers, variantFamily, speedOptions } = useMemo(() => {
    const models = selectedCredential ? allCredentialModels(selectedCredential) : [];
    const thinking = modelThinkingControls(selectedModelMetadata, models, selectedEffort);
    const serviceTiers = modelServiceTiers(selectedModelMetadata);
    const variantFamily = !serviceTiers.length && selectedModelMetadata?.variant?.parameters.speed !== undefined
      ? models.filter((model) => model.variant?.family_id === selectedModelMetadata.variant?.family_id)
      : [];
    const speedOptions: ComposerSpeedOption[] = serviceTiers.length
      ? serviceTierSpeedOptions(serviceTiers)
      : [...new Set(variantFamily.map((model) => model.variant?.parameters.speed).filter((value): value is string => value !== undefined))]
        .map((value) => variantSpeedOption(value, !selectedModelMetadata || !modelVariantForParameter(variantFamily, selectedModelMetadata, "speed", value)));
    return { thinking, serviceTiers, variantFamily, speedOptions };
  }, [selectedCredential, selectedModelMetadata, selectedEffort]);
  const effortLevels = thinking.choices.map(choice => choice.value);
  const activeTier = effectiveServiceTier(selectedModelMetadata, selectedServiceTier);
  const variantSpeed = variantFamily.length ? selectedModelMetadata?.variant?.parameters.speed ?? "" : "";
  const activeSpeedValue = serviceTiers.length ? activeTier : variantSpeed;
  const activeSpeed = speedOptions.find((option) => option.value === activeSpeedValue);
  const speedActive = Boolean(activeSpeed && activeSpeed.level !== "standard");
  const hasEffort = effortLevels.length > 0;
  const showEffort = hasEffort || speedOptions.length > 1;
  const activeEffort = thinking.value;
  const effortUnavailable = Boolean(hasEffort && activeEffort && !effortLevels.includes(activeEffort));
  const modelDefault = thinking.defaultValue;
  const labelOf = (value: string) => {
    const label = thinking.choices.find(choice => choice.value === value)?.label ?? value;
    return !thinking.variant && selectedModelMetadata?.reasoning?.kind === "variant" && label ? label : effortLabelOf(label);
  };
  const effortSummary = [
    hasEffort ? effortUnavailable ? "思考档位不可用，请重新选择" : `思考：${activeEffort ? labelOf(activeEffort) : modelDefault ? `${labelOf(modelDefault)}（默认）` : "跟随模型默认"}` : "",
    activeSpeed && speedOptions.length > 1 ? [`速度：${activeSpeed.label}`, activeSpeed.description].filter(Boolean).join(" · ") : "",
  ].filter(Boolean).join(" · ");
  const showAccess = accessModes.length > 0;
  const planMode = interactionMode === "plan";
  const showInteractionMode = typeof onInteractionModeChange === "function" && (planMode || planModeAvailable);
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
      if (showEffort && matchesBinding(event, shortcutBinding("effortPicker"), mac)) {
        event.preventDefault(); setEffortOpen(!effortOpen); setAccessOpen(false);
      } else if (showAccess && matchesBinding(event, shortcutBinding("accessPicker"), mac)) {
        event.preventDefault(); setAccessOpen(!accessOpen); setEffortOpen(false);
      } else if (
        showInteractionMode
        && !planToggleDisabled
        && matchesBinding(event, shortcutBinding("interactionMode"), mac)
        && isEditableTarget(event.target)
      ) {
        event.preventDefault();
        onInteractionModeChange?.(interactionMode === "plan" ? "default" : "plan");
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [showEffort, showAccess, effortOpen, accessOpen, setEffortOpen, setAccessOpen, showInteractionMode, planToggleDisabled, interactionMode, onInteractionModeChange]);

  if (!showEffort && !showAccess && !showInteractionMode) return null;

  const chooseAccess = (mode: string) => {
    if (mode === activeAccess || !accessModes.includes(mode)) return;
    onSelect({ credentialId: selectedCredentialId, model: selectedModel, accessMode: mode });
  };

  const chooseVariant = (next: ConversationCredentialModel | undefined) => {
    if (!next || next.id === selectedModel) return;
    onSelect({
      credentialId: selectedCredentialId,
      model: next.id,
      effort: validModelEffort(next, selectedEffort)
        ? selectedEffort
        : rememberedModelEffort(`${selectedCredentialId}:${selectedCredential?.runtime_instance || ""}`, next),
      serviceTier: effectiveServiceTier(next, selectedServiceTier),
    });
  };
  const chooseEffort = (value: string) => {
    if (thinking.variant) {
      chooseVariant(thinking.choices.find(choice => choice.value === (value || thinking.defaultValue))?.model);
    } else {
      onSelect({ credentialId: selectedCredentialId, model: selectedModel, effort: value });
    }
  };
  const chooseSpeed = (value: string) => {
    if (value === activeSpeedValue || !speedOptions.some((option) => option.value === value)) return;
    if (serviceTiers.length) {
      rememberServiceTier(value);
      onSelect({ credentialId: selectedCredentialId, model: selectedModel, serviceTier: value });
      return;
    }
    const next = selectedModelMetadata && modelVariantForParameter(variantFamily, selectedModelMetadata, "speed", value);
    chooseVariant(next);
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
      {showEffort ? (
        <Tooltip content={effortSummary} shortcut={splitShortcut(bindings.effortPicker)} disabled={effortOpen}>
          <span className="inline-flex min-w-0">
            <Popover
              open={effortOpen}
              onOpenChange={(open) => { setEffortOpen(open); if (open) setAccessOpen(false); }}
              placement="top-end"
              ariaLabel={hasEffort ? speedOptions.length > 1 ? "思考强度与速度" : "思考强度" : "速度"}
              className={cn(hasEffort ? "w-[256px]" : "w-[232px]", "max-w-[calc(100vw-24px)]")}
              trigger={(
                <button type="button" aria-label={effortSummary} aria-keyshortcuts="Meta+Shift+E"
                  className={cn(controlClass, effortUnavailable && "text-cx-warning")}
                  data-testid="composer-effort-control">
                  {hasEffort && !speedActive ? <Icon name="brain" size={14} className="shrink-0" /> : (
                    <ComposerSpeedIcon level={activeSpeed?.level ?? "standard"} size={14} className={speedActive ? "text-cx-accent" : undefined} />
                  )}
                  <span className="max-w-24 truncate">{hasEffort ? effortUnavailable ? "重选思考" : compact ? "思考" : `思考 · ${labelOf(activeEffort || modelDefault)}` : compact ? "速度" : `速度 · ${activeSpeed?.label ?? "标准"}`}</span>
                  <Icon name="chevronDown" size={12} className="shrink-0 text-cx-fg-4" />
                </button>
              )}
            >
              {effortUnavailable ? <p role="status" className="px-3.5 pt-3 text-cx-xs text-cx-warning">当前模型不支持已保存的档位，请重新选择。</p> : null}
              <ComposerEffortSlider
                key={`${selectedCredentialId}:${selectedModelMetadata?.variant?.family_id || selectedModel}`}
                levels={effortLevels}
                value={activeEffort}
                modelDefault={modelDefault}
                modelLabel={selectedModelMetadata?.variant?.family_label || selectedModelMetadata?.label || selectedModel}
                labelOf={labelOf}
                onChange={chooseEffort}
                canReset={thinking.canReset}
                speedOptions={speedOptions.length > 1 ? speedOptions : []}
                speed={activeSpeedValue}
                onSpeedChange={chooseSpeed}
              />
            </Popover>
          </span>
        </Tooltip>
      ) : null}
      {showInteractionMode && showAccess ? <span aria-hidden className="mx-0.5 h-4 w-px shrink-0 bg-cx-border-subtle" /> : null}
      {showAccess ? (
        <Tooltip content={accessUnavailable ? accessModeReason || "当前权限模式不可用，请重新选择" : access.detail} shortcut={splitShortcut(bindings.accessPicker)} disabled={accessOpen}>
          <span className="inline-flex">
            <Menu
              open={accessOpen}
              onOpenChange={(open) => { setAccessOpen(open); if (open) setEffortOpen(false); }}
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
              {accessUnavailable ? <p role="status" className="px-3 py-2 text-cx-xs text-cx-warning" data-testid="composer-access-mode-unavailable">{accessModeReason || "已保存的权限模式不可用，请明确选择其他模式后发送。"}</p> : null}
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
