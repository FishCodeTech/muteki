"use client";

import type { CSSProperties, ReactNode } from "react";
import type {
  ArtifactViewerContribution,
  BoardContribution,
  CommandFormContribution,
  NavigationContribution,
  StatusLabelContribution,
} from "./types";
import { STATUS_COLORS } from "./types";
import { FieldListForm } from "./SchemaForm";

/**
 * EXT-02：声明式 UI Contribution renderer。
 *
 * 每个贡献种类一个纯渲染组件，数据（projection 内容、提交回调）由宿主页面
 * 注入；渲染器自身不取数、不 eval、不加载第三方 React 代码。`iframe` 种类的
 * Artifact viewer 用 ``sandbox=""``（无 allow-scripts）呈现 projection 文本，
 * 复杂界面只能以这种受控方式或独立页面（导航路由）出现。
 */

const muted: CSSProperties = { color: "var(--muted)", fontSize: 12 };
const mono: CSSProperties = { fontFamily: "var(--font-mono)", fontSize: 11 };

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
  const color = STATUS_COLORS[spec.color ?? "muted"] ?? "var(--muted)";
  return (
    <span
      style={{
        display: "inline-flex",
        alignItems: "center",
        height: 20,
        padding: "0 7px",
        borderRadius: 999,
        border: `1px solid color-mix(in srgb, ${color} 34%, var(--line))`,
        background: `color-mix(in srgb, ${color} 9%, transparent)`,
        color,
        fontSize: 10.5,
        fontWeight: 700,
      }}
    >
      {spec.text ?? value}
    </span>
  );
}

export function NavigationItems({ items }: { items: NavigationContribution[] }) {
  if (!items.length) return null;
  return (
    <div style={{ display: "flex", alignItems: "center", gap: 10, flexWrap: "wrap" }}>
      <span style={muted}>扩展贡献的导航：</span>
      {items.map((item) => (
        <a
          key={item.id}
          href={item.route}
          style={{
            ...muted,
            color: "var(--blue)",
            textDecoration: "none",
            border: "1px solid var(--line2)",
            borderRadius: 8,
            padding: "4px 10px",
          }}
        >
          {item.title}（{item.route}）
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
    <div
      style={{
        border: "1px dashed var(--line2)",
        borderRadius: 10,
        padding: 10,
        display: "grid",
        gap: 6,
      }}
    >
      <div style={{ display: "flex", alignItems: "center", gap: 8, flexWrap: "wrap" }}>
        <strong style={{ color: "var(--bright)", fontSize: 12.5 }}>{form.title}</strong>
        <span style={{ ...mono, color: "var(--dim)" }}>{form.command_type}</span>
      </div>
      {form.description ? <div style={muted}>{form.description}</div> : null}
      <FieldListForm
        fields={form.fields}
        submitLabel="执行"
        busy={busy}
        onSubmit={(params) => onInvoke(form.command_type, params)}
      />
      {result !== undefined ? (
        <pre
          style={{
            ...mono,
            margin: 0,
            padding: 8,
            borderRadius: 8,
            background: "var(--panel)",
            color: "var(--text)",
            whiteSpace: "pre-wrap",
            wordBreak: "break-all",
          }}
        >
          {typeof result === "string" ? result : JSON.stringify(result, null, 2)}
        </pre>
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
  // 未声明 items_field：把整个 projection data 当作单条目。
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
    <div style={{ display: "flex", gap: 10, alignItems: "flex-start", flexWrap: "wrap" }}>
      {board.columns.map((col) => {
        const columnItems = items.filter((item) =>
          col.statuses.includes(String(item[board.status_field] ?? "")),
        );
        return (
          <div
            key={col.id}
            style={{
              minWidth: 180,
              flex: 1,
              border: "1px solid var(--line)",
              borderRadius: 10,
              background: "var(--panel)",
              padding: 10,
              display: "grid",
              gap: 8,
              alignContent: "start",
            }}
          >
            <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
              <strong style={{ color: "var(--bright)", fontSize: 12 }}>{col.title}</strong>
              <span style={muted}>{columnItems.length}</span>
            </div>
            {columnItems.length ? (
              columnItems.map((item, index) => (
                <div
                  key={index}
                  style={{
                    border: "1px solid var(--line2)",
                    borderRadius: 8,
                    background: "var(--panel2)",
                    padding: "8px 10px",
                    display: "grid",
                    gap: 4,
                  }}
                >
                  <div style={{ display: "flex", alignItems: "center", gap: 8, flexWrap: "wrap" }}>
                    <strong style={{ ...mono, color: "var(--bright)", fontSize: 12 }}>
                      {String(item[board.card.title_field] ?? "（无标题）")}
                    </strong>
                    {board.card.badge_field ? (
                      <StatusLabel
                        value={String(item[board.card.badge_field] ?? "")}
                        contribution={board.status_labels}
                        fallback={String(item[board.card.badge_field] ?? "")}
                      />
                    ) : null}
                  </div>
                  {board.card.subtitle_field ? (
                    <span style={muted}>{String(item[board.card.subtitle_field] ?? "")}</span>
                  ) : null}
                </div>
              ))
            ) : (
              <span style={muted}>暂无条目</span>
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
    <div
      style={{
        border: "1px solid var(--line)",
        borderRadius: 10,
        background: "var(--panel)",
        padding: 10,
        display: "grid",
        gap: 6,
      }}
    >
      <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
        <strong style={{ color: "var(--bright)", fontSize: 12 }}>{viewer.title}</strong>
        <span style={{ ...mono, color: "var(--dim)" }}>
          {viewer.kind}
          {viewer.projection ? ` · projection:${viewer.projection}` : ""}
        </span>
      </div>
      {viewer.kind === "iframe" ? (
        // 受控沙箱：无 allow-scripts，内容只来自公开 API 的 projection 数据。
        <iframe
          title={viewer.title}
          sandbox=""
          style={{
            width: "100%",
            minHeight: 120,
            border: "1px solid var(--line2)",
            borderRadius: 8,
            background: "var(--panel2)",
          }}
          srcDoc={`<pre style="font:12px/1.5 monospace;color:#c8d0e0;padding:8px;white-space:pre-wrap;">${text
            .replace(/&/g, "&amp;")
            .replace(/</g, "&lt;")}</pre>`}
        />
      ) : (
        <pre
          style={{
            ...mono,
            margin: 0,
            padding: 8,
            borderRadius: 8,
            background: "var(--panel2)",
            color: "var(--text)",
            whiteSpace: "pre-wrap",
            wordBreak: "break-all",
          }}
        >
          {viewer.kind === "json" ? text : text}
        </pre>
      )}
    </div>
  );
}
