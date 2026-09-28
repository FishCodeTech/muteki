export type NavItem = { href: string; label: string; badge: string; icon: 'chat' | 'task' | 'pentest' | 'competition' | 'extension' };
export type DesktopState = {
  origin: string; status: 'idle' | 'connecting' | 'connected' | 'error'; message: string; configuring: boolean;
  platform: string; theme: 'light' | 'dark'; items: NavItem[]; active: string; pageTitle: string;
  canGoBack: boolean; canGoForward: boolean; search: boolean; sidebar: boolean; themeToggle: boolean;
  sidebarCollapsed: boolean; usage: boolean; settingsHref: string;
};
export interface DesktopBridge {
  getState(): Promise<DesktopState>;
  connect(origin: string): Promise<DesktopState>;
  configure(): Promise<void>;
  resume(): Promise<void>;
  navigate(href: string): Promise<void>;
  action(name: string): Promise<void>;
  windowAction(name: string): Promise<void>;
  onState(callback: (state: DesktopState) => void): () => void;
}
declare global { interface Window { mutekiDesktop: DesktopBridge } }
