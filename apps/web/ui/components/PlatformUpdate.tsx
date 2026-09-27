"use client";

import { Alert, Button, Card, Chip, ProgressBar } from "@heroui/react";
import styles from "./PlatformUpdate.module.css";

import { useCallback, useEffect, useMemo, useState } from "react";
import { Icon } from "@/components/Icon";
import {
  type PlatformUpdateStatus,
  checkPlatformUpdate,
  getPlatformUpdateStatus,
  installPlatformUpdate,
  rollbackPlatformUpdate,
} from "@/lib/useRun";

const ACTIVE_STATES = new Set(["checking", "downloading", "preparing", "switching"]);

function formatTime(value?: string | null): string {
  if (!value) return "尚未检查";
  const parsed = new Date(value);
  return Number.isNaN(parsed.valueOf()) ? value : parsed.toLocaleString("zh-CN", { hour12: false });
}

export function PlatformUpdate() {
  const [update, setUpdate] = useState<PlatformUpdateStatus | null>(null);
  const [action, setAction] = useState<"check" | "install" | "rollback" | null>(null);
  const [requestError, setRequestError] = useState("");

  const refresh = useCallback(async () => {
    const next = await getPlatformUpdateStatus();
    if (next) setUpdate(next);
  }, []);

  useEffect(() => { void refresh(); }, [refresh]);
  useEffect(() => {
    if (!update || (!update.running && !ACTIVE_STATES.has(update.status))) return;
    const timer = window.setInterval(() => void refresh(), 900);
    return () => window.clearInterval(timer);
  }, [refresh, update]);

  const busy = Boolean(action || update?.running || (update && ACTIVE_STATES.has(update.status)));
  const statusLabel = useMemo(() => {
    if (!update) return "正在读取";
    if (update.status === "available") return "发现新版本";
    if (update.status === "current") return "当前已是最新版本";
    if (update.status === "installed") return "安装完成";
    if (update.status === "rolled_back") return "回滚完成";
    if (update.status === "error") return "操作失败";
    if (ACTIVE_STATES.has(update.status)) return update.message || "升级处理中";
    return "等待检查";
  }, [update]);

  const run = useCallback(async (kind: "check" | "install" | "rollback") => {
    setAction(kind);
    setRequestError("");
    const next = kind === "check"
      ? await checkPlatformUpdate()
      : kind === "install"
        ? await installPlatformUpdate()
        : await rollbackPlatformUpdate();
    if (next) setUpdate(next);
    else setRequestError(kind === "check" ? "检查更新失败，请确认网络连接和发布地址。" : "操作未能启动，请查看服务端日志。")
    setAction(null);
  }, []);

  const runningVersion = update?.active_version || update?.current_version;
  const installation = !update ? "—" : update.install_kind === "compose" ? "容器部署" : update.install_kind === "managed" ? "托管安装" : "源码运行";
  const statusTone = update?.status === "error" ? "danger" : update?.available || update?.restart_required ? "warning" : "default";

  return (
    <div className={styles.page}>
      <div className={styles.layout}>
        <div className={styles.main}>
          <Card className={styles.card}>
            <Card.Header className={styles.heading}>
              <div><Icon name="cpu" size={17} /><Card.Title>版本概览</Card.Title></div>
              <Button type="button" size="sm" variant="secondary" className={styles.button} onClick={() => void run("check")} isDisabled={busy}><Icon name="refresh" size={14} />{action === "check" ? "检查中…" : "检查更新"}</Button>
            </Card.Header>
            <Card.Content className={styles.versions} aria-live="polite">
              <div className={styles.version}>
                <span>当前运行版本</span>
                <strong>{runningVersion ? `v${runningVersion}` : "—"}</strong>
                <Chip size="sm" variant="soft">{update ? "正在运行" : "读取中…"}</Chip>
              </div>
              <div className={styles.version}>
                <span>发布版本</span>
                <strong>{update?.latest_version ? `v${update.latest_version}` : "待检查"}</strong>
                <small>上次检查：{formatTime(update?.checked_at)}</small>
              </div>
            </Card.Content>
            <Card.Footer className={styles.status} aria-live="polite">
              <Chip size="sm" variant="soft" color={statusTone}>{statusLabel}</Chip>
              <span>{update?.message || update?.error || "检查稳定版本更新"}</span>
            </Card.Footer>
            {busy || (update?.progress != null && update.progress < 100) ? (
              <div className={styles.progress}>
                <ProgressBar aria-label="更新进度" value={update?.progress ?? 0}>
                  <div className={styles.progressHeading}><span>{update?.message || "正在处理"}</span><ProgressBar.Output /></div>
                  <ProgressBar.Track><ProgressBar.Fill /></ProgressBar.Track>
                </ProgressBar>
              </div>
            ) : null}
          </Card>

          {update?.restart_required ? (
            <Alert status="warning" className={styles.notice}>
              <Alert.Indicator><Icon name="alert" size={18} /></Alert.Indicator>
              <Alert.Content><Alert.Title>需要重启 Muteki 服务</Alert.Title><Alert.Description>已安装 v{update.current_version}，重启后生效；当前仍运行 {runningVersion ? `v${runningVersion}` : "原版本"}。</Alert.Description></Alert.Content>
            </Alert>
          ) : null}
          {update?.error || requestError ? (
            <Alert status="danger" className={styles.notice}>
              <Alert.Indicator><Icon name="alert" size={18} /></Alert.Indicator>
              <Alert.Content><Alert.Title>更新未完成</Alert.Title><Alert.Description>{requestError || update?.error}</Alert.Description></Alert.Content>
            </Alert>
          ) : null}

          <Card className={styles.card}>
            <Card.Header className={styles.heading}><div><Icon name="refresh" size={17} /><Card.Title>版本管理</Card.Title></div></Card.Header>
            <Card.Content className={styles.actions}>
              <div className={styles.actionRow}>
                <span className={styles.actionIcon}><Icon name="download" size={18} /></span>
                <div className={styles.actionCopy}>
                  <strong>版本升级</strong>
                  <p>{update?.install_kind === "source" ? "首次安装创建托管目录，保留当前任务记录和凭据。" : update?.deployment === "compose" ? "使用终端命令更新容器部署" : "从稳定通道安装发布版本"}</p>
                </div>
                <Button type="button" size="sm" variant="primary" className={`${styles.button} ${styles.primary}`} isDisabled={busy || !update?.available || update?.deployment === "compose"} onClick={() => void run("install")}><Icon name="upload" size={14} />{action === "install" ? "正在启动…" : update?.deployment === "compose" ? "使用终端升级" : update?.install_kind === "source" ? "安装托管版本" : "立即升级"}</Button>
              </div>
              <div className={styles.actionRow}>
                <span className={styles.actionIcon}><Icon name="retry" size={18} /></span>
                <div className={styles.actionCopy}>
                  <strong>版本回滚{update?.previous_version ? <Chip size="sm" variant="soft">v{update.previous_version}</Chip> : null}</strong>
                  <p>{update?.previous_version ? `可恢复到 v${update.previous_version}` : "升级后可回滚到上一版本"}</p>
                </div>
                <Button type="button" size="sm" variant="secondary" className={styles.button} isDisabled={busy || !update?.previous_version || update?.deployment === "compose"} onClick={() => void run("rollback")}><Icon name="retry" size={14} />{action === "rollback" ? "回滚中…" : update?.deployment === "compose" ? "使用终端回滚" : "回滚版本"}</Button>
              </div>
            </Card.Content>
            <Card.Footer className={styles.retention}><Icon name="lock" size={13} />升级与回滚保留任务数据及凭据</Card.Footer>
          </Card>
        </div>

        <aside className={styles.aside} aria-label="部署与终端管理">
          <Card className={styles.card}>
            <Card.Header className={styles.heading}><div><Icon name="layers" size={17} /><Card.Title>部署信息</Card.Title></div></Card.Header>
            <Card.Content className={styles.metadata}>
              <div><span>安装方式</span><strong>{installation}</strong></div>
              <div><span>更新通道</span><Chip size="sm" variant="soft">{update?.channel || "stable"}</Chip></div>
              <div className={styles.installPath}><span>安装目录</span><code>{update?.install_root || "—"}</code></div>
            </Card.Content>
          </Card>
          <Card className={styles.card}>
            <Card.Header className={styles.heading}><div><Icon name="terminal" size={17} /><Card.Title>终端管理</Card.Title></div></Card.Header>
            <Card.Content className={styles.commands}>
              <div><span>升级版本</span><code>{update?.deployment === "compose" ? "muteki upgrade --compose" : "muteki upgrade"}</code></div>
              <div><span>回滚版本</span><code>{update?.deployment === "compose" ? "muteki rollback --compose" : "muteki rollback"}</code></div>
            </Card.Content>
          </Card>
        </aside>
      </div>
    </div>
  );
}
