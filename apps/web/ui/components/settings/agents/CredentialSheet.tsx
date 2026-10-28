"use client";

import { useState } from "react";
import { Button, Callout, Collapse, SegmentedControl, Sheet, TextArea, TextField, Tooltip } from "@/components/chat/ui";
import { Icon } from "@/components/Icon";
import { cn } from "@/lib/cn";
import type { GlobalCredential } from "@/lib/useRun";
import { splitModels, type Connection } from "./shared";

export type CredentialValues = {
  accountId: string; connection: Connection; provider: string; baseUrl: string; secret: string; defaultModel: string; models: string[];
};
export type ModelRefreshResult = { ok: boolean; models: string[]; detail: string };

const CONNECTIONS: { value: Connection; label: string }[] = [{ value: "official", label: "官方账号" }, { value: "custom_endpoint", label: "自定义端点" }];

/** Create or edit one stored access method. Owns its draft state; remount (key) to reset. */
export function CredentialSheet({ open, mode, engineLabel, credential, authJson, onClose, onSubmit, onRefreshModels, onDelete }: {
  open: boolean;
  mode: "create" | "edit";
  engineLabel: string;
  credential: GlobalCredential | null;
  /** The engine's official login is an auth.json document, not a single secret. */
  authJson: boolean;
  onClose: () => void;
  onSubmit: (values: CredentialValues, baseRevision?: string) => Promise<void>;
  onRefreshModels: (values: Pick<CredentialValues, "connection" | "baseUrl" | "secret">) => Promise<ModelRefreshResult>;
  onDelete?: () => void;
}) {
  const editing = mode === "edit" && credential;
  const [accountId, setAccountId] = useState("");
  const [connection, setConnection] = useState<Connection>(credential?.connection === "custom_endpoint" ? "custom_endpoint" : "official");
  const [provider, setProvider] = useState(credential?.provider || "");
  const [baseUrl, setBaseUrl] = useState(credential?.base_url || "");
  const [secret, setSecret] = useState("");
  const [showSecret, setShowSecret] = useState(false);
  const [defaultModel, setDefaultModel] = useState(credential?.default_model || credential?.candidate_models?.[0] || credential?.models[0] || "");
  const [modelsText, setModelsText] = useState((credential?.candidate_models || credential?.models || []).join("\n"));
  const [baseRevision, setBaseRevision] = useState(credential?.revision);
  const [advanced, setAdvanced] = useState(false);
  const [busy, setBusy] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [error, setError] = useState("");
  const [note, setNote] = useState<{ ok: boolean; text: string } | null>(null);
  const readonly = Boolean(credential?.sharing?.readonly);
  const stale = Boolean(editing && credential.revision !== baseRevision);
  const secretLabel = authJson && connection === "official" ? "Auth JSON" : "Secret / Token / API Key";

  const reload = () => {
    if (!credential) return;
    setBaseRevision(credential.revision); setConnection(credential.connection === "custom_endpoint" ? "custom_endpoint" : "official");
    setProvider(credential.provider || ""); setBaseUrl(credential.base_url || ""); setDefaultModel(credential.default_model || "");
    setModelsText((credential.candidate_models || credential.models).join("\n")); setSecret("");
  };
  const submit = async () => {
    if (!editing && !accountId.trim()) { setError("请填写凭据 ID。"); return; }
    if (connection === "custom_endpoint" && !baseUrl.trim()) { setError("请填写 Base URL。"); return; }
    if (!editing && connection === "custom_endpoint" && !secret.trim()) { setError("请填写 API Key。"); return; }
    setBusy(true); setError("");
    try {
      await onSubmit({ accountId: accountId.trim(), connection, provider: provider.trim(), baseUrl: baseUrl.trim(), secret: secret.trim(), defaultModel: defaultModel.trim(), models: splitModels(modelsText) }, baseRevision);
      onClose();
    } catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); } finally { setBusy(false); }
  };
  const refresh = async () => {
    if (connection === "custom_endpoint" && !baseUrl.trim()) { setNote({ ok: false, text: "请先填写 Base URL。" }); return; }
    setRefreshing(true); setNote(null);
    try {
      const result = await onRefreshModels({ connection, baseUrl: baseUrl.trim(), secret: secret.trim() });
      if (!result.ok) { setNote({ ok: false, text: result.detail || "服务未返回模型" }); return; }
      setModelsText(result.models.join("\n"));
      setDefaultModel(current => current && result.models.includes(current) ? current : result.models[0] || "");
      setNote({ ok: true, text: result.detail || `已获取 ${result.models.length} 个模型。` });
    } catch (cause) { setNote({ ok: false, text: cause instanceof Error ? cause.message : String(cause) }); } finally { setRefreshing(false); }
  };

  return (
    <Sheet
      open={open} onOpenChange={next => { if (!next && !busy) onClose(); }} width={480}
      title={editing ? `编辑 · ${credential.label || credential.account_id}` : `新增接入方式 · ${engineLabel}`}
      footer={
        <div className="flex items-center gap-2">
          {editing && onDelete ? <Button size="sm" variant="danger-soft" icon="trash" disabled={busy || readonly} onClick={onDelete}>删除</Button> : null}
          <span className="flex-1" />
          <Button size="sm" variant="ghost" disabled={busy} onClick={onClose}>取消</Button>
          <Button size="sm" variant="primary" icon="check" loading={busy} onClick={() => void submit()}>{editing ? readonly ? "保存本环境模型设置" : "保存" : "添加"}</Button>
        </div>
      }
      bodyClassName="px-4 py-4"
    >
      <div className="flex flex-col gap-4">
        {error ? <Callout tone="danger" icon="alert" role="alert">{error}</Callout> : null}
        {stale ? (
          <Callout tone="warning" icon="alert" role="alert" action={<Button size="xs" variant="outline" onClick={reload}>载入最新配置</Button>}>配置已在其它视图更新，本地编辑仍保留。</Callout>
        ) : null}
        {readonly && credential?.sharing ? <p className="text-[12.5px] text-cx-fg-3">{credential.sharing.reason}</p> : null}
        {!editing ? <TextField label="凭据 ID" autoComplete="off" placeholder="例如 work-account" value={accountId} onChange={event => setAccountId(event.target.value)} /> : null}
        <fieldset disabled={readonly} className="flex flex-col gap-4">
          <div className="flex items-center gap-2">
            <SegmentedControl size="md" ariaLabel="连接方式" value={connection} onChange={setConnection} options={CONNECTIONS} />
            {connection === "custom_endpoint" ? (
              <Tooltip content={`Worker 使用 ${engineLabel} 引擎调用该端点；Reason / Titler 复用时由 Muteki 后端直接发送 HTTP 请求，不会启动该 Agent。`}>
                <span tabIndex={0} aria-label="自定义端点的使用方式" className="grid size-6 place-items-center rounded-md text-cx-fg-4 outline-none hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"><Icon name="info" size={14} /></span>
              </Tooltip>
            ) : null}
          </div>
          {connection === "custom_endpoint" ? (
            <div className="grid gap-4 sm:grid-cols-2">
              <TextField label="Base URL" placeholder="https://api.example.com/v1" value={baseUrl} onChange={event => setBaseUrl(event.target.value)} />
              <TextField label="服务名称" placeholder="团队网关" value={provider} onChange={event => setProvider(event.target.value)} />
            </div>
          ) : null}
          <TextField
            label={secretLabel} type={showSecret ? "text" : "password"} autoComplete="off"
            placeholder={editing ? "留空时保留已保存的凭据" : authJson && connection === "official" ? "粘贴 auth.json 内容" : ""}
            value={secret} onChange={event => setSecret(event.target.value)}
            trailing={<button type="button" aria-label={showSecret ? "隐藏凭据" : "显示凭据"} onClick={() => setShowSecret(value => !value)} className="grid size-6 place-items-center rounded-md text-cx-fg-4 hover:bg-cx-hover hover:text-cx-fg"><Icon name={showSecret ? "eyeOff" : "eye"} size={14} /></button>}
          />
        </fieldset>
        <div>
          <button type="button" aria-expanded={advanced} onClick={() => setAdvanced(value => !value)}
            className="inline-flex items-center gap-1.5 rounded-md text-[12.5px] text-cx-fg-3 outline-none hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]">
            <Icon name="chevronDown" size={13} className={cn("transition-transform", advanced && "rotate-180")} />高级：默认模型与候选模型
          </button>
          <Collapse open={advanced}>
            <div className="mt-3 flex flex-col gap-3">
              <TextField label="默认模型" list="credential-sheet-models" autoComplete="off" placeholder="输入或选择模型 ID" value={defaultModel} onChange={event => setDefaultModel(event.target.value)} />
              <datalist id="credential-sheet-models">{splitModels(modelsText).map(model => <option key={model} value={model} />)}</datalist>
              <TextArea label="候选模型" rows={4} className="resize-none font-mono text-[12px]" placeholder="每行一个模型 ID" value={modelsText} onChange={event => setModelsText(event.target.value)} />
              {editing ? (
                <div className="flex items-center gap-2">
                  <Button size="xs" variant="outline" icon="refresh" loading={refreshing} onClick={() => void refresh()}>从端点读取模型</Button>
                  {note ? <span role={note.ok ? "status" : "alert"} className={cn("min-w-0 truncate text-[12px]", note.ok ? "text-cx-success" : "text-cx-danger")} title={note.text}>{note.text}</span> : null}
                </div>
              ) : <p className="text-[12px] text-cx-fg-3">留空时，添加后会自动从该接入方式读取模型。</p>}
            </div>
          </Collapse>
        </div>
      </div>
    </Sheet>
  );
}
