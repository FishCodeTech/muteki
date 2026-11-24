"use client";

import { lazy, Suspense, useCallback, useEffect, useMemo, useState, type ComponentType, type ReactNode } from "react";
import { usePathname, useRouter, useSearchParams } from "next/navigation";
import { ConversationChromeContext, useConversationChrome } from "@/components/conversationChrome";
import { useDesktopChrome } from "@/components/DesktopConnection";
import { RetainedOverlayScope } from "@muteki/ui";
import { WorkspaceActivity as Activity } from "@/lib/workspaceActivity";
import { WorkspaceBoundary } from "@/components/WorkspaceBoundary";
import { CHAT_MAIN_CONTENT_ID, isSrcConversationPath, SRC_MAIN_CONTENT_ID } from "@/lib/workspaceSkipTarget";
import type { ConversationNavigation } from "@/components/conversation/ConversationShell";

function retainedWorkspace<Props extends object>(load: () => Promise<ComponentType<Props>>) {
  let loaded: ComponentType<Props> | undefined;
  const preload = async () => {
    if (!loaded) loaded = await load();
    return loaded;
  };
  const create = () => {
    const LazyWorkspace = lazy(async () => ({ default: await preload() }));
    return function RetainedWorkspace(props: Props) {
      const [Workspace] = useState(() => loaded ?? LazyWorkspace);
      return <Workspace {...props} />;
    };
  };
  return { preload, create };
}

const chatWorkspace = retainedWorkspace(() => import("@/components/conversation/ConversationShell").then((module) => module.ConversationShell));
const srcHomeWorkspace = retainedWorkspace(() => import("@/components/src/SrcHome").then((module) => module.SrcHome));
const srcTaskWorkspace = retainedWorkspace(() => import("@/components/src/SrcWorkbench").then((module) => module.SrcWorkbench));

export function preloadRetainedWorkspace(href: string): void {
  const pathname = href.split(/[?#]/, 1)[0];
  if (pathname === "/chat" || pathname.startsWith("/chat/")) {
    void chatWorkspace.preload().catch((error: unknown) => {
      console.warn("muteki.workspace.preload_failed", { pathname, resource: "chat", error });
    });
  } else if (pathname === "/src" || pathname.startsWith("/src/tasks/")) {
    const workspace = pathname.startsWith("/src/tasks/") ? srcTaskWorkspace : srcHomeWorkspace;
    void workspace.preload().catch((error: unknown) => {
      console.warn("muteki.workspace.preload_failed", { pathname, resource: "src", error });
    });
    void import("@/lib/srcApi").then(({ prefetchSrcTasks }) => prefetchSrcTasks()).catch((error: unknown) => {
      console.warn("muteki.workspace.preload_failed", { pathname, resource: "src-tasks", error });
    });
  }
}

function RetainedWorkspaceLoading({ label, id }: { label: string; id?: string }) {
  return (
    <div id={id} tabIndex={id ? -1 : undefined} className="workspace-route-state" role="status" aria-label={label}>
      <div className="workspace-route-card" aria-hidden="true">
        <div className="t-skeleton ux-skeleton-line short" />
        <div className="t-skeleton ux-skeleton-line" />
        <div className="t-skeleton ux-skeleton-line" />
        <p>{label}…</p>
      </div>
    </div>
  );
}

function ScopedRetainedChrome({
  scope,
  visible,
  headerSlot,
  children,
}: {
  scope: string;
  visible: boolean;
  headerSlot: HTMLElement | null | undefined;
  children: ReactNode;
}) {
  const parentChrome = useConversationChrome();
  useEffect(() => {
    if (!visible && headerSlot) {
      headerSlot.style.removeProperty("--desktop-sidebar-width");
    }
  }, [headerSlot, visible]);
  const previewSidebarWidth = useCallback((width: number | null) => {
    if (!headerSlot || !visible) return;
    if (width == null) headerSlot.style.removeProperty("--desktop-sidebar-width");
    else headerSlot.style.setProperty("--desktop-sidebar-width", `${width}px`);
  }, [headerSlot, visible]);
  const chrome = useMemo(() => ({
    ...parentChrome,
    headerSlot,
    previewSidebarWidth,
  }), [parentChrome, headerSlot, previewSidebarWidth]);
  return (
    <ConversationChromeContext.Provider value={chrome}>
      <RetainedOverlayScope scope={scope} visible={visible}>
        {children}
      </RetainedOverlayScope>
    </ConversationChromeContext.Provider>
  );
}

function RetainedWorkspaceShells() {
  const [ConversationShell, setConversationShell] = useState(chatWorkspace.create);
  const [SrcHome, setSrcHome] = useState(srcHomeWorkspace.create);
  const [SrcWorkbench, setSrcWorkbench] = useState(srcTaskWorkspace.create);
  const desktop = useDesktopChrome();
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

  const srcVisible = isSrcConversationPath(pathname);
  const srcTaskId = srcVisible && pathname.split("/")[2] === "tasks"
    ? decodeURIComponent(pathname.split("/")[3] || "")
    : "";
  const [retainedSrc, setRetainedSrc] = useState<{ navigation: ConversationNavigation; taskId: string } | null>(null);
  if (srcVisible && (retainedSrc?.navigation !== navigation || retainedSrc.taskId !== srcTaskId)) {
    setRetainedSrc({ navigation, taskId: srcTaskId });
  }

  useEffect(() => {
    if (!srcVisible || srcTaskId) return;
    const timer = window.setTimeout(() => {
      void srcTaskWorkspace.preload().catch((error: unknown) => {
        console.warn("muteki.workspace.preload_failed", { pathname: "/src/tasks", resource: "src-task", error });
      });
    }, 150);
    return () => window.clearTimeout(timer);
  }, [srcVisible, srcTaskId]);

  return (
    <>
      <ScopedRetainedChrome scope="chat" visible={chatVisible} headerSlot={desktop?.headerSlots.chat}>
        <Activity mode={chatVisible ? "visible" : "hidden"}>
          <WorkspaceBoundary name="聊天" resetKey={retainedChat?.threadId || "chat"} onRetry={() => setConversationShell(chatWorkspace.create)}>
            <Suspense fallback={<RetainedWorkspaceLoading label="正在打开对话" id={CHAT_MAIN_CONTENT_ID} />}>
              {retainedChat ? (
                <ConversationShell
                  threadId={retainedChat.threadId}
                  navigation={retainedChat.navigation}
                  mainContentId={CHAT_MAIN_CONTENT_ID}
                  sidebarId="chat-conversation-sidebar"
                />
              ) : null}
            </Suspense>
          </WorkspaceBoundary>
        </Activity>
      </ScopedRetainedChrome>
      <ScopedRetainedChrome scope="src" visible={srcVisible} headerSlot={desktop?.headerSlots.src}>
        <Activity mode={srcVisible ? "visible" : "hidden"}>
          <WorkspaceBoundary name="SRC" resetKey={retainedSrc?.taskId || "src"} onRetry={() => { setSrcHome(srcHomeWorkspace.create); setSrcWorkbench(srcTaskWorkspace.create); }}>
            <Suspense fallback={<RetainedWorkspaceLoading label="正在打开 SRC 工作区" id={SRC_MAIN_CONTENT_ID} />}>
              {retainedSrc ? (
                retainedSrc.taskId
                  ? <SrcWorkbench taskId={retainedSrc.taskId} navigation={retainedSrc.navigation} />
                  : <SrcHome navigation={retainedSrc.navigation} onOpenTask={(taskId) => router.push(`/src/tasks/${encodeURIComponent(taskId)}`)} />
              ) : null}
            </Suspense>
          </WorkspaceBoundary>
        </Activity>
      </ScopedRetainedChrome>
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
