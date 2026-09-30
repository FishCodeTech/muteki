"use client";

import { Suspense, useMemo } from "react";
import { useParams, usePathname, useRouter, useSearchParams } from "next/navigation";
import { ConversationShell } from "@/components/conversation/ConversationShell";

function ConversationShellRoute() {
  const params = useParams<{ id?: string | string[] }>();
  const rawId = Array.isArray(params?.id) ? params.id[0] : params?.id;
  const threadId = rawId ? decodeURIComponent(rawId) : "";
  const router = useRouter();
  const pathname = usePathname();
  const search = useSearchParams();
  const navigation = useMemo(() => ({ router, pathname,
    searchParams: new URLSearchParams(search.toString()) }), [router, pathname, search]);
  return <ConversationShell threadId={threadId} navigation={navigation} />;
}

export default function ConversationLayout() {
  return (
    <Suspense fallback={null}>
      <ConversationShellRoute />
    </Suspense>
  );
}
