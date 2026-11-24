"use client";

import { useState, type KeyboardEvent, type ReactNode } from "react";
import { Button, Icon, Input, Popover, Tooltip } from "@muteki/ui";
import type { CollaborationRelationKind, CollaborationScope } from "@/lib/agentCollaboration";
import type { CollabCanvasLayout } from "@/lib/agentCollaborationLayout";
import { useT } from "@/lib/i18n";
import { apiFetch } from "@/lib/serviceAuth";

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
    <span className="collab-icon-tool">
      <Tooltip content={label} delay={280} closeDelay={60} className="collab-toolbar-tip">
        <Button
          variant="secondary"
          className={className}
          aria-label={label}
          aria-pressed={pressed}
          onClick={onClick}
        >
          {children}
        </Button>
      </Tooltip>
    </span>
  );
}

export function CollaborationToolbar({
  runId,
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
  runId?: string;
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
  const [exporting, setExporting] = useState(false);
  const [exportError, setExportError] = useState("");
  const exportGraph = async (format: "jsonl" | "timeline" | "sqlite") => {
    if (!runId || exporting) return;
    setExporting(true);
    setExportError("");
    try {
      const response = await apiFetch(`/api/runs/${encodeURIComponent(runId)}/graph/export?format=${format}`);
      if (!response.ok) {
        const detail = await response.text();
        throw new Error(detail || `HTTP ${response.status}`);
      }
      const url = URL.createObjectURL(await response.blob());
      const link = document.createElement("a");
      link.href = url;
      link.download = `${runId}-graph.${format === "timeline" ? "txt" : format}`;
      link.click();
      setTimeout(() => URL.revokeObjectURL(url), 1000);
    } catch (error) {
      setExportError(error instanceof Error ? error.message : String(error));
    } finally {
      setExporting(false);
    }
  };
  const relationTotal = RELATION_KINDS.length;
  const relationsFiltered = relationKinds.size < relationTotal;
  const relationsLabel = relationsFiltered
    ? t("collab.relationsFiltered", { n: relationKinds.size, total: relationTotal })
    : t("collab.relations");
  const indexLabel = t(indexOpen ? "collab.hideIndex" : "collab.showIndex");
  const detailLabel = t(detailOpen ? "collab.hideDetails" : "collab.showDetails");
  const viewAdjusted = scope !== "all" || layoutMode !== "hierarchy" || recentOnly || flowView;
  return (
    <div className="collab-toolbar">
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
          <Button iconOnly variant="ghost" aria-label={t("common.clear")} onClick={() => onQueryChange("")}>
            <Icon name="x" size={12} />
          </Button>
        )}
        {searchHits && <span className="collab-search-hits">{searchHits}</span>}
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
      <Popover
        placement="bottom"
        className="collab-relation-menu"
        ariaLabel={t("collab.relations")}
        trigger={
          <button type="button" className={`collab-tool ${relationsFiltered ? "on" : ""}`} aria-label={relationsLabel} title={relationsLabel}>
            <Icon name="network" size={13} />
            <span className="collab-tool-label">{t("collab.relations")}</span>
            {relationsFiltered && <b className="collab-tool-badge">{relationKinds.size}/{relationTotal}</b>}
            <span className="collab-tool-caret" aria-hidden="true"><Icon name="chevronDown" size={12} /></span>
          </button>
        }
      >
        <div role="dialog" aria-label={t("collab.relations")}>
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
        </div>
      </Popover>
      <Popover
        placement="bottom-end"
        className="collab-relation-menu collab-view-menu-pop"
        ariaLabel={t("collab.viewMenu")}
        trigger={
          <button type="button" className={`collab-tool collab-view-menu ${viewAdjusted ? "on" : ""}`} aria-label={t("collab.viewMenu")} title={t("collab.viewMenu")}>
            <Icon name="sliders" size={13} />
            <span className="collab-tool-label">{t("collab.viewMenu")}</span>
            <span className="collab-tool-caret" aria-hidden="true"><Icon name="chevronDown" size={12} /></span>
          </button>
        }
      >
        <div role="dialog" aria-label={t("collab.viewMenu")}>
          <span className="collab-menu-label">{t("collab.scope")}</span>
          {(["current", "all"] as const).map((value) => (
            <Button key={value} className={scope === value ? "on" : ""} aria-pressed={scope === value} onClick={() => onScopeChange(value)}>
              <Icon name={value === "current" ? "play" : "history"} size={13} />
              <span>{t(`collab.scope.${value}`)}</span>
              <Icon name={scope === value ? "check" : "minus"} size={12} />
            </Button>
          ))}
          <span className="collab-menu-label">{t("collab.layout")}</span>
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
          <span className="collab-menu-label">{t("collab.viewOptions")}</span>
          <Button className={recentOnly ? "on" : ""} aria-pressed={recentOnly} onClick={onToggleRecent}>
            <Icon name="radio" size={13} />
            <span>{t("collab.recentOnly")}</span>
            <Icon name={recentOnly ? "check" : "minus"} size={12} />
          </Button>
          <Button className={flowView ? "on" : ""} aria-pressed={flowView} onClick={onToggleFlow}>
            <Icon name="grid" size={13} />
            <span>{t("collab.flow")}</span>
            <Icon name={flowView ? "check" : "minus"} size={12} />
          </Button>
          <Button className={followNew ? "on" : ""} aria-pressed={followNew} onClick={onToggleFollow}>
            <Icon name="target" size={13} />
            <span>{t("collab.followNew")}</span>
            <Icon name={followNew ? "check" : "minus"} size={12} />
          </Button>
          <Button onClick={onFit}>
            <Icon name="crosshair" size={13} />
            <span>{t("collab.fit")}</span>
          </Button>
          <span className="collab-menu-label">{t("collab.panels")}</span>
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
          <span className="collab-menu-label">{t("collab.export")}</span>
          {runId && (["jsonl", "timeline", "sqlite"] as const).map((format) => (
            <Button key={format} disabled={exporting} onClick={() => void exportGraph(format)}>
              <Icon name="download" size={13} />
              <span>{t(`collab.export.graph.${format}`)}</span>
            </Button>
          ))}
          {exportError && <p className="tone-danger" role="alert">{exportError}</p>}
          <Button onClick={onExportJson}>
            <Icon name="file" size={13} />
            <span>{t("collab.export.json")}</span>
          </Button>
          <Button onClick={onExportPng}>
            <Icon name="image" size={13} />
            <span>{t("collab.export.png")}</span>
          </Button>
        </div>
      </Popover>
      <div className="collab-toolbar-panels" role="group" aria-label={t("collab.panels")}>
        <IconTool className="collab-tool collab-panel-toggle collab-panel-toggle-detail icon-only" label={detailLabel} pressed={detailOpen} onClick={onToggleDetail}>
          <Icon name="panel" size={14} />
        </IconTool>
      </div>
    </div>
  );
}
