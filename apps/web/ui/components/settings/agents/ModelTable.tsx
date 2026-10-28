"use client";

import { useEffect, useMemo, useState, type ReactNode } from "react";
import { Badge, Button, Checkbox, CopyButton, IconButton, Menu, MenuItem, MenuSeparator, SearchInput, StatusDot, Tooltip } from "@/components/chat/ui";
import { Icon } from "@/components/Icon";
import { cn } from "@/lib/cn";
import type { ChatDefaultModel } from "@/lib/conversationDefaults";
import { useLang } from "@/lib/i18n";
import { modelEffortLevels, modelServiceTiers } from "@/lib/modelReasoning";
import { isModelHidden } from "@/lib/modelVisibility";
import { priceKey, useModelPrices, type PriceIdentity, type PricingCatalog } from "@/lib/modelPricing";
import type { ConversationCredentialModel } from "@/lib/useConversation";
import { PriceEditor, priceCellView } from "./PriceEditor";
import { statusTone, type StatusKind } from "./shared";

export interface AgentModelRow {
  key: string;
  credentialId: string;
  engine: string;
  providerId?: string;
  credentialName: string;
  model: ConversationCredentialModel;
  verified: boolean;
}

/** One access method (system login or a stored credential) and the models it offers. */
export interface ModelGroup {
  id: string;
  name: string;
  typeLabel: string;
  statusKind: StatusKind;
  statusText: string;
  summary: string;
  detail: string;
  rows: AgentModelRow[];
  ready: boolean;
  isDefaultSource: boolean;
  testing: boolean;
  refreshing: boolean;
  canEdit: boolean;
  canImport: boolean;
  canDelete: boolean;
  isSystem: boolean;
  hasTestResult: boolean;
}

export const rowIdentity = (row: AgentModelRow): PriceIdentity => ({ engine: row.engine, model: row.model.id, credential_id: row.credentialId, provider_id: row.providerId });

const TONE_CLASS = { ok: "text-cx-fg-2", warn: "text-cx-warning", mute: "text-cx-fg-3" } as const;

export function ModelTable({
  groups, chatDefault, hidden, catalog, expandedPrice, onExpandPrice, onSetDefault, onToggleHidden,
  onTest, onRefresh, onViewTest, onEdit, onImport, onDelete, renderExtra, createPanel, focusId, toolbarActions, empty, refreshDisabled, loginCommand, showDefault = true, showPrice = true,
}: {
  groups: ModelGroup[];
  chatDefault: ChatDefaultModel | null;
  hidden: ReadonlySet<string>;
  catalog: PricingCatalog | null;
  expandedPrice: string;
  onExpandPrice: (key: string) => void;
  onSetDefault: (credentialId: string, modelId: string) => void;
  onToggleHidden: (credentialId: string, modelId: string, hide: boolean) => void;
  onTest: (id: string) => void;
  onRefresh: (id: string) => void;
  onViewTest: (id: string) => void;
  onEdit: (id: string) => void;
  onImport: (id: string) => void;
  onDelete: (id: string) => void;
  /** In-card content under a group's header: editor, delete confirmation, import form, test result. */
  renderExtra?: (group: ModelGroup) => ReactNode;
  /** In-place form for a new access method, shown above the groups. */
  createPanel?: ReactNode;
  /** Group to scroll into view (e.g. a freshly added one). */
  focusId?: string;
  toolbarActions?: ReactNode;
  empty?: ReactNode;
  refreshDisabled?: boolean;
  loginCommand?: string;
  showDefault?: boolean;
  showPrice?: boolean;
}) {
  const { lang } = useLang(); const en = lang === "en";
  const [query, setQuery] = useState("");
  const [onlyUnpriced, setOnlyUnpriced] = useState(false);
  const [collapsed, setCollapsed] = useState<ReadonlySet<string>>(new Set());
  const allRows = useMemo(() => groups.flatMap(group => group.rows), [groups]);
  const pricing = useModelPrices(allRows.map(rowIdentity), showPrice);
  const priceOf = (row: AgentModelRow) => pricing.values.get(priceKey(rowIdentity(row)));
  const unpriced = !showPrice || pricing.loading ? 0 : allRows.filter(row => { const price = priceOf(row); return price && !price.rates; }).length;
  const needle = query.trim().toLowerCase();

  useEffect(() => {
    if (!focusId) return;
    document.querySelector<HTMLElement>(`[data-group-id="${CSS.escape(focusId)}"]`)?.scrollIntoView({ block: "nearest", behavior: "smooth" });
  }, [focusId, groups.length]);
  useEffect(() => {
    if (!expandedPrice) return;
    const element = Array.from(document.querySelectorAll<HTMLElement>("[data-price-row]")).find(node => node.dataset.priceRow === expandedPrice);
    element?.scrollIntoView({ block: "nearest", behavior: "smooth" });
  }, [expandedPrice, groups.length]);

  const visibleRows = (group: ModelGroup) => group.rows.filter(row => {
    if (needle && !`${row.model.id} ${row.model.label}`.toLowerCase().includes(needle)) return false;
    if (showPrice && onlyUnpriced) { const price = priceOf(row); return !price || !price.rates; }
    return true;
  });
  const toggleGroup = (id: string) => setCollapsed(current => { const next = new Set(current); if (!next.delete(id)) next.add(id); return next; });

  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center gap-2" data-settings-anchor="agents-credentials" id="setting-agents-credentials">
        <SearchInput value={query} onValueChange={setQuery} placeholder="搜索模型…" aria-label="搜索模型" className="min-w-[160px] flex-1 sm:max-w-[280px]" />
        {showPrice ? <Checkbox checked={onlyUnpriced} onCheckedChange={setOnlyUnpriced} label={`仅看未定价${unpriced ? `（${unpriced}）` : ""}`} /> : null}
        <span className="flex-1" />
        {toolbarActions}
      </div>
      {createPanel ? <section className="cx-settings-card" aria-label="新增接入方式">{createPanel}</section> : null}
      {!groups.length && !createPanel ? empty : null}
      {groups.map(group => {
        const rows = visibleRows(group);
        const open = !collapsed.has(group.id);
        return (
          <section key={group.id} data-group-id={group.id} className="cx-settings-card" aria-label={group.name}>
            <div className="flex flex-wrap items-center gap-x-2 gap-y-1.5 border-b border-cx-border-subtle bg-cx-bg-subtle px-3 py-2.5 last:border-b-0">
              <button type="button" aria-expanded={open} onClick={() => toggleGroup(group.id)}
                className="flex min-w-0 flex-1 basis-56 items-center gap-2 rounded-lg px-1 py-0.5 text-left outline-none focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]">
                <Icon name="chevronDown" size={15} className={cn("shrink-0 text-cx-fg-3 transition-transform", !open && "-rotate-90")} />
                <span className="min-w-0">
                  <span className="flex items-center gap-1.5 text-[13.5px] font-medium text-cx-fg">
                    <span className="truncate">{group.name}</span>
                    {group.isDefaultSource ? <Icon name="star" size={12} filled className="shrink-0 text-cx-warning" aria-label="新对话默认来源" /> : null}
                    {["warn", "bad"].includes(group.statusKind) ? <StatusDot tone={statusTone(group.statusKind)} label={group.statusText} /> : null}
                  </span>
                  <span className="block truncate text-[11.5px] leading-4 text-cx-fg-3">{group.typeLabel} · {group.statusText} · {group.summary}</span>
                </span>
              </button>
              <Tooltip content={group.detail}><span tabIndex={0} aria-label="接入方式详情" className="grid size-6 place-items-center rounded-md text-cx-fg-4 outline-none hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"><Icon name="info" size={14} /></span></Tooltip>
              <Button size="xs" variant="outline" icon="plug" loading={group.testing} disabled={!group.ready} onClick={() => onTest(group.id)}>测试</Button>
              <Button size="xs" variant="outline" icon="refresh" loading={group.refreshing} disabled={!group.ready || refreshDisabled} onClick={() => onRefresh(group.id)}>刷新模型</Button>
              {group.canEdit || group.canImport || group.canDelete || group.hasTestResult ? (
                <Menu trigger={<IconButton icon="more" label="更多操作" />} placement="bottom-end" ariaLabel={`${group.name} 操作`}>
                  {group.hasTestResult ? <MenuItem icon="terminal" onSelect={() => onViewTest(group.id)}>查看最近测试</MenuItem> : null}
                  {group.canEdit ? <MenuItem icon="pencil" onSelect={() => onEdit(group.id)}>编辑接入方式</MenuItem> : null}
                  {group.canImport ? <MenuItem icon="upload" onSelect={() => onImport(group.id)} description="供容器 Worker 使用">导入为独立凭据</MenuItem> : null}
                  {group.canDelete ? <>{group.canEdit || group.canImport || group.hasTestResult ? <MenuSeparator /> : null}<MenuItem icon="trash" danger onSelect={() => onDelete(group.id)}>删除</MenuItem></> : null}
                </Menu>
              ) : null}
            </div>
            {renderExtra?.(group)}
            {open ? (rows.length ? rows.map(row => {
              const identity = rowIdentity(row);
              const key = priceKey(identity);
              const price = priceOf(row);
              const view = priceCellView(price, pricing.loading, pricing.error, en);
              const isDefault = chatDefault?.credentialId === row.credentialId && chatDefault?.modelId === row.model.id;
              const isHidden = isModelHidden(hidden, row.credentialId, row.model.id);
              const editing = expandedPrice === key;
              const fast = modelServiceTiers(row.model).length > 0;
              const reasoning = modelEffortLevels(row.model).length > 0;
              return (
                <div key={row.key} data-price-row={key} className="border-b border-cx-border-subtle last:border-b-0">
                  <div className={cn("flex flex-wrap items-center gap-x-2.5 gap-y-1 px-3 py-2", isHidden && "opacity-60")}>
                    {showDefault ? <button type="button" aria-pressed={isDefault} title={isDefault ? "新对话默认模型" : "设为新对话默认模型"}
                      aria-label={isDefault ? `${row.model.id} 是新对话默认模型` : `将 ${row.model.id} 设为新对话默认模型`}
                      onClick={() => onSetDefault(row.credentialId, row.model.id)}
                      className={cn("grid size-7 shrink-0 place-items-center rounded-lg outline-none transition-colors focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]", isDefault ? "text-cx-warning" : "text-cx-fg-4 hover:bg-cx-hover hover:text-cx-fg")}>
                      <Icon name="star" size={15} filled={isDefault} />
                    </button> : null}
                    <div className="flex min-w-0 flex-1 basis-48 flex-wrap items-center gap-x-2 gap-y-1">
                      <code className="truncate text-[12.5px] text-cx-fg" title={row.model.label !== row.model.id ? row.model.label : undefined}>{row.model.id}</code>
                      {row.verified ? <Tooltip content="已测通"><span tabIndex={0} aria-label="已测通" className="inline-flex outline-none"><Badge tone="success" icon="check">{null}</Badge></span></Tooltip> : null}
                      {fast ? <Tooltip content="支持快速模式"><span tabIndex={0} aria-label="支持快速模式" className="inline-flex outline-none"><Badge tone="accent" icon="zap">{null}</Badge></span></Tooltip> : null}
                      {reasoning ? <Tooltip content="支持推理强度"><span tabIndex={0} aria-label="支持推理强度" className="inline-flex outline-none"><Badge tone="accent" icon="brain">{null}</Badge></span></Tooltip> : null}
                    </div>
                    {showPrice ? <button type="button" aria-expanded={editing} aria-label={`${row.model.id} 价格：${view.text}`} onClick={() => onExpandPrice(editing ? "" : key)}
                      className={cn("inline-flex max-w-full items-center gap-1.5 rounded-lg px-2 py-1 text-[12px] outline-none transition-colors hover:bg-cx-hover focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]", TONE_CLASS[view.tone], editing && "bg-cx-hover")}>
                      {view.tone === "warn" ? <Icon name="alert" size={13} className="shrink-0" /> : null}
                      <span className="cx-tabular truncate">{view.text}</span>
                      {view.source ? <span className="shrink-0 rounded bg-cx-hover px-1 text-[10.5px] text-cx-fg-3">{view.source}</span> : null}
                      <Icon name="pencil" size={12} className="shrink-0 opacity-50" />
                    </button> : null}
                    <IconButton icon={isHidden ? "eyeOff" : "eye"} label={isHidden ? "在模型选择器中显示" : "在模型选择器中隐藏"} onClick={() => onToggleHidden(row.credentialId, row.model.id, !isHidden)} />
                  </div>
                  {showPrice && editing ? <PriceEditor identity={identity} price={price} catalog={catalog} credentialLabel={row.credentialName} onClose={() => onExpandPrice("")} /> : null}
                </div>
              );
            }) : (
              <div className="flex flex-wrap items-center justify-center gap-2 px-4 py-5 text-center text-[12.5px] text-cx-fg-3">
                <span>{group.rows.length ? "没有匹配的模型。" : group.ready ? "尚无模型，点击“刷新模型”从该接入方式读取。" : group.isSystem ? "未检测到本机登录，请先在终端完成登录后点“探测”。" : "该接入方式暂不可用，请检查凭据。"}</span>
                {!group.ready && group.isSystem && loginCommand ? <><code className="rounded-md bg-cx-code px-2 py-1 text-[12px] text-cx-fg">{loginCommand}</code><CopyButton text={loginCommand} label="复制登录命令" /></> : null}
              </div>
            )) : null}
          </section>
        );
      })}
    </div>
  );
}
