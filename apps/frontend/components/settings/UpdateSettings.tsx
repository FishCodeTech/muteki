"use client";

import { useEffect, useState } from "react";
import { Alert, Button, Card, CardBody, CardFooter, CardHeader, CardTitle, Chip, ProgressBar } from "@muteki/ui";
import { Icon } from "@muteki/ui";
import { PlatformUpdate } from "@/components/PlatformUpdate";
import styles from "@/components/PlatformUpdate.module.css";
import { desktopChatBridge, type DesktopUpdateStatus } from "@/lib/desktopChatBridge";
import { useLang } from "@/lib/i18n";
import { useNativeDesktopState } from "@/lib/nativeDesktop";
import { useSettingsHost } from "./SettingsHost";

const ACTIVE = new Set(["checking", "downloading", "validating", "installing"]);
const RELEASES = "https://github.com/FishCodeTech/muteki/releases/latest";

function DesktopUpdateSettings() {
  const { lang } = useLang();
  const t = (zh: string, en: string) => lang === "en" ? en : zh;
  const bridge = desktopChatBridge();
  const updates = bridge?.updates;
  const [status, setStatus] = useState<DesktopUpdateStatus | null>(null);
  const [action, setAction] = useState<"check" | "install" | null>(null);
  const [error, setError] = useState("");
  useEffect(() => {
    if (!updates) return;
    let active = true;
    let revision = 0;
    const accept = (next: DesktopUpdateStatus) => { if (active) { revision++; setStatus(next); } };
    const off = updates.onStatus(accept);
    const initialRevision = revision;
    void updates.getStatus().then(next => { if (revision === initialRevision) accept(next); }).catch(cause => {
      if (active) setError(cause instanceof Error ? cause.message : String(cause));
    });
    return () => { active = false; off(); };
  }, [updates]);
  const run = async (kind: "check" | "install") => {
    if (!updates) return;
    setAction(kind); setError("");
    try { setStatus(await updates[kind]()); }
    catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
    finally { setAction(null); }
  };
  const openRelease = async () => {
    try { await bridge?.openExternal?.(status?.releaseUrl || RELEASES); }
    catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
  };
  const busy = Boolean(action || (status && ACTIVE.has(status.state)));
  const available = status?.state === "available" || status?.state === "ready";
  const labels: Record<DesktopUpdateStatus["state"], string> = {
    idle: t("等待检查", "Ready to check"), checking: t("检查中…", "Checking…"), current: t("当前已是最新版本", "Up to date"),
    available: t("发现新版本", "Update available"), downloading: t("下载中…", "Downloading…"), validating: t("正在验证更新", "Validating update"),
    ready: t("更新已准备好", "Update ready"), installing: t("正在安装更新", "Installing update"), error: t("更新失败", "Update failed"),
  };
  const failure = error || status?.error?.message;
  return <div className={styles.page}><div className={styles.layout}>
    <div className={styles.main}>
      <Card className={styles.card}>
        <CardHeader className={styles.heading}>
          <div><Icon name="cpu" size={17} /><CardTitle>{t("客户端版本", "Desktop version")}</CardTitle></div>
          <Button size="sm" variant="secondary" className={styles.button} onClick={() => void run("check")} disabled={!updates || busy}><Icon name="refresh" size={14} />{action === "check" ? t("检查中…", "Checking…") : t("检查更新", "Check for updates")}</Button>
        </CardHeader>
        <CardBody className={styles.versions} aria-live="polite">
          <div className={styles.version}><span>{t("当前运行版本", "Installed version")}</span><strong>{status?.currentVersion ? `v${status.currentVersion}` : "—"}</strong></div>
          <div className={styles.version}><span>{t("最新稳定版本", "Latest stable version")}</span><strong>{status?.latestVersion ? `v${status.latestVersion}` : t("待检查", "Not checked")}</strong><small>{t("上次检查：", "Last checked: ")}{status?.checkedAt ? new Date(status.checkedAt).toLocaleString(lang === "en" ? "en-US" : "zh-CN") : t("尚未检查", "Never")}</small></div>
        </CardBody>
        <CardFooter className={styles.status} aria-live="polite"><Chip size="xs" variant="soft" tone={failure ? "danger" : available ? "warning" : "neutral"}>{status ? labels[status.state] : t("正在读取", "Loading")}</Chip><span>{status?.message || t("检查客户端及内置服务的完整版本。", "Check the complete desktop app and bundled service.")}</span></CardFooter>
        {busy && status?.state !== "checking" ? <div className={styles.progress}><ProgressBar ariaLabel={t("更新进度", "Update progress")} label={status?.message} value={status?.progress ?? 0} /></div> : null}
      </Card>
      {failure || !updates ? <Alert tone="danger" title={t("更新未完成", "Update unavailable")} className={styles.notice}>{failure || t("当前客户端未提供更新接口，请从发布页面安装新版客户端。", "This desktop app does not provide the update interface. Install the latest app from the release page.")}</Alert> : null}
      <Card className={styles.card}>
        <CardHeader className={styles.heading}><div><Icon name="download" size={17} /><CardTitle>{t("客户端更新", "Desktop updates")}</CardTitle></div></CardHeader>
        <CardBody className={styles.actions}><div className={styles.actionRow}><span className={styles.actionIcon}><Icon name="download" size={18} /></span><div className={styles.actionCopy}><strong>{t("安装稳定版本", "Install stable release")}</strong><p>{status?.installReason || t("下载后验证版本，保存草稿并重启客户端。", "Download and validate the update, save drafts, and restart the app.")}</p></div><Button size="sm" variant="primary" className={`${styles.button} ${styles.primary}`} disabled={busy || !available || !status?.canInstall} onClick={() => void run("install")}>{status?.state === "ready" ? t("安装并重启", "Install and restart") : t("下载并更新", "Download and update")}</Button></div></CardBody>
        <CardFooter className={styles.retention}><Icon name="lock" size={13} />{t("更新前备份工作台数据，启动验收失败时恢复原版本。", "Workspace data is backed up before updating. Failed startup checks restore the previous app.")}</CardFooter>
      </Card>
    </div>
    <aside className={styles.aside} aria-label={t("发布信息", "Release information")}><Card className={styles.card}>
      <CardHeader className={styles.heading}><div><Icon name="layers" size={17} /><CardTitle>{t("发布信息", "Release information")}</CardTitle></div></CardHeader>
      <CardBody className={styles.metadata}><div><span>{t("安装方式", "Installation")}</span><strong>{t("macOS 客户端", "macOS app")}</strong></div><div><span>{t("更新通道", "Channel")}</span><Chip size="xs" variant="soft">stable</Chip></div></CardBody>
      <CardFooter className={styles.retention}><Button size="sm" variant="secondary" className={styles.button} onClick={() => void openRelease()}>{t("查看发布说明", "View release notes")}<Icon name="arrowUpRight" size={13} /></Button></CardFooter>
    </Card></aside>
  </div></div>;
}

export function UpdateSettings() {
  const native = useNativeDesktopState();
  return useSettingsHost().client === "desktop" || native.desktop ? <DesktopUpdateSettings /> : <PlatformUpdate />;
}
