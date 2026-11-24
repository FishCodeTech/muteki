"use client";

import { useMemo } from "react";
import { usePathname, useSearchParams } from "next/navigation";
import { WorkerOrchestration } from "./WorkerOrchestration";

export function WebWorkerOrchestration({ defaultReturnTo = "/" }: { defaultReturnTo?: string }) {
  const pathname = usePathname();
  const search = useSearchParams();
  const navigation = useMemo(() => ({ pathname,
    searchParams: new URLSearchParams(search.toString()) }), [pathname, search]);
  return <WorkerOrchestration defaultReturnTo={defaultReturnTo} navigation={navigation} />;
}
