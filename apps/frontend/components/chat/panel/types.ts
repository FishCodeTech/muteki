import type { ConversationEvent, ConversationView } from "@/lib/useConversation";
import type { ConversationToolRecord } from "@/components/conversation/conversationEventViews";
import type { DiffLineAnnotation } from "@/lib/conversationDiff";
import type { DrawerDetailPayload } from "@/components/conversation/ConversationDetailsDrawer";
import type { ChatSurface } from "@/lib/chatPanelStore";

export interface SurfaceContext {
  threadId: string;
  view: ConversationView;
  events: ConversationEvent[];
  tools: ConversationToolRecord[];
  active: boolean;
  hasWorkspace: boolean;
  onOpenDetails: (payload: DrawerDetailPayload) => void;
  onCiteToComposer?: (excerpt: string) => void;
  onAttachToComposer?: (input: { text: string; files: File[]; send: boolean }) => void;
  onDiffAnnotationSend?: (annotations: DiffLineAnnotation[]) => void;
}

export interface SurfaceProps<S extends ChatSurface = ChatSurface> extends SurfaceContext {
  surface: S;
}
