import type { Element, Root, Text } from "hast";

export function rehypeMessageSource() {
  return (tree: Root) => {
    const visit = (parent: Root | Element) => {
      if (parent.type === "element" && ["pre", "code", "svg", "math"].includes(parent.tagName)) return;
      parent.children = parent.children.map((child) => {
        if (child.type === "element") visit(child);
        if (child.type !== "text") return child;
        const start = child.position?.start.offset;
        const end = child.position?.end.offset;
        if (start === undefined || end === undefined) return child;
        return {
          type: "element", tagName: "span",
          properties: { "data-md-text-start": start, "data-md-text-end": end },
          children: [child as Text],
        } as Element;
      });
    };
    visit(tree);
  };
}
