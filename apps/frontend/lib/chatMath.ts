import type { Root, RootContent } from "mdast";
import { renderLatexToHtml } from "@muteki/ui/engines/math";
import type { Plugin } from "@muteki/ui/engines/markdown";

export const MAX_MATH_INPUT = 8192;
const MAX_MATH_NODES = 64;
const MAX_MATH_TOTAL = 32768;
const UNSUPPORTED_SIDE_EFFECTS = /\\(?:href|url|includegraphics|html[A-Za-z]*|gdef|global)\b/;

type MathNode = {
  type: string;
  value?: string;
  children?: MathNode[];
  position?: { start: { offset?: number }; end: { offset?: number } };
  data?: Record<string, unknown>;
};

export function isInlineMathCandidate(source: string, value: string, before = "", after = ""): boolean {
  if (!source.startsWith("$") || !source.endsWith("$")) return false;
  if (source.startsWith("$$")) return true;
  if (!value || value !== value.trim() || /\d/.test(after[0] || "")) return false;
  if (/(?:US|CA|AU|NZ|HK|SG)$/.test(before) && /^\d/.test(value)) return false;
  if (/^[+-]?[\d.,]/.test(value) && !/[\\^_=+*/<>]/.test(value)) return false;
  return true;
}

export function isCompleteMath(source: string, block: boolean): boolean {
  const raw = source.trim();
  const marker = /^(\${1,})/.exec(raw)?.[1];
  if (!marker) return false;
  if (!block || !raw.includes("\n")) return raw.length > marker.length * 2 && raw.endsWith(marker);
  const lines = raw.split("\n");
  return lines.length > 2
    && lines[0].trim() === marker
    && /^\${2,}$/.test(lines.at(-1)!.trim())
    && lines.at(-1)!.trim().length >= marker.length;
}

export function renderChatMath(value: string, displayMode: boolean): string | null {
  if (!value.trim() || value.length > MAX_MATH_INPUT || UNSUPPORTED_SIDE_EFFECTS.test(value)) return null;
  return renderLatexToHtml(value, displayMode);
}

export const remarkChatMath: Plugin<[], Root> = () => (tree, file) => {
  const source = String(file.value);
  let count = 0;
  let total = 0;
  const rawOf = (node: MathNode) => source.slice(node.position?.start.offset ?? 0, node.position?.end.offset ?? 0);
  const walk = (parent: MathNode) => {
    if (!parent.children) return;
    parent.children = parent.children.map((original) => {
      let node = original;
      if (node.type === "paragraph" && node.children?.length === 1 && node.children[0].type === "inlineMath") {
        const child = node.children[0];
        if (rawOf(child).startsWith("$$")) node = { ...child, type: "math" };
      }
      if (node.type !== "math" && node.type !== "inlineMath") {
        walk(node);
        return node;
      }
      const raw = rawOf(node);
      const value = node.value || "";
      const start = node.position?.start.offset ?? 0;
      const end = node.position?.end.offset ?? source.length;
      const display = node.type === "math";
      if (!display && !isInlineMathCandidate(raw, value, source.slice(0, start), source.slice(end))) {
        return { type: "text", value: raw, position: node.position };
      }
      count += 1;
      total += value.length;
      const complete = isCompleteMath(raw, display);
      const withinBudget = count <= MAX_MATH_NODES && total <= MAX_MATH_TOTAL && value.length <= MAX_MATH_INPUT;
      node.data = {
        ...node.data,
        hName: display ? "div" : "span",
        hProperties: {
          "data-chat-math": complete && withinBudget ? value : "",
          "data-math-source": raw,
          "data-math-display": display ? "true" : "false",
        },
        hChildren: [],
      };
      return node;
    });
  };
  walk(tree as unknown as MathNode);
  tree.children = (tree as unknown as MathNode).children as RootContent[];
};
