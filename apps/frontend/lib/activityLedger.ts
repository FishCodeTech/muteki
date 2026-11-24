import type { ChatMessage } from "@/lib/events";

export function toolCommandLabel(message: ChatMessage): string {
  return message.content.replace(/^[▶↳]\s*/, "").trim() || "tool";
}

export function splitToolCommand(command: string): { name: string; args: string } {
  const match = /^([\w.-]{1,32}):\s*(.*)$/s.exec(command);
  if (!match || !match[2] || match[2].startsWith("//")) return { name: "", args: command };
  return { name: match[1], args: match[2] };
}

export function abbreviateWorkspacePaths(text: string): string {
  return text
    .replace(/\S*?\/sessions\/[^/\s]+\/workspace\/workers\/[^/\s]+/g, "$WORKER")
    .replace(/\S*?\/sessions\/[^/\s]+\/workspace/g, "$WORKSPACE");
}
