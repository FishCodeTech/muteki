"use client";

import { useT } from "@/lib/i18n";
import { Icon } from "@/components/Icon";
import { PanelEmpty } from "@/components/PanelEmpty";

/** Cross-solver Insight Bus stream (§14.3, §5.3): verified facts / dead-ends /
 *  FlagFound broadcast among solvers. */
export function InsightFeed({ insights }: { insights: string[] }) {
  const t = useT();
  if (insights.length === 0)
    return <PanelEmpty icon="radio" title={t("empty.noBroadcasts")} hint={t("empty.noBroadcastsHint")} />;
  return (
    <div>
      {insights.map((s, i) => (
        <div className="insight" key={i}><Icon name="radio" size={13} /> {s}</div>
      ))}
    </div>
  );
}
