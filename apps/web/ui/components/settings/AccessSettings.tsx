"use client";

import { useCallback, useEffect, useState, type FormEvent } from "react";
import { Badge, Button, TextField } from "@/components/chat/ui";
import { SettingsNote, SettingsRow, SettingsSection } from "./primitives";
import { useLang } from "@/lib/i18n";
import { accessSettingsChanged, apiFetch, currentServiceOrigin, isNativeAuth, logout } from "@/lib/serviceAuth";

type AccessConfig = {
  auth_required: boolean; source: "settings" | "environment" | "none";
  environment_configured: boolean; has_override: boolean;
  session_ttl_seconds: number; can_disable: boolean; service_id: string;
};

export function AccessSettings() {
  const { lang } = useLang(); const en = lang === "en";
  const t = (zh: string, english: string) => en ? english : zh;
  const [config, setConfig] = useState<AccessConfig | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [action, setAction] = useState<"set" | "inherit">("set");
  const [current, setCurrent] = useState("");
  const [password, setPassword] = useState("");
  const [confirmation, setConfirmation] = useState("");
  const load = useCallback(async (signal?: AbortSignal) => {
    try {
      setError("");
      const response = await apiFetch("/api/settings/access", { signal: signal ? AbortSignal.any([signal, AbortSignal.timeout(15_000)]) : AbortSignal.timeout(15_000), cache: "no-store" });
      if (!response.ok) throw new Error(`无法读取访问设置（HTTP ${response.status}）`);
      const data = await response.json() as AccessConfig;
      if (typeof data.auth_required !== "boolean" || typeof data.environment_configured !== "boolean"
          || !["settings", "environment", "none"].includes(data.source)) throw new Error("访问设置返回了无效响应。");
      setConfig(data);
    } catch (cause) { if (!signal?.aborted) setError(cause instanceof Error ? cause.message : "无法读取访问设置。"); }
  }, []);
  useEffect(() => { const controller = new AbortController(); void load(controller.signal); return () => controller.abort(); }, [load]);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (!config || busy) return;
    if (config.auth_required && !current) { setError(t("请输入当前密码。", "Enter your current password.")); return; }
    if (action === "set" && (password.length < 8 || password !== confirmation)) {
      setError(t(password.length < 8 ? "新密码至少需要 8 个字符。" : "两次输入的新密码不一致。",
        password.length < 8 ? "Use at least 8 characters." : "The new passwords do not match.")); return;
    }
    setBusy(true); setError("");
    try {
      const response = await apiFetch("/api/settings/access", { method: "PUT", headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ action, current_password: current, ...(action === "set" ? { new_password: password } : {}) }),
        signal: AbortSignal.timeout(20_000) });
      const data = await response.json();
      if (!response.ok) throw new Error(data.error?.message || data.detail || `保存失败（HTTP ${response.status}）`);
      setCurrent(""); setPassword(""); setConfirmation(""); setConfig(data);
      accessSettingsChanged(data.auth_required === true);
    } catch (cause) { setError(cause instanceof Error ? cause.message : t("保存失败。", "Could not save.")); }
    finally { setBusy(false); }
  };
  const exit = async () => {
    setBusy(true); setError("");
    try { await logout(); }
    catch (cause) { setError(cause instanceof Error ? cause.message : t("退出失败。", "Could not sign out.")); }
    finally { setBusy(false); }
  };
  if (!config) return <SettingsNote tone={error ? "danger" : "neutral"}>{error || t("正在读取访问设置…", "Loading access settings…")}
    {error ? <Button className="ml-3" size="sm" variant="secondary" onClick={() => void load()}>{t("重试", "Retry")}</Button> : null}</SettingsNote>;
  const source = config.source === "settings" ? t("设置页面", "Settings")
    : config.source === "environment" ? t("环境变量", "Environment") : t("未设置", "Not configured");

  return <>
    <SettingsSection anchor="access-status" title={t("当前服务", "Connected service")}>
      <SettingsRow title={t("密码访问", "Password access")} description={t("网页端和桌面端共用当前服务的访问密码，各设备分别登录。", "Web and desktop share this service password. Each device signs in separately.")}
        control={<Badge tone={config.auth_required ? "success" : "warning"}>{config.auth_required ? t("已启用", "Enabled") : t("未启用", "Disabled")}</Badge>} />
      <SettingsRow title={t("配置来源", "Configuration source")} description={t("设置页面配置优先；恢复环境变量后，将使用服务启动时读取的环境变量。", "Settings take precedence. Restoring the environment uses the variables read when this service started.")} control={<span className="text-[13px] text-cx-fg-2">{source}</span>} />
      <SettingsRow title={t("环境变量密码", "Environment password")} description="MUTEKI_WEB_PASSWORD" control={<span className="text-[13px] text-cx-fg-2">{config.environment_configured ? t("已配置", "Configured") : t("未配置", "Not configured")}</span>} />
      <SettingsRow title={t("连接地址", "Service address")} description={<span className="break-all">{currentServiceOrigin()}</span>} />
    </SettingsSection>

    <SettingsSection anchor="access-password" title={t("访问密码", "Access password")} description={t("保存立即生效，并退出所有旧登录会话。密码不会保存在客户端。", "Saving takes effect immediately and signs out old sessions. The password is not stored on clients.")}>
      <form onSubmit={submit} noValidate className="flex flex-col gap-4 p-5" aria-busy={busy}>
        {config.has_override ? <div className="flex flex-wrap gap-2" role="group" aria-label={t("密码配置方式", "Password configuration")}>
          <Button size="sm" variant={action === "set" ? "secondary" : "ghost"} aria-pressed={action === "set"} disabled={busy} onClick={() => { setAction("set"); setError(""); }}>{t("设置密码", "Set password")}</Button>
          <Button size="sm" variant={action === "inherit" ? "secondary" : "ghost"} aria-pressed={action === "inherit"} disabled={busy} onClick={() => { setAction("inherit"); setError(""); }}>{t("恢复环境变量", "Restore environment")}</Button>
        </div> : null}
        {config.auth_required ? <TextField label={t("当前密码", "Current password")} type="password" autoComplete="current-password" value={current} onChange={event => setCurrent(event.target.value)} disabled={busy} required /> : null}
        {action === "set" ? <>
          <TextField label={t("新密码", "New password")} type="password" autoComplete="new-password" description={t("至少 8 个字符。", "At least 8 characters.")} minLength={8} value={password} onChange={event => setPassword(event.target.value)} disabled={busy} required />
          <TextField label={t("确认新密码", "Confirm new password")} type="password" autoComplete="new-password" value={confirmation} onChange={event => setConfirmation(event.target.value)} disabled={busy} required />
        </> : <SettingsNote tone={!config.environment_configured ? "danger" : "neutral"}>{config.environment_configured
          ? t("保存后将使用环境变量密码登录。", "After saving, sign in using the environment password.")
          : config.can_disable ? t("环境变量没有密码。保存后将关闭密码访问，仅适用于本机使用。", "No environment password is configured. Saving disables password access for local use.")
            : t("服务对外开放，必须先配置环境变量密码才能恢复。", "Configure an environment password before restoring access on an exposed service.")}</SettingsNote>}
        {error ? <SettingsNote tone="danger">{error}</SettingsNote> : null}
        <div className="flex justify-end"><Button variant="primary" type="submit" loading={busy} disabled={busy || (action === "inherit" && !config.environment_configured && !config.can_disable)}>{t(action === "set" ? "保存密码" : "确认恢复", action === "set" ? "Save password" : "Restore")}</Button></div>
      </form>
    </SettingsSection>

    <SettingsSection anchor="access-session" title={t("当前登录", "Current session")}>
      <SettingsRow title={t("登录有效期", "Session duration")} description={t(`每次登录最多保持 ${Math.round(config.session_ttl_seconds / 3600 * 10) / 10} 小时。修改密码会提前退出。`, `Each sign-in lasts up to ${Math.round(config.session_ttl_seconds / 3600 * 10) / 10} hours. Password changes sign you out sooner.`)} />
      <SettingsRow title={t("记住登录", "Remember sign-in")} description={isNativeAuth() ? t("桌面端使用系统安全存储保存会话；下次登录可选择仅本次使用。", "Desktop sessions use system secure storage. You can choose a temporary session when signing in.") : t("网页端会话由浏览器管理；下次登录可选择仅本次浏览器会话使用。", "Web sessions are managed by the browser. You can choose a temporary browser session when signing in.")} />
      {config.auth_required ? <SettingsRow title={t("退出当前登录", "Sign out")} description={t("退出这台设备的登录。服务中的对话和设置会保留。", "Sign out on this device. Chats and settings remain on the service.")} control={<Button variant="secondary" size="sm" disabled={busy} onClick={() => void exit()}>{t("退出登录", "Sign out")}</Button>} /> : null}
    </SettingsSection>
  </>;
}
