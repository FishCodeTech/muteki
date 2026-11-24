import { Type } from "typebox";

const allowed = new Set(['board_query', 'events_query', 'artifact_read', 'workspace_read']);
export default function (pi: any) {
  let turn = 0;
  pi.on('tool_call', async (event: any) => {
    if (!allowed.has(event.toolName)) return { block: true, reason: 'Read-only followup tool not allowed' };
  });
  for (const tool of JSON.parse(process.env.MUTEKI_READONLY_TOOLS || '[]')) {
    const fn = tool.function;
    if (!allowed.has(fn.name)) throw new Error('Unknown readonly tool');
    pi.registerTool({ name: fn.name, label: fn.name, description: fn.description, parameters: Type.Unsafe(fn.parameters),
      async execute(_id: string, args: any, signal: AbortSignal) {
        if (!allowed.has(fn.name)) throw new Error('Read-only tool denied');
        const response = await fetch(process.env.MUTEKI_READONLY_URL!, {
          method: 'POST', headers: { 'content-type': 'application/json', authorization: process.env.MUTEKI_READONLY_TOKEN! },
          body: JSON.stringify({ name: fn.name, args, turn: ++turn }), signal
        });
        const text = await response.text();
        if (!response.ok) throw new Error(text);
        return { content: [{ type: 'text', text }], details: {} };
      }
    });
  }
}
