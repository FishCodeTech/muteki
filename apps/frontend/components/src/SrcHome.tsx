"use client";

import { Button, Popover } from "@muteki/ui";
import { ConversationShell, type ConversationNavigation } from "@/components/conversation/ConversationShell";
import { Icon } from "@muteki/ui";
import { useSrcConversationExperience } from "./useSrcConversationExperience";
import { SrcError } from "./ui";

export function SrcHome({ navigation }: { navigation: ConversationNavigation; onOpenTask?: (taskId: string) => void }) {
  const { experience, error, reload, historicalTasks } = useSrcConversationExperience();
  const home = {
    ...experience.home,
    introAccessory: historicalTasks.length > 0 ? (
      <div className="flex justify-center">
        <Popover
          placement="bottom"
          ariaLabel="历史研究"
          className="w-[min(360px,calc(100vw-32px))] p-1.5"
          trigger={
            <Button type="button" size="sm" variant="outline" icon="history" iconRight="chevronDown">
              历史研究（{historicalTasks.length}）
            </Button>
          }
        >
          <ul className="cx-scroll flex max-h-72 flex-col gap-0.5 overflow-y-auto">
            {historicalTasks.map(task => (
              <li key={task.task_id}>
                <a
                  href={`/src/tasks/${encodeURIComponent(task.task_id)}`}
                  className="flex items-center justify-between gap-2 rounded-lg px-2.5 py-1.5 text-cx-sm text-cx-fg transition hover:bg-cx-hover"
                >
                  <span className="min-w-0 truncate">{task.title || task.task_id}</span>
                  <Icon name="arrowRight" size={13} className="shrink-0 text-cx-fg-4" />
                </a>
              </li>
            ))}
          </ul>
        </Popover>
      </div>
    ) : undefined,
  };
  return <div className="src-task-workbench src-home-workbench">
    {error ? <div className="src-task-list-error"><SrcError error={error} onRetry={() => void reload()} /></div> : null}
    <ConversationShell navigation={navigation} experience={{ ...experience, home }} mainContentId="src-main-content" sidebarId="src-conversation-sidebar" />
  </div>;
}
