"use client";

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

  return (
    <div className="wsystem-page">
      <header className="wsettings-section-head">
        <div className="wsettings-section-copy"><h2>系统更新</h2><p>通过发布清单下载完整应用包，校验后切换版本。任务数据和凭据配置保留在独立目录。</p></div>
        <button type="button" className="wsystem-check" onClick={() => void run("check")} disabled={busy}><Icon name="refresh" size={14} />{action === "check" ? "检查中…" : "检查更新"}</button>
      </header>

      <section className="wsystem-overview" aria-live="polite">
        <div className="wsystem-version-card">
          <span>当前运行版本</span><strong>v{update?.active_version || update?.current_version || "—"}</strong>
          <small>{update?.install_kind === "compose" ? "容器部署" : update?.install_kind === "managed" ? "托管安装" : "源码运行"} · {update?.channel || "stable"} 通道</small>
        </div>
        <div className={`wsystem-version-card latest${update?.available ? " available" : ""}`}>
          <span>最新版本</span><strong>{update?.latest_version ? `v${update.latest_version}` : "待检查"}</strong>
          <small>上次检查：{formatTime(update?.checked_at)}</small>
        </div>
        <div className={`wsystem-state ${update?.status === "error" ? "bad" : update?.available || update?.restart_required ? "attention" : ""}`}>
          <i aria-hidden="true" /><span><strong>{statusLabel}</strong><small>{update?.message || update?.error || "可以随时检查 GitHub Release 中的稳定版本。"}</small></span>
        </div>
      </section>

      {busy || (update?.progress != null && update.progress < 100) ? <section className="wsystem-progress"><div><span>{update?.message || "正在处理"}</span><strong>{update?.progress ?? 0}%</strong></div><progress max={100} value={update?.progress ?? 0} /></section> : null}
      {update?.restart_required ? <section className="wsystem-notice"><Icon name="alert" size={16} /><span><strong>需要重启 Muteki 服务</strong><small>新版本已切换完成。当前页面仍由原进程提供，重启后加载 v{update.current_version}。</small></span></section> : null}
      {update?.error || requestError ? <section className="wsystem-notice error"><Icon name="alert" size={16} /><span><strong>更新未完成</strong><small>{requestError || update?.error}</small></span></section> : null}

      <section className="wsystem-actions">
        <div><h3>应用版本</h3><p>{update?.install_kind === "source" ? "首次安装会创建托管目录，并继续使用当前源码目录中的任务记录和 .env。" : `安装目录：${update?.install_root || "—"}`}</p></div>
        <button type="button" className="primary" disabled={busy || !update?.available || update?.deployment === "compose"} onClick={() => void run("install")}><Icon name="upload" size={14} />{action === "install" ? "正在启动…" : update?.deployment === "compose" ? "使用终端升级" : update?.install_kind === "source" ? "安装托管版本" : "立即升级"}</button>
      </section>
      <section className="wsystem-actions secondary">
        <div><h3>上一版本</h3><p>{update?.previous_version ? `可回滚到 v${update.previous_version}，数据目录保持不变。` : "完成一次版本升级后，这里会保留上一版本。"}</p></div>
        <button type="button" disabled={busy || !update?.previous_version || update?.deployment === "compose"} onClick={() => void run("rollback")}><Icon name="refresh" size={14} />{action === "rollback" ? "回滚中…" : update?.deployment === "compose" ? "使用终端回滚" : "回滚版本"}</button>
      </section>
      <footer className="wsystem-command"><span>终端也可以直接执行</span><code>{update?.deployment === "compose" ? "muteki upgrade --compose" : "muteki upgrade"}</code><code>{update?.deployment === "compose" ? "muteki rollback --compose" : "muteki rollback"}</code></footer>
    </div>
  );
}
