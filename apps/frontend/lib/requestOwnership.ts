export function createRequestOwner() {
  let generation = 0;
  return {
    issue(): () => boolean { const issued = ++generation; return () => issued === generation; },
    invalidate(): void { generation += 1; },
  };
}

type VersionedOwner = { connectionVersion?: number; serviceId?: string; identityId?: string };
export function admitsConnectionOwner(previous: VersionedOwner | null, next: VersionedOwner): boolean {
  if (!previous) return true;
  const before = previous.connectionVersion ?? 0, after = next.connectionVersion ?? 0;
  if (after !== before) return after > before;
  return (!previous.serviceId || previous.serviceId === next.serviceId)
    && (!previous.identityId || previous.identityId === next.identityId);
}
