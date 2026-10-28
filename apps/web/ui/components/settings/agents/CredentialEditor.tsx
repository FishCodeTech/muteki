"use client";

import { useEffect, useRef, useState } from "react";
import { Button, Callout, Collapse, IconButton, SegmentedControl, Select, TextArea, TextField, Tooltip } from "@/components/chat/ui";
import { ModelTestTerminal } from "@/components/ModelTestTerminal";
import { Icon } from "@/components/Icon";
import { cn } from "@/lib/cn";
import type { GlobalCredential, WorkerModelTestResult } from "@/lib/useRun";
import { splitModels, type Connection } from "./shared";

export type CredentialValues = {
  accountId: string; connection: Connection; provider: string; baseUrl: string; secret: string; defaultModel: string; models: string[];
};
export type ModelRefreshResult = { ok: boolean; models: string[]; detail: string };
export type DraftTestValues = Pick<CredentialValues, "connection" | "baseUrl" | "provider" | "secret"> & { model: string };
type Check = { key: string; models: string[]; model: string; syncOk: boolean; syncDetail: string; result: WorkerModelTestResult };

const TYPES: { value: Connection; label: string; hint: string }[] = [
  { value: "official", label: "官方账号", hint: "Token 或 auth.json" },
  { value: "custom_endpoint", label: "自定义端点", hint: "Base URL + API Key" },
];

/** Create or edit one stored access method, in place inside its card. Owns its draft; remount (key) to reset. */
export function CredentialEditor({ mode, engineLabel, credential, authJson, onClose, onSubmit, onRefreshModels, onTestDraft, onDelete, onDirtyChange }: {
  mode: "create" | "edit";
  engineLabel: string;
  credential: GlobalCredential | null;
  /** The engine's official login is an auth.json document, not a single secret. */
  authJson: boolean;
  onClose: () => void;
  onSubmit: (values: CredentialValues, baseRevision?: string) => Promise<void>;
  onRefreshModels: (values: Pick<CredentialValues, "connection" | "baseUrl" | "secret">) => Promise<ModelRefreshResult>;
  /** Real model turn for the unsaved draft; a new access method is only added after this passes. */
  onTestDraft: (values: DraftTestValues) => Promise<WorkerModelTestResult>;
  onDelete?: () => void;
  onDirtyChange?: (dirty: boolean) => void;
}) {
  const editing = mode === "edit" && credential;
  const initial = {
    connection: (credential?.connection === "custom_endpoint" ? "custom_endpoint" : "official") as Connection,
    provider: credential?.provider || "", baseUrl: credential?.base_url || "",
    defaultModel: credential?.default_model || credential?.candidate_models?.[0] || credential?.models[0] || "",
    modelsText: (credential?.candidate_models || credential?.models || []).join("\n"),
  };
  const [accountId, setAccountId] = useState("");
  const [connection, setConnection] = useState<Connection>(initial.connection);
  const [provider, setProvider] = useState(initial.provider);
  const [baseUrl, setBaseUrl] = useState(initial.baseUrl);
  const [secret, setSecret] = useState("");
  const [showSecret, setShowSecret] = useState(false);
  const [defaultModel, setDefaultModel] = useState(initial.defaultModel);
  const [modelsText, setModelsText] = useState(initial.modelsText);
  const [baseRevision, setBaseRevision] = useState(credential?.revision);
  const [advanced, setAdvanced] = useState(false);
  const [busy, setBusy] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [error, setError] = useState("");
  const [note, setNote] = useState<{ ok: boolean; text: string } | null>(null);
  const [check, setCheck] = useState<Check | null>(null);
  const [checking, setChecking] = useState<"sync" | "test" | null>(null);
  const [pickedModel, setPickedModel] = useState("");
  const [showLog, setShowLog] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const readonly = Boolean(credential?.sharing?.readonly);
  const stale = Boolean(editing && credential.revision !== baseRevision);
  const secretLabel = authJson && connection === "official" ? "Auth JSON" : "Secret / Token / API Key";
  const fingerprint = JSON.stringify([connection, baseUrl.trim(), provider.trim(), secret.trim()]);
  const verified = Boolean(check && check.key === fingerprint && check.result.ok);
  const checkStale = Boolean(check && check.key !== fingerprint);
  const canCheck = Boolean(secret.trim() && (connection !== "custom_endpoint" || baseUrl.trim()));
  const dirty = Boolean(secret || accountId || connection !== initial.connection || provider !== initial.provider || baseUrl !== initial.baseUrl
    || defaultModel !== initial.defaultModel || modelsText !== initial.modelsText);

  useEffect(() => { onDirtyChange?.(dirty); }, [dirty]); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => () => onDirtyChange?.(false), []); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => {
    root.current?.scrollIntoView({ block: "nearest", behavior: "smooth" });
    root.current?.querySelector<HTMLElement>("input:not([type=hidden])")?.focus({ preventScroll: true });
  }, []);

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
    if (!editing && !verified) { setError("请先同步模型并通过连通测试，再添加。"); return; }
    setBusy(true); setError("");
    try {
      await onSubmit({ accountId: accountId.trim(), connection, provider: provider.trim(), baseUrl: baseUrl.trim(), secret: secret.trim(), defaultModel: defaultModel.trim(), models: splitModels(modelsText) }, baseRevision);
      onDirtyChange?.(false);
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

  /** Sync the model list from the draft, then run one real model turn; both must pass before the draft can be added. */
  const runCheck = async (options: { sync: boolean; model?: string }) => {
    setError(""); setShowLog(false);
    let models = check?.models || [];
    let syncOk = check?.syncOk ?? false;
    let syncDetail = check?.syncDetail || "";
    if (options.sync) {
      setChecking("sync");
      try {
        const synced = await onRefreshModels({ connection, baseUrl: baseUrl.trim(), secret: secret.trim() });
        syncOk = synced.ok; models = synced.ok ? synced.models : []; syncDetail = synced.detail;
      } catch (cause) { syncOk = false; models = []; syncDetail = cause instanceof Error ? cause.message : String(cause); }
      if (syncOk) setModelsText(models.join("\n"));
    }
    const manual = splitModels(modelsText);
    const model = options.model || defaultModel.trim() || models[0] || manual[0] || "";
    const key = fingerprint;
    if (!model) {
      setChecking(null);
      setCheck({ key, models, model: "", syncOk, syncDetail, result: { ok: false, detail: "没有读取到可测试的模型，请在“高级”里手动填写一个模型 ID 后重试。", model: "", engine: "", backend: "local", logs: [] } });
      return;
    }
    setChecking("test");
    try {
      const result = await onTestDraft({ connection, baseUrl: baseUrl.trim(), provider: provider.trim(), secret: secret.trim(), model });
      setCheck({ key, models: models.length ? models : manual, model, syncOk, syncDetail, result });
      setPickedModel(model);
      if (result.ok) setDefaultModel(model);
    } catch (cause) {
      setCheck({ key, models, model, syncOk, syncDetail, result: { ok: false, detail: cause instanceof Error ? cause.message : String(cause), model, engine: "", backend: "local", logs: [] } });
    } finally { setChecking(null); }
  };
  const modelOptions = (check?.models.length ? check.models : splitModels(modelsText)).map(value => ({ value, label: value }));

  return (
    <div ref={root} role="group" aria-label={editing ? "编辑接入方式" : "新增接入方式"} onKeyDown={event => { if (event.key === "Escape" && !busy) { event.stopPropagation(); onClose(); } }}
      className="flex flex-col gap-4 border-b border-cx-border-subtle bg-cx-bg-subtle px-4 py-4">
      <div className="flex items-center gap-2">
        <h3 className="min-w-0 flex-1 truncate text-[13.5px] font-medium text-cx-fg">{editing ? "编辑接入方式" : `新增接入方式 · ${engineLabel}`}</h3>
        <IconButton icon="x" label="取消" disabled={busy} onClick={onClose} />
      </div>
      {error ? <Callout tone="danger" icon="alert" role="alert">{error}</Callout> : null}
      {stale ? <Callout tone="warning" icon="alert" role="alert" action={<Button size="xs" variant="outline" onClick={reload}>载入最新配置</Button>}>配置已在其它视图更新，本地编辑仍保留。</Callout> : null}
      {readonly && credential?.sharing ? <p className="text-[12.5px] text-cx-fg-3">{credential.sharing.reason}</p> : null}
      <fieldset disabled={readonly} className="flex flex-col gap-4">
        {!editing ? (
          <div className="grid gap-2 sm:grid-cols-2" role="radiogroup" aria-label="接入类型">
            {TYPES.map(item => (
              <button key={item.value} type="button" role="radio" aria-checked={connection === item.value} onClick={() => setConnection(item.value)}
                className={cn("rounded-xl border px-3 py-2.5 text-left outline-none transition-colors focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]",
                  connection === item.value ? "border-cx-accent bg-cx-elevated" : "border-cx-border bg-cx-elevated hover:border-cx-border-strong")}>
                <span className="block text-[13px] font-medium text-cx-fg">{item.label}</span>
                <span className="block text-[11.5px] text-cx-fg-3">{item.hint}</span>
              </button>
            ))}
          </div>
        ) : (
          <div className="flex items-center gap-2">
            <SegmentedControl size="md" ariaLabel="连接方式" value={connection} onChange={setConnection} options={TYPES.map(({ value, label }) => ({ value, label }))} />
          </div>
        )}
        <div className="grid gap-4 sm:grid-cols-2">
          {!editing ? <TextField label="凭据 ID" autoComplete="off" placeholder="例如 work-account" value={accountId} onChange={event => setAccountId(event.target.value)} /> : null}
          {connection === "custom_endpoint" ? (
            <>
              <TextField label="Base URL" placeholder="https://api.example.com/v1" value={baseUrl} onChange={event => setBaseUrl(event.target.value)} />
              <TextField label="服务名称" placeholder="团队网关" value={provider} onChange={event => setProvider(event.target.value)} />
            </>
          ) : null}
          <TextField
            label={secretLabel} type={showSecret ? "text" : "password"} autoComplete="off"
            placeholder={editing ? "留空时保留已保存的凭据" : authJson && connection === "official" ? "粘贴 auth.json 内容" : ""}
            value={secret} onChange={event => setSecret(event.target.value)}
            containerClassName={connection === "custom_endpoint" && editing ? "sm:col-span-2" : undefined}
            trailing={<button type="button" aria-label={showSecret ? "隐藏凭据" : "显示凭据"} onClick={() => setShowSecret(value => !value)} className="grid size-6 place-items-center rounded-md text-cx-fg-4 hover:bg-cx-hover hover:text-cx-fg"><Icon name={showSecret ? "eyeOff" : "eye"} size={14} /></button>}
          />
        </div>
      </fieldset>
      {!editing ? (
        <div className="flex flex-col gap-2.5 rounded-xl border border-cx-border bg-cx-elevated px-3 py-3" aria-live="polite">
          <div className="flex flex-wrap items-center gap-2">
            <Button size="sm" variant={verified ? "outline" : "secondary"} icon="plug" loading={Boolean(checking)} disabled={!canCheck || busy} onClick={() => void runCheck({ sync: true })}>同步模型并测试</Button>
            <span className="min-w-0 flex-1 text-[12.5px] text-cx-fg-3">
              {checking === "sync" ? "正在从端点同步模型列表…" : checking === "test" ? "正在向模型发起一次真实请求…" : !canCheck ? "先填写连接信息，再同步并测试。" : !check ? "添加前需要读取到模型，并通过一次真实连通测试。" : checkStale ? "连接信息已修改，请重新同步并测试。" : ""}
            </span>
          </div>
          {check && !checking ? (
            <ul className="flex flex-col gap-1 text-[12.5px]">
              <li className="flex items-start gap-1.5">
                <Icon name={check.syncOk ? "checkCircle" : "alert"} size={14} className={cn("mt-0.5 shrink-0", check.syncOk ? "text-cx-success" : "text-cx-warning")} />
                <span className="min-w-0 break-words text-cx-fg-2">{check.syncOk ? `模型列表：${check.syncDetail || `读取到 ${check.models.length} 个模型`}` : `模型列表同步失败：${check.syncDetail || "服务未返回模型"}`}</span>
              </li>
              <li className="flex items-start gap-1.5">
                <Icon name={check.result.ok ? "checkCircle" : "alert"} size={14} className={cn("mt-0.5 shrink-0", check.result.ok ? "text-cx-success" : "text-cx-danger")} />
                <span className="min-w-0 break-words text-cx-fg-2">
                  {check.result.ok ? "连通测试" : "连通测试失败"}{check.model ? `（${check.model}）` : ""}：{check.result.detail || (check.result.ok ? "可用" : "未通过")}
                  {check.result.logs?.length ? <button type="button" onClick={() => setShowLog(value => !value)} className="ml-2 text-cx-accent underline underline-offset-2">{showLog || !check.result.ok ? "收起日志" : "查看日志"}</button> : null}
                </span>
              </li>
            </ul>
          ) : null}
          {check && !checking && modelOptions.length ? (
            <div className="flex flex-wrap items-center gap-2">
              <span className="text-[12px] text-cx-fg-3">测试模型</span>
              <Select size="sm" ariaLabel="测试模型" value={pickedModel || check.model} onChange={value => setPickedModel(value)} options={modelOptions} className="min-w-[200px] max-w-full" />
              <Button size="xs" variant="outline" disabled={busy || !canCheck} onClick={() => void runCheck({ sync: false, model: pickedModel || check.model })}>用此模型重新测试</Button>
            </div>
          ) : null}
          {check && !checking && check.result.logs?.length && (showLog || !check.result.ok) ? <ModelTestTerminal testing={false} result={check.result} compact title="连通测试终端" /> : null}
        </div>
      ) : null}
      {connection === "custom_endpoint" ? (
        <p className="flex items-center gap-1.5 text-[12px] text-cx-fg-3">
          <Tooltip content={`Worker 使用 ${engineLabel} 引擎调用该端点；Reason / Titler 复用时由 Muteki 后端直接发送 HTTP 请求，不会启动该 Agent。`}>
            <span tabIndex={0} aria-label="自定义端点的使用方式" className="grid size-5 place-items-center rounded-md outline-none hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"><Icon name="info" size={13} /></span>
          </Tooltip>
          同时可供 Worker 和 Reason / Titler 使用
        </p>
      ) : null}
      <div>
        <button type="button" aria-expanded={advanced} onClick={() => setAdvanced(value => !value)}
          className="inline-flex items-center gap-1.5 rounded-md text-[12.5px] text-cx-fg-3 outline-none hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]">
          <Icon name="chevronDown" size={13} className={cn("transition-transform", advanced && "rotate-180")} />高级：默认模型与候选模型
        </button>
        <Collapse open={advanced}>
          <div className="mt-3 grid gap-4 sm:grid-cols-2">
            <div className="flex flex-col gap-3">
              <TextField label="默认模型" list="credential-editor-models" autoComplete="off" placeholder="输入或选择模型 ID" value={defaultModel} onChange={event => setDefaultModel(event.target.value)} />
              <datalist id="credential-editor-models">{splitModels(modelsText).map(model => <option key={model} value={model} />)}</datalist>
              {editing ? (
                <div className="flex items-center gap-2">
                  <Button size="xs" variant="outline" icon="refresh" loading={refreshing} onClick={() => void refresh()}>从端点读取模型</Button>
                  {note ? <span role={note.ok ? "status" : "alert"} className={cn("min-w-0 truncate text-[12px]", note.ok ? "text-cx-success" : "text-cx-danger")} title={note.text}>{note.text}</span> : null}
                </div>
              ) : <p className="text-[12px] text-cx-fg-3">同步成功后会自动填入；读不到模型时，可手动填写再测试。</p>}
            </div>
            <TextArea label="候选模型" rows={4} className="resize-none font-mono text-[12px]" placeholder="每行一个模型 ID" value={modelsText} onChange={event => setModelsText(event.target.value)} />
          </div>
        </Collapse>
      </div>
      <div className="flex items-center gap-2">
        {editing && onDelete ? <Button size="sm" variant="danger-soft" icon="trash" disabled={busy || readonly} onClick={onDelete}>删除</Button> : null}
        <span className="flex-1" />
        <Button size="sm" variant="ghost" disabled={busy} onClick={onClose}>取消</Button>
        <Button size="sm" variant="primary" icon="check" loading={busy} disabled={!editing && !verified} title={!editing && !verified ? "需先同步模型并通过连通测试" : undefined} onClick={() => void submit()}>{editing ? readonly ? "保存本环境模型设置" : "保存" : "添加"}</Button>
      </div>
    </div>
  );
}
