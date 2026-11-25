export type FolderArrangeSection = "active" | "recent";

export type FolderArrangeBaselineItem = {
  id: string;
  section: FolderArrangeSection;
  order: number;
};

export type FolderArrangeTarget = {
  id: string;
  section: FolderArrangeSection;
};

export type FolderArrangePatch = {
  section?: FolderArrangeSection;
  order?: number;
};

export function createFolderArrangeQueue(
  getFolders: () => readonly FolderArrangeBaselineItem[],
  patchFolder: (id: string, patch: FolderArrangePatch) => Promise<boolean>,
  onSettled: () => void,
) {
  let chain: Promise<void> = Promise.resolve();
  let pending = 0;
  let baseline: Map<string, FolderArrangeBaselineItem> | null = null;
  let baselineSource: readonly FolderArrangeBaselineItem[] | null = null;
  let active = false;

  return (targets: readonly FolderArrangeTarget[]): Promise<boolean> => {
    pending += 1;
    let resolveResult!: (ok: boolean) => void;
    const result = new Promise<boolean>((resolve) => {
      resolveResult = resolve;
    });

    chain = chain.then(async () => {
      const latest = getFolders();
      if (!baseline || (!active && latest !== baselineSource)) {
        baseline = new Map(latest.map((folder) => [folder.id, { ...folder }]));
      } else {
        for (const folder of latest) {
          if (!baseline.has(folder.id)) {
            baseline.set(folder.id, { ...folder });
          }
        }
      }
      active = true;

      let ok = true;
      for (const [index, item] of targets.entries()) {
        const current = baseline.get(item.id);
        if (!current) {
          ok = false;
          break;
        }
        const desiredOrder = index + 1;
        const patch: FolderArrangePatch = {};
        if (current.section !== item.section) patch.section = item.section;
        if (current.order !== desiredOrder) patch.order = desiredOrder;
        if (patch.section === undefined && patch.order === undefined) continue;
        if (!(await patchFolder(item.id, patch))) {
          ok = false;
          break;
        }
        baseline.set(item.id, {
          id: item.id,
          section: patch.section ?? current.section,
          order: patch.order ?? current.order,
        });
      }

      pending -= 1;
      if (pending === 0) {
        active = false;
        baselineSource = getFolders();
        onSettled();
      }
      resolveResult(ok);
    });

    return result;
  };
}
