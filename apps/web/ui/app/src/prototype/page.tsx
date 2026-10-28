"use client";

import { Suspense, useMemo } from "react";
import { usePathname, useRouter, useSearchParams } from "next/navigation";
import { SrcResearchWorkspace } from "@/components/src/SrcResearchWorkspace";
import { SRC_DEMO_TASK_ID } from "@/lib/srcDemoData";

/** Offline prototype: always bound to the reserved demo task, which never leaves the browser. */
function SrcPrototypeRoute() {
  const router = useRouter(); const pathname = usePathname(); const search = useSearchParams();
  const navigation = useMemo(() => ({ router, pathname, searchParams: new URLSearchParams(search.toString()) }), [router, pathname, search]);
  return <SrcResearchWorkspace taskId={SRC_DEMO_TASK_ID} navigation={navigation} basePath="/src/prototype" />;
}

export default function SrcPrototypePage() {
  return <Suspense fallback={null}><SrcPrototypeRoute /></Suspense>;
}
