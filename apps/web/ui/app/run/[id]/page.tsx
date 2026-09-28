"use client";

// Per-run deep link (/run/<id>). The deck reads the run id from the URL on mount
// (see components/RunWorkbench.tsx), so this route renders the exact same shell — a refresh
// or shared link to /run/<id> restores that conversation. Re-export keeps one
// source of truth for the deck.
export { default } from "@/components/RunWorkbench";
