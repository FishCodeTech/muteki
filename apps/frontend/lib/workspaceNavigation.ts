import type { IconName } from "@muteki/ui";
import type { WorkspaceKindEntry } from "./workspace-kinds";

export function iconOf(entry: WorkspaceKindEntry): IconName {
  if (entry.id === "pentest") return "target";
  if (entry.id === "src") return "shield";
  if (entry.icon === "flag") return "crosshair";
  if (entry.icon === "trophy") return "trophy";
  if (entry.icon === "chat") return "messages";
  return "layers";
}

export function publicDescriptionOf(entry: WorkspaceKindEntry): string {
  if (entry.id === "pentest") return "用自然语言启动授权测试并查看证据报告";
  if (entry.aggregateType === "thread") return "与外部 Agent 协作并查看工具、审批和产物";
  if (entry.aggregateType === "run") return "创建 CTF 单题并交给 Coordinator 调度";
  if (entry.aggregateType === "competition") return "同步比赛题目、调度 Run 并跟踪提交裁定";
  return entry.description;
}

export function recentStateLabel(state: string, kindId?: string, running?: boolean): string {
  if (kindId === "conversation" && state === "active" && !running) return "可继续";
  const labels: Record<string, string> = {
    active: "进行中",
    running: "运行中",
    archived: "已归档",
    completed: "已完成",
    failed: "失败",
    paused: "已暂停",
    stopping: "停止中",
    interrupted: "已中断",
    finished: "已完成",
    stopped: "已停止",
    cancelled: "已取消",
    queued: "排队中",
    pending: "等待中",
    idle: "空闲",
    draft: "草稿",
    solved: "已解出",
  };
  return labels[state] || state || "可恢复";
}
