"use client";

import { useEffect, useRef, useState } from "react";
import { Button, SearchInput, Shortcut } from "@/components/chat/ui";
import { ConversationShortcutsList } from "@/components/conversation/ConversationShortcutsHelp";
import { desktopChatBridge } from "@/lib/desktopChatBridge";
import { useLang } from "@/lib/i18n";
import {
  SHORTCUT_ACTIONS, bindingFromEvent, isMacPlatform, resetShortcutBinding, setShortcutBinding, useDesktopMenuAcceleratorSync, useShortcutBindings,
  type ShortcutActionMeta,
} from "@/lib/shortcutBindings";
import { SettingsNote, SettingsRow, SettingsSection } from "./primitives";

function CaptureButton({ en, onCapture, onCancel }: { en: boolean; onCapture: (binding: string) => void; onCancel: () => void }) {
  const ref = useRef<HTMLButtonElement>(null);
  useEffect(() => { ref.current?.focus(); }, []);
  return <button ref={ref} type="button" className="cx-settings-shortcut-capture" aria-label={en ? "Press the new key combination; Escape cancels" : "按下新的组合键，Esc 取消"}
    onBlur={onCancel}
    onKeyDown={event => {
      if (event.nativeEvent.isComposing) return;
      event.preventDefault(); event.stopPropagation();
      if (event.key === "Escape") { onCancel(); return; }
      const binding = bindingFromEvent(event.nativeEvent, isMacPlatform());
      if (binding) onCapture(binding);
    }}>
    {en ? "Press keys… (Esc cancels)" : "按下组合键…（Esc 取消）"}
  </button>;
}

export function ShortcutsSettings() {
  const { lang } = useLang(); const en = lang === "en";
  const t = (zh: string, english: string) => en ? english : zh;
  const [query, setQuery] = useState("");
  const bindings = useShortcutBindings();
  const [editing, setEditing] = useState<string>("");
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [storageError, setStorageError] = useState("");
  const desktop = Boolean(desktopChatBridge());
  const menuFollows = Boolean(desktopChatBridge()?.setMenuAccelerators);
  useDesktopMenuAcceleratorSync();
  const overridden = SHORTCUT_ACTIONS.some(action => bindings[action.id] !== action.defaultBinding);

  const capture = (action: ShortcutActionMeta, binding: string) => {
    const result = setShortcutBinding(action.id, binding, isMacPlatform());
    if (!result.ok) { setErrors(current => ({ ...current, [action.id]: result.error[en ? 1 : 0] })); return; }
    setErrors(current => { const next = { ...current }; delete next[action.id]; return next; });
    setStorageError(result.persisted ? "" : t("本地存储不可写，快捷键只在当前窗口生效。", "Local storage is not writable; shortcuts only apply to this window."));
    setEditing("");
  };
  const reset = (id?: ShortcutActionMeta["id"]) => {
    const persisted = resetShortcutBinding(id);
    setStorageError(persisted ? "" : t("本地存储不可写，快捷键只在当前窗口生效。", "Local storage is not writable; shortcuts only apply to this window."));
    setErrors(id ? current => { const next = { ...current }; delete next[id]; return next; } : {});
  };

  return <>
    <SettingsSection anchor="shortcuts-rebind" title={t("自定义快捷键", "Custom shortcuts")}
      description={t("点击“修改”后按下新的组合键。与其他快捷键或系统保留键冲突时不会保存。保存在这台设备上。", "Click Change, then press the new combination. Combinations that clash with another shortcut or a reserved key are not saved. Stored on this device.")}
      actions={<Button size="sm" variant="ghost" icon="refresh" disabled={!overridden} onClick={() => reset()}>{t("全部恢复默认", "Reset all")}</Button>}>
      {SHORTCUT_ACTIONS.map(action => {
        const binding = bindings[action.id];
        const changed = binding !== action.defaultBinding;
        const error = errors[action.id];
        const nativeNote = desktop && !menuFollows && action.desktopNative && changed
          ? t(`桌面菜单中的默认组合键仍然可用。`, "The desktop menu keeps its default combination as well.") : "";
        return <SettingsRow key={action.id} title={action.label[en ? 1 : 0]}
          description={error ? <span className="text-cx-danger" role="alert">{error}</span> : nativeNote || (changed ? <span>{t("默认：", "Default: ")}<Shortcut keys={action.defaultBinding} className="ml-1 inline-flex gap-[3px] align-middle [&>kbd]:h-5 [&>kbd]:min-w-5 [&>kbd]:text-[12px]" /></span> : undefined)}
          control={<div className="flex items-center gap-2">
            {editing === action.id
              ? <CaptureButton en={en} onCapture={value => capture(action, value)} onCancel={() => setEditing("")} />
              : <Shortcut keys={binding} className="gap-[3px] [&>kbd]:h-6 [&>kbd]:min-w-6 [&>kbd]:text-[12px]" />}
            <Button size="sm" variant="secondary" onClick={() => setEditing(editing === action.id ? "" : action.id)}>{t("修改", "Change")}</Button>
            {changed ? <Button size="sm" variant="ghost" onClick={() => reset(action.id)}>{t("恢复", "Reset")}</Button> : null}
          </div>} />;
      })}
    </SettingsSection>
    {storageError ? <SettingsNote tone="danger">{storageError}</SettingsNote> : null}

    <SettingsSection anchor="shortcuts-list" title={t("全部快捷键", "All shortcuts")}>
      <div className="flex flex-col gap-4 px-5 py-4">
        <SearchInput size="md" value={query} onValueChange={setQuery} placeholder={en ? "Search shortcuts or actions…" : "搜索快捷键或操作…"} aria-label={en ? "Search shortcuts" : "搜索快捷键"} />
        <ConversationShortcutsList query={query} />
      </div>
    </SettingsSection>
  </>;
}
