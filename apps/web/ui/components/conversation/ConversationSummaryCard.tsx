"use client";

import { useCallback, useEffect, useMemo, useState } from "react";
import { cn } from "@/lib/cn";
import { Icon } from "../Icon";
import { IconButton, Menu, MenuItem, MenuSeparator, Popover, StatusDot, toast } from "@/components/chat/ui";
import { chatPanel } from "@/lib/chatPanelStore";
import { streamCornerTop, useThreadDetailsCardHeight } from "@/lib/threadDetailsOverlayStore";
import { AgentAvatar, AgentAvatarStack, AgentDuration, agentTone } from "./ConversationSubagentGroup";
import {
  agentDisplayName,
  agentStatusLabel,
  countAgentStatuses,
  formatAgentCounts,
  type ConversationAgentNodeView,
} from "./conversationAgentTree";
import type { ConversationSources } from "./conversationSources";

const DENSITY_KEY = "muteki.chat.summary-card.v1";
type Density = "full" | "compact";

function readDensity(): Density {
  if (typeof window === "undefined") return "full";
  try {
    return window.localStorage.getItem(DENSITY_KEY) === "compact" ? "compact" : "full";
  } catch {
    return "full";
  }
}

function SectionTitle({ icon, children, aside }: { icon: Parameters<typeof Icon>[0]["name"]; children: React.ReactNode; aside?: React.ReactNode }) {
  return (
    <div className="flex items-center gap-1.5 px-3 pb-1 pt-2.5 text-[11px] font-medium uppercase tracking-wide text-cx-fg-4">
      <Icon name={icon} size={12} />
      <span className="min-w-0 flex-1 truncate">{children}</span>
      {aside}
    </div>
  );
}

export function ConversationSummaryCard({
  threadId,
  agents,
  sources,
}: {
  threadId: string;
  /** Flat list (parents before children) from buildConversationAgentTree. */
  agents: ConversationAgentNodeView[];
  sources: ConversationSources;
}) {
  const [density, setDensity] = useState<Density>("full");
  const [open, setOpen] = useState(false);
  const detailsHeight = useThreadDetailsCardHeight();
  useEffect(() => setDensity(readDensity()), []);

  const counts = useMemo(() => countAgentStatuses(agents), [agents]);
  const sourceCount = sources.linkCount + sources.mcp.length;

  const changeDensity = useCallback((next: Density) => {
    setDensity(next);
    try {
      window.localStorage.setItem(DENSITY_KEY, next);
    } catch {
      // Private mode: the choice simply does not persist.
    }
  }, []);

  const copyLinks = useCallback(async () => {
    const urls = sources.linkGroups.flatMap((group) => group.links.map((link) => link.url));
    try {
      await navigator.clipboard.writeText(urls.join("\n"));
      toast({ title: `已复制 ${urls.length} 个链接`, tone: "success" });
    } catch (error) {
      toast({ title: "复制失败", description: error instanceof Error ? error.message : String(error), tone: "danger" });
    }
  }, [sources.linkGroups]);

  if (!agents.length && !sourceCount) return null;

  const pillLabel = [
    agents.length ? `${agents.length} 个子智能体` : "",
    sourceCount ? `${sourceCount} 个来源` : "",
  ].filter(Boolean).join(" · ");

  const trigger = (
    <button
      type="button"
      aria-label={`本对话概览：${pillLabel}`}
      className={cn(
        "cx-press pointer-events-auto inline-flex h-7 items-center gap-1.5 rounded-full border border-cx-border bg-cx-elevated/90 px-2 text-[12px] font-medium text-cx-fg-2 shadow-cx-sm backdrop-blur",
        "hover:border-cx-border-strong hover:text-cx-fg",
        open && "border-cx-border-strong text-cx-fg",
      )}
    >
      {agents.length ? <AgentAvatarStack agents={agents} size={16} max={3} /> : <Icon name="link" size={13} />}
      {density === "full" ? (
        <span className="cx-tabular whitespace-nowrap">{pillLabel}</span>
      ) : (
        <span className="cx-tabular">{agents.length + sourceCount}</span>
      )}
      {counts.running ? <StatusDot tone="running" /> : null}
    </button>
  );

  return (
    <div
      className="pointer-events-none absolute right-3 z-10"
      style={{ top: streamCornerTop(12, detailsHeight) }}
      data-testid="conversation-summary-card"
    >
      <Popover
        trigger={trigger}
        open={open}
        onOpenChange={setOpen}
        placement="bottom-end"
        ariaLabel="本对话概览"
        className="w-[min(22rem,calc(100vw-2rem))] overflow-hidden p-0"
      >
        <div className="flex items-center gap-2 border-b border-cx-border-subtle px-3 py-2">
          <span className="min-w-0 flex-1 truncate text-[13px] font-semibold text-cx-fg">本对话概览</span>
          <Menu
            placement="bottom-end"
            ariaLabel="概览操作"
            trigger={<IconButton icon="more" label="更多操作" size="sm" />}
          >
            {agents.length ? (
              <MenuItem icon="bot" onSelect={() => { setOpen(false); chatPanel.open(threadId, "agents"); }}>
                打开子智能体面板
              </MenuItem>
            ) : null}
            {sources.linkCount ? (
              <MenuItem icon="copy" onSelect={() => void copyLinks()}>
                复制全部链接
              </MenuItem>
            ) : null}
            <MenuSeparator />
            <MenuItem
              icon={density === "full" ? "minimize" : "maximize"}
              onSelect={() => changeDensity(density === "full" ? "compact" : "full")}
            >
              {density === "full" ? "入口只显示数量" : "入口显示完整计数"}
            </MenuItem>
          </Menu>
        </div>
        <div className="cx-scroll max-h-[min(28rem,60vh)] overflow-y-auto pb-2">
          {agents.length ? (
            <section aria-label="子智能体">
              <SectionTitle icon="bot" aside={<span className="normal-case tracking-normal">{formatAgentCounts(counts)}</span>}>
                子智能体
              </SectionTitle>
              <ul>
                {agents.map((agent) => (
                  <li key={agent.agentId}>
                    <button
                      type="button"
                      onClick={() => { setOpen(false); chatPanel.open(threadId, "agents"); }}
                      className="flex w-full min-w-0 items-center gap-2 px-3 py-1.5 text-left hover:bg-cx-hover"
                      title={agent.activity || agent.title}
                    >
                      <AgentAvatar agent={agent} size={18} />
                      <span className="min-w-0 flex-1 truncate text-[13px] text-cx-fg-2">{agentDisplayName(agent)}</span>
                      {agent.role ? <span className="shrink-0 truncate text-[11px] text-cx-fg-4">{agent.role}</span> : null}
                      <StatusDot tone={agentTone(String(agent.status))} label={agentStatusLabel(String(agent.status))} />
                      <AgentDuration agent={agent} />
                    </button>
                  </li>
                ))}
              </ul>
            </section>
          ) : null}
          {sources.linkGroups.length ? (
            <section aria-label="链接来源">
              <SectionTitle icon="globe" aside={<span className="normal-case tracking-normal">{sources.linkCount} 个</span>}>
                链接
              </SectionTitle>
              {sources.linkGroups.map((group) => (
                <div key={group.host} className="pb-1">
                  <div className="truncate px-3 pt-1 text-[12px] font-medium text-cx-fg-3">{group.host}</div>
                  <ul>
                    {group.links.map((link) => (
                      <li key={link.url} className="group/link flex min-w-0 items-center gap-1 px-3 hover:bg-cx-hover">
                        <button
                          type="button"
                          className="min-w-0 flex-1 truncate py-1 text-left font-cx-mono text-[11.5px] text-cx-fg-2 hover:text-cx-accent"
                          title={`${link.url}\n来自：${link.via.join("、")}`}
                          onClick={() => { setOpen(false); chatPanel.openPreview(threadId, link.url, { newTab: true }); }}
                        >
                          {link.url.replace(/^https?:\/\/(www\.)?/, "")}
                        </button>
                        <a
                          href={link.url}
                          target="_blank"
                          rel="noreferrer noopener"
                          aria-label="在系统浏览器打开"
                          className="grid size-6 shrink-0 place-items-center rounded text-cx-fg-4 opacity-0 hover:text-cx-fg group-hover/link:opacity-100 focus-visible:opacity-100"
                        >
                          <Icon name="externalLink" size={12} />
                        </a>
                      </li>
                    ))}
                  </ul>
                </div>
              ))}
            </section>
          ) : null}
          {sources.mcp.length ? (
            <section aria-label="MCP 来源">
              <SectionTitle icon="plug" aside={<span className="normal-case tracking-normal">{sources.mcpCalls} 次调用</span>}>
                MCP 与插件
              </SectionTitle>
              <ul>
                {sources.mcp.map((server) => (
                  <li key={server.server} className="px-3 py-1">
                    <div className="flex items-center gap-2 text-[13px] text-cx-fg-2">
                      <Icon name="package" size={13} className="text-cx-fg-4" />
                      <span className="min-w-0 flex-1 truncate font-medium">{server.server}</span>
                      <span className="cx-tabular shrink-0 text-[11px] text-cx-fg-4">{server.calls} 次</span>
                    </div>
                    <div className="mt-0.5 flex flex-wrap gap-1 pl-5">
                      {server.tools.map((tool) => (
                        <span key={tool.name} className="rounded bg-cx-bg-subtle px-1.5 py-0.5 font-cx-mono text-[11px] text-cx-fg-3">
                          {tool.name}{tool.calls > 1 ? ` ×${tool.calls}` : ""}
                        </span>
                      ))}
                    </div>
                  </li>
                ))}
              </ul>
            </section>
          ) : null}
          <p className="px-3 pt-2 text-[11px] leading-4 text-cx-fg-4">按已载入的过程统计；展开更早回合的过程后会补全。</p>
        </div>
      </Popover>
    </div>
  );
}
