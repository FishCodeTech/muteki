"use client";

import { memo } from "react";
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
import { ExtensionSurfaceHost } from "./extensions";

interface SurfaceRendererProps {
  surface: ChatSurface;
  ctx: SurfaceContext;
}

// Stream-driven data; hidden surfaces keep their last-rendered snapshot and re-sync on activation.
const LIVE_DATA_KEYS = ["view", "events", "tools"] as const satisfies readonly (keyof SurfaceContext)[];

function sameRendererProps(prev: SurfaceRendererProps, next: SurfaceRendererProps): boolean {
  if (prev.surface !== next.surface) return false;
  const a = prev.ctx;
  const b = next.ctx;
  if (
    a.active !== b.active
    || a.threadId !== b.threadId
    || a.hasWorkspace !== b.hasWorkspace
    || a.onOpenDetails !== b.onOpenDetails
    || a.onCiteToComposer !== b.onCiteToComposer
    || a.onAttachToComposer !== b.onAttachToComposer
    || a.onDiffAnnotationSend !== b.onDiffAnnotationSend
  ) return false;
  if (!b.active) return true;
  return LIVE_DATA_KEYS.every((key) => a[key] === b[key]);
}

export const SurfaceRenderer = memo(function SurfaceRenderer({ surface, ctx }: SurfaceRendererProps) {
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
    case "extension":
      return <ExtensionSurfaceHost surface={surface} threadId={ctx.threadId} active={ctx.active} />;
    default: {
      const exhaustive: never = surface;
      return exhaustive;
    }
  }
}, sameRendererProps);
