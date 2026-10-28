"use client";

import { ConversationRouteLink } from "@/components/conversation/ConversationNavigation";
import { priceKey, pricingIssueMessage, pricingSettingsHref, type PricingIssue } from "@/lib/modelPricing";

export function PricingIssues({ issues, en = false }: { issues?: PricingIssue[]; en?: boolean }) {
  if (!issues?.length) return null;
  const groups = new Map<string, PricingIssue[]>();
  for (const issue of issues) {
    const key = priceKey({ ...issue, engine: issue.engine || "", model: issue.model || "" });
    groups.set(key, [...(groups.get(key) ?? []), issue]);
  }
  return <div className="flex min-w-0 max-w-sm flex-col gap-2 whitespace-normal text-[11px] leading-5">
    {[...groups].map(([key, group]) => {
      const identity = group[0];
      const reasons = [...new Map(group.map(issue => [issue.reason, issue])).values()];
      const canSetPrice = Boolean(identity.engine && identity.model && !reasons.some(issue => issue.reason === "historical_scope_unknown") && reasons.some(issue => !["missing_tokens", "historical_scope_unknown"].includes(issue.reason)));
      return <div key={key} className="min-w-0">
        {identity.model ? <p className="break-all font-medium text-ink-2">{identity.model}</p> : null}
        {reasons.map(issue => <p key={issue.reason} className="break-words text-ink-3">{pricingIssueMessage(issue, en)}</p>)}
        {canSetPrice ? <ConversationRouteLink href={pricingSettingsHref(identity)} className="inline-flex min-h-7 items-center rounded-sm font-medium text-accent underline underline-offset-2 hover:text-ink focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2" aria-label={`${en ? "Resolve pricing for" : "处理定价："} ${identity.engine} · ${identity.model}`}>{en ? "Resolve pricing" : "处理定价"}</ConversationRouteLink> : null}
      </div>;
    })}
  </div>;
}
