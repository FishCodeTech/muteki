import type { ToolItem } from "@muteki/ui/agent";
import { toolKind } from "../chat/timeline/toolPresentation";
import type { DrawerDetailPayload } from "./ConversationDetailsDrawer";

export function toolDrawerPayload(tool: ToolItem): DrawerDetailPayload {
  const terminal = toolKind(tool) === "command";
  return {
    type: tool.status === "failed" ? "error" : "tool",
    title: tool.title || tool.name,
    subtitle: tool.chip || tool.argsSummary,
    status: tool.status,
    toolName: tool.name,
    input: tool.argsSummary,
    output: terminal ? undefined : tool.outputSummary,
    outputUnavailable: tool.outputUnavailable,
    terminalOutput: terminal ? tool.outputSummary : undefined,
    error: tool.error,
  };
}
