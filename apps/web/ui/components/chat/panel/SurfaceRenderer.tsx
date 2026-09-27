"use client";

import type { ChatSurface } from "@/lib/chatPanelStore";
import type { SurfaceContext } from "./types";
import { OverviewSurface } from "@/components/chat/surfaces/OverviewSurface";
import { DiffSurface } from "@/components/chat/surfaces/DiffSurface";
import { PreviewSurface } from "@/components/chat/surfaces/PreviewSurface";
import { FileSurface } from "@/components/chat/surfaces/FileSurface";
import { FilesSurface } from "@/components/chat/surfaces/FilesSurface";
import { TerminalSurface } from "@/components/chat/surfaces/TerminalSurface";
import { AgentsSurface } from "@/components/chat/surfaces/AgentsSurface";
import { PlanSurface } from "@/components/chat/surfaces/PlanSurface";
import { PullRequestSurface } from "@/components/chat/surfaces/PullRequestSurface";

export function SurfaceRenderer({ surface, ctx }: { surface: ChatSurface; ctx: SurfaceContext }) {
  switch (surface.kind) {
    case "overview":
      return <OverviewSurface {...ctx} surface={surface} />;
    case "diff":
      return <DiffSurface {...ctx} surface={surface} />;
    case "preview":
      return <PreviewSurface {...ctx} surface={surface} />;
    case "file":
      return <FileSurface {...ctx} surface={surface} />;
    case "files":
      return <FilesSurface {...ctx} surface={surface} />;
    case "terminal":
      return <TerminalSurface {...ctx} surface={surface} />;
    case "agents":
      return <AgentsSurface {...ctx} surface={surface} />;
    case "plan":
      return <PlanSurface {...ctx} surface={surface} />;
    case "pull-request":
      return <PullRequestSurface {...ctx} surface={surface} />;
    default: {
      const exhaustive: never = surface;
      return exhaustive;
    }
  }
}
