"use client";

import { useCallback, useEffect, useState } from "react";
import { Badge, Button, Callout, Input, Select, type Tone } from "@muteki/ui";
import { SettingsNote, SettingsRow, SettingsSection } from "./primitives";
import { useLang } from "@/lib/i18n";
import { apiFetch } from "@/lib/serviceAuth";

type Mode = "inherit" | "direct" | "custom";
type ProxyView = {
  mode: Mode; url: string; username: string; no_proxy: string; has_password: boolean;
  effective_no_proxy: string; required_no_proxy: string; default_no_proxy: string;
  proxy_display: string | null; test_url: string;
};
type Draft = { mode: Mode; url: string; username: string; no_proxy: string; password: string; clear_password: boolean };
type Leg = { ok: boolean; ip?: string; loc?: string | null; error?: string; elapsed_ms: number };
type TestStatus = "proxied" | "same_exit" | "mismatch" | "failed" | "direct" | "inherit";
type TestResult = { ok: boolean; status: TestStatus; mode: Mode; proxy_display: string | null; test_url: string; legs: { direct: Leg; server: Leg; process: Leg } };

const ENDPOINT = "/api/settings/network-proxy";

function draftOf(view: ProxyView): Draft {
  return { mode: view.mode, url: view.url, username: view.username, no_proxy: view.no_proxy, password: "", clear_password: false };
}

async function readJson(response: Response) {
  const body = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(String(body?.detail ?? body?.error?.message ?? `HTTP ${response.status}`));
  return body;
}

export function NetworkProxySettings() {
  const { lang } = useLang(); const en = lang === "en";
  const t = (zh: string, english: string) => en ? english : zh;
  const [view, setView] = useState<ProxyView | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState(false);
  const [result, setResult] = useState<TestResult | null>(null);
  const [testError, setTestError] = useState("");

  const load = useCallback(async (signal?: AbortSignal) => {
    try {
      setError("");
      const data = await readJson(await apiFetch(ENDPOINT, { cache: "no-store", signal })) as ProxyView;
      setView(data); setDraft(draftOf(data));
    } catch (cause) { if (!signal?.aborted) setError(cause instanceof Error ? cause.message : String(cause)); }
  }, []);
  useEffect(() => { const controller = new AbortController(); void load(controller.signal); return () => controller.abort(); }, [load]);

  if (!view || !draft) return <SettingsNote tone={error ? "danger" : "neutral"}>{error || t("正在读取网络代理设置…", "Loading network proxy settings…")}
    {error ? <Button className="ml-3" size="sm" variant="secondary" onClick={() => void load()}>{t("重试", "Retry")}</Button> : null}</SettingsNote>;

  const dirty = draft.mode !== view.mode || draft.url !== view.url || draft.username !== view.username
    || draft.no_proxy !== view.no_proxy || Boolean(draft.password) || draft.clear_password;
  const update = (patch: Partial<Draft>) => { setDraft(current => current ? { ...current, ...patch } : current); setNotice(""); setError(""); setResult(null); setTestError(""); };
  const payload = () => ({
    mode: draft.mode, url: draft.url.trim(), username: draft.username.trim(), no_proxy: draft.no_proxy,
    ...(draft.password ? { password: draft.password } : {}), ...(draft.clear_password ? { clear_password: true } : {}),
  });
  const save = async () => {
    setSaving(true); setError(""); setNotice("");
    try {
      const data = await readJson(await apiFetch(ENDPOINT, { method: "PUT", headers: { "Content-Type": "application/json" }, body: JSON.stringify(payload()), signal: AbortSignal.timeout(20_000) })) as ProxyView;
      setView(data); setDraft(draftOf(data));
      setNotice(t("已保存。之后新启动的 Agent、Worker 和服务请求会使用新设置；正在运行的会话需要重新开始才会切换。", "Saved. Agents, Workers and service requests started from now on use it. Running sessions switch after they restart."));
    } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
    finally { setSaving(false); }
  };
  const test = async () => {
    setTesting(true); setTestError(""); setResult(null);
    try {
      setResult(await readJson(await apiFetch(`${ENDPOINT}/test`, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(payload()), signal: AbortSignal.timeout(60_000) })) as TestResult);
    } catch (cause) { setTestError(cause instanceof Error ? cause.message : String(cause)); }
    finally { setTesting(false); }
  };

  const verdicts: Record<TestStatus, { tone: Tone; label: string; detail: string }> = {
    proxied: { tone: "success", label: t("代理已生效", "Proxy in use"), detail: t("服务请求和子进程都从代理出口访问外网，且与直连出口不同。", "Service requests and child processes reach the internet through the proxy exit, which differs from the direct exit.") },
    same_exit: { tone: "warning", label: t("无法确认", "Not confirmed"), detail: t("请求成功，但代理出口与直连出口相同，无法确认流量经过代理。代理可能没有改变出口，或直连本身也被系统转发。", "Requests succeeded, but the proxy exit equals the direct exit, so proxy use cannot be confirmed. The proxy may not change the exit, or direct traffic is already forwarded.") },
    mismatch: { tone: "danger", label: t("出口不一致", "Exits differ"), detail: t("服务请求与子进程的出口不同，部分流量没有走同一个代理。", "Service requests and child processes use different exits, so some traffic does not use the same proxy.") },
    failed: { tone: "danger", label: t("验证失败", "Check failed"), detail: t("至少一路请求失败，请检查代理地址、认证和代理本身是否可用。", "At least one request failed. Check the proxy address, credentials and whether the proxy is reachable.") },
    direct: { tone: "neutral", label: t("直连", "Direct"), detail: t("直连模式不使用任何代理，下面是直接访问外网的出口。", "Direct mode uses no proxy. The exits below are direct.") },
    inherit: { tone: "neutral", label: t("继承环境", "Inherited"), detail: t("服务和子进程使用服务启动时环境变量中的代理（如有）。", "The service and child processes use any proxy from the environment the service started with.") },
  };
  const legs: Array<{ key: keyof TestResult["legs"]; title: string; description: string }> = [
    { key: "direct", title: t("直连出口", "Direct exit"), description: t("不使用代理，用于对比。", "Without a proxy, for comparison.") },
    { key: "server", title: t("服务请求", "Service requests"), description: t("服务自身发出的请求，例如模型测试、规划器和平台接口。", "Requests from the service itself, such as model tests, the planner and platform APIs.") },
    { key: "process", title: t("Agent 与工具子进程", "Agent and tool processes"), description: t("以 Agent 子进程相同的环境变量运行 curl。", "curl run with the same environment as Agent processes.") },
  ];
  const legText = (leg: Leg) => leg.ok ? `${leg.ip}${leg.loc ? ` · ${leg.loc}` : ""} · ${leg.elapsed_ms} ms` : `${t("失败", "Failed")} · ${leg.elapsed_ms} ms`;
  const verdict = result ? verdicts[result.status] : null;

  return <>
    <SettingsSection anchor="network-proxy-mode" title={t("代理设置", "Proxy")}
      description={t("一处设置，整个软件生效：聊天 Agent（ACP 与原生）、CTF / 渗透 Worker、编排与集群，以及服务自身的外部请求。", "One setting for the whole app: chat Agents (ACP and native), CTF and pentest Workers, orchestration and swarms, and the service's own outbound requests.")}
      actions={<Button size="sm" variant="primary" loading={saving} disabled={saving || !dirty} onClick={() => void save()}>{t("保存", "Save")}</Button>}>
      <SettingsRow title={t("代理模式", "Proxy mode")}
        description={draft.mode === "custom" ? t("所有外部流量经过下面的代理。", "All outbound traffic goes through the proxy below.")
          : draft.mode === "direct" ? t("忽略环境变量中的代理，全部直接连接。", "Ignore proxies from the environment and connect directly.")
            : t("沿用服务启动时环境变量中的 HTTP_PROXY / HTTPS_PROXY / ALL_PROXY。", "Use HTTP_PROXY / HTTPS_PROXY / ALL_PROXY from the environment the service started with.")}
        control={<Select<Mode> ariaLabel={t("代理模式", "Proxy mode")} className="w-52" value={draft.mode} onChange={mode => update({ mode })}
          options={[
            { value: "inherit", label: t("继承环境变量（默认）", "Inherit environment (default)") },
            { value: "direct", label: t("直连（不使用代理）", "Direct (no proxy)") },
            { value: "custom", label: t("自定义代理", "Custom proxy") },
          ]} />} />
      {draft.mode === "custom" ? <>
        <SettingsRow title={t("代理地址", "Proxy address")}
          description={t("支持 http://、https://、socks5://、socks5h://。容器 Worker 会把 127.0.0.1 / localhost 自动换成宿主机地址。", "Supports http://, https://, socks5:// and socks5h://. Container Workers map 127.0.0.1 / localhost to the host automatically.")}
          control={<Input className="w-64 font-cx-mono" size="sm" aria-label={t("代理地址", "Proxy address")} value={draft.url} placeholder="http://127.0.0.1:7890" spellCheck={false} autoComplete="off" onChange={event => update({ url: event.target.value })} />} />
        <SettingsRow title={t("用户名（可选）", "Username (optional)")} description={t("代理需要认证时填写。", "Fill in when the proxy requires authentication.")}
          control={<Input className="w-52 font-cx-mono" size="sm" aria-label={t("代理用户名", "Proxy username")} value={draft.username} placeholder={t("留空表示无需认证", "Leave empty for none")} spellCheck={false} autoComplete="off" onChange={event => update({ username: event.target.value })} />} />
        <SettingsRow title={t("密码（可选）", "Password (optional)")}
          description={view.has_password && !draft.clear_password ? t("已保存密码；留空保存会保留原密码。", "A password is saved. Leave empty to keep it.") : t("只保存在服务端私有文件中，日志和错误信息会自动隐藏。", "Stored only in a private file on the service. Logs and errors hide it.")}
          control={<div className="flex items-center gap-2">
            {view.has_password && !draft.clear_password ? <Badge tone="success">{t("已保存", "Saved")}</Badge> : null}
            <Input className="w-44 font-cx-mono" size="sm" type="password" autoComplete="new-password" aria-label={t("代理密码", "Proxy password")} value={draft.password}
              placeholder={view.has_password && !draft.clear_password ? t("留空保持不变", "Unchanged") : t("可选", "Optional")} onChange={event => update({ password: event.target.value, clear_password: false })} />
            {view.has_password && !draft.clear_password ? <Button size="xs" variant="ghost" onClick={() => update({ password: "", clear_password: true })}>{t("清除", "Clear")}</Button> : null}
          </div>} />
        <SettingsRow title={t("不走代理的地址", "Bypass list")}
          description={<>{t("逗号分隔的域名、IP 或 CIDR。留空时内网地址直连：", "Comma-separated domains, IPs or CIDRs. When empty, private networks connect directly: ")}<code className="font-cx-mono">{view.default_no_proxy}</code>{t("。本机地址始终直连：", ". Local addresses always connect directly: ")}<code className="font-cx-mono">{view.required_no_proxy}</code></>}
          control={<Input className="w-64 font-cx-mono" size="sm" aria-label={t("不走代理的地址", "Bypass list")} value={draft.no_proxy} placeholder={view.default_no_proxy} spellCheck={false} autoComplete="off" onChange={event => update({ no_proxy: event.target.value })} />} />
      </> : null}
    </SettingsSection>
    {error ? <SettingsNote tone="danger">{error}</SettingsNote> : null}
    {notice ? <SettingsNote>{notice}</SettingsNote> : null}

    <SettingsSection anchor="network-proxy-test" title={t("验证", "Verify")}
      description={<>{t("用当前填写的设置（无需先保存）访问 ", "Uses the values above (no need to save first) to request ")}<code className="font-cx-mono">{view.test_url}</code>{t("，对比直连、服务请求和子进程三路出口。", " and compares the direct, service and child-process exits.")}</>}
      actions={<Button size="sm" variant="secondary" icon="refresh" loading={testing} disabled={testing} onClick={() => void test()}>{testing ? t("验证中…", "Checking…") : t("验证代理", "Verify proxy")}</Button>}>
      {verdict ? <SettingsRow title={t("结果", "Result")} description={verdict.detail} control={<Badge tone={verdict.tone}>{verdict.label}</Badge>} /> : null}
      {result ? legs.map(leg => <SettingsRow key={leg.key} title={leg.title} description={leg.description}
        control={<span className={`font-cx-mono text-cx-xs ${result.legs[leg.key].ok ? "text-cx-fg-2" : "text-cx-danger"}`}>{legText(result.legs[leg.key])}</span>}>
        {!result.legs[leg.key].ok && result.legs[leg.key].error ? <p className="whitespace-pre-wrap break-all font-cx-mono text-cx-xs leading-5 text-cx-danger">{result.legs[leg.key].error}</p> : null}
      </SettingsRow>) : null}
      {!result && !testError ? <SettingsRow title={t("尚未验证", "Not checked yet")} description={t("点击“验证代理”查看实际出口。", "Select Verify proxy to see the actual exits.")} /> : null}
      {testError ? <div className="px-5 py-3"><Callout tone="danger" title={t("验证请求失败", "Verification request failed")}>{testError}</Callout></div> : null}
    </SettingsSection>
    <SettingsNote>{t("Docker 部署的 Web 服务中，127.0.0.1 指容器自身；请填写宿主机或局域网中代理的地址。", "In a Docker Web deployment, 127.0.0.1 refers to the container. Use the proxy's host or LAN address instead.")}</SettingsNote>
  </>;
}
