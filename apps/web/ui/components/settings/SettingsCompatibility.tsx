"use client";
import { useEffect, useState, useSyncExternalStore, type ReactNode } from "react";
import { useNativeDesktopState } from "@/lib/nativeDesktop";
import { subscribeUiPreferences, uiPreferenceSnapshot, serverUiPreferenceSnapshot } from "@/lib/uiPreferences";
import { apiFetch } from "@/lib/useRun";
import { SETTINGS_PAGES, type SettingsPageId } from "./catalog";
import { useLang } from "@/lib/i18n";
import { SettingsNote } from "./primitives";

export function SettingsCompatibility({page, children}: {page: SettingsPageId | null; children: ReactNode}) {
  const prefs = useSyncExternalStore(subscribeUiPreferences, uiPreferenceSnapshot, serverUiPreferenceSnapshot);
  const native = useNativeDesktopState();
  const {lang} = useLang(); const en = lang === "en";
  const [health, setHealth] = useState<{version?: string; features?: Record<string, number>} | null>(null);
  const [error, setError] = useState("");
  useEffect(() => {
    const abort = new AbortController();
    void apiFetch("/api/health", {signal: abort.signal}).then(async response => {
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      const value = await response.json();
      if (!value || typeof value !== "object" || (value.version !== undefined && typeof value.version !== "string")
        || (value.features !== undefined && (!value.features || typeof value.features !== "object" || Array.isArray(value.features) || Object.values(value.features).some(version => !Number.isSafeInteger(version))))) throw new Error("service.features.invalid: 服务功能声明无效");
      if (!abort.signal.aborted) setHealth(value);
    }).catch(cause => { if (!abort.signal.aborted) setError(String(cause)); });
    return () => abort.abort();
  }, []);
  const missing = page && health?.features ? SETTINGS_PAGES[page].requiredFeatures.filter(feature => health.features?.[feature] !== 1) : [];
  return <>
    {prefs.status === "saving" ? <span role="status" className="text-[12px] text-cx-fg-3">{en ? "Saving shared preferences…" : "正在保存共享偏好…"}</span> : null}
    {error ? <SettingsNote tone="danger">{en ? "Could not read service compatibility: " : "无法读取服务兼容信息："}{error}</SettingsNote> : null}
    {health && (!health.features || missing.length > 0) ? <SettingsNote tone="neutral">{!health.features
      ? (en ? "This service has not declared its settings features. Update the service to verify compatibility." : "当前服务尚未声明设置功能支持情况，请更新服务后确认兼容性。")
      : `${en ? "The service does not support these required features: " : "服务不支持此页面所需的功能："}${missing.join(", ")}`}</SettingsNote> : null}
    <details className="text-[12px] text-cx-fg-3"><summary>{en ? "Version compatibility" : "版本兼容信息"}</summary>
      <div>{en ? "This interface: " : "当前界面："}{process.env.NEXT_PUBLIC_MUTEKI_UI_BUILD || "development"}</div>
      <div>{en ? "Service: " : "服务："}{health?.version || (en ? "Unknown" : "未知")}</div>
      {native.desktop ? <><div>桌面客户端：{native.state?.desktopVersion || "未知"}</div><div>远程 Web 界面：{native.state?.remoteUiBuild || "尚未加载"}</div>
        {native.state?.remoteUiBuild && native.state.remoteUiBuild !== "unknown" && native.state.remoteUiBuild !== process.env.NEXT_PUBLIC_MUTEKI_UI_BUILD ? <p>{en ? "The local and remote interfaces use different builds; page content may differ. Feature availability follows the service contract." : "本地与远程界面的构建不同，页面内容可能存在差异；功能可用性以服务声明为准。"}</p> : null}
        {native.state?.remoteUiBuild && native.state.remoteAppearanceContract !== "1" ? <p>远程 Web 界面未声明外观同步契约，请更新服务端 Web 界面。</p> : null}</> : null}
    </details>
    {health && missing.length === 0 ? children : !health && !error ? <p role="status">{en ? "Checking service compatibility…" : "正在检查服务兼容性…"}</p> : null}
  </>;
}
