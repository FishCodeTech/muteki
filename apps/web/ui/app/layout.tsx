import type { Metadata, Viewport } from "next";
import "./globals.css";
import "@/styles/transitions/_root.css";
import "@/styles/transitions/index.css";
import "@/styles/transitions/surfaces.css";
import "@/styles/transitions/accessibility.css";
import SchemeBoot from "../components/SchemeBoot";
import { Providers } from "./providers";
import { WorkspaceAppShell } from "@/components/WorkspaceAppShell";

export const metadata: Metadata = {
  title: {
    default: "Muteki — Agent 安全工作台",
    template: "%s · Muteki",
  },
  description: "管理 Agent 对话、单项安全任务和比赛调度。",
  appleWebApp: {
    capable: true,
    statusBarStyle: "black-translucent",
    title: "Muteki",
  },
};

// C35: cover mode + C37: themeColor for PWA standalone status bar
export const viewport: Viewport = {
  width: "device-width",
  initialScale: 1,
  viewportFit: "cover",
  themeColor: [
    { media: "(prefers-color-scheme: dark)", color: "#0f1011" },
    { media: "(prefers-color-scheme: light)", color: "#ffffff" },
  ],
};

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="zh-CN" data-theme="dark" className="dark" suppressHydrationWarning>
      <head>
        {/* Stable head links are recolored by the palette engine. */}
        <link id="muteki-favicon" rel="icon" href="/favicon.svg" type="image/svg+xml" sizes="any" />
        <link id="muteki-apple-icon" rel="apple-touch-icon" href="/apple-touch-icon.png" sizes="180x180" />
        <link id="muteki-manifest" rel="manifest" href="/manifest.json" />
        <script dangerouslySetInnerHTML={{ __html: 'try{document.documentElement.dataset.motion=localStorage.getItem("muteki.motion")==="reduce"?"reduce":"system"}catch{}' }} />
      </head>
      <body suppressHydrationWarning>
        <SchemeBoot />
        <Providers><WorkspaceAppShell>{children}</WorkspaceAppShell></Providers>
      </body>
    </html>
  );
}
