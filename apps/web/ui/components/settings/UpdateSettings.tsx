"use client";

import { useEffect, useState } from "react";
import { Alert, Button, Card, Chip, ProgressBar } from "@heroui/react";
import { Icon } from "@/components/Icon";
import { PlatformUpdate } from "@/components/PlatformUpdate";
import styles from "@/components/PlatformUpdate.module.css";
import { desktopChatBridge, type DesktopUpdateStatus } from "@/lib/desktopChatBridge";
import { useLang } from "@/lib/i18n";
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
        <Card.Header className={styles.heading}>
          <div><Icon name="cpu" size={17} /><Card.Title>{t("客户端版本", "Desktop version")}</Card.Title></div>
          <Button size="sm" variant="secondary" className={styles.button} onClick={() => void run("check")} isDisabled={!updates || busy}><Icon name="refresh" size={14} />{action === "check" ? t("检查中…", "Checking…") : t("检查更新", "Check for updates")}</Button>
        </Card.Header>
        <Card.Content className={styles.versions} aria-live="polite">
          <div className={styles.version}><span>{t("当前运行版本", "Installed version")}</span><strong>{status?.currentVersion ? `v${status.currentVersion}` : "—"}</strong></div>
          <div className={styles.version}><span>{t("最新稳定版本", "Latest stable version")}</span><strong>{status?.latestVersion ? `v${status.latestVersion}` : t("待检查", "Not checked")}</strong><small>{t("上次检查：", "Last checked: ")}{status?.checkedAt ? new Date(status.checkedAt).toLocaleString(lang === "en" ? "en-US" : "zh-CN") : t("尚未检查", "Never")}</small></div>
        </Card.Content>
        <Card.Footer className={styles.status} aria-live="polite"><Chip size="sm" variant="soft" color={failure ? "danger" : available ? "warning" : "default"}>{status ? labels[status.state] : t("正在读取", "Loading")}</Chip><span>{status?.message || t("检查客户端及内置服务的完整版本。", "Check the complete desktop app and bundled service.")}</span></Card.Footer>
        {busy && status?.state !== "checking" ? <div className={styles.progress}><ProgressBar aria-label={t("更新进度", "Update progress")} value={status?.progress ?? 0}><div className={styles.progressHeading}><span>{status?.message}</span><ProgressBar.Output /></div><ProgressBar.Track><ProgressBar.Fill /></ProgressBar.Track></ProgressBar></div> : null}
      </Card>
      {failure || !updates ? <Alert status="danger" className={styles.notice}><Alert.Indicator><Icon name="alert" size={18} /></Alert.Indicator><Alert.Content><Alert.Title>{t("更新未完成", "Update unavailable")}</Alert.Title><Alert.Description>{failure || t("当前客户端未提供更新接口，请从发布页面安装新版客户端。", "This desktop app does not provide the update interface. Install the latest app from the release page.")}</Alert.Description></Alert.Content></Alert> : null}
      <Card className={styles.card}>
        <Card.Header className={styles.heading}><div><Icon name="download" size={17} /><Card.Title>{t("客户端更新", "Desktop updates")}</Card.Title></div></Card.Header>
        <Card.Content className={styles.actions}><div className={styles.actionRow}><span className={styles.actionIcon}><Icon name="download" size={18} /></span><div className={styles.actionCopy}><strong>{t("安装稳定版本", "Install stable release")}</strong><p>{status?.installReason || t("下载后验证版本，保存草稿并重启客户端。", "Download and validate the update, save drafts, and restart the app.")}</p></div><Button size="sm" variant="primary" className={`${styles.button} ${styles.primary}`} isDisabled={busy || !available || !status?.canInstall} onClick={() => void run("install")}>{status?.state === "ready" ? t("安装并重启", "Install and restart") : t("下载并更新", "Download and update")}</Button></div></Card.Content>
        <Card.Footer className={styles.retention}><Icon name="lock" size={13} />{t("更新前备份工作台数据，启动验收失败时恢复原版本。", "Workspace data is backed up before updating. Failed startup checks restore the previous app.")}</Card.Footer>
      </Card>
    </div>
    <aside className={styles.aside} aria-label={t("发布信息", "Release information")}><Card className={styles.card}>
      <Card.Header className={styles.heading}><div><Icon name="layers" size={17} /><Card.Title>{t("发布信息", "Release information")}</Card.Title></div></Card.Header>
      <Card.Content className={styles.metadata}><div><span>{t("安装方式", "Installation")}</span><strong>{t("macOS 客户端", "macOS app")}</strong></div><div><span>{t("更新通道", "Channel")}</span><Chip size="sm" variant="soft">stable</Chip></div></Card.Content>
      <Card.Footer className={styles.retention}><Button size="sm" variant="secondary" className={styles.button} onClick={() => void openRelease()}>{t("查看发布说明", "View release notes")}<Icon name="arrowUpRight" size={13} /></Button></Card.Footer>
    </Card></aside>
  </div></div>;
}

export function UpdateSettings() {
  return useSettingsHost().client === "desktop" ? <DesktopUpdateSettings /> : <PlatformUpdate />;
}
