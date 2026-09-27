"use client";

import { Button, Input, Popover, Tooltip } from "@heroui/react";
import { type KeyboardEvent, type ReactNode } from "react";

import { Icon } from "@/components/Icon";
import type { CollaborationRelationKind, CollaborationScope } from "@/lib/agentCollaboration";
import type { CollabCanvasLayout } from "@/lib/agentCollaborationLayout";
import { useT } from "@/lib/i18n";

import { RELATION_KINDS, relationLabel } from "./collaborationPresentation";
import { RelationSwatch } from "./RelationSwatch";

function IconTool({
  label,
  className,
  pressed,
  onClick,
  children,
}: {
  label: string;
  className: string;
  pressed?: boolean;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <Tooltip delay={280} closeDelay={60}>
      <Tooltip.Trigger className="collab-icon-tool" role="presentation" tabIndex={-1}>
        <Button
          variant="secondary"
          className={className}
          aria-label={label}
          aria-pressed={pressed}
          onClick={onClick}
        >
          {children}
        </Button>
      </Tooltip.Trigger>
      <Tooltip.Content className="collab-toolbar-tip">{label}</Tooltip.Content>
    </Tooltip>
  );
}

/**
 * Full-width control strip above the canvas and both drawers.
 * Index toggle on the left; filters in the middle; view actions and the detail
 * toggle on the right. Below 1080px the trailing group folds into "more".
 */
export function CollaborationToolbar({
  query,
  onQueryChange,
  searchHits,
  onCycleMatch,
  scope,
  onScopeChange,
  onlyAnomalies,
  onToggleAnomalies,
  anomalyCount,
  recentOnly,
  onToggleRecent,
  relationKinds,
  onToggleRelation,
  onResetRelations,
  flowView,
  onToggleFlow,
  followNew,
  onToggleFollow,
  layoutMode,
  onLayoutChange,
  onExportJson,
  onExportPng,
  indexOpen,
  onToggleIndex,
  detailOpen,
  onToggleDetail,
  onFit,
}: {
  query: string;
  onQueryChange: (value: string) => void;
  searchHits?: string;
  onCycleMatch?: (delta: number) => void;
  scope: CollaborationScope;
  onScopeChange: (scope: CollaborationScope) => void;
  onlyAnomalies: boolean;
  onToggleAnomalies: () => void;
  anomalyCount: number;
  recentOnly: boolean;
  onToggleRecent: () => void;
  relationKinds: Set<CollaborationRelationKind>;
  onToggleRelation: (kind: CollaborationRelationKind) => void;
  onResetRelations: () => void;
  flowView: boolean;
  onToggleFlow: () => void;
  followNew: boolean;
  onToggleFollow: () => void;
  layoutMode: CollabCanvasLayout;
  onLayoutChange: (layout: CollabCanvasLayout) => void;
  onExportJson: () => void;
  onExportPng: () => void;
  indexOpen: boolean;
  onToggleIndex: () => void;
  detailOpen: boolean;
  onToggleDetail: () => void;
  onFit: () => void;
}) {
  const t = useT();
  const relationTotal = RELATION_KINDS.length;
  const relationsFiltered = relationKinds.size < relationTotal;
  const relationsLabel = relationsFiltered
    ? t("collab.relationsFiltered", { n: relationKinds.size, total: relationTotal })
    : t("collab.relations");
  const indexLabel = t(indexOpen ? "collab.hideIndex" : "collab.showIndex");
  const detailLabel = t(detailOpen ? "collab.hideDetails" : "collab.showDetails");
  return (
    <div className="collab-toolbar">
      {/* Left drawer control — leading edge of the bar. */}
      <IconTool className="collab-tool collab-panel-toggle collab-panel-toggle-index icon-only" label={indexLabel} pressed={indexOpen} onClick={onToggleIndex}>
        <Icon name="panelLeft" size={14} />
      </IconTool>
      <div className="collab-toolbar-search">
        <Icon name="search" size={14} />
        <Input
          value={query}
          onChange={(event) => onQueryChange(event.target.value)}
          onKeyDown={(event: KeyboardEvent<HTMLInputElement>) => {
            if (event.nativeEvent.isComposing) return;
            if (event.key === "Escape") {
              if (!query) return;
              event.preventDefault();
              event.stopPropagation();
              onQueryChange("");
              return;
            }
            if (event.key !== "Enter" || !query.trim()) return;
            event.preventDefault();
            event.stopPropagation();
            onCycleMatch?.(event.shiftKey ? -1 : 1);
          }}
          placeholder={t("collab.searchShort")}
          aria-label={t("collab.search")}
        />
        {query && (
          <Button isIconOnly variant="ghost" aria-label={t("common.clear")} onClick={() => onQueryChange("")}>
            <Icon name="x" size={12} />
          </Button>
        )}
        {searchHits && <span className="collab-search-hits">{searchHits}</span>}
      </div>
      <div
        className="collab-scope"
        role="radiogroup"
        aria-label={t("collab.scope")}
        onKeyDown={(event: KeyboardEvent<HTMLDivElement>) => {
          if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
          event.preventDefault();
          event.stopPropagation();
          onScopeChange(scope === "current" ? "all" : "current");
          requestAnimationFrame(() => {
            event.currentTarget.querySelector<HTMLElement>("[role='radio'][aria-checked='true']")?.focus();
          });
        }}
      >
        <button
          type="button"
          className={scope === "current" ? "on" : ""}
          role="radio"
          aria-checked={scope === "current"}
          tabIndex={scope === "current" ? 0 : -1}
          onClick={() => onScopeChange("current")}
        >
          {t("collab.scope.current")}
        </button>
        <button
          type="button"
          className={scope === "all" ? "on" : ""}
          role="radio"
          aria-checked={scope === "all"}
          tabIndex={scope === "all" ? 0 : -1}
          onClick={() => onScopeChange("all")}
        >
          {t("collab.scope.all")}
        </button>
      </div>
      <div
        className="collab-scope"
        role="radiogroup"
        aria-label={t("collab.layout")}
        onKeyDown={(event: KeyboardEvent<HTMLDivElement>) => {
          if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
          event.preventDefault();
          event.stopPropagation();
          onLayoutChange(layoutMode === "hierarchy" ? "timeline" : "hierarchy");
          requestAnimationFrame(() => {
            event.currentTarget.querySelector<HTMLElement>("[role='radio'][aria-checked='true']")?.focus();
          });
        }}
      >
        <button
          type="button"
          className={layoutMode === "hierarchy" ? "on" : ""}
          role="radio"
          aria-checked={layoutMode === "hierarchy"}
          tabIndex={layoutMode === "hierarchy" ? 0 : -1}
          onClick={() => onLayoutChange("hierarchy")}
        >
          {t("collab.layout.hierarchy")}
        </button>
        <button
          type="button"
          className={layoutMode === "timeline" ? "on" : ""}
          role="radio"
          aria-checked={layoutMode === "timeline"}
          tabIndex={layoutMode === "timeline" ? 0 : -1}
          onClick={() => onLayoutChange("timeline")}
        >
          {t("collab.layout.timeline")}
        </button>
      </div>
      <Button
        variant="secondary"
        className={`collab-tool ${onlyAnomalies ? "on" : ""} ${anomalyCount > 0 ? "danger" : ""}`}
        aria-pressed={onlyAnomalies}
        aria-label={anomalyCount > 0 ? `${t("collab.anomalies")} · ${t("collab.tabAnomalyCount", { n: anomalyCount })}` : t("collab.anomalies")}
        onClick={onToggleAnomalies}
      >
        <Icon name="alert" size={13} /><span className="collab-tool-label">{t("collab.anomalies")}</span>
        <b className="collab-tool-badge">{anomalyCount}</b>
      </Button>
      <Button
        variant="secondary"
        className={`collab-tool ${recentOnly ? "on" : ""}`}
        aria-pressed={recentOnly}
        aria-label={t("collab.recentOnly")}
        onClick={onToggleRecent}
      >
        <Icon name="radio" size={13} /><span className="collab-tool-label">{t("collab.recentOnly")}</span>
      </Button>
      <Popover>
        <Popover.Trigger className={`collab-tool ${relationsFiltered ? "on" : ""}`} aria-label={relationsLabel} title={relationsLabel}>
          <Icon name="network" size={13} />
          <span className="collab-tool-label">{t("collab.relations")}</span>
          {relationsFiltered && <b className="collab-tool-badge">{relationKinds.size}/{relationTotal}</b>}
          <span className="collab-tool-caret" aria-hidden="true"><Icon name="chevronDown" size={12} /></span>
        </Popover.Trigger>
        <Popover.Content className="collab-relation-menu" placement="bottom">
          <Popover.Arrow />
          <Popover.Dialog aria-label={t("collab.relations")}>
            {RELATION_KINDS.map((kind) => {
              const on = relationKinds.has(kind);
              return (
                <Button key={kind} className={on ? "on" : ""} aria-pressed={on} onClick={() => onToggleRelation(kind)}>
                  <RelationSwatch kind={kind} />
                  <span>{relationLabel(kind, t)}</span>
                  <Icon name={on ? "check" : "minus"} size={12} />
                </Button>
              );
            })}
            {relationsFiltered && (
              <Button className="collab-relation-reset" onClick={onResetRelations}>
                <Icon name="eye" size={12} />
                <span>{t("collab.showAllRelations")}</span>
              </Button>
            )}
          </Popover.Dialog>
        </Popover.Content>
      </Popover>
      <Popover>
        <Popover.Trigger className="collab-tool icon-only export" aria-label={t("collab.export")} title={t("collab.export")}>
          <Icon name="download" size={14} />
        </Popover.Trigger>
        <Popover.Content className="collab-relation-menu collab-export-menu" placement="bottom">
          <Popover.Arrow />
          <Popover.Dialog aria-label={t("collab.export")}>
            <Button onClick={onExportJson}>
              <Icon name="file" size={13} />
              <span>{t("collab.export.json")}</span>
            </Button>
            <Button onClick={onExportPng}>
              <Icon name="download" size={13} />
              <span>{t("collab.export.png")}</span>
            </Button>
          </Popover.Dialog>
        </Popover.Content>
      </Popover>
      <span className="collab-toolbar-divider" aria-hidden="true" />
      <IconTool
        className={`collab-tool icon-only flow ${flowView ? "on" : ""}`}
        label={t("collab.flow")}
        pressed={flowView}
        onClick={onToggleFlow}
      >
        <Icon name="grid" size={14} />
      </IconTool>
      <IconTool className="collab-tool icon-only fit" label={t("collab.fit")} onClick={onFit}>
        <Icon name="crosshair" size={14} />
      </IconTool>
      <IconTool
        className={`collab-tool icon-only follow ${followNew ? "on" : ""}`}
        label={t("collab.followNew")}
        pressed={followNew}
        onClick={onToggleFollow}
      >
        <Icon name="target" size={14} />
      </IconTool>
      {/* Right drawer control — trailing edge, opposite the index toggle. */}
      <div className="collab-toolbar-panels" role="group" aria-label={t("collab.panels")}>
        <IconTool className="collab-tool collab-panel-toggle collab-panel-toggle-detail icon-only" label={detailLabel} pressed={detailOpen} onClick={onToggleDetail}>
          <Icon name="panel" size={14} />
        </IconTool>
      </div>
      {/* Shown only when the container breakpoint hides the view group above. */}
      <Popover>
        <Popover.Trigger className="collab-tool icon-only collab-toolbar-more" aria-label={t("collab.more")} title={t("collab.more")}>
          <Icon name="more" size={14} />
        </Popover.Trigger>
        <Popover.Content className="collab-relation-menu collab-more-menu" placement="bottom end">
          <Popover.Arrow />
          <Popover.Dialog aria-label={t("collab.more")}>
            <Button className={flowView ? "on" : ""} aria-pressed={flowView} onClick={onToggleFlow}>
              <Icon name="grid" size={13} />
              <span>{t("collab.flow")}</span>
              <Icon name={flowView ? "check" : "minus"} size={12} />
            </Button>
            <Button className={layoutMode === "hierarchy" ? "on" : ""} aria-pressed={layoutMode === "hierarchy"} onClick={() => onLayoutChange("hierarchy")}>
              <Icon name="layers" size={13} />
              <span>{t("collab.layout.hierarchy")}</span>
              <Icon name={layoutMode === "hierarchy" ? "check" : "minus"} size={12} />
            </Button>
            <Button className={layoutMode === "timeline" ? "on" : ""} aria-pressed={layoutMode === "timeline"} onClick={() => onLayoutChange("timeline")}>
              <Icon name="clock" size={13} />
              <span>{t("collab.layout.timeline")}</span>
              <Icon name={layoutMode === "timeline" ? "check" : "minus"} size={12} />
            </Button>
            <Button onClick={onExportJson}>
              <Icon name="file" size={13} />
              <span>{t("collab.export.json")}</span>
            </Button>
            <Button onClick={onExportPng}>
              <Icon name="download" size={13} />
              <span>{t("collab.export.png")}</span>
            </Button>
            <Button onClick={onFit}>
              <Icon name="crosshair" size={13} />
              <span>{t("collab.fit")}</span>
            </Button>
            <Button className={followNew ? "on" : ""} aria-pressed={followNew} onClick={onToggleFollow}>
              <Icon name="target" size={13} />
              <span>{t("collab.followNew")}</span>
              <Icon name={followNew ? "check" : "minus"} size={12} />
            </Button>
            <Button className={indexOpen ? "on" : ""} aria-pressed={indexOpen} onClick={onToggleIndex}>
              <Icon name="panelLeft" size={13} />
              <span>{indexLabel}</span>
              <Icon name={indexOpen ? "check" : "minus"} size={12} />
            </Button>
            <Button className={detailOpen ? "on" : ""} aria-pressed={detailOpen} onClick={onToggleDetail}>
              <Icon name="panel" size={13} />
              <span>{detailLabel}</span>
              <Icon name={detailOpen ? "check" : "minus"} size={12} />
            </Button>
          </Popover.Dialog>
        </Popover.Content>
      </Popover>
    </div>
  );
}
