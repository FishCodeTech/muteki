"use client";

import { Suspense, useMemo, useState, type ActivityProps, type ExoticComponent } from "react";
import { usePathname, useRouter, useSearchParams } from "next/navigation";
import { ConversationShell, type ConversationNavigation } from "@/components/conversation/ConversationShell";
import { SrcHome } from "@/components/src/SrcHome";
import { SrcTaskShell } from "@/components/src/SrcTaskShell";

// Next's bundled app-router React reconciles <Activity> fibers but only exports
// the element type as unstable_Activity on the experimental channel. Element
// types are Symbol.for-registered, so this is the same value React uses.
const Activity = Symbol.for("react.activity") as unknown as ExoticComponent<ActivityProps>;

/**
 * Keeps the chat and SRC shells mounted across workspace switches (like the
 * desktop shell's React Activity slots) so drafts, scroll position and live
 * streams survive navigation. Retained snapshots only update while the
 * workspace is visible: a hidden shell keeps the route it was left on instead
 * of tracking another page's navigation.
 */
function RetainedWorkspaceShells() {
  const router = useRouter();
  const pathname = usePathname();
  const search = useSearchParams();
  const navigation = useMemo<ConversationNavigation>(() => ({
    router,
    pathname,
    searchParams: new URLSearchParams(search.toString()),
  }), [router, pathname, search]);

  const chatVisible = pathname === "/chat" || pathname.startsWith("/chat/");
  const chatThreadId = chatVisible ? decodeURIComponent(pathname.split("/")[2] || "") : "";
  const [retainedChat, setRetainedChat] = useState<{ navigation: ConversationNavigation; threadId: string } | null>(null);
  if (chatVisible && (retainedChat?.navigation !== navigation || retainedChat.threadId !== chatThreadId)) {
    setRetainedChat({ navigation, threadId: chatThreadId });
  }

  const srcVisible = pathname === "/src" || pathname.startsWith("/src/tasks/");
  const srcTaskId = srcVisible && pathname.split("/")[2] === "tasks"
    ? decodeURIComponent(pathname.split("/")[3] || "")
    : "";
  const [retainedSrc, setRetainedSrc] = useState<{ navigation: ConversationNavigation; taskId: string } | null>(null);
  if (srcVisible && (retainedSrc?.navigation !== navigation || retainedSrc.taskId !== srcTaskId)) {
    setRetainedSrc({ navigation, taskId: srcTaskId });
  }

  // Each retained shell keeps its own stable landmark ids — the defaults stay
  // reserved for /studio-prototype, the other conversation-layout surface.
  return (
    <>
      <Activity mode={chatVisible ? "visible" : "hidden"}>
        {retainedChat ? (
          <ConversationShell
            threadId={retainedChat.threadId}
            navigation={retainedChat.navigation}
            mainContentId="chat-main-content"
            sidebarId="chat-conversation-sidebar"
          />
        ) : null}
      </Activity>
      <Activity mode={srcVisible ? "visible" : "hidden"}>
        {retainedSrc ? (
          retainedSrc.taskId
            ? <SrcTaskShell taskId={retainedSrc.taskId} navigation={retainedSrc.navigation} />
            : <SrcHome navigation={retainedSrc.navigation} onOpenTask={(taskId) => router.push(`/src/tasks/${encodeURIComponent(taskId)}`)} />
        ) : null}
      </Activity>
    </>
  );
}

export function RetainedWorkspaces() {
  return (
    <Suspense fallback={null}>
      <RetainedWorkspaceShells />
    </Suspense>
  );
}
