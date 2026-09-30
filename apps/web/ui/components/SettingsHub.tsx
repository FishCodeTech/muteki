"use client";

import Link from "next/link";
import { usePathname } from "next/navigation";
import { useEffect, useMemo, useState, type ReactNode } from "react";
import { Button, Modal, SearchField } from "@heroui/react";
import { Icon, type IconName } from "@/components/Icon";
import { useT } from "@/lib/i18n";
import { MotionIcon } from "@/components/MotionIcon";
import { MutekiLogo } from "@/components/MutekiLogo";
import { useSolveOnlyMode } from "@/lib/workspaceMode";

export type SettingsNavItem = {
  id: string;
  href: string;
  labelKey: string;
  descriptionKey: string;
  keywords: string;
  icon: IconName;
};

export type SettingsNavGroup = {
  id: string;
  titleKey: string;
  items: SettingsNavItem[];
};

const SETTINGS_NAV_GROUPS: SettingsNavGroup[] = [
  {
    id: "account",
    titleKey: "settingsHub.group.account",
    items: [
      {
        id: "agents",
        href: "/settings/agents",
        labelKey: "settingsHub.agents",
        descriptionKey: "settingsHub.agentsDesc",
        keywords: "agent provider runtime adapter 引擎 登录 模型 claude codex cursor",
        icon: "plug",
      },
    ],
  },
  {
    id: "conversation-data",
    titleKey: "settingsHub.group.conversationData",
    items: [
      {
        id: "archives",
        href: "/settings/archives",
        labelKey: "settingsHub.archives",
        descriptionKey: "settingsHub.archivesDesc",
        keywords: "对话 归档 历史 搜索 恢复 archive conversation restore",
        icon: "archive",
      },
      {
        id: "import",
        href: "/settings/import",
        labelKey: "settingsHub.import",
        descriptionKey: "settingsHub.importDesc",
        keywords: "导入 历史会话 claude codex provider import",
        icon: "download",
      },
    ],
  },
  {
    id: "runtime",
    titleKey: "settingsHub.group.runtime",
    items: [
      {
        id: "capabilities",
        href: "/settings/capabilities",
        labelKey: "settingsHub.capabilities",
        descriptionKey: "settingsHub.capabilitiesDesc",
        keywords: "能力 mcp skill tool adapter binding grant",
        icon: "network",
      },
      {
        id: "operations",
        href: "/settings/operations",
        labelKey: "settingsHub.operations",
        descriptionKey: "settingsHub.operationsDesc",
        keywords: "运维 operations 诊断 回执 维护 模块 指标",
        icon: "radio",
      },
    ],
  },
  {
    id: "extensions",
    titleKey: "settingsHub.group.extensions",
    items: [
      {
        id: "agent-extensions", href: "/settings/agent-extensions",
        labelKey: "settingsHub.chatPlugins", descriptionKey: "settingsHub.chatPluginsDesc",
        keywords: "聊天 CTF 渗透 Worker 插件 skills mcp plugins", icon: "plug",
      },
      {
        id: "extensions",
        href: "/settings/extensions",
        labelKey: "settingsHub.extensions",
        descriptionKey: "settingsHub.extensionsDesc",
        keywords: "扩展 extension 安装 升级 回滚 catalog",
        icon: "layers",
      },
    ],
  },
  {
    id: "appearance",
    titleKey: "settingsHub.group.appearance",
    items: [
      {
        id: "appearance",
        href: "/settings/appearance",
        labelKey: "settingsHub.appearance",
        descriptionKey: "settingsHub.appearanceDesc",
        keywords: "外观 配色 主题 语言 theme appearance language 亮色 暗色",
        icon: "droplet",
      },
      {
        id: "notifications",
        href: "/settings/notifications",
        labelKey: "settingsHub.notifications",
        descriptionKey: "settingsHub.notificationsDesc",
        keywords: "通知 notification sound 声音 待办 活动 quiet 勿扰 desktop",
        icon: "bell",
      },
    ],
  },
];

function findSettingsItem(pathname: string): SettingsNavItem | null {
  let best: SettingsNavItem | null = null;
  for (const group of SETTINGS_NAV_GROUPS) {
    for (const item of group.items) {
      if (pathname === item.href || pathname.startsWith(`${item.href}/`)) {
        if (!best || item.href.length > best.href.length) best = item;
      }
    }
  }
  return best;
}

function itemMatches(item: SettingsNavItem, needle: string, t: (key: string) => string): boolean {
  if (!needle) return true;
  const haystack = `${t(item.labelKey)} ${t(item.descriptionKey)} ${item.keywords} ${item.href}`.toLowerCase();
  return haystack.includes(needle);
}

export function SettingsHub({ children }: { children: ReactNode }) {
  const pathname = usePathname() || "/settings/agents";
  const t = useT();
  const solveOnly = useSolveOnlyMode();
  const [query, setQuery] = useState("");
  const [navOpen, setNavOpen] = useState(false);
  useEffect(() => {
    const desktop = window.matchMedia("(min-width: 841px)");
    const closeOnDesktop = () => { if (desktop.matches) setNavOpen(false); };
    desktop.addEventListener("change", closeOnDesktop);
    return () => desktop.removeEventListener("change", closeOnDesktop);
  }, []);
  const current = findSettingsItem(pathname);
  const needle = query.trim().toLowerCase();

  const groups = useMemo(() => (
    SETTINGS_NAV_GROUPS
      .map((group) => ({
        ...group,
        items: group.items.filter((item) => (!solveOnly || item.id === "appearance") && itemMatches(item, needle, t)),
      }))
      .filter((group) => group.items.length > 0)
  ), [needle, solveOnly, t]);

  const flush = current?.id === "agents";
  const navigation = <>
        <Link href="/" className="settings-hub-brand" aria-label="返回 Muteki 首页">
          <MutekiLogo size={32} wordmark decorative />
        </Link>
        <Link href={solveOnly ? "/ctf" : "/chat"} className="settings-hub-back">
          <Icon name="chevronRight" size={14} />
          <span>{t("settingsHub.back")}</span>
        </Link>
        <SearchField
          value={query}
          onChange={setQuery}
          aria-label={t("settingsHub.search")}
          fullWidth
          className="is-compact settings-hub-search"
        >
          <SearchField.Group>
            <SearchField.SearchIcon>
              <Icon name="search" size={14} />
            </SearchField.SearchIcon>
            <SearchField.Input placeholder={t("settingsHub.search")} autoComplete="off" spellCheck={false} />
            <SearchField.ClearButton aria-label={t("settingsHub.searchClear")}>
              <Icon name="x" size={13} />
            </SearchField.ClearButton>
          </SearchField.Group>
        </SearchField>
        <nav className="settings-hub-groups">
          {groups.length ? groups.map((group) => (
            <div key={group.id} className="settings-hub-group">
              <h2>{t(group.titleKey)}</h2>
              <ul>
                {group.items.map((item) => {
                  const active = current?.id === item.id;
                  return (
                    <li key={item.id}>
                      <Link
                        href={item.href}
                        className={active ? "on" : ""}
                        aria-current={active ? "page" : undefined}
                        onClick={() => setNavOpen(false)}
                      >
                        <Icon name={item.icon} size={15} />
                        <span>{t(item.labelKey)}</span>
                      </Link>
                    </li>
                  );
                })}
              </ul>
            </div>
          )) : <p className="settings-hub-empty">{t("settingsHub.empty")}</p>}
        </nav>

  </>;

  return (
    <div className="settings-hub" data-nav-open={navOpen ? "true" : "false"} data-page={current?.id || "unknown"}>
      <a className="skip-link" href="#settings-main">{t("settingsHub.skip")}</a>
      <Modal isOpen={navOpen} onOpenChange={setNavOpen}>
        <Modal.Backdrop>
          <Modal.Container size="sm" placement="top">
            <Modal.Dialog className="settings-mobile-dialog" aria-label={t("settingsHub.nav")}>
              <Modal.CloseTrigger aria-label={t("settingsHub.menuClose")} />
              <Modal.Header><Modal.Heading>{t("settingsHub.nav")}</Modal.Heading></Modal.Header>
              <Modal.Body><div id="settings-mobile-nav">{navigation}</div></Modal.Body>
            </Modal.Dialog>
          </Modal.Container>
        </Modal.Backdrop>
      </Modal>
      <aside className="settings-hub-nav" aria-label={t("settingsHub.nav")}>{navigation}</aside>
      <div className="settings-hub-main">
        <header className="settings-hub-pagehead">
          <Button
            size="sm"
            variant="ghost"
            isIconOnly
            className="settings-hub-menu"
            aria-expanded={navOpen}
            aria-controls={navOpen ? "settings-mobile-nav" : undefined}
            aria-label={navOpen ? t("settingsHub.menuClose") : t("settingsHub.menu")}
            onPress={() => setNavOpen((value) => !value)}
          >
            <MotionIcon active={navOpen} from="menu" to="x" />
          </Button>
          <div>
            <h1>{current ? t(current.labelKey) : t("settingsHub.title")}</h1>
            <p>{current ? t(current.descriptionKey) : t("settingsHub.fallbackDesc")}</p>
          </div>
        </header>
        <div
          id="settings-main"
          tabIndex={-1}
          className={`settings-hub-body t-texts-reveal${flush ? " is-flush" : ""}`}
        >
          {children}
        </div>
      </div>
    </div>
  );
}
