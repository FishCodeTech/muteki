import type { SettingsNavGroup } from "./primitives";

export function taskManagementGroups(en: boolean): SettingsNavGroup[] {
  return [{
    id: "workspace-settings",
    label: en ? "Workspace settings" : "工作区设置",
    items: [
      {
        id: "ctf-settings",
        href: "/ctf/workers?return=%2Fsettings",
        label: en ? "CTF workbench" : "CTF 工作台",
        icon: "crosshair",
        trailingIcon: "externalLink",
        keywords: "CTF 工作台 完整设置 workbench settings",
      },
      {
        id: "pentest-settings",
        href: "/pentest/workers?return=%2Fsettings",
        label: en ? "Penetration testing" : "渗透测试",
        icon: "target",
        trailingIcon: "externalLink",
        keywords: "Pentest 渗透 测试 完整设置 settings",
      },
    ],
  }];
}
