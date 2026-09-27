"use client";
// Vendored from AgentUI (https://www.agentui.pro), MIT License. See ./LICENSE.


import { createContext } from "react";

export type MessageSide = "start" | "end";

export const MessageSideContext = createContext<MessageSide | undefined>(
  undefined,
);
