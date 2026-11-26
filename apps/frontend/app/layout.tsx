import type { Metadata, Viewport } from "next";
import "./globals.css";
import "@/styles/transitions/_root.css";
import "@/styles/transitions/index.css";
import "@/styles/transitions/surfaces.css";
import "@/styles/transitions/accessibility.css";
import SchemeBoot from "../components/SchemeBoot";
import { Providers } from "./providers";
import { DesktopConnection } from "@/components/DesktopConnection";
import { DesktopQuitDialog } from "@/components/DesktopQuitDialog";
import { WorkspaceAppShell } from "@/components/WorkspaceAppShell";
import { APP_NAME, APP_ICON_PATHS, BRAND_VERSION } from "@/lib/brand";

export const metadata: Metadata = {
  title: {
    default: `${APP_NAME} — Agent 安全工作台`,
    template: `%s · ${APP_NAME}`,
  },
  description: "管理 Agent 对话、单项安全任务和比赛调度。",
  appleWebApp: {
    capable: true,
    statusBarStyle: "black-translucent",
    title: APP_NAME,
  },
};

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
    <html data-muteki-ui-build={process.env.NEXT_PUBLIC_MUTEKI_UI_BUILD || "development"} data-muteki-appearance-contract="1" lang="zh-CN" data-theme="dark" className="dark" suppressHydrationWarning>
      <head>
        <script dangerouslySetInnerHTML={{ __html: `try{var m="";if(/(?:^|\\s)MutekiDesktop\\/[\\w.+-]+(?:\\s|$)/.test(navigator.userAgent)){var a=JSON.parse(localStorage.getItem("muteki.desktop.appearanceOverride")||"null");if(a&&(a.resolvedTheme==="light"||a.resolvedTheme==="dark"))m=a.resolvedTheme}if(!m){var p=localStorage.getItem("muteki.themePreference")||localStorage.getItem("muteki.theme");m=p==="light"||p==="dark"?p:window.matchMedia&&window.matchMedia("(prefers-color-scheme: light)").matches?"light":"dark"}var r=document.documentElement;r.dataset.theme=m;r.classList.toggle("dark",m==="dark");r.classList.toggle("light",m==="light")}catch{}` }} />
        <link id="muteki-favicon" rel="icon" href={`/favicon.svg?v=${BRAND_VERSION}`} type="image/svg+xml" sizes="any" />
        <link id="muteki-apple-icon" rel="apple-touch-icon" href={`${APP_ICON_PATHS[180]}?v=${BRAND_VERSION}`} sizes="180x180" />
        <link id="muteki-manifest" rel="manifest" href="/manifest.json" />
        <script dangerouslySetInnerHTML={{ __html: 'try{document.documentElement.dataset.motion=localStorage.getItem("muteki.motion")==="reduce"?"reduce":"system"}catch{}' }} />
        <script dangerouslySetInnerHTML={{ __html: 'try{var l=localStorage.getItem("muteki.lang");var v=l==="en"?"en":"zh-CN";document.documentElement.lang=v;document.documentElement.setAttribute("xml:lang",v)}catch{}' }} />
      </head>
      <body suppressHydrationWarning>
        <SchemeBoot />
        <Providers><DesktopConnection><WorkspaceAppShell>{children}</WorkspaceAppShell></DesktopConnection><DesktopQuitDialog /></Providers>
      </body>
    </html>
  );
}
