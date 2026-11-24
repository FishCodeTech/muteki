"use client";

import { useId, useMemo, useState } from "react";
import { EngineLogo } from "@/components/EngineLogo";
import { Icon } from "@muteki/ui";
import { Button, Callout, Dialog, IconButton, Input, Label, Select, type ListOption } from "@muteki/ui";
import { SettingsEmpty, SettingsSection } from "@/components/settings/primitives";
import { useAgentVersionPolicy, workerVersionIssue, type PolicyModeId } from "@/lib/agentVersionPolicy";
import { rosterSlotEngines, rosterSummary, type RosterSeat } from "@/lib/dispatchTemplates";
import { engineDisplayName, readyCatalog, useProviderDescriptors } from "@/lib/providerDescriptors";
import type { DispatchTemplate, GlobalCredential, WorkerModelOptions, WorkerSettings } from "@/lib/useRun";
import { RosterAvatar } from "./RosterAvatar";
import { ConcurrencyStepper, CredentialSelect, EffortSelect, ModelSelect, pickerTrigger } from "./SeatFields";
import {
  ORDINARY_ROLES, credentialsForEngine, engineEffortLevels, engineLocalOnly, globalCredentialKey, globalCredentialUsable,
  normalizeEffortForModel, workerEngines,
} from "./model";

function newId(prefix: string): string {
  const suffix = typeof crypto !== "undefined" && "randomUUID" in crypto
    ? crypto.randomUUID().replaceAll("-", "").slice(0, 8)
    : `${Date.now().toString(36)}${Math.random().toString(36).slice(2, 6)}`;
  return `${prefix}_${suffix}`;
}

function blankSeat(engine: string, credential: GlobalCredential | undefined): RosterSeat {
  return {
    id: newId("seat"),
    label: "",
    engine,
    credential_id: credential?.id || "",
    model: credential?.default_model || "",
    reasoning_effort: "default",
    roles: [...ORDINARY_ROLES, "review"],
    capacity: { max_running: 1, max_review_running: 0 },
    priority: 10,
    enabled: true,
  };
}

type Issue = { seatId: string; text: string };

export function DispatchTemplates({
  templates, currentSeats, availableCredentials, models, backend, mode, nodeId, onSave, onOpenCredentials,
}: {
  templates: DispatchTemplate[];
  currentSeats: RosterSeat[];
  availableCredentials: GlobalCredential[];
  models: WorkerModelOptions;
  backend: WorkerSettings["worker_backend"];
  mode: PolicyModeId;
  nodeId: string;
  onSave: (next: DispatchTemplate[], notice: string) => Promise<string | null>;
  onOpenCredentials: () => void;
}) {
  const catalog = readyCatalog(useProviderDescriptors());
  const versionPolicy = useAgentVersionPolicy();
  const [draft, setDraft] = useState<DispatchTemplate | null>(null);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");
  const formId = useId();
  const nameOf = (engine: string) => engineDisplayName(catalog, engine);
  const isNew = Boolean(draft && !templates.some((template) => template.id === draft.id));

  const usableFor = (engine: string) => credentialsForEngine(availableCredentials, engine, backend).filter(globalCredentialUsable);
  const firstEngine = () => workerEngines(catalog).find((engine) => (backend === "local" || !engineLocalOnly(catalog, engine))
    && !workerVersionIssue(versionPolicy.data, engine, backend, mode, nodeId)
    && usableFor(engine).length) || "";

  const engineOptions = useMemo<ListOption[]>(() => workerEngines(catalog).map((engine) => ({
    value: engine,
    label: engineDisplayName(catalog, engine),
    leading: <EngineLogo engine={engine} size={16} />,
    disabled: backend !== "local" && engineLocalOnly(catalog, engine),
  })), [backend, catalog]);

  const open = (template: DispatchTemplate) => { setError(""); setDraft(template); };
  const startBlank = () => {
    const engine = firstEngine();
    open({ id: newId("tpl"), name: `出战模板 ${templates.length + 1}`, seats: engine ? [blankSeat(engine, usableFor(engine)[0])] : [] });
  };
  const startFromRoster = () => open({
    id: newId("tpl"),
    name: `出战模板 ${templates.length + 1}`,
    seats: currentSeats.map((seat) => ({ ...seat, id: newId("seat"), capacity: { ...seat.capacity }, roles: [...seat.roles] })),
  });

  const patchSeat = (id: string, patch: Partial<RosterSeat>) => setDraft((current) => current && {
    ...current,
    seats: current.seats.map((seat) => (seat.id === id ? { ...seat, ...patch } : seat)),
  });

  const issues: Issue[] = (draft?.seats || []).flatMap((seat): Issue[] => {
    const credential = availableCredentials.find((item) => item.id === seat.credential_id);
    const version = workerVersionIssue(versionPolicy.data, seat.engine, backend, mode, nodeId);
    if (version) return [{ seatId: seat.id, text: version.detail }];
    if (backend !== "local" && engineLocalOnly(catalog, seat.engine)) return [{ seatId: seat.id, text: `${nameOf(seat.engine)} 仅支持本地运行` }];
    if (!globalCredentialUsable(credential) || credential.engine !== seat.engine) return [{ seatId: seat.id, text: `${nameOf(seat.engine)} 需要选择可用凭据` }];
    if (backend !== "local" && credential.source === "system") return [{ seatId: seat.id, text: "容器运行不能使用宿主登录，请选择账号凭据" }];
    if (!seat.model?.trim()) return [{ seatId: seat.id, text: `${nameOf(seat.engine)} 需要选择模型` }];
    return [];
  });

  const commit = async () => {
    if (!draft) return;
    const name = draft.name.trim();
    if (!name) { setError("请填写模板名称"); return; }
    if (!draft.seats.length) { setError("模板至少需要一个 Worker"); return; }
    if (issues.length) { setError(issues[0].text); return; }
    const counts = new Map<string, number>();
    const seats = draft.seats.map((seat, index) => {
      const n = (counts.get(seat.engine) || 0) + 1;
      counts.set(seat.engine, n);
      return { ...seat, label: seat.label.trim() || `${nameOf(seat.engine)} Worker ${n}`, priority: (index + 1) * 10, enabled: true };
    });
    const next = { ...draft, name, seats };
    const list = isNew ? [...templates, next] : templates.map((template) => (template.id === next.id ? next : template));
    setSaving(true);
    const failure = await onSave(list, `出战模板「${name}」已保存`);
    setSaving(false);
    if (failure) setError(failure);
    else setDraft(null);
  };

  const remove = (template: DispatchTemplate) => {
    void onSave(templates.filter((item) => item.id !== template.id), `出战模板「${template.name}」已删除`);
  };

  const actions = (
    <div className="flex flex-wrap items-center gap-2">
      <Button size="sm" variant="outline" icon="copy" disabled={!currentSeats.length} onClick={startFromRoster}>从当前阵容创建</Button>
      <Button size="sm" variant="secondary" icon="plus" onClick={startBlank}>新建模板</Button>
    </div>
  );

  return (
    <>
      <SettingsSection
        title="出战模板"
        description="保存多套 Worker 阵容。蜂群模式发题时在输入框里切换，只影响那一次任务，不改动下方的默认阵容。"
        actions={actions}
      >
        {templates.length ? templates.map((template) => (
          <div key={template.id} className="flex items-center gap-3 border-b border-cx-border-subtle px-4 py-3 last:border-b-0 hover:bg-cx-hover/60">
            <RosterAvatar engines={rosterSlotEngines(template.seats)} size={32} />
            <button type="button" onClick={() => open(template)} className="min-w-0 flex-1 rounded-md text-left outline-none focus-visible:ring-2 focus-visible:ring-[var(--cx-focus)]">
              <div className="truncate text-cx-sm font-medium text-cx-fg">{template.name}</div>
              <div className="truncate text-cx-xs text-cx-fg-3">{rosterSummary(template.seats, nameOf) || "没有 Worker"}</div>
            </button>
            <IconButton icon="pencil" label={`编辑 ${template.name}`} onClick={() => open(template)} />
            <IconButton icon="trash" label={`删除 ${template.name}`} onClick={() => remove(template)} />
          </div>
        )) : <SettingsEmpty>还没有出战模板。可以从当前阵容创建，或新建一套。</SettingsEmpty>}
      </SettingsSection>

      <Dialog
        open={Boolean(draft)}
        onOpenChange={(next) => { if (!next) setDraft(null); }}
        size="xl"
        title={isNew ? "新建出战模板" : "编辑出战模板"}
        description="为模板选择 Agent、凭据、模型、推理强度和并发数。凭据按当前运行环境筛选。"
        dismissable={false}
        footer={<>
          <Button variant="ghost" onClick={() => setDraft(null)}>取消</Button>
          <Button variant="primary" type="submit" form={formId} loading={saving} disabled={saving}>保存模板</Button>
        </>}
      >
        {draft ? (
          <form
            id={formId}
            className="@container flex min-w-0 flex-col gap-5"
            onSubmit={(event) => { event.preventDefault(); void commit(); }}
          >
            <div className="flex items-end gap-3">
              <RosterAvatar engines={rosterSlotEngines(draft.seats)} size={40} />
              <div className="min-w-0 flex-1">
                <Label htmlFor={`${formId}-name`}>模板名称</Label>
                <Input
                  id={`${formId}-name`} value={draft.name} maxLength={60}
                  onChange={(event) => setDraft({ ...draft, name: event.target.value })}
                />
              </div>
            </div>

            <div className="flex flex-col gap-2">
              <div className="flex items-center justify-between gap-2">
                <Label className="mb-0">Worker</Label>
                <Button size="sm" variant="ghost" onClick={onOpenCredentials}>管理凭据…</Button>
              </div>
              <div className="hidden grid-cols-[150px_minmax(0,1fr)_minmax(0,1.2fr)_120px_96px_28px] gap-2 px-1 text-cx-xs text-cx-fg-3 @[720px]:grid">
                <span>Agent</span><span>凭据</span><span>模型</span><span>推理强度</span><span>并发数</span><span />
              </div>
              {draft.seats.map((seat) => {
                const credential = availableCredentials.find((item) => item.id === seat.credential_id);
                const accountKey = credential ? globalCredentialKey(credential) : "";
                const modelOptions = models.models[seat.engine] || [];
                const issue = issues.find((item) => item.seatId === seat.id);
                const changeModel = (model: string) => patchSeat(seat.id, {
                  model,
                  reasoning_effort: normalizeEffortForModel(seat.reasoning_effort, engineEffortLevels(catalog, seat.engine), model, modelOptions),
                });
                return (
                  <div key={seat.id} className="flex flex-col gap-1.5 rounded-lg border border-cx-border-subtle p-2 @[720px]:border-0 @[720px]:p-0">
                    <div className="grid grid-cols-2 items-center gap-2 @[720px]:grid-cols-[150px_minmax(0,1fr)_minmax(0,1.2fr)_120px_96px_28px]">
                      <Select
                        value={seat.engine}
                        options={engineOptions}
                        ariaLabel="Agent"
                        searchable={engineOptions.length > 8}
                        popoverClassName="w-[240px]"
                        onChange={(engine) => {
                          if (engine === seat.engine) return;
                          const next = usableFor(engine)[0];
                          patchSeat(seat.id, { engine, label: "", credential_id: next?.id || "", model: next?.default_model || "", reasoning_effort: "default" });
                        }}
                        trigger={pickerTrigger({
                          label: nameOf(seat.engine), placeholder: "选择 Agent", hasValue: true, ariaLabel: "Agent",
                          leading: <EngineLogo engine={seat.engine} size={14} />,
                        })}
                      />
                      <CredentialSelect
                        engine={seat.engine} accountKey={accountKey} model={seat.model || ""}
                        availableCredentials={availableCredentials} backend={backend}
                        onBind={(key, resolved) => {
                          const next = resolved || availableCredentials.find((item) => item.engine === seat.engine && globalCredentialKey(item) === key);
                          patchSeat(seat.id, { credential_id: next?.id || "" });
                        }}
                        onModelChange={changeModel}
                        onOpenCredentials={onOpenCredentials}
                      />
                      <ModelSelect engine={seat.engine} accountKey={accountKey} model={seat.model || ""} availableCredentials={availableCredentials} onModelChange={changeModel} />
                      <EffortSelect engine={seat.engine} model={seat.model || ""} options={modelOptions} value={seat.reasoning_effort || "default"} onChange={(reasoning_effort) => patchSeat(seat.id, { reasoning_effort })} />
                      <ConcurrencyStepper value={seat.capacity.max_running} onChange={(max_running) => patchSeat(seat.id, { capacity: { ...seat.capacity, max_running } })} />
                      <IconButton
                        icon="trash" label="移除此 Worker"
                        onClick={() => setDraft({ ...draft, seats: draft.seats.filter((item) => item.id !== seat.id) })}
                      />
                    </div>
                    {issue ? (
                      <p className="flex items-center gap-1.5 px-1 text-cx-xs text-cx-warning">
                        <Icon name="circleAlert" size={13} className="shrink-0" />{issue.text}
                      </p>
                    ) : null}
                  </div>
                );
              })}
              <div>
                <Button
                  size="sm" variant="ghost" icon="plus"
                  onClick={() => {
                    const engine = draft.seats[draft.seats.length - 1]?.engine || firstEngine();
                    if (!engine) { setError("还没有可用的 Agent 凭据，请先配置凭据"); return; }
                    setDraft({ ...draft, seats: [...draft.seats, blankSeat(engine, usableFor(engine)[0])] });
                  }}
                >
                  添加 Worker
                </Button>
              </div>
            </div>

            {error ? <Callout tone="warning" icon="alert" title={error} /> : null}
          </form>
        ) : null}
      </Dialog>
    </>
  );
}
