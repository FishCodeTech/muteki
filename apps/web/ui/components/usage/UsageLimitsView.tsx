"use client";

import { useEffect, useState } from "react";
import Link from "next/link";
import { EngineLogo } from "@/components/EngineLogo";
import { Callout } from "@/components/chat/ui/Feedback";
import { engineName, timestampLabel, UNSUPPORTED_LIMIT_ENGINES, type UsageLimits, type UsageLimitAccount, type UsageLimitWindow } from "@/lib/providerUsage";
import styles from "../UsageDashboard.module.css";

function remainingLabel(resetAt: number, now: number) {
  const minutes = Math.max(0, Math.ceil((resetAt * 1000 - now) / 60_000));
  if (minutes === 0) return "等待窗口重置";
  const days = Math.floor(minutes / 1440);
  const hours = Math.floor(minutes % 1440 / 60);
  if (days) return `${days} 天 ${hours} 小时后重置`;
  if (hours) return `${hours} 小时 ${minutes % 60} 分钟后重置`;
  return `${minutes} 分钟后重置`;
}

function LimitWindow({ window, now }: { window: UsageLimitWindow; now: number }) {
  const remaining = window.used_percent === null ? null : Math.max(0, Math.min(100, 100 - window.used_percent));
  return <div className={styles.limitWindow}>
    <div className={styles.limitLabel}>{window.label || "额度窗口"}</div>
    <div className={styles.limitMeterRow}>
      <div className={styles.limitMeter} role={remaining === null ? "img" : "meter"} aria-label={`${window.label}剩余额度${remaining === null ? "未知" : ""}`} aria-valuemin={remaining === null ? undefined : 0} aria-valuemax={remaining === null ? undefined : 100} aria-valuenow={remaining ?? undefined} aria-valuetext={remaining === null ? "未知" : `${remaining.toFixed(0)}% 剩余`} data-low={remaining !== null && remaining <= 10}>
        {remaining !== null ? <span style={{ width: `${remaining}%` }} /> : null}
      </div>
      <span className={styles.limitAmount}>{remaining === null ? "未知" : `${remaining.toFixed(0)}%`}<small>{remaining === null ? "" : " 剩余"}</small></span>
    </div>
    <div className={styles.limitReset} title={window.reset_at === null ? undefined : new Date(window.reset_at * 1000).toLocaleString()}>{window.reset_at === null ? "重置时间未上报" : remainingLabel(window.reset_at, now)}</div>
  </div>;
}

function LimitCell({ account, kind, now }: { account: UsageLimitAccount; kind: UsageLimitWindow["kind"]; now: number }) {
  const matching = account.windows.filter(window => window.kind === kind);
  const separatePools = matching.filter(window => !window.aggregate);
  const windows = separatePools.length ? separatePools : matching;
  if (!windows.length) return <div className={styles.limitUnavailable}><span>—</span><small>{account.status === "ok" ? "未提供此窗口" : "暂无额度数据"}</small></div>;
  return <>{windows.map(window => <LimitWindow key={window.id} window={window} now={now} />)}</>;
}

function accountStatus(account: UsageLimitAccount) {
  if (account.stale) return "上次成功结果";
  if (account.status === "ok") return account.windows.length ? "已读取" : "未提供额度";
  if (account.status === "unauthenticated") return "未登录";
  if (account.status === "unsupported") return "账户不适用";
  if (account.status === "disabled") return "未开启";
  return "读取失败";
}

export function UsageLimitsView({ data }: { data: UsageLimits }) {
  const [selectedAccount, setSelectedAccount] = useState("");
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => { const timer = window.setInterval(() => setNow(Date.now()), 60_000); return () => window.clearInterval(timer); }, []);
  const supported = data.accounts.filter(account => !(UNSUPPORTED_LIMIT_ENGINES as readonly string[]).includes(account.engine));
  const currentAccount = supported.some(account => account.id === selectedAccount) ? selectedAccount : "";
  const selected = currentAccount ? supported.filter(account => account.id === currentAccount) : supported;
  const accounts = selected.filter(account => account.windows.length > 0);
  const accountStates = selected.filter(account => account.windows.length === 0);
  const extraWindows = accounts.filter(account => account.windows.some(window => window.kind === "other"));

  return <div className={styles.limitsView}>
    <div className={styles.contextBar}><label className={styles.inlineField}><span className={styles.srOnly}>额度账户</span><select value={currentAccount} onChange={event => setSelectedAccount(event.target.value)}><option value="">全部账户</option>{supported.map(account => <option key={account.id} value={account.id}>{engineName(account.engine)} · {account.label}</option>)}</select></label><p>按账户去重 · 以供应商重置时间为准</p></div>
    <div className={styles.sectionHeading}><h2>订阅额度</h2><span>更新于 {timestampLabel(data.as_of)}</span></div>
    {accounts.length ? <div className={styles.limitMatrix}>
      <div className={styles.limitTableHead} aria-hidden="true"><span>引擎 / 账户</span><span>会话窗口</span><span>周额度</span><span>月额度</span></div>
      {accounts.map(account => <article key={account.id} className={styles.limitRow} aria-label={`${engineName(account.engine)} ${account.label}`}>
        <div className={styles.limitIdentity}><EngineLogo engine={account.engine} size={26} /><div><h3>{account.engine === "opencode" ? "OpenCode Go" : engineName(account.engine)}</h3><p>{account.label}</p><span data-warning={account.status !== "ok" || account.stale} className={styles.accountStatus}>{accountStatus(account)}</span></div></div>
        {(["session", "weekly", "monthly"] as const).map(kind => <div key={kind} className={styles.limitCell}><span className={styles.mobileLimitLabel}>{kind === "session" ? "会话窗口" : kind === "weekly" ? "周额度" : "月额度"}</span><LimitCell account={account} kind={kind} now={now} /></div>)}
        {account.error ? <div className={styles.limitError}><Callout tone={account.stale ? "warning" : "danger"} role="status">{account.error}{account.stale ? ` · 保留 ${timestampLabel(account.updated_at)} 的结果` : ""}</Callout></div> : null}
      </article>)}
    </div> : <div className={styles.empty}><strong>暂无可显示的额度窗口</strong><p>{accountStates.length ? "账户的读取状态与具体原因见下方。" : "使用已接入引擎的订阅账户登录后，刷新额度即可查看。"}</p></div>}
    {accountStates.length ? <details key={currentAccount || "all"} className={styles.accountStates} open={currentAccount || accounts.length === 0 ? true : undefined}>
      <summary>账户状态 <span>{accountStates.length} 个账户暂无额度窗口</span></summary>
      <div>{accountStates.map(account => <article key={account.id}>
        <div className={styles.accountStateIdentity}><EngineLogo engine={account.engine} size={19} /><strong>{engineName(account.engine)}</strong><span>{account.label}</span><span className={styles.accountStatus} data-warning={account.status === "error"}>{accountStatus(account)}</span></div>
        <p>{account.error || "供应商暂未返回可用的订阅额度窗口。"}</p>
        {account.engine === "cursor" ? <div className={styles.accountAction}>{account.error_code === "keychain_disabled" ? <span>在已有的用量来源设置中开启 Cursor 账号用量后，再刷新此页面。</span> : null}<Link href="/settings/agents#setting-usage-cursor-account">用量来源设置 →</Link></div> : account.status === "unauthenticated" ? <div className={styles.accountAction}><Link href="/settings/agents#setting-agents-credentials">查看凭据与登录设置 →</Link></div> : null}
      </article>)}</div>
    </details> : null}
    {extraWindows.length ? <section className={styles.otherWindows}><h2>其他额度窗口</h2><div>{extraWindows.map(account => <article key={account.id}><h3>{engineName(account.engine)} · {account.label}</h3>{account.windows.filter(window => window.kind === "other").map(window => <LimitWindow key={window.id} window={window} now={now} />)}</article>)}</div></section> : null}
    <section className={styles.unsupportedLimits} aria-label="额度暂未支持的引擎"><div><h2>额度暂未支持</h2><p>这些引擎的 Token 和费用仍参与用量统计。</p></div><div className={styles.unsupportedList}>
      {UNSUPPORTED_LIMIT_ENGINES.map(engine => <div key={engine}><EngineLogo engine={engine} size={18} /><span>{engineName(engine)}</span><span>暂未支持</span></div>)}
    </div></section>
    <p className={styles.footnote}>额度与 Token、费用独立统计。OpenCode 额度仅适用于 Go 订阅；供应商没有返回的窗口不会推算为零。刷新会重新读取供应商额度。</p>
  </div>;
}
