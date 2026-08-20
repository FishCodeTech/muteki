/** Client-owned idempotency keys for decision commands.
 *
 * The id is allocated before POST /control. If the server commits and the HTTP
 * response is lost, every retry for the same request/action reuses this key so
 * the backend can return/reconcile the original command instead of creating a
 * second answer.
 */
export type DecisionControlAction = "answer_decision" | "dismiss";

export function newClientCommandId(): string {
  const webCrypto = globalThis.crypto;
  if (webCrypto && typeof webCrypto.randomUUID === "function") {
    return webCrypto.randomUUID();
  }
  const words = new Uint32Array(4);
  if (webCrypto && typeof webCrypto.getRandomValues === "function") {
    webCrypto.getRandomValues(words);
  } else {
    for (let i = 0; i < words.length; i += 1) {
      words[i] = Math.floor(Math.random() * 0x1_0000_0000);
    }
  }
  return `ui-${Date.now().toString(36)}-${Array.from(words, (n) => n.toString(36)).join("-")}`;
}

export function commandIdForDecision(
  ids: Partial<Record<DecisionControlAction, string>>,
  action: DecisionControlAction,
  create: () => string = newClientCommandId,
): string {
  const existing = ids[action];
  if (existing) return existing;
  const commandId = create();
  ids[action] = commandId;
  return commandId;
}
