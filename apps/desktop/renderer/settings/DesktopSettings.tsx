import { useEffect } from 'react';
import { SettingsHost, type SettingsNavigation } from '@/components/settings/SettingsHost';
import { SettingsContent } from '@/components/settings/SettingsContent';
import { settingsPageFromPath, settingsRedirect } from '@/components/settings/catalog';
export interface DesktopSettingsProps {
  pathname: string; navigation: SettingsNavigation; go(href: string, replace?: boolean): void;
  origin: string; connected: boolean; onConfigure(): void; onHelp(): void; onBack(): void;
}
export function DesktopSettings({pathname, navigation, go, origin, connected, onConfigure, onHelp, onBack}: DesktopSettingsProps) {
  const redirect = settingsRedirect(pathname, navigation.searchParams);
  useEffect(() => { if (redirect) go(redirect + (navigation.hash || ''), true); }, [redirect, navigation.hash, go]);
  const active = settingsPageFromPath(pathname);
  if (redirect) return <div role="status" className="p-8">正在打开设置…</div>;
  return <SettingsHost client="desktop" navigation={navigation} desktop={{origin, connected, onConfigure, onHelp}} back={{onClick: onBack}}>
    {!redirect && (active ? <SettingsContent page={active} /> : <p role="alert">此设置页面不存在。</p>)}
  </SettingsHost>;
}
