"use client";

import { useId, useMemo, useState } from "react";
import { EngineLogo } from "@/components/EngineLogo";
import {
  Badge, Button, Callout, Checkbox, Collapse, Dialog, EmptyState, Input, Label, OptionList,
  ScrollArea, SearchInput, Select, Spinner, useListKeyboard, type ListOption,
} from "@muteki/ui";
import { copyToClipboard } from "@/lib/clipboard";
import {
  useAgentVersionPolicy, workerVersionIssue, type PolicyModeId,
} from "@/lib/agentVersionPolicy";
import {
  engineDisplayName, loadProviderDescriptors, readyCatalog, useProviderDescriptors,
} from "@/lib/providerDescriptors";
import type { GlobalCredential, WorkerModelOptions, WorkerSettings } from "@/lib/useRun";
import { ConcurrencyStepper, CredentialSelect, EffortSelect, ModelSelect } from "./SeatFields";
import {
  credentialsForEngine, engineEffortLevels, engineLocalOnly, globalCredentialKey, globalCredentialUsable,
  isOrdinarySeat, normalizeEffortForModel, workerEngines, type NewWorkerOptions, type Seat,
} from "./model";

type Draft = {
  engine: string;
  credentialId: string;
  model?: string;
  label: string;
  reasoning_effort: string;
  max_running: number;
};

const EMPTY_DRAFT: Draft = { engine: "", credentialId: "", label: "", reasoning_effort: "default", max_running: 1 };

export function AddWorkerDialog({
  open, onOpenChange, seats, availableCredentials, models, backend, mode, nodeId = "local",
  onAdd, onOpenCredentials, onOpenRuntime, onOpenVersions,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  seats: Seat[];
  availableCredentials: GlobalCredential[];
  models: WorkerModelOptions;
  backend: WorkerSettings["worker_backend"];
  mode?: PolicyModeId;
  nodeId?: string;
  onAdd: (credential: GlobalCredential, options: NewWorkerOptions) => boolean;
  onOpenCredentials: () => void;
  onOpenRuntime: () => void;
  onOpenVersions?: () => void;
}) {
  const descriptorState = useProviderDescriptors();
  const catalog = readyCatalog(descriptorState);
  const versionPolicy = useAgentVersionPolicy();
  const [draft, setDraft] = useState<Draft>(EMPTY_DRAFT);
  const [query, setQuery] = useState("");
  const [advanced, setAdvanced] = useState(false);
  const [keepAdding, setKeepAdding] = useState(false);
  const [addedLabel, setAddedLabel] = useState("");
  const [copiedCmd, setCopiedCmd] = useState(false);
  const formId = useId();
  const engineListId = useId();
  const advancedId = useId();

  const engineOptions = useMemo<ListOption[]>(() => workerEngines(catalog).map((engine) => {
    const localOnly = engineLocalOnly(catalog, engine) && backend !== "local";
    const verIssue = workerVersionIssue(versionPolicy.data, engine, backend, mode, nodeId);
    const configured = availableCredentials.some((item) => item.engine === engine);
    const usable = credentialsForEngine(availableCredentials, engine, backend).filter(globalCredentialUsable);
    return {
      value: engine,
      label: engineDisplayName(catalog, engine),
      textValue: `${engineDisplayName(catalog, engine)} ${engine} ${availableCredentials.filter((item) => item.engine === engine).map((item) => item.label || item.id).join(" ")}`,
      leading: <EngineLogo engine={engine} size={18} />,
      trailing: verIssue ? (
        <span className="shrink-0 text-cx-xs font-medium text-cx-danger">版本过低</span>
      ) : (
        <span className="shrink-0 text-cx-xs text-cx-fg-3">{localOnly ? "仅本地" : usable.length || (configured ? "不可用" : "待配置")}</span>
      ),
    };
  }), [availableCredentials, backend, catalog, mode, nodeId, versionPolicy.data]);
  const engine = engineOptions.some((item) => item.value === draft.engine) ? draft.engine
    : engineOptions.find((item) => (backend === "local" || !engineLocalOnly(catalog, item.value))
      && !workerVersionIssue(versionPolicy.data, item.value, backend, mode, nodeId)
      && credentialsForEngine(availableCredentials, item.value, backend).some(globalCredentialUsable))?.value
      || engineOptions[0]?.value || "";
  const engineName = engineDisplayName(catalog, engine);
  const engineCredentials = availableCredentials.filter((item) => item.engine === engine);
  const matching = credentialsForEngine(availableCredentials, engine, backend);
  const credential = matching.find((item) => item.id === draft.credentialId && draft.engine === engine)
    || matching.find(globalCredentialUsable) || null;
  const localOnly = engineLocalOnly(catalog, engine) && backend !== "local";
  const versionIssue = workerVersionIssue(versionPolicy.data, engine, backend, mode, nodeId);
  const credentialStatusDetail = credential?.status_detail;
  const usable = globalCredentialUsable(credential);
  const model = draft.engine === engine && (!draft.credentialId || draft.credentialId === credential?.id)
    ? draft.model ?? credential?.default_model ?? "" : credential?.default_model || "";
  const modelOptions = models.models[engine] || [];
  const effort = normalizeEffortForModel(draft.reasoning_effort, engineEffortLevels(catalog, engine), model, modelOptions);
  const defaultLabel = `${engineName} Worker ${seats.filter((seat) => isOrdinarySeat(seat) && seat.engine === engine).length + 1}`;
  const accountKey = credential ? globalCredentialKey(credential) : "";
  const canAdd = Boolean(engine && usable && !localOnly && !versionIssue);
  const hostLoginOnly = backend !== "local" && !matching.length && engineCredentials.some((item) => item.source === "system");
  const needle = query.trim().toLocaleLowerCase("en-US");
  const filtered = engineOptions.filter((option) => !needle || option.textValue?.toLocaleLowerCase("en-US").includes(needle));

  const chooseEngine = (next: string) => {
    setDraft({ ...EMPTY_DRAFT, engine: next });
    setAddedLabel("");
  };
  const keyboard = useListKeyboard(filtered, chooseEngine);
  const patchDraft = (patch: Partial<Draft>) => {
    setDraft((current) => ({ ...current, engine, credentialId: credential?.id || "", model, ...patch }));
    setAddedLabel("");
  };
  const changeModel = (next: string) => patchDraft({
    model: next,
    reasoning_effort: normalizeEffortForModel(draft.reasoning_effort, engineEffortLevels(catalog, engine), next, modelOptions),
  });
  const changeOpen = (next: boolean) => {
    if (!next) { setQuery(""); setAddedLabel(""); }
    onOpenChange(next);
  };

  return (
    <Dialog
      open={open} onOpenChange={changeOpen} size="xl"
      title="添加 Worker" description="选择引擎、账号和模型，添加到当前出战配置。"
      className="h-[min(560px,calc(100dvh-48px))]"
      bodyClassName="overflow-hidden p-0"
      footer={<>
        <Checkbox checked={keepAdding} onCheckedChange={setKeepAdding} label="添加后继续" className="w-full sm:mr-auto sm:w-auto" />
        <Button variant="ghost" onClick={() => changeOpen(false)}>取消</Button>
        <Button variant="primary" type="submit" form={formId} disabled={!canAdd}>添加 Worker</Button>
      </>}
    >
      {descriptorState.status === "error" ? (
        <div className="p-5"><Callout tone="warning" title="引擎目录读取失败" action={<Button size="sm" variant="outline" onClick={() => void loadProviderDescriptors({ fresh: true }).catch(() => undefined)}>重试</Button>}>{descriptorState.message}</Callout></div>
      ) : descriptorState.status === "loading" ? (
        <div className="flex h-full items-center justify-center gap-2" role="status"><Spinner size={16} />正在读取引擎…</div>
      ) : !engineOptions.length ? (
        <EmptyState icon="grid" title="还没有可用引擎" description="请先在 Agent 设置中配置引擎。" />
      ) : (
        <div className="grid h-full min-h-0 grid-cols-1 sm:grid-cols-[208px_minmax(0,1fr)]">
          <aside className="hidden min-h-0 flex-col border-r border-cx-border-subtle bg-cx-bg-subtle sm:flex" aria-label="Worker 引擎">
            <div className="p-3">
              <SearchInput
                value={query} onValueChange={(next) => { setQuery(next); keyboard.setActiveIndex(0); }}
                placeholder="搜索引擎或账号…" aria-label="搜索引擎或账号" role="combobox" aria-expanded
                aria-controls={engineListId} aria-activedescendant={filtered[keyboard.activeIndex] ? `${engineListId}-opt-${keyboard.activeIndex}` : undefined}
                onKeyDown={keyboard.onKeyDown}
              />
            </div>
            <OptionList
              id={engineListId} options={filtered} selected={engine} onSelect={chooseEngine}
              activeIndex={keyboard.activeIndex} onActiveIndexChange={keyboard.setActiveIndex}
              className="flex-1 px-2 pb-3" emptyText="没有匹配的引擎或账号"
            />
          </aside>

          <ScrollArea fade={false} className="min-h-0 p-5">
            <form id={formId} className="flex min-w-0 flex-col gap-5" onSubmit={(event) => {
              event.preventDefault();
              if (!canAdd || !credential) return;
              const label = draft.label.trim() || defaultLabel;
              if (!onAdd(credential, { label, model, reasoning_effort: effort, max_running: draft.max_running })) return;
              setDraft((current) => ({ ...current, label: "" }));
              setAddedLabel(`已添加 ${label}`);
              if (!keepAdding) changeOpen(false);
            }}>
              <div className="sm:hidden">
                <Label>Worker 引擎</Label>
                <Select value={engine} onChange={chooseEngine} options={engineOptions} searchable searchPlaceholder="搜索引擎或账号…" ariaLabel="Worker 引擎" className="w-full" />
              </div>
              <div className="flex items-center justify-between gap-3">
                <div className="flex min-w-0 items-center gap-2.5">
                  <EngineLogo engine={engine} size={24} />
                  <h3 className="truncate text-cx-base font-semibold text-cx-fg">{engineName}</h3>
                </div>
                <Badge>{backend === "local" ? "本地运行" : "容器运行"}</Badge>
              </div>

              {versionIssue ? (
                <Callout
                  tone="danger"
                  icon="alert"
                  title="版本低于最低版本 · 禁止设为出战 Worker"
                  action={
                    <div className="flex flex-wrap items-center gap-1.5">
                      {onOpenVersions ? (
                        <Button size="sm" variant="outline" onClick={() => { changeOpen(false); onOpenVersions(); }}>
                          版本要求设置
                        </Button>
                      ) : null}
                      {backend === "container" ? (
                        <Button size="sm" variant="ghost" onClick={() => { changeOpen(false); onOpenRuntime(); }}>
                          切换运行环境
                        </Button>
                      ) : null}
                    </div>
                  }
                >
                  <div className="flex flex-col gap-2">
                    <p>{versionIssue.detail}</p>
                    {versionIssue.upgradeCommand ? (
                      <div className="flex flex-wrap items-center gap-2">
                        <code className="rounded bg-cx-code px-2 py-0.5 font-cx-mono text-cx-xs text-cx-fg">
                          {versionIssue.upgradeCommand}
                        </code>
                        <Button
                          size="xs"
                          variant="outline"
                          icon={copiedCmd ? "check" : "copy"}
                          onClick={() => void copyToClipboard(versionIssue.upgradeCommand).then((ok) => {
                            if (ok) {
                              setCopiedCmd(true);
                              window.setTimeout(() => setCopiedCmd(false), 1800);
                            }
                          })}
                        >
                          {copiedCmd ? "已复制" : "复制升级命令"}
                        </Button>
                      </div>
                    ) : null}
                  </div>
                </Callout>
              ) : localOnly ? (
                <Callout tone="warning" title="此引擎仅支持本地运行" action={<Button size="sm" variant="outline" onClick={onOpenRuntime}>查看运行环境</Button>}>
                  当前出战配置使用容器，无法添加 {engineName}。切换到本地运行后可用。
                </Callout>
              ) : !matching.some(globalCredentialUsable) ? (
                <EmptyState
                  icon="lock" title={hostLoginOnly ? "当前运行方式没有可用账号" : matching.length ? "账号暂不可用" : "还没有可用账号"}
                  description={hostLoginOnly ? "此引擎目前只有宿主登录。请切换到本地运行，或配置用于容器的账号。" : matching.find((item) => item.status_detail)?.status_detail || "请在 Agent 凭据中完成登录或配置接入点，返回后继续添加。"}
                  action={<div className="flex flex-wrap justify-center gap-2"><Button size="sm" variant="outline" onClick={onOpenCredentials}>去配置凭据</Button>{hostLoginOnly ? <Button size="sm" variant="ghost" onClick={onOpenRuntime}>查看运行环境</Button> : null}</div>}
                  className="py-6"
                />
              ) : <>
                <div className="min-w-0">
                  <div className="mb-1 flex items-center justify-between gap-2">
                    <Label className="mb-0">账号 / 接入点</Label>
                    <Button size="sm" variant="ghost" onClick={onOpenCredentials}>管理凭据…</Button>
                  </div>
                  <CredentialSelect
                    engine={engine} accountKey={accountKey} model={model} availableCredentials={availableCredentials} backend={backend}
                    onBind={(key, resolved) => {
                      const next = resolved || matching.find((item) => globalCredentialKey(item) === key);
                      patchDraft({ credentialId: next?.id || "", model: next?.default_model || "", reasoning_effort: "default" });
                    }}
                    onModelChange={changeModel} onOpenCredentials={onOpenCredentials}
                  />
                  {!usable ? <p className="mt-1 text-cx-xs text-cx-warning">{credentialStatusDetail || "当前账号不可用，请重新选择或管理凭据。"}</p> : null}
                </div>
                <div className="min-w-0">
                  <Label>模型</Label>
                  <ModelSelect engine={engine} accountKey={accountKey} model={model} availableCredentials={availableCredentials} onModelChange={changeModel} />
                </div>
                <div>
                  <Button variant="ghost" size="sm" iconRight={advanced ? "chevronUp" : "chevronDown"} aria-expanded={advanced} aria-controls={advancedId} onClick={() => setAdvanced(!advanced)}>更多设置</Button>
                  <div id={advancedId}>
                    <Collapse open={advanced}>
                      <div className="grid min-w-0 grid-cols-1 gap-4 pt-3 sm:grid-cols-2">
                        <div className="min-w-0 sm:col-span-2">
                          <Label htmlFor={`${formId}-name`}>名称</Label>
                          <Input id={`${formId}-name`} value={draft.label} placeholder={defaultLabel} onChange={(event) => patchDraft({ label: event.target.value })} />
                        </div>
                        <div className="min-w-0">
                          <Label>推理强度</Label>
                          <EffortSelect engine={engine} model={model} options={modelOptions} value={effort} onChange={(reasoning_effort) => patchDraft({ reasoning_effort })} />
                        </div>
                        <div className="min-w-0">
                          <Label>并发数</Label>
                          <ConcurrencyStepper value={draft.max_running} onChange={(max_running) => patchDraft({ max_running })} />
                        </div>
                      </div>
                    </Collapse>
                  </div>
                </div>
                <p className="border-t border-cx-border-subtle pt-4 text-cx-xs leading-5 text-cx-fg-3">添加后可在 Worker 列表中调整优先级与辅助角色，保存后用于下次任务。</p>
              </>}
              {addedLabel ? <p role="status" className="text-cx-xs text-cx-success">{addedLabel}</p> : null}
            </form>
          </ScrollArea>
        </div>
      )}
    </Dialog>
  );
}
