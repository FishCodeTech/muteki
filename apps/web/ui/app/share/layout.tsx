import type { Metadata } from "next";
export const metadata: Metadata = { title: "只读会话分享", robots: { index: false, follow: false }, referrer: "no-referrer" };
export default function ShareLayout({ children }: { children: React.ReactNode }) { return children; }
