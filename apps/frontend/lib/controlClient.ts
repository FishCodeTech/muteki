export type DecisionControlAction = "answer_decision" | "dismiss";

function newClientCommandId(): string {
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
