"use client";

import { createContext, useContext, type AnchorHTMLAttributes } from "react";

export interface ConversationNavigation {
  router: {
    push(href: string, options?: { scroll?: boolean }): void;
    replace(href: string, options?: { scroll?: boolean }): void;
  };
  pathname: string;
  searchParams: URLSearchParams;
}

export const ConversationNavigationContext = createContext<ConversationNavigation | null>(null);

export function useConversationNavigation(): ConversationNavigation {
  const navigation = useContext(ConversationNavigationContext);
  if (!navigation) throw new Error("ConversationNavigation Provider is required");
  return navigation;
}

/** Plain anchor semantics outside the chat host; the chat host owns SPA routing. */
export function ConversationRouteLink({ onClick, ...props }: AnchorHTMLAttributes<HTMLAnchorElement>) {
  const navigation = useContext(ConversationNavigationContext);
  return <a {...props} onClick={(event) => {
    onClick?.(event);
    if (!navigation || event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.altKey || event.shiftKey || props.target || !props.href?.startsWith("/")) return;
    event.preventDefault();
    navigation.router.push(props.href);
  }} />;
}
