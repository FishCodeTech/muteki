"use client";

import type { ReactNode } from "react";
import type {
  ArtifactViewerContribution,
  BoardContribution,
  CommandFormContribution,
  NavigationContribution,
  StatusLabelContribution,
} from "./types";
import { FieldListForm } from "./SchemaForm";


const TONES: Record<string, string> = { green: "success", amber: "warning", red: "danger" };

export function StatusLabel({
  value,
  contribution,
  fallback,
}: {
  value: string;
  contribution?: StatusLabelContribution;
  fallback?: ReactNode;
}) {
  const spec = contribution?.labels?.[value];
  if (!spec) return <>{fallback ?? value}</>;
  return <span className="cx-uic-chip" data-tone={TONES[spec.color ?? "muted"] ?? "neutral"}>{spec.text ?? value}</span>;
}

export function NavigationItems({ items }: { items: NavigationContribution[] }) {
  if (!items.length) return null;
  return (
    <div className="cx-uic-nav">
      <span className="cx-uic-label">扩展贡献的导航</span>
      {items.map((item) => (
        <a key={item.id} href={item.route} className="cx-uic-nav-link">
          {item.title}<code>{item.route}</code>
        </a>
      ))}
    </div>
  );
}

export function CommandFormView({
  form,
  busy,
  onInvoke,
  result,
}: {
  form: CommandFormContribution;
  busy?: boolean;
  onInvoke: (commandType: string, params: Record<string, unknown>) => void;
  result?: unknown;
}) {
  return (
    <div className="cx-uic-block">
      <div className="cx-uic-block-head">
        <strong>{form.title}</strong>
        {form.command_type ? <code>{form.command_type}</code> : null}
      </div>
      {form.description ? <p className="cx-uic-muted">{form.description}</p> : null}
      <FieldListForm
        fields={form.fields}
        submitLabel="执行"
        busy={busy}
        onSubmit={(params) => onInvoke(form.command_type, params)}
      />
      {result !== undefined ? (
        <pre className="cx-uic-pre">{typeof result === "string" ? result : JSON.stringify(result, null, 2)}</pre>
      ) : null}
    </div>
  );
}

function itemsOf(board: BoardContribution, data: unknown): Record<string, unknown>[] {
  if (data === null || typeof data !== "object") return [];
  const container = data as Record<string, unknown>;
  if (board.items_field) {
    const raw = container[board.items_field];
    return Array.isArray(raw)
      ? raw.filter((x): x is Record<string, unknown> => x !== null && typeof x === "object")
      : [];
  }
  return [container];
}

export function BoardView({
  board,
  data,
}: {
  board: BoardContribution;
  data: unknown;
}) {
  const items = itemsOf(board, data);
  return (
    <div className="cx-uic-board">
      {board.columns.map((col) => {
        const columnItems = items.filter((item) =>
          col.statuses.includes(String(item[board.status_field] ?? "")),
        );
        return (
          <div key={col.id} className="cx-uic-board-col">
            <div className="cx-uic-block-head">
              <strong>{col.title}</strong>
              <span className="cx-uic-muted">{columnItems.length}</span>
            </div>
            {columnItems.length ? (
              columnItems.map((item, index) => (
                <div key={index} className="cx-uic-board-card">
                  <div className="cx-uic-block-head">
                    <code className="cx-uic-strong-code">{String(item[board.card.title_field] ?? "（无标题）")}</code>
                    {board.card.badge_field ? (
                      <StatusLabel
                        value={String(item[board.card.badge_field] ?? "")}
                        contribution={board.status_labels}
                        fallback={String(item[board.card.badge_field] ?? "")}
                      />
                    ) : null}
                  </div>
                  {board.card.subtitle_field ? (
                    <span className="cx-uic-muted">{String(item[board.card.subtitle_field] ?? "")}</span>
                  ) : null}
                </div>
              ))
            ) : (
              <span className="cx-uic-muted">暂无条目</span>
            )}
          </div>
        );
      })}
    </div>
  );
}

function viewerContent(viewer: ArtifactViewerContribution, data: unknown): unknown {
  if (viewer.field && data !== null && typeof data === "object") {
    return (data as Record<string, unknown>)[viewer.field];
  }
  return data;
}

export function ArtifactViewerView({
  viewer,
  data,
}: {
  viewer: ArtifactViewerContribution;
  data: unknown;
}) {
  const content = viewerContent(viewer, data);
  const text =
    typeof content === "string" ? content : JSON.stringify(content, null, 2);
  return (
    <div className="cx-uic-block">
      <div className="cx-uic-block-head">
        <strong>{viewer.title}</strong>
        <code>
          {viewer.kind}
          {viewer.projection ? ` · projection:${viewer.projection}` : ""}
        </code>
      </div>
      {viewer.kind === "iframe" ? (
        <iframe
          title={viewer.title}
          sandbox=""
          className="cx-uic-frame"
          srcDoc={`<pre style="margin:0;font:12px/1.5 monospace;color:#7c8594;padding:8px;white-space:pre-wrap;">${text
            .replace(/&/g, "&amp;")
            .replace(/</g, "&lt;")}</pre>`}
        />
      ) : (
        <pre className="cx-uic-pre">{text}</pre>
      )}
    </div>
  );
}
