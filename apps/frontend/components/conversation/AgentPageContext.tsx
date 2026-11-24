"use client";

import { createContext, useContext } from "react";

export const AgentPageContext = createContext<((threadId: string) => void) | null>(null);
export function useAgentPage() { return useContext(AgentPageContext); }
