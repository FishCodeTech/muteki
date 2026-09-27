/**
 * Collaboration export: JSON of the model, and a schematic PNG drawn on a
 * canvas. PNG is schematic (no html-to-image) so it stays dependency-free.
 */

import { getNodesBounds, type Edge, type Node } from "@xyflow/react";

import type { AgentCollaborationModel, CollaborationScope } from "./agentCollaboration";
import { LAYOUT } from "./agentCollaborationLayout";
import type { DeckState } from "./events";

const RELATION_COLOR: Record<string, string> = {
  dispatch: "var(--blue)",
  handoff: "var(--blue)",
  review: "var(--violet)",
  verify: "var(--green)",
  report: "var(--amber)",
  directive: "var(--magenta)",
  lock: "var(--red)",
};

function downloadHref(href: string, filename: string): void {
  const anchor = document.createElement("a");
  anchor.href = href;
  anchor.download = filename;
  anchor.click();
}

function resolveColor(expr: string): string {
  const probe = document.createElement("span");
  probe.style.color = expr;
  document.body.appendChild(probe);
  const color = getComputedStyle(probe).color;
  probe.remove();
  return color;
}

function absBox(node: Node, byId: Map<string, Node>): { x: number; y: number; w: number; h: number } {
  let x = node.position.x;
  let y = node.position.y;
  let parent = node.parentId ? byId.get(node.parentId) : undefined;
  while (parent) {
    x += parent.position.x;
    y += parent.position.y;
    parent = parent.parentId ? byId.get(parent.parentId) : undefined;
  }
  return {
    x,
    y,
    w: node.measured?.width ?? node.width ?? LAYOUT.nodeWidth,
    h: node.measured?.height ?? node.height ?? LAYOUT.nodeHeight,
  };
}

export function collaborationExportPayload(
  deck: DeckState,
  model: AgentCollaborationModel,
  scope: CollaborationScope,
  layout: string,
) {
  return {
    runId: deck.runId,
    exportedAt: Date.now(),
    scope,
    layout,
    agents: model.allAgents.map((agent) => {
      const rest = { ...agent } as Omit<typeof agent, "presentation"> & { presentation?: typeof agent.presentation };
      delete rest.presentation;
      return rest;
    }),
    relations: model.relations,
    knowledge: model.knowledge,
  };
}

export function downloadCollaborationJson(
  deck: DeckState,
  model: AgentCollaborationModel,
  scope: CollaborationScope,
  layout: string,
): void {
  const payload = collaborationExportPayload(deck, model, scope, layout);
  const blob = new Blob([JSON.stringify(payload, null, 2)], { type: "application/json" });
  const url = URL.createObjectURL(blob);
  downloadHref(url, `${deck.runId || "run"}-collaboration.json`);
  URL.revokeObjectURL(url);
}

export function downloadCollaborationPng(
  nodes: Node[],
  edges: Edge[],
  filename: string,
): void {
  const exportable = nodes.filter((node) => node.type !== "group");
  const bounds = getNodesBounds(exportable);
  const pad = 48;
  const width = Math.ceil(bounds.width + pad * 2);
  const height = Math.ceil(bounds.height + pad * 2);
  const byId = new Map(nodes.map((node) => [node.id, node]));
  const bg = resolveColor("var(--bg)");
  const text = resolveColor("var(--text)");
  const muted = resolveColor("var(--muted)");
  const line = resolveColor("var(--line)");
  const panel = resolveColor("var(--panel)");
  const canvas = document.createElement("canvas");
  const dpr = window.devicePixelRatio || 1;
  canvas.width = width * dpr;
  canvas.height = height * dpr;
  const ctx = canvas.getContext("2d");
  if (!ctx) return;
  ctx.scale(dpr, dpr);
  ctx.fillStyle = bg;
  ctx.fillRect(0, 0, width, height);
  const ox = pad - bounds.x;
  const oy = pad - bounds.y;

  for (const edge of edges) {
    const source = byId.get(edge.source);
    const target = byId.get(edge.target);
    if (!source || !target) continue;
    const a = absBox(source, byId);
    const b = absBox(target, byId);
    ctx.strokeStyle = resolveColor(RELATION_COLOR[String(edge.className?.match(/relation-(\w+)/)?.[1] ?? "")] || "var(--blue)");
    ctx.lineWidth = 1.5;
    ctx.beginPath();
    ctx.moveTo(a.x + a.w / 2 + ox, a.y + a.h / 2 + oy);
    ctx.lineTo(b.x + b.w / 2 + ox, b.y + b.h / 2 + oy);
    ctx.stroke();
  }

  ctx.font = "600 12px ui-sans-serif, system-ui, sans-serif";
  for (const node of exportable) {
    const box = absBox(node, byId);
    const x = box.x + ox;
    const y = box.y + oy;
    const data = node.data as { title?: string; color?: string; roleLabel?: string; statusLabel?: string };
    const accent = resolveColor(data.color || "var(--blue)");
    ctx.fillStyle = panel;
    ctx.strokeStyle = line;
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.roundRect(x, y, box.w, box.h, 12);
    ctx.fill();
    ctx.stroke();
    ctx.fillStyle = accent;
    ctx.fillRect(x, y, 3, box.h);
    ctx.fillStyle = text;
    ctx.fillText(data.title || node.id, x + 12, y + 22, box.w - 24);
    ctx.fillStyle = muted;
    ctx.font = "500 10px ui-sans-serif, system-ui, sans-serif";
    const sub = [data.roleLabel, data.statusLabel].filter(Boolean).join(" · ");
    if (sub) ctx.fillText(sub, x + 12, y + 38, box.w - 24);
    ctx.font = "600 12px ui-sans-serif, system-ui, sans-serif";
  }

  downloadHref(canvas.toDataURL("image/png"), filename);
}
