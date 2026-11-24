"use client";

import { useEffect, useRef, useState } from "react";
import { useLang } from "@/lib/i18n";
import { Button, Checkbox, Input, Label } from "@muteki/ui";
import { desktopChatBridge, type DesktopSelectedPath } from "@/lib/desktopChatBridge";
import { nativeCapability, nativeDisplayMessage, useNativeDesktopState } from "@/lib/nativeDesktop";

export interface NativePathPickerProps {
  id: string; label: string; value: string; onChange: (path: string) => void;
  kind: "directory" | "file"; disabled?: boolean; placeholder?: string;
  onSelection?: (selection: DesktopSelectedPath) => void;
  onServerPath?: (path: string) => void;
  onBusyChange?: (busy: boolean) => void;
  inputTestId?: string; describedBy?: string; invalid?: boolean;
}

export function NativePathPicker({ id, label, value, onChange, kind, disabled = false, placeholder, onSelection, onServerPath, onBusyChange, inputTestId, describedBy, invalid }: NativePathPickerProps) {
  const { lang } = useLang(); const en = lang === "en";
  const t = (zh: string, english: string) => en ? english : zh;
  const { desktop, state, error: stateError } = useNativeDesktopState();
  const capability = nativeCapability(state, "pathSelection");
  const pickerAvailable = capability.available && Boolean(desktopChatBridge()?.selectPath);
  const [selection, setSelection] = useState<DesktopSelectedPath | null>(null);
  const [confirmed, setConfirmed] = useState(false);
  const [picking, setPicking] = useState(false);
  const [error, setError] = useState("");
  const [status, setStatus] = useState("");
  const trigger = useRef<HTMLButtonElement>(null);
  const alive = useRef(true);
  const kindRef = useRef(kind); kindRef.current = kind;
  useEffect(() => { alive.current = true; return () => { alive.current = false; onBusyChange?.(false); }; }, [onBusyChange]);
  useEffect(() => { setSelection(null); setConfirmed(false); setError(""); setStatus(""); }, [kind]);
  useEffect(() => { setConfirmed(false); }, [state?.connectionVersion, state?.serviceId, state?.identityId]);
  const select = async () => {
    const bridge = desktopChatBridge();
    if (!capability.available || !bridge?.selectPath || picking || disabled) return;
    const selectedKind = kind;
    setPicking(true); onBusyChange?.(true); setError(""); setStatus("");
    try {
      const result = await bridge.selectPath({ kind: selectedKind });
      if (!alive.current || kindRef.current !== selectedKind) return;
      if (!result) { setStatus(t("已取消选择，服务宿主路径保持原值。", "Selection canceled; the service host path is unchanged.")); return; }
      if (!result.id || !result.path || result.host !== "desktop-client" || result.serverMapped !== false) throw new Error(t("桌面返回了无效的路径或宿主范围。", "The desktop returned an invalid path or host scope."));
      setSelection(result); setConfirmed(false); onSelection?.(result);
    } catch (cause) {
      if (alive.current) setError(nativeDisplayMessage(cause instanceof Error ? cause : { message: String(cause) }, en));
    } finally {
      if (alive.current) { setPicking(false); onBusyChange?.(false); trigger.current?.focus(); }
    }
  };
  return <div className="flex flex-col gap-2" aria-busy={picking || undefined}>
    <Label htmlFor={id} className="-mb-0.5">{label}{desktop ? t(" · 服务宿主路径", " · service host path") : ""}</Label>
    <Input id={id} value={value} onChange={event => onChange(event.target.value)} placeholder={placeholder} disabled={disabled || picking} data-testid={inputTestId} invalid={invalid} aria-describedby={describedBy} autoComplete="off" spellCheck={false} />
    {desktop && <>
      <Button ref={trigger} variant="secondary" size="sm" disabled={disabled || picking || !pickerAvailable} onClick={() => void select()} aria-describedby={`${id}-scope`} className="self-start">
        {picking ? t("正在选择…", "Selecting…") : t(`选择桌面本机${kind === "directory" ? "目录" : "文件"}`, `Choose a client ${kind === "directory" ? "directory" : "file"}`)}
      </Button>
      <p id={`${id}-scope`} className="text-cx-xs leading-4 text-cx-fg-4">{t("原生选择属于桌面本机。当前服务可能在远程或容器中；服务会校验上方输入路径。", "Native selections belong to this desktop client. The service may be remote or containerized and validates the path entered above.")}</p>
      {selection && <div className="flex flex-col gap-2.5 rounded-xl border border-cx-border-subtle bg-cx-bg-subtle p-3 text-cx-xs">
        <p className="text-cx-fg-2">{t("桌面本机路径：", "Client path: ")}<code className="break-all font-cx-mono">{selection.path}</code></p>
        <Checkbox checked={confirmed} disabled={disabled || picking} onCheckedChange={setConfirmed} label={t("已确认当前服务可访问这个路径", "I have confirmed that the current service can access this path.")} />
        <Button variant="secondary" size="sm" disabled={!confirmed || disabled || picking} onClick={() => { onChange(selection.path); onServerPath?.(selection.path); setStatus(t("已填写服务宿主路径，实际可访问性由服务校验。", "The service host path is filled in; the service validates accessibility.")); }} className="self-start">{t("使用这个服务路径", "Use this service path")}</Button>
      </div>}
      {!pickerAvailable && <p className="text-cx-xs leading-4 text-cx-fg-4">{stateError || (!capability.available ? nativeDisplayMessage(capability, en) : t("桌面路径选择接口不可用，请使用服务宿主路径输入。", "The native picker is unavailable. Enter a service host path."))}</p>}
    </>}
    {onServerPath && <Button variant="secondary" size="sm" disabled={disabled || picking || !value.trim()} onClick={() => onServerPath(value.trim())} className="self-start">{t("使用输入的服务宿主路径", "Use the entered service host path")}</Button>}
    {status && <p role="status" className="text-cx-xs leading-4 text-cx-fg-4">{status}</p>}
    {error && <p role="alert" className="text-cx-xs text-cx-danger">{error}</p>}
  </div>;
}
