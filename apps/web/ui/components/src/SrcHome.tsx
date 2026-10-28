"use client";

import { ConversationShell, type ConversationNavigation } from "@/components/conversation/ConversationShell";
import { useSrcConversationExperience } from "./useSrcConversationExperience";
import { SrcError } from "./ui";
import { Button } from "@/components/chat/ui/Button";
import { SrcTaskShell } from "./SrcTaskShell";
import { SRC_DEMO_TASK_ID } from "@/lib/srcDemoData";

export function SrcHome({ navigation }: { navigation: ConversationNavigation; onOpenTask?: (taskId: string) => void }) {
  if (navigation.searchParams.get("demo") === "1") return <SrcTaskShell taskId={SRC_DEMO_TASK_ID} navigation={navigation} />;
  return <SrcLiveHome navigation={navigation} />;
}

function SrcLiveHome({ navigation }: { navigation: ConversationNavigation }) {
  const { experience, error, reload } = useSrcConversationExperience();
  return <main className="src-task-workbench src-home-workbench">
    {error ? <div className="src-task-list-error"><SrcError error={error} onRetry={() => void reload()} /></div> : null}
    <ConversationShell navigation={navigation} experience={experience} mainContentId="src-main-content" sidebarId="src-conversation-sidebar" />
    <div className="src-preview-entry"><Button size="xs" icon="eye" onClick={() => navigation.router.push("/src?demo=1")}>查看完整界面示例</Button><Button size="xs" icon="layers" className="ml-1" onClick={() => navigation.router.push("/src/prototype")}>研究工作台原型</Button></div>
  </main>;
}
