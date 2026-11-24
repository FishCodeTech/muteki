export type CommandReceiptClass = "completed" | "failed" | "pending" | "unknown";

export function classifyCommandReceipt(receipt: { state?: string; error?: unknown; detail?: { outcome_unknown?: boolean; delivery_unknown?: boolean } } | null | undefined): CommandReceiptClass {
  if (!receipt) return "unknown";
  const error = receipt.error && typeof receipt.error === "object" ? receipt.error as { detail?: { outcome_unknown?: boolean; delivery_unknown?: boolean } } : null;
  if (receipt.state === "unknown" || receipt.detail?.outcome_unknown === true || receipt.detail?.delivery_unknown === true || error?.detail?.outcome_unknown === true || error?.detail?.delivery_unknown === true) return "unknown";
  if (["failed", "rejected", "conflict", "cancelled"].includes(receipt.state || "")) return "failed";
  if (receipt.error) return "unknown";
  if (receipt.state === "completed") return "completed";
  if (["accepted", "running", "waiting", "processing"].includes(receipt.state || "")) return "pending";
  return "unknown";
}

export function admitsCommandReceipt(previous: { state?: string; error?: unknown } | undefined, next: { state?: string; error?: unknown }): boolean {
  const before = classifyCommandReceipt(previous);
  const after = classifyCommandReceipt(next);
  if (before === "completed" || before === "failed") return after === before;
  return next.state !== "accepted" || !["running", "waiting"].includes(previous?.state || "");
}
