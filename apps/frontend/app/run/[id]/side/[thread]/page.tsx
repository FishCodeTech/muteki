"use client";

import { useParams } from "next/navigation";
import { useEffect, useState } from "react";
import { RunSideChatWindow } from "@/components/run-side/RunSideChat";

export default function Page() {
  const params = useParams<{ id?: string | string[]; thread?: string | string[] }>();
  const first = (value?: string | string[]) => decodeURIComponent((Array.isArray(value) ? value[0] : value) || "");
  const runId = first(params?.id);
  const threadId = first(params?.thread);
  const [detached, setDetached] = useState(false);
  useEffect(() => { setDetached(new URLSearchParams(window.location.search).get("popout") === "1"); }, []);
  useEffect(() => {
    if (!detached) return;
    const root = document.documentElement;
    root.dataset.runSidePopout = "true";
    return () => { delete root.dataset.runSidePopout; };
  }, [detached]);
  if (!runId) return null;
  return <RunSideChatWindow key={runId} runId={runId} threadId={threadId} detached={detached} />;
}
