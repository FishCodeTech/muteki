"use client";

import { useCallback, useEffect, useId, useMemo, useState } from "react";
import { Badge, Button, Dialog, Input, Label, Select, StatusDot, Switch, toast, type Tone } from "@muteki/ui";
import { Icon } from "@muteki/ui";
import { cn } from "@/lib/cn";
import { apiFetch } from "@/lib/useRun";
import { fetchConversationProjects, type ConversationProject } from "@/lib/useConversation";
import { SettingsEmpty, SettingsNote, SettingsRow, SettingsSection } from "./primitives";

type Rules = { auto_settle_after_days?: number | null; auto_settle_on_merge?: boolean };
type HostKind = "github" | "gitlab" | "forgejo" | "gitea" | "bitbucket" | "azure";
type Host = { kind: HostKind; enabled: boolean; account?: string | null };
type CredentialSource =
  | { type: "saved" | "saved_bearer" }
  | { type: "saved_basic"; email: string }
  | { type: "env"; name: string }
  | { type: "cli"; tool: string };
type HostProblem =
  | { type: "no_credential"; tools_missing: string[] }
  | { type: "not_signed_in"; tool: string; command?: string | null }
  | { type: "unverified" }
  | { type: "rejected"; status: number; at: number }
  | { type: "credentials_unreadable"; error: string }
  | { type: "cli_failed"; tool: string; error: string };
type TokenCheck =
  | { type: "accepted"; account?: { login: string; name?: string } | null }
  | { type: "rejected"; status: number }
  | { type: "missing_scopes"; scopes: string[] }
  | { type: "unreachable"; status?: number | null }
  | { type: "unreadable"; status: number; error: string }
  | { type: "unconfirmed" };
type HostStatus = {
  kind: HostKind;
  added: boolean;
  token_set: boolean;
  login?: string | null;
  accounts: string[];
  env_overrides_account: boolean;
  source?: CredentialSource | null;
  order?: CredentialSource[];
  problem?: HostProblem | null;
  token_url?: string | null;
  access_token_url?: string | null;
  token_scopes: string[];
};
type Policy = Rules & {
  settlement_automation: string;
  project_settlement_overrides: Record<string, Rules>;
  source_control: { hosts: Record<string, Host>; status: Record<string, HostStatus> };
};
type PolicyPatch = Partial<Omit<Policy, "source_control">> & { source_control?: { hosts: Record<string, Host> } };

async function request<T>(path: string, body?: unknown, method = "PUT"): Promise<T> {
  const response = await apiFetch(path, body === undefined ? {} : { method, headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
  const result = await response.json().catch(() => null);
  if (!response.ok) throw new Error(result?.error?.message || result?.detail || `请求失败（HTTP ${response.status}）`);
  return result as T;
}

function errorText(cause: unknown, fallback: string): string {
  return cause instanceof Error && cause.message ? cause.message : fallback;
}

function usePolicy() {
  const [policy, setPolicy] = useState<Policy>();
  const [error, setError] = useState("");
  const [saving, setSaving] = useState(false);
  const refresh = useCallback(async () => {
    try {
      setPolicy(await request<Policy>("/api/control/policy"));
      setError("");
    } catch (cause) {
      setError(errorText(cause, "读取设置失败"));
    }
  }, []);
  useEffect(() => {
    void refresh();
  }, [refresh]);
  const save = useCallback(async (patch: PolicyPatch) => {
    setSaving(true);
    setError("");
    try {
      const next = await request<Policy>("/api/control/policy", patch);
      setPolicy(next);
      return next;
    } catch (cause) {
      setError(errorText(cause, "保存失败，请重试"));
      return null;
    } finally {
      setSaving(false);
    }
  }, []);
  return { policy, setPolicy, error, setError, saving, save, refresh };
}


const AUTOMATION_OPTIONS: Array<{ value: string; label: string; description: string }> = [
  { value: "off", label: "关闭", description: "Muteki 从不自行收尾线程。" },
  { value: "suggest", label: "建议", description: "模型可以建议收尾；在你或你要求的线程执行前，不会收尾任何线程。" },
  { value: "explicit_signals", label: "明确信号", description: "按下方的不活跃和拉取请求规则收尾线程，但待跟进、已搁置或持有其他线程排队消息的线程除外。" },
];

function daysLabel(days: number | null | undefined): string {
  return days == null ? "从不" : `${days} 天`;
}

function DaysInput({ value, label, disabled, onCommit }: { value: number; label: string; disabled?: boolean; onCommit: (days: number) => void }) {
  const [draft, setDraft] = useState(String(value));
  useEffect(() => setDraft(String(value)), [value]);
  const days = Number(draft);
  const valid = draft.trim() !== "" && Number.isFinite(days) && days >= 1 && days <= 90;
  const commit = () => {
    if (!valid) setDraft(String(value));
    else if (days !== value) onCommit(days);
  };
  return (
    <div className="flex items-center gap-2">
      <Input
        aria-label={label}
        type="number"
        inputMode="decimal"
        min={1}
        max={90}
        className="w-20 text-right cx-tabular"
        value={draft}
        invalid={!valid}
        disabled={disabled}
        onChange={(event) => setDraft(event.target.value)}
        onBlur={commit}
        onKeyDown={(event) => {
          if (event.key === "Enter") {
            event.preventDefault();
            commit();
          }
        }}
      />
      <span className="text-cx-sm text-cx-fg-3">天</span>
    </div>
  );
}

export function SettlementSettings() {
  const { policy, error, setError, saving, save } = usePolicy();
  const [projects, setProjects] = useState<ConversationProject[]>([]);
  useEffect(() => {
    fetchConversationProjects()
      .then(setProjects)
      .catch((cause) => setError(errorText(cause, "读取项目列表失败")));
  }, [setError]);
  const overrides = useMemo(() => policy?.project_settlement_overrides ?? {}, [policy?.project_settlement_overrides]);
  const projectName = (id: string) => projects.find((p) => p.project_id === id)?.name || id;
  const available = projects.filter((p) => !overrides[p.project_id]);
  const saveRules = (id: string, rules?: Rules) => {
    const next = { ...overrides };
    if (rules) next[id] = rules;
    else delete next[id];
    void save({ project_settlement_overrides: next });
  };
  const locked = !policy || saving;
  const automation = policy?.settlement_automation ?? "explicit_signals";
  const globalDays = policy?.auto_settle_after_days ?? null;
  const globalMerge = policy?.auto_settle_on_merge ?? false;

  return (
    <>
      {error ? <SettingsNote tone="danger">{error}</SettingsNote> : null}
      <SettingsSection anchor="settlement" title="线程收尾" description="手动收尾，或由你要求的线程收尾，始终可用。修改这些设置不会重新打开已收尾的线程。">
        <SettingsRow
          title="自动收尾"
          description={AUTOMATION_OPTIONS.find((option) => option.value === automation)?.description}
          control={
            <Select
              size="md"
              ariaLabel="自动收尾"
              value={automation}
              disabled={locked}
              onChange={(value) => void save({ settlement_automation: value })}
              popoverClassName="w-[320px]"
              options={AUTOMATION_OPTIONS.map((option) => ({ value: option.value, label: option.label, description: option.description }))}
            />
          }
        />
        <SettingsRow
          title="自动收尾不活跃的线程"
          description="超过设定天数没有活动的线程会移入“已收尾”。有新活动时会自动回来。"
          control={
            <Switch
              ariaLabel="自动收尾不活跃的线程"
              checked={globalDays != null}
              disabled={locked}
              onCheckedChange={(on) => void save({ auto_settle_after_days: on ? 3 : null })}
            />
          }
        />
        {globalDays != null ? (
          <SettingsRow
            title="自动收尾前的天数"
            description="1 到 90 天。"
            control={<DaysInput value={globalDays} label="自动收尾前的天数" disabled={locked} onCommit={(days) => void save({ auto_settle_after_days: days })} />}
          />
        ) : null}
        <SettingsRow
          title="拉取请求合并后收尾"
          description="关联的拉取请求合并后收尾该线程。已关闭的拉取请求也会让空闲的线程收尾。"
          control={
            <Switch
              ariaLabel="拉取请求合并后收尾"
              checked={globalMerge}
              disabled={locked}
              onCheckedChange={(on) => void save({ auto_settle_on_merge: on })}
            />
          }
        />
      </SettingsSection>

      <SettingsSection
        anchor="settlement-projects"
        title="项目单独设置"
        description="这里列出的项目使用各自的自动收尾规则，其余项目沿用上面的设置。"
        actions={
          <Select<string>
            size="sm"
            ariaLabel="添加单独设置的项目"
            value={null}
            placeholder="添加项目…"
            disabled={locked || available.length === 0}
            searchable={available.length > 8}
            onChange={(id) => saveRules(id, {})}
            options={available.map((p) => ({ value: p.project_id, label: p.name || p.project_id }))}
          />
        }
      >
        {Object.keys(overrides).length === 0 ? (
          <SettingsEmpty>{projects.length === 0 ? "还没有项目。" : "没有单独设置的项目。"}</SettingsEmpty>
        ) : (
          Object.entries(overrides).map(([id, rules]) => {
            const daysMode = rules.auto_settle_after_days === undefined ? "default" : rules.auto_settle_after_days === null ? "never" : "days";
            const mergeMode = rules.auto_settle_on_merge === undefined ? "default" : String(rules.auto_settle_on_merge);
            return (
              <div key={id} className="border-b border-cx-border-subtle px-5 py-4 last:border-b-0">
                <div className="flex items-center gap-3">
                  <Icon name="folder" size={16} className="shrink-0 text-cx-fg-3" />
                  <span className="min-w-0 flex-1 truncate text-cx-sm font-medium text-cx-fg" title={projectName(id)}>{projectName(id)}</span>
                  <Button variant="ghost" size="sm" icon="trash" disabled={saving} onClick={() => saveRules(id)}>
                    移除单独设置
                  </Button>
                </div>
                <div className="mt-3 grid gap-4 sm:grid-cols-2 sm:pl-7">
                  <div className="flex min-w-0 flex-col">
                    <Label>不活跃后收尾</Label>
                    <div className="flex flex-wrap items-center gap-2">
                      <Select
                        size="md"
                        className="min-w-[180px] flex-1"
                        ariaLabel={`${projectName(id)} 不活跃后收尾`}
                        value={daysMode}
                        disabled={saving}
                        onChange={(value) =>
                          saveRules(id, {
                            ...rules,
                            auto_settle_after_days:
                              value === "default" ? undefined : value === "never" ? null : typeof rules.auto_settle_after_days === "number" ? rules.auto_settle_after_days : globalDays ?? 3,
                          })
                        }
                        options={[
                          { value: "default", label: `使用默认值（${daysLabel(globalDays)}）` },
                          { value: "never", label: "从不" },
                          { value: "days", label: "一定天数后" },
                        ]}
                      />
                      {typeof rules.auto_settle_after_days === "number" ? (
                        <DaysInput
                          value={rules.auto_settle_after_days}
                          label={`${projectName(id)} 自动收尾天数`}
                          disabled={saving}
                          onCommit={(days) => saveRules(id, { ...rules, auto_settle_after_days: days })}
                        />
                      ) : null}
                    </div>
                  </div>
                  <div className="flex min-w-0 flex-col">
                    <Label>拉取请求合并后收尾</Label>
                    <Select
                      size="md"
                      className="w-full"
                      ariaLabel={`${projectName(id)} 拉取请求合并后收尾`}
                      value={mergeMode}
                      disabled={saving}
                      onChange={(value) => saveRules(id, { ...rules, auto_settle_on_merge: value === "default" ? undefined : value === "true" })}
                      options={[
                        { value: "default", label: `使用默认值（${globalMerge ? "开启" : "关闭"}）` },
                        { value: "true", label: "开启" },
                        { value: "false", label: "关闭" },
                      ]}
                    />
                  </div>
                </div>
              </div>
            );
          })
        )}
      </SettingsSection>
    </>
  );
}


const KINDS: Array<{ kind: HostKind; name: string; cover: string; publicHost: string; placeholder: string; note: string }> = [
  { kind: "github", name: "GitHub", cover: "Enterprise Server", publicHost: "github.com", placeholder: "github.example.com", note: "如使用 GitHub Enterprise Server，请输入其域名，例如 github.example.com。" },
  { kind: "gitlab", name: "GitLab", cover: "自管理", publicHost: "gitlab.com", placeholder: "gitlab.example.com", note: "如使用自管理的 GitLab，请输入其域名，例如 gitlab.example.com。如服务器带有端口，请一并填写。" },
  { kind: "forgejo", name: "Forgejo", cover: "Codeberg 或自托管", publicHost: "codeberg.org", placeholder: "git.example.com:3000/forgejo", note: "Codeberg 或你自己的 Forgejo 服务器。如服务器带有端口或路径，请一并填写，例如 git.example.com:3000/forgejo。" },
  { kind: "gitea", name: "Gitea", cover: "gitea.com 或自托管", publicHost: "gitea.com", placeholder: "git.example.com:3000/gitea", note: "gitea.com 或你自己的 Gitea 服务器。如服务器带有端口或路径，请一并填写，例如 git.example.com:3000/gitea。" },
  { kind: "bitbucket", name: "Bitbucket", cover: "仅 Cloud", publicHost: "bitbucket.org", placeholder: "bitbucket.org", note: "仅支持 Bitbucket Cloud，不支持 Bitbucket Data Center。" },
  { kind: "azure", name: "Azure DevOps", cover: "按组织添加", publicHost: "", placeholder: "fabrikam", note: "输入组织名称，例如 dev.azure.com/fabrikam 中的 fabrikam。也可以使用旧的 fabrikam.visualstudio.com 地址。不支持 Azure DevOps Server。" },
];

function kindMeta(kind: HostKind | undefined) {
  return KINDS.find((item) => item.kind === kind);
}

function listedForm(kind: HostKind, typed: string): string {
  const value = typed.trim().toLowerCase().replace(/^https?:\/\//, "").replace(/\/+$/, "");
  if (kind !== "azure" || !value) return value;
  const [host, organization] = value.split("/");
  if (host === "dev.azure.com") return organization ? `dev.azure.com/${organization}` : value;
  return `dev.azure.com/${host.endsWith(".visualstudio.com") ? host.slice(0, -".visualstudio.com".length) : value}`;
}

function sourceLabel(source: CredentialSource): string {
  switch (source.type) {
    case "saved":
    case "saved_bearer":
      return "保存的访问令牌";
    case "saved_basic":
      return `${source.email} 的 API 令牌`;
    case "env":
      return `环境变量 ${source.name}`;
    case "cli":
      return `${source.tool} 登录`;
  }
}

function hostState(host: string, choice: Host, status: HostStatus | undefined): { tone: Tone; text: string } {
  if (!choice.enabled) return { tone: "neutral", text: "已暂停" };
  const problem = status?.problem;
  if (problem) {
    switch (problem.type) {
      case "no_credential":
        return { tone: "neutral", text: "未连接" };
      case "not_signed_in":
        return { tone: "neutral", text: `${problem.tool} 尚未登录此平台` };
      case "unverified":
        return { tone: "warning", text: "已保存，尚未验证" };
      case "rejected":
        return { tone: "danger", text: `${host} 拒绝了正在使用的访问令牌（HTTP ${problem.status}），在你更换之前，来自它的拉取请求不会更新。` };
      case "credentials_unreadable":
        return { tone: "danger", text: `无法读取保存的访问令牌（${problem.error}），修复该文件之前不会发送任何请求。` };
      case "cli_failed":
        return { tone: "danger", text: `无法向 ${problem.tool} 询问登录信息（${problem.error}）` };
    }
  }
  if (status?.login) return { tone: "success", text: `已连接为 ${status.login}` };
  if (status?.source) return { tone: "success", text: `已连接 · ${sourceLabel(status.source)}` };
  return { tone: "neutral", text: "未连接" };
}

function checkMessage(host: string, check: TokenCheck | undefined): string {
  switch (check?.type) {
    case "rejected":
      return `${host} 拒绝了这个访问令牌（HTTP ${check.status}）。它可能已过期、被撤销，或复制不完整。`;
    case "missing_scopes":
      return `这个访问令牌缺少权限范围 ${check.scopes.join("、")}。请在 ${host} 上为它加上后重试。`;
    case "unreachable":
      return check.status ? `${host} 没有正常回答（HTTP ${check.status}），请等它恢复后再验证。` : `主机连不上 ${host}，请确认运行 Muteki 的主机能访问它后再验证。`;
    case "unreadable":
      return `${host} 接受了这个令牌（HTTP ${check.status}），但无法读取它的回答，因此不知道令牌属于谁：${check.error}`;
    default:
      return `无法验证 ${host} 的访问令牌。`;
  }
}

type CredentialAction = "connect" | "identify" | "disconnect" | "remove";

function HostEditor({
  name,
  kind,
  status,
  busy,
  checkError,
  onConnect,
  onCancel,
}: {
  name: string;
  kind: HostKind;
  status: HostStatus | undefined;
  busy: boolean;
  checkError: string;
  onConnect: (token: string, email: string) => void;
  onCancel: () => void;
}) {
  const [token, setToken] = useState("");
  const [email, setEmail] = useState("");
  const tokenId = useId();
  const emailId = useId();
  const problem = status?.problem;
  const order = status?.order ?? [];
  const submit = () => {
    if (token.trim() && !busy) onConnect(token.trim(), email.trim());
  };
  return (
    <div className="mt-3 flex flex-col gap-4 rounded-xl border border-cx-border-subtle bg-cx-bg-subtle p-4">
      <div className="flex flex-col gap-1.5">
        <div className="text-cx-sm font-medium text-cx-fg">{status?.token_set ? "更换访问令牌" : `在 ${name} 创建个人访问令牌`}</div>
        {status?.source && status.source.type !== "saved" && status.source.type !== "saved_basic" && status.source.type !== "saved_bearer" ? (
          <p className="text-cx-xs leading-4 text-cx-fg-3">现在使用{sourceLabel(status.source)}。保存的访问令牌优先于其他来源。</p>
        ) : null}
        {status?.token_url ? (
          <a href={status.token_url} target="_blank" rel="noreferrer" className="inline-flex w-fit items-center gap-1 text-cx-sm text-cx-accent hover:underline">
            打开 {name} 的令牌页面
            <Icon name="externalLink" size={12} />
          </a>
        ) : null}
        {status?.token_scopes.length ? (
          <div className="flex flex-wrap items-center gap-1.5 text-cx-xs text-cx-fg-3">
            <span>需要权限范围：</span>
            {status.token_scopes.map((scope) => (
              <code key={scope} className="rounded bg-cx-hover px-1.5 py-0.5 font-mono text-cx-xs text-cx-fg-2">{scope}</code>
            ))}
          </div>
        ) : null}
      </div>
      {kind === "bitbucket" ? (
        <div className="flex flex-col">
          <Label htmlFor={emailId}>Atlassian 账号邮箱</Label>
          <Input id={emailId} type="email" autoComplete="off" placeholder="name@example.com" value={email} onChange={(event) => setEmail(event.target.value)} />
          <p className="mt-1.5 text-cx-xs leading-4 text-cx-fg-4">
            使用 API 令牌时填写，可访问你能访问的所有仓库；使用仓库、项目或工作区的访问令牌时留空。
            {status?.access_token_url ? (
              <>
                {" "}
                <a href={status.access_token_url} target="_blank" rel="noreferrer" className="text-cx-accent hover:underline">关于访问令牌</a>
              </>
            ) : null}
          </p>
        </div>
      ) : null}
      <div className="flex flex-col">
        <Label htmlFor={tokenId}>访问令牌</Label>
        <Input
          id={tokenId}
          type="password"
          autoComplete="off"
          placeholder={status?.token_set ? "粘贴新的访问令牌" : "粘贴访问令牌"}
          value={token}
          invalid={Boolean(checkError)}
          onChange={(event) => setToken(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter") {
              event.preventDefault();
              submit();
            }
          }}
        />
        <p className="mt-1.5 text-cx-xs leading-4 text-cx-fg-4">只保存在运行 Muteki 的主机上，保存后不会再显示。</p>
      </div>
      {problem?.type === "not_signed_in" && problem.command ? (
        <div className="flex flex-col gap-1 text-cx-xs text-cx-fg-3">
          <span>也可以在主机的终端里登录 {problem.tool}，Muteki 会自动使用它：</span>
          <code className="w-fit max-w-full overflow-x-auto whitespace-nowrap rounded-md bg-cx-hover px-2 py-1 font-mono text-cx-xs text-cx-fg-2">{problem.command}</code>
        </div>
      ) : null}
      {order.length > 1 ? (
        <p className="text-cx-xs leading-4 text-cx-fg-4">Muteki 按顺序使用第一个可用的来源：{order.map(sourceLabel).join("、")}。</p>
      ) : null}
      {checkError ? (
        <p role="alert" className="rounded-lg bg-cx-danger-soft px-3 py-2 text-cx-sm leading-5 text-cx-danger">{checkError}</p>
      ) : null}
      <div className="flex flex-wrap items-center justify-end gap-2">
        <Button variant="ghost" size="sm" disabled={busy} onClick={onCancel}>取消</Button>
        <Button variant="primary" size="sm" loading={busy} disabled={!token.trim() || busy} onClick={submit}>
          {busy ? "正在验证…" : "验证并连接"}
        </Button>
      </div>
    </div>
  );
}

function AddHostDialog({
  open,
  listed,
  hosts,
  onOpenChange,
  onAdded,
}: {
  open: boolean;
  listed: string[];
  hosts: Record<string, Host>;
  onOpenChange: (open: boolean) => void;
  onAdded: (policy: Policy, host: string) => void;
}) {
  const [kind, setKind] = useState<HostKind>("github");
  const [address, setAddress] = useState("github.com");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const addressId = useId();
  useEffect(() => {
    if (!open) return;
    setKind("github");
    setAddress("github.com");
    setError("");
  }, [open]);
  const meta = kindMeta(kind)!;
  const fixed = kind === "bitbucket";
  const choose = (next: HostKind) => {
    const previous = kindMeta(kind)!;
    const typed = address.trim().toLowerCase();
    setKind(next);
    setError("");
    if (!typed || typed === previous.publicHost || next === "bitbucket" || kind === "bitbucket") setAddress(kindMeta(next)!.publicHost);
  };
  const submit = async () => {
    const shown = listedForm(kind, address);
    if (shown && listed.includes(shown)) {
      setError(`${shown} 已在列表中。`);
      return;
    }
    setBusy(true);
    setError("");
    try {
      const before = new Set(Object.keys(hosts));
      const next = await request<Policy>("/api/control/policy", { source_control: { hosts: { ...hosts, [address]: { kind, enabled: true } } } });
      onAdded(next, Object.keys(next.source_control.hosts).find((host) => !before.has(host)) ?? shown);
    } catch (cause) {
      setError(errorText(cause, "添加失败，请重试"));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!busy) onOpenChange(next);
      }}
      size="md"
      icon="gitBranch"
      title="添加代码平台"
      description="添加后可以立即为它连接访问令牌。"
      footer={
        <>
          <Button variant="secondary" disabled={busy} onClick={() => onOpenChange(false)}>取消</Button>
          <Button variant="primary" loading={busy} disabled={busy} onClick={() => void submit()}>继续</Button>
        </>
      }
    >
      <div className="flex flex-col gap-4 pb-1">
        <div className="flex flex-col">
          <Label>类型</Label>
          <div role="radiogroup" aria-label="代码平台类型" className="grid grid-cols-2 gap-2 sm:grid-cols-3">
            {KINDS.map((item) => {
              const on = item.kind === kind;
              return (
                <button
                  key={item.kind}
                  type="button"
                  role="radio"
                  aria-checked={on}
                  onClick={() => choose(item.kind)}
                  className={cn(
                    "flex min-w-0 flex-col items-start gap-0.5 rounded-xl border px-3 py-2.5 text-left outline-none transition-colors focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]",
                    on ? "border-cx-accent bg-cx-accent-soft" : "border-cx-border hover:bg-cx-hover",
                  )}
                >
                  <span className="w-full truncate text-cx-sm font-medium text-cx-fg">{item.name}</span>
                  <span className="w-full truncate text-cx-xs text-cx-fg-3">{item.cover}</span>
                </button>
              );
            })}
          </div>
        </div>
        <div className="flex flex-col">
          <Label htmlFor={addressId}>{kind === "azure" ? "组织" : "地址"}</Label>
          <Input
            id={addressId}
            value={address}
            placeholder={meta.placeholder}
            disabled={fixed || busy}
            invalid={Boolean(error)}
            autoComplete="off"
            spellCheck={false}
            onChange={(event) => {
              setAddress(event.target.value);
              setError("");
            }}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                event.preventDefault();
                void submit();
              }
            }}
          />
          <p className="mt-1.5 text-cx-xs leading-4 text-cx-fg-4">{meta.note}</p>
        </div>
        {error ? <p role="alert" className="rounded-lg bg-cx-danger-soft px-3 py-2 text-cx-sm leading-5 text-cx-danger">{error}</p> : null}
      </div>
    </Dialog>
  );
}

export function SourceControlSettings() {
  const { policy, setPolicy, error, setError, saving, save, refresh } = usePolicy();
  const [adding, setAdding] = useState(false);
  const [editing, setEditing] = useState("");
  const [busy, setBusy] = useState("");
  const [checkError, setCheckError] = useState("");
  const [confirm, setConfirm] = useState<{ host: string; action: "disconnect" | "remove" } | null>(null);
  const hosts = useMemo(() => policy?.source_control.hosts ?? {}, [policy?.source_control.hosts]);
  const statuses = useMemo(() => policy?.source_control.status ?? {}, [policy?.source_control.status]);
  const names = useMemo(() => [...new Set([...Object.keys(statuses), ...Object.keys(hosts)])].sort(), [hosts, statuses]);

  const setHost = (name: string, value: Host | null) => {
    const next = { ...hosts };
    if (value) next[name] = value;
    else delete next[name];
    return save({ source_control: { hosts: next } });
  };

  const credential = async (name: string, action: CredentialAction, token = "", email = "") => {
    const kind = hosts[name]?.kind ?? statuses[name]?.kind;
    setBusy(`${name}:${action}`);
    setError("");
    if (action === "connect") setCheckError("");
    try {
      const result = await request<{ connected?: boolean; check?: TokenCheck } & Partial<Policy>>(
        "/api/control/host-credential",
        { host: name, kind, action, token, email },
        "POST",
      );
      if (action === "connect") {
        if (result.connected === false) {
          setCheckError(checkMessage(name, result.check));
          return;
        }
        setEditing("");
        const account = result.check?.type === "accepted" ? result.check.account?.login : undefined;
        toast({
          tone: "success",
          title: account ? `已连接为 ${account}` : `已保存 ${name} 的访问令牌`,
          description: result.check?.type === "unconfirmed" ? `${name} 不说明访问令牌属于谁，因此无法在保存前确认它。` : undefined,
        });
      }
      if (action === "identify" && result.source_control) setPolicy(result as Policy);
      else await refresh();
      if (action === "disconnect") toast({ tone: "success", title: `已清除 ${name} 保存的访问令牌` });
      if (action === "remove") {
        if (editing === name) setEditing("");
        toast({ tone: "success", title: `已移除 ${name}` });
      }
    } catch (cause) {
      const text = errorText(cause, "操作失败，请重试");
      if (action === "connect") setCheckError(text);
      else setError(text);
    } finally {
      setBusy("");
    }
  };

  const confirmMeta = confirm
    ? confirm.action === "remove"
      ? { title: `移除 ${confirm.host}？`, description: "它保存的访问令牌会从主机上删除。来自它的已关联拉取请求将停止更新，直到你重新添加并连接。", action: "移除" }
      : { title: `断开 ${confirm.host}？`, description: "保存的访问令牌会从主机上删除。之后 Muteki 改用下一个可用的来源（环境变量或命令行登录）。", action: "断开连接" }
    : null;

  return (
    <div className="flex flex-col gap-3" aria-busy={!policy && !error}>
      <SettingsSection
        anchor="source-control-hosts"
        title="已知平台"
        description="Muteki 通过这些代码平台读取和写入拉取请求与合并请求。访问令牌只保存在运行 Muteki 的主机上，保存后不会再显示。"
        actions={
          <Button variant="secondary" size="sm" icon="plus" disabled={!policy} onClick={() => setAdding(true)}>
            添加代码平台
          </Button>
        }
      >
        {error ? (
          <div className="flex items-center justify-between gap-3 border-b border-cx-border-subtle px-5 py-3">
            <SettingsNote tone="danger">{error}</SettingsNote>
            {!policy ? <Button variant="secondary" size="sm" onClick={() => void refresh()}>重试</Button> : null}
          </div>
        ) : null}
        {!policy && !error ? <SettingsEmpty>正在读取代码平台…</SettingsEmpty> : null}
        {policy && names.length === 0 ? <SettingsEmpty>还没有代码平台。登录 gh、glab、tea 或 az 后会自动出现，也可以手动添加。</SettingsEmpty> : null}
        {names.map((name) => {
          const status = statuses[name];
          const choice: Host = hosts[name] ?? { kind: status?.kind ?? "github", enabled: true };
          const state = hostState(name, choice, status);
          const open = editing === name;
          const connected = Boolean(status?.source) && !status?.problem;
          return (
            <div key={name} className="border-b border-cx-border-subtle px-5 py-4 last:border-b-0">
              <div className="flex flex-wrap items-start gap-3">
                <span className="mt-0.5 grid size-8 shrink-0 place-items-center rounded-lg bg-cx-hover text-cx-fg-2">
                  <Icon name="gitBranch" size={16} />
                </span>
                <div className="min-w-0 flex-1">
                  <div className="flex min-w-0 flex-wrap items-center gap-2">
                    <span className="min-w-0 truncate text-cx-sm font-medium text-cx-fg" title={name}>{name}</span>
                    <Badge tone="neutral">{kindMeta(choice.kind)?.name ?? choice.kind}</Badge>
                    {status?.added ? <Badge tone="neutral">由你添加</Badge> : null}
                  </div>
                  <div className={cn("mt-1 flex items-start gap-1.5 text-cx-sm leading-5", state.tone === "danger" ? "text-cx-danger" : "text-cx-fg-3")}>
                    <StatusDot tone={state.tone} className="mt-[7px]" />
                    <span className="min-w-0 break-words">{state.text}</span>
                  </div>
                </div>
                <div className="ml-auto flex shrink-0 items-center gap-2">
                  <Button
                    variant={open ? "soft" : "secondary"}
                    size="sm"
                    disabled={!choice.enabled}
                    onClick={() => {
                      setEditing(open ? "" : name);
                      setCheckError("");
                    }}
                  >
                    {status?.token_set ? "更换令牌" : connected ? "改用访问令牌" : "连接"}
                  </Button>
                  <Switch ariaLabel={`使用 ${name}`} checked={choice.enabled} disabled={saving} onCheckedChange={(enabled) => void setHost(name, { ...choice, enabled })} />
                </div>
              </div>
              {(status?.accounts.length ?? 0) > 1 ? (
                <div className="mt-3 flex flex-wrap items-center justify-between gap-3 sm:pl-11">
                  <div className="min-w-0">
                    <div className="text-cx-sm text-cx-fg-2">当前账号</div>
                    {status?.env_overrides_account ? <div className="text-cx-xs text-cx-fg-4">环境变量中的访问令牌会覆盖所选账号。</div> : null}
                  </div>
                  <Select
                    size="sm"
                    ariaLabel={`${name} 当前账号`}
                    value={choice.account || ""}
                    disabled={saving}
                    onChange={(account) => void setHost(name, { ...choice, account: account || null })}
                    options={[{ value: "", label: "默认账号" }, ...status!.accounts.map((account) => ({ value: account, label: account }))]}
                  />
                </div>
              ) : null}
              {status?.source?.type === "env" ? (
                <p className="mt-2 text-cx-xs leading-4 text-cx-fg-4 sm:pl-11">来自主机上的环境变量 {status.source.name}。要停用它，请在主机上取消该变量并重启 Muteki。</p>
              ) : null}
              {open ? (
                <div className="sm:pl-11">
                  <HostEditor
                    name={name}
                    kind={choice.kind}
                    status={status}
                    busy={busy === `${name}:connect`}
                    checkError={checkError}
                    onConnect={(token, email) => void credential(name, "connect", token, email)}
                    onCancel={() => {
                      setEditing("");
                      setCheckError("");
                    }}
                  />
                  <div className="mt-2 flex flex-wrap gap-1">
                    <Button variant="ghost" size="sm" icon="refresh" loading={busy === `${name}:identify`} disabled={busy !== ""} onClick={() => void credential(name, "identify")}>
                      重新检查
                    </Button>
                    {status?.token_set ? (
                      <Button variant="ghost" size="sm" disabled={busy !== ""} onClick={() => setConfirm({ host: name, action: "disconnect" })}>
                        断开连接…
                      </Button>
                    ) : null}
                    {status?.added ? (
                      <Button variant="ghost" size="sm" icon="trash" disabled={busy !== ""} onClick={() => setConfirm({ host: name, action: "remove" })}>
                        移除代码平台…
                      </Button>
                    ) : null}
                  </div>
                </div>
              ) : null}
            </div>
          );
        })}
      </SettingsSection>
      <SettingsNote>gh、glab、tea、az 的登录和环境变量指向的代码平台会自动出现。自托管的 GitHub Enterprise、GitLab、Forgejo、Gitea 请自行添加。</SettingsNote>

      <AddHostDialog
        open={adding}
        listed={names}
        hosts={hosts}
        onOpenChange={setAdding}
        onAdded={(next, host) => {
          setPolicy(next);
          setAdding(false);
          setEditing(host);
          setCheckError("");
          toast({ tone: "success", title: `已添加 ${host}` });
        }}
      />

      <Dialog
        open={confirm !== null}
        onOpenChange={(open) => {
          if (!open) setConfirm(null);
        }}
        size="sm"
        tone="danger"
        icon={confirm?.action === "remove" ? "trash" : "link"}
        title={confirmMeta?.title}
        description={confirmMeta?.description}
        footer={
          <>
            <Button variant="secondary" onClick={() => setConfirm(null)}>取消</Button>
            <Button
              variant="danger"
              onClick={() => {
                if (!confirm) return;
                const target = confirm;
                setConfirm(null);
                void credential(target.host, target.action);
              }}
            >
              {confirmMeta?.action}
            </Button>
          </>
        }
      />
    </div>
  );
}
