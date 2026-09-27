import { readFileSync } from "node:fs";

import { Type } from "typebox";

type ToolDescriptor = {
  name: string;
  description?: string;
  input_schema?: Record<string, unknown>;
};

function readToolDescriptors(): ToolDescriptor[] {
  const source = process.env.MUTEKI_CAPABILITY_TOOLS_FILE || "";
  if (!source) {
    throw new Error("MUTEKI_CAPABILITY_TOOLS_FILE is required");
  }
  const parsed = JSON.parse(readFileSync(source, "utf8"));
  if (!Array.isArray(parsed)) {
    throw new Error("Muteki capability tool catalog must be an array");
  }
  return parsed.filter(
    (item): item is ToolDescriptor =>
      Boolean(item && typeof item === "object" && typeof item.name === "string"),
  );
}

export default function (pi: any) {
  const endpoint = process.env.MUTEKI_CAPABILITY_ENDPOINT || "";
  const token = process.env.MUTEKI_CAPABILITY_TOKEN || "";
  if (!endpoint || !token) {
    throw new Error("Muteki capability endpoint and token are required");
  }

  for (const descriptor of readToolDescriptors()) {
    const schema = descriptor.input_schema || {
      type: "object",
      properties: {},
    };
    pi.registerTool({
      name: descriptor.name,
      label: descriptor.name,
      description: descriptor.description || descriptor.name,
      promptSnippet: `${descriptor.name}: ${descriptor.description || "Muteki capability tool"}`,
      parameters: Type.Unsafe(schema),
      async execute(
        toolCallId: string,
        args: Record<string, unknown>,
        signal?: AbortSignal,
      ) {
        const response = await fetch(endpoint, {
          method: "POST",
          headers: {
            "content-type": "application/json",
            authorization: `Bearer ${token}`,
          },
          body: JSON.stringify({
            jsonrpc: "2.0",
            id: `pi-${toolCallId}`,
            method: "muteki.invoke",
            params: {
              tool_name: descriptor.name,
              arguments: args || {},
            },
          }),
          signal,
        });
        const payload: any = await response.json();
        if (!response.ok || payload?.error) {
          const detail = payload?.error || payload;
          throw new Error(
            typeof detail === "string" ? detail : JSON.stringify(detail),
          );
        }
        const result = payload?.result ?? payload;
        return {
          content: [
            {
              type: "text",
              text: typeof result === "string" ? result : JSON.stringify(result),
            },
          ],
          details: {
            tool_name: descriptor.name,
            invocation_id: result?.invocation_id,
          },
        };
      },
    });
  }
}
