"use client";

import { useEffect, useMemo, useState, type ReactNode } from "react";
import { Badge, Button, Checkbox, CopyButton, IconButton, Menu, MenuItem, MenuSeparator, SearchInput, Spinner, StatusDot, Tooltip, useContextMenu } from "@muteki/ui";
import { Icon } from "@muteki/ui";
import { cn } from "@/lib/cn";
import { scrollBehavior } from "@/lib/usePrefersReducedMotion";
import type { ChatDefaultModel } from "@/lib/conversationDefaults";
import { useLang } from "@/lib/i18n";
import { modelEffortLevels, modelServiceTiers } from "@/lib/modelReasoning";
import { isModelHidden } from "@/lib/modelVisibility";
import { priceKey, useModelPrices, type PriceIdentity, type PricingCatalog } from "@/lib/modelPricing";
import type { ConversationCredentialModel } from "@/lib/useConversation";
import { PriceEditor, priceCellView } from "./PriceEditor";
import { formatTokenCount } from "./piModelRecommendations";
import { statusTone, type StatusKind } from "./shared";

export interface AgentModelRow {
  key: string;
  credentialId: string;
  engine: string;
  providerId?: string;
  credentialName: string;
  model: ConversationCredentialModel;
  verified: boolean;
  contextWindow?: number;
  maxOutputTokens?: number;
}

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
const FILTER_THRESHOLD = 8;
const COLLAPSE_AFTER = 10;
const PREVIEW_ROWS = 8;

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
  renderExtra?: (group: ModelGroup) => ReactNode;
  createPanel?: ReactNode;
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
  const [openChoice, setOpenChoice] = useState<Readonly<Record<string, boolean>>>({});
  const [fullList, setFullList] = useState<ReadonlySet<string>>(new Set());
  const [hiddenShown, setHiddenShown] = useState<ReadonlySet<string>>(new Set());
  const contextMenu = useContextMenu();
  const [contextGroupId, setContextGroupId] = useState("");
  const allRows = useMemo(() => groups.flatMap(group => group.rows), [groups]);
  const pricing = useModelPrices(allRows.map(rowIdentity), showPrice);
  const priceOf = (row: AgentModelRow) => pricing.values.get(priceKey(rowIdentity(row)));
  const unpriced = !showPrice || pricing.loading ? 0 : allRows.filter(row => { const price = priceOf(row); return price && !price.rates; }).length;
  const needle = query.trim().toLowerCase();
  const filtering = Boolean(needle) || (showPrice && onlyUnpriced);
  const showSearch = allRows.length > FILTER_THRESHOLD || Boolean(query);
  const showUnpricedFilter = showPrice && (unpriced > 0 || onlyUnpriced);
  const primaryGroupId = (groups.length === 1 ? groups[0] : groups.find(group => group.isDefaultSource) || groups.find(group => group.ready && group.rows.length) || groups[0])?.id;
  const priceGroupId = expandedPrice ? groups.find(group => group.rows.some(row => priceKey(rowIdentity(row)) === expandedPrice))?.id : undefined;

  useEffect(() => {
    if (!focusId) return;
    setOpenChoice(current => current[focusId] ? current : { ...current, [focusId]: true });
    document.querySelector<HTMLElement>(`[data-group-id="${CSS.escape(focusId)}"]`)?.scrollIntoView({ block: "nearest", behavior: scrollBehavior() });
  }, [focusId, groups.length]);
  useEffect(() => {
    if (!expandedPrice) return;
    const element = Array.from(document.querySelectorAll<HTMLElement>("[data-price-row]")).find(node => node.dataset.priceRow === expandedPrice);
    element?.scrollIntoView({ block: "nearest", behavior: scrollBehavior() });
  }, [expandedPrice, groups.length]);

  const matchesFilter = (row: AgentModelRow) => {
    if (needle && !`${row.model.id} ${row.model.label}`.toLowerCase().includes(needle)) return false;
    if (showPrice && onlyUnpriced) { const price = priceOf(row); return !price || !price.rates; }
    return true;
  };
  const isDefaultRow = (row: AgentModelRow) => chatDefault?.credentialId === row.credentialId && chatDefault?.modelId === row.model.id;
  const isHiddenRow = (row: AgentModelRow) => isModelHidden(hidden, row.credentialId, row.model.id);
  const holdsPrice = (rows: AgentModelRow[]) => Boolean(expandedPrice) && rows.some(row => priceKey(rowIdentity(row)) === expandedPrice);
  const toggleIn = (setter: typeof setFullList, id: string) => setter(current => { const next = new Set(current); if (!next.delete(id)) next.add(id); return next; });
  const isOpen = (group: ModelGroup) => group.id === priceGroupId || (openChoice[group.id] ?? group.id === primaryGroupId);
  const toggleGroup = (group: ModelGroup) => {
    const open = isOpen(group);
    if (open && group.id === priceGroupId) onExpandPrice("");
    setOpenChoice(current => ({ ...current, [group.id]: !open }));
  };

  const renderRow = (row: AgentModelRow) => {
    const identity = rowIdentity(row);
    const key = priceKey(identity);
    const price = priceOf(row);
    const view = priceCellView(price, pricing.loading, pricing.error, en);
    const isDefault = isDefaultRow(row);
    const isHidden = isHiddenRow(row);
    const editing = expandedPrice === key;
    const fast = modelServiceTiers(row.model).length > 0;
    const reasoning = modelEffortLevels(row.model).length > 0;
    return (
      <div key={row.key} data-price-row={key} className="border-b border-cx-border-subtle last:border-b-0">
        <div className={cn("group/model flex flex-wrap items-center gap-x-2.5 gap-y-1 px-3 py-1.5", isHidden && "opacity-60")}>
          {showDefault ? <button type="button" aria-pressed={isDefault} title={isDefault ? "新对话默认模型" : "设为新对话默认模型"}
            aria-label={isDefault ? `${row.model.id} 是新对话默认模型` : `将 ${row.model.id} 设为新对话默认模型`}
            onClick={() => onSetDefault(row.credentialId, row.model.id)}
            className={cn("grid size-7 shrink-0 place-items-center rounded-lg outline-none transition-colors focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]", isDefault ? "text-cx-warning" : "text-cx-fg-4 hover:bg-cx-hover hover:text-cx-fg")}>
            <Icon name="star" size={15} filled={isDefault} />
          </button> : null}
          <div className="flex min-w-0 flex-1 basis-48 flex-wrap items-center gap-x-2 gap-y-1">
            <code className="truncate text-cx-xs text-cx-fg" title={row.model.label !== row.model.id ? row.model.label : undefined}>{row.model.id}</code>
            {row.verified ? <Tooltip content="已测通"><span tabIndex={0} aria-label="已测通" className="inline-flex outline-none"><Badge tone="success" icon="check">{null}</Badge></span></Tooltip> : null}
            {fast ? <Tooltip content="支持快速模式"><span tabIndex={0} aria-label="支持快速模式" className="inline-flex outline-none"><Badge tone="accent" icon="zap">{null}</Badge></span></Tooltip> : null}
            {reasoning ? <Tooltip content="支持推理强度"><span tabIndex={0} aria-label="支持推理强度" className="inline-flex outline-none"><Badge tone="accent" icon="brain">{null}</Badge></span></Tooltip> : null}
            {row.contextWindow ? (
              <Tooltip
                content={`最大上下文 ${row.contextWindow.toLocaleString()} tokens${row.maxOutputTokens ? ` · 最大输出 ${row.maxOutputTokens.toLocaleString()} tokens` : ""}`}
              >
                <span tabIndex={0} aria-label={`上下文 ${formatTokenCount(row.contextWindow)}`} className="inline-flex outline-none">
                  <Badge tone="neutral">{`${formatTokenCount(row.contextWindow)} 上下文`}</Badge>
                </span>
              </Tooltip>
            ) : null}
          </div>
          {showPrice ? <button type="button" aria-expanded={editing} aria-label={`${row.model.id} 价格：${view.text}`} onClick={() => onExpandPrice(editing ? "" : key)}
            className={cn("inline-flex max-w-full items-center gap-1.5 rounded-lg px-2 py-1 text-cx-xs outline-none transition-colors hover:bg-cx-hover focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]", TONE_CLASS[view.tone], editing && "bg-cx-hover")}>
            {view.tone === "warn" ? <Icon name="alert" size={13} className="shrink-0" /> : null}
            <span className="cx-tabular truncate">{view.text}</span>
            {view.source ? <span className="shrink-0 rounded bg-cx-hover px-1 text-cx-2xs text-cx-fg-3">{view.source}</span> : null}
            <Icon name="pencil" size={12} className={cn("shrink-0 transition-opacity", editing ? "opacity-50" : "opacity-0 group-hover/model:opacity-50 group-focus-within/model:opacity-50")} />
          </button> : null}
          <IconButton icon={isHidden ? "eyeOff" : "eye"} label={isHidden ? "在模型选择器中显示" : "在模型选择器中隐藏"} onClick={() => onToggleHidden(row.credentialId, row.model.id, !isHidden)} />
        </div>
        {showPrice && editing ? <PriceEditor identity={identity} price={price} catalog={catalog} credentialLabel={row.credentialName} onClose={() => onExpandPrice("")} /> : null}
      </div>
    );
  };
  const foldButton = (props: { expanded: boolean; controls: string; onClick: () => void; children: ReactNode }) => (
    <button type="button" aria-expanded={props.expanded} aria-controls={props.controls} onClick={props.onClick}
      className="flex w-full items-center gap-1.5 px-3 py-2 text-left text-cx-xs text-cx-fg-3 outline-none transition-colors hover:bg-cx-hover hover:text-cx-fg focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-[var(--cx-focus)]">
      <Icon name="chevronDown" size={13} className={cn("shrink-0 transition-transform", !props.expanded && "-rotate-90")} />
      {props.children}
    </button>
  );

  const shownGroups = groups.flatMap(group => {
    const matches = group.rows.filter(matchesFilter);
    return filtering && !matches.length ? [] : [{ group, matches }];
  });
  const groupActions = (group: ModelGroup) => <>
    <MenuItem icon="plug" disabled={!group.ready || group.testing} onSelect={() => onTest(group.id)}>测试连通</MenuItem>
    <MenuItem icon="refresh" disabled={!group.ready || refreshDisabled || group.refreshing} onSelect={() => onRefresh(group.id)}>刷新模型</MenuItem>
    {group.hasTestResult ? <MenuItem icon="terminal" onSelect={() => onViewTest(group.id)}>查看最近测试</MenuItem> : null}
    {group.canEdit ? <MenuItem icon="pencil" onSelect={() => onEdit(group.id)}>编辑接入方式</MenuItem> : null}
    {group.canImport ? <MenuItem icon="upload" onSelect={() => onImport(group.id)} description="供容器 Worker 使用">导入为独立凭据</MenuItem> : null}
    {group.canDelete ? <><MenuSeparator /><MenuItem icon="trash" danger onSelect={() => onDelete(group.id)}>删除</MenuItem></> : null}
  </>;
  const contextGroup = groups.find(group => group.id === contextGroupId);

  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center gap-2" data-settings-anchor="agents-credentials" id="setting-agents-credentials">
        {showSearch ? <SearchInput value={query} onValueChange={setQuery} placeholder="搜索模型…" aria-label="搜索模型" className="min-w-[160px] flex-1 sm:max-w-[280px]" /> : null}
        {showUnpricedFilter ? <Checkbox checked={onlyUnpriced} onCheckedChange={setOnlyUnpriced} label={`仅看未定价${unpriced ? `（${unpriced}）` : ""}`} /> : null}
        <span className="flex-1" />
        {toolbarActions}
      </div>
      {createPanel ? <section className="cx-settings-card" aria-label="新增接入方式">{createPanel}</section> : null}
      {!groups.length && !createPanel ? empty : null}
      {groups.length && filtering && !shownGroups.length ? (
        <div className="cx-settings-card px-4 py-5 text-center text-cx-xs text-cx-fg-3" role="status">没有匹配的模型。</div>
      ) : null}
      {shownGroups.map(({ group, matches }) => {
        const open = filtering || isOpen(group);
        const hiddenCount = group.rows.filter(isHiddenRow).length;
        const lead = [...matches.filter(isDefaultRow), ...matches.filter(row => !isDefaultRow(row) && !isHiddenRow(row))];
        const tucked = matches.filter(row => !isDefaultRow(row) && isHiddenRow(row));
        const long = !filtering && lead.length > COLLAPSE_AFTER;
        const showAllLead = !long || fullList.has(group.id) || holdsPrice(lead.slice(PREVIEW_ROWS));
        const showTucked = filtering || hiddenShown.has(group.id) || holdsPrice(tucked);
        const bodyId = `agents-group-${group.id}`;
        return (
          <section key={group.id} data-group-id={group.id} className="cx-settings-card" aria-label={group.name}>
            <div
              onContextMenu={event => { contextMenu.onContextMenu(event); if (event.defaultPrevented) setContextGroupId(group.id); }}
              onKeyDown={event => { contextMenu.onKeyDown(event); if (event.defaultPrevented) setContextGroupId(group.id); }}
              className="flex flex-wrap items-center gap-x-2 gap-y-1.5 border-b border-cx-border-subtle bg-cx-bg-subtle px-3 py-2.5 last:border-b-0">
              <button type="button" aria-expanded={open} aria-controls={bodyId} disabled={filtering} onClick={() => toggleGroup(group)}
                className="flex min-w-0 flex-1 basis-56 items-center gap-2 rounded-lg px-1 py-0.5 text-left outline-none focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)] disabled:cursor-default">
                <Icon name="chevronDown" size={15} className={cn("shrink-0 text-cx-fg-3 transition-transform", !open && "-rotate-90")} />
                <span className="min-w-0">
                  <span className="flex items-center gap-1.5 text-cx-sm font-medium text-cx-fg">
                    <span className="truncate">{group.name}</span>
                    {group.isDefaultSource ? <Icon name="star" size={12} filled className="shrink-0 text-cx-warning" aria-label="新对话默认来源" /> : null}
                    {["warn", "bad"].includes(group.statusKind) ? <StatusDot tone={statusTone(group.statusKind)} label={group.statusText} /> : null}
                  </span>
                  <span className="block truncate text-cx-xs leading-4 text-cx-fg-3">
                    {group.typeLabel} · {group.statusText} · {filtering ? `${matches.length} / ${group.rows.length} 个模型匹配` : `${group.summary}${hiddenCount ? ` · ${hiddenCount} 个已隐藏` : ""}`}
                  </span>
                </span>
              </button>
              <Tooltip content={group.detail}><span tabIndex={0} aria-label="接入方式详情" className="grid size-6 place-items-center rounded-md text-cx-fg-4 outline-none hover:text-cx-fg focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]"><Icon name="info" size={14} /></span></Tooltip>
              {open ? <>
                <Button size="xs" variant="outline" icon="plug" loading={group.testing} disabled={!group.ready} onClick={() => onTest(group.id)}>测试</Button>
                <Button size="xs" variant="outline" icon="refresh" loading={group.refreshing} disabled={!group.ready || refreshDisabled} onClick={() => onRefresh(group.id)}>刷新模型</Button>
              </> : group.testing || group.refreshing ? <span role="status" aria-label={group.testing ? "正在测试" : "正在刷新模型"} className="grid size-6 place-items-center text-cx-fg-3"><Spinner size={14} /></span> : null}
              <Menu trigger={<IconButton icon="more" label={`${group.name} 更多操作`} />} placement="bottom-end" ariaLabel={`${group.name} 操作`}>
                {groupActions(group)}
              </Menu>
            </div>
            {renderExtra?.(group)}
            {open ? <div id={bodyId}>
              {matches.length ? <>
                {(showAllLead ? lead : lead.slice(0, PREVIEW_ROWS)).map(renderRow)}
                {long ? <div className="border-b border-cx-border-subtle last:border-b-0">
                  {foldButton({ expanded: showAllLead, controls: bodyId, onClick: () => toggleIn(setFullList, group.id), children: showAllLead ? "收起" : `显示其余 ${lead.length - PREVIEW_ROWS} 个模型` })}
                </div> : null}
                {tucked.length ? <div>
                  {filtering ? <p className="px-3 pb-1 pt-2 text-cx-xs text-cx-fg-3">已在模型选择器中隐藏</p>
                    : foldButton({ expanded: showTucked, controls: `${bodyId}-hidden`, onClick: () => toggleIn(setHiddenShown, group.id), children: `已在模型选择器中隐藏 · ${tucked.length}` })}
                  {showTucked ? <div id={`${bodyId}-hidden`} className="border-t border-cx-border-subtle">{tucked.map(renderRow)}</div> : null}
                </div> : null}
              </> : (
                <div className="flex flex-wrap items-center justify-center gap-2 px-4 py-5 text-center text-cx-xs text-cx-fg-3">
                  <span>{group.ready ? "尚无模型，点击“刷新模型”从该接入方式读取。" : group.isSystem ? "未检测到本机登录，请先在终端完成登录后点“探测”。" : "该接入方式暂不可用，请检查凭据。"}</span>
                  {!group.ready && group.isSystem && loginCommand ? <><code className="rounded-md bg-cx-code px-2 py-1 text-cx-xs text-cx-fg">{loginCommand}</code><CopyButton text={loginCommand} label="复制登录命令" /></> : null}
                </div>
              )}
            </div> : null}
          </section>
        );
      })}
      {contextGroup ? contextMenu.render(groupActions(contextGroup), `${contextGroup.name} 操作`) : null}
    </div>
  );
}
