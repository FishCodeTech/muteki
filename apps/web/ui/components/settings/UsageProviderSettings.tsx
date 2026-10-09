"use client";

import { useEffect, useState } from "react";
import { Switch } from "@/components/chat/ui";
import { readUsageSettings, writeUsageSettings, type UsageSettings } from "@/lib/cursorAccountUsage";
import { useLang } from "@/lib/i18n";
import { SettingsRow, SettingsSection } from "./primitives";

type State = { status: "loading" } | { status: "ready"; settings: UsageSettings } | { status: "error"; message: string };

/** Account-wide usage sources the Muteki service reads with logins already on its host. */
export function UsageProviderSettings() {
  const { lang } = useLang();
  const t = (zh: string, en: string) => lang === "en" ? en : zh;
  const [state, setState] = useState<State>({ status: "loading" });
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState("");

  useEffect(() => {
    const controller = new AbortController();
    readUsageSettings(controller.signal)
      .then(settings => setState({ status: "ready", settings }))
      .catch(cause => { if (!controller.signal.aborted) setState({ status: "error", message: cause instanceof Error ? cause.message : String(cause) }); });
    return () => controller.abort();
  }, []);

  const toggle = async (enabled: boolean) => {
    setSaving(true);
    setSaveError("");
    try {
      setState({ status: "ready", settings: await writeUsageSettings({ cursor_account_usage_enabled: enabled }) });
    } catch (cause) {
      setSaveError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setSaving(false);
    }
  };

  const settings = state.status === "ready" ? state.settings : null;
  const keychain = settings?.keychain;
  const loginNote = !keychain?.applies ? ""
    : keychain.error ? t(`无法检查钥匙串：${keychain.error.message}`, `Could not check the Keychain: ${keychain.error.message}`)
    : keychain.login_present === false ? t("钥匙串中还没有 Cursor CLI 登录，先在服务所在的电脑上执行 cursor-agent login。", "No Cursor CLI login in the Keychain yet; run cursor-agent login on the service host first.")
    : keychain.login_present ? t("已在钥匙串中找到 Cursor CLI 登录。", "Found a Cursor CLI login in the Keychain.")
    : "";
  const description = state.status === "error"
    ? t(`无法读取服务端设置：${state.message}`, `Could not load the service setting: ${state.message}`)
    : saveError
    ? t(`保存失败：${saveError}`, `Could not save: ${saveError}`)
    : settings && !keychain?.applies
    ? t(`服务端不使用 macOS 钥匙串，直接读取 ${settings.auth_file ?? "CURSOR_AUTH_TOKEN"} 中的 Cursor CLI 登录，无需开启。`,
      `This service does not use the macOS Keychain; it reads the Cursor CLI login from ${settings.auth_file ?? "CURSOR_AUTH_TOKEN"} with no switch needed.`)
    : `${t("使用已有的钥匙串读取授权获取 Cursor CLI 登录，显示账号历史和月度额度。读取失败或超时会显示原因。",
      "Use existing Keychain read authorization for the Cursor CLI login to show account history and monthly limits. Read failures or timeouts show their cause.")}${loginNote ? ` ${loginNote}` : ""}`;

  return <SettingsSection anchor="usage-providers" title={t("用量来源", "Usage providers")}
    description={t("服务用本机已有的登录读取账号级用量；凭据只在服务端使用。", "The service reads account-wide usage with logins already on its host; credentials stay on the service.")}>
    <SettingsRow anchor="usage-cursor-account" title={t("Cursor 账号用量", "Cursor account usage")} description={description}
      control={keychain?.applies ? <Switch ariaLabel={t("Cursor 账号用量", "Cursor account usage")}
        checked={settings?.cursor_account_usage_enabled === true}
        disabled={state.status !== "ready" || saving}
        onCheckedChange={enabled => void toggle(enabled)} /> : undefined} />
  </SettingsSection>;
}
