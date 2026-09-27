"use client";

import { Suspense } from "react";
import { useParams } from "next/navigation";
import { ConversationShell } from "@/components/conversation/ConversationShell";

function ConversationShellRoute() {
  const params = useParams<{ id?: string | string[] }>();
  const rawId = Array.isArray(params?.id) ? params.id[0] : params?.id;
  const threadId = rawId ? decodeURIComponent(rawId) : "";
  return <ConversationShell threadId={threadId} />;
}

export default function ConversationLayout() {
  return (
    <Suspense fallback={null}>
      <ConversationShellRoute />
    </Suspense>
  );
}
