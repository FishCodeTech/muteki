"use client";

import { useEffect, useState } from 'react';
import { Button, Input, SegmentedControl, Slider, Switch } from '@/components/chat/ui';
import { writeUiPreferences } from '@/lib/uiPreferences';
import { cn } from '@/lib/cn';
import { useLang } from '@/lib/i18n';
import { setMotionPreference, useMotionPreference } from '@/lib/motionPreference';
import { SCHEMES, applySelection, buildPalette, buildPaletteFromHue, readSavedSelection, type SchemeSelection, type ThemeMode } from '@/lib/palette-engine';
import { subscribeThemePreference, setThemePreference, useThemePreference, type ThemePreference } from '@/lib/themePreference';
import { UI_FONT_SIZES, sanitizeCodeFont, useChatPreferences, writeChatPreferences, type UiFontSize } from '@/lib/chatPreferences';
import { readConversationReadingPrefs, subscribeConversationReadingPrefs, writeConversationReadingPrefs, type ConversationContentWidth, type ConversationFontScale } from '@/lib/conversationReadingPrefs';
import { setSolveOnlyMode, useSolveOnlyMode } from '@/lib/workspaceMode';
import { SettingsRow, SettingsSection } from './primitives';

const SCHEME_NAMES: Record<string, [string, string]> = { azure: ['湛蓝', 'Azure'], violet: ['紫罗兰', 'Violet'], teal: ['青', 'Teal'], ember: ['焦橙', 'Ember'] };

function documentTheme(): ThemeMode {
  return document.documentElement.dataset.theme === 'light' ? 'light' : 'dark';
}

/** The rail toggle and this page both write the theme; the document is the single source of truth. */
function useDocumentTheme(): ThemeMode {
  const [theme, setTheme] = useState<ThemeMode>(documentTheme);
  useEffect(() => {
    const observer = new MutationObserver(() => setTheme(documentTheme()));
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ['data-theme'] });
    return () => observer.disconnect();
  }, []);
  return theme;
}

/** An explicit light/dark choice (rail toggle, desktop menu) stops following the system. */
export function applyTheme(next: ThemeMode, selection: SchemeSelection = readSavedSelection()) {
  setThemePreference(next, selection);
}

function FontFamilyField({ field, value, en }: { field: 'uiFont' | 'codeFont'; value: string; en: boolean }) {
  const [draft, setDraft] = useState(value);
  useEffect(() => setDraft(value), [value]);
  const mono = field === 'codeFont';
  const commit = () => { const next = sanitizeCodeFont(draft); if (next !== value) writeChatPreferences({ [field]: next }); setDraft(next); };
  return <div className="flex items-center gap-2">
    <Input size="sm" className={cn('w-[220px]', mono && 'font-cx-mono')} value={draft}
      placeholder={mono ? (en ? 'System monospace' : '系统等宽字体') : (en ? 'System font' : '系统字体')}
      aria-label={mono ? (en ? 'Code font family' : '代码字体') : (en ? 'Interface font family' : '界面字体')}
      onChange={event => setDraft(event.target.value)} onBlur={commit} onKeyDown={event => { if (event.key === 'Enter') commit(); }} />
    {value ? <Button size="sm" variant="ghost" onClick={() => { setDraft(''); writeChatPreferences({ [field]: '' }); }}>{en ? 'Reset' : '恢复默认'}</Button> : null}
  </div>;
}

export function AppearanceSettings() {
  const { lang, setLang } = useLang(); const en = lang === 'en';
  const t = (zh: string, english: string) => en ? english : zh;
  const theme = useDocumentTheme();
  const themePreference = useThemePreference();
  const prefs = useChatPreferences();
  const solveOnly = useSolveOnlyMode();
  const motion = useMotionPreference();
  const [selection, setSelection] = useState<SchemeSelection>(readSavedSelection);
  const [reading, setReading] = useState(readConversationReadingPrefs);
  useEffect(() => subscribeConversationReadingPrefs(setReading), []);

  useEffect(() => subscribeThemePreference(() => setSelection(readSavedSelection())), []);
  const choose = (next: SchemeSelection) => { writeUiPreferences({accent: next}); setSelection(next); applySelection(next, theme); };
  const hue = Math.round(selection.kind === 'custom' ? selection.hue : SCHEMES.find(scheme => scheme.id === selection.id)?.hue ?? 268);
  const customAccent = buildPaletteFromHue(hue, theme)['--accent'];
  const fontScales: ConversationFontScale[] = ['sm', 'md', 'lg'];

  return <>
    <SettingsSection title={t('界面', 'Interface')}>
      <SettingsRow anchor="appearance-language" title={t('界面语言', 'Language')} description={t('只影响界面文案，不影响 Agent 输出。', 'Changes interface text only, not Agent output.')}
        control={<SegmentedControl size="md" ariaLabel={t('界面语言', 'Language')} value={lang} onChange={value => setLang(value)} options={[{ value: 'zh', label: '中文' }, { value: 'en', label: 'English' }]} />} />
      <SettingsRow anchor="appearance-theme" title={t('主题', 'Theme')} description={t('“跟随系统”会随系统外观实时切换；左侧栏底部的按钮会切回固定主题。', '“System” follows the OS appearance live; the rail toggle switches back to a fixed theme.')}
        control={<SegmentedControl<ThemePreference> size="md" ariaLabel={t('主题', 'Theme')} value={themePreference} onChange={next => setThemePreference(next, selection)} options={[{ value: 'light', label: t('亮色', 'Light'), icon: 'sun' }, { value: 'dark', label: t('暗色', 'Dark'), icon: 'moon' }, { value: 'system', label: t('跟随系统', 'System'), icon: 'monitor' }]} />} />
      <SettingsRow anchor="appearance-ui-font" title={t('界面字号', 'Interface text size')} description={t('调整界面基础字号；对话正文字号在下方“对话阅读”中设置。', 'Base size for interface text. Chat message size is under Reading below.')}
        control={<SegmentedControl<string> size="md" ariaLabel={t('界面字号', 'Interface text size')} value={String(prefs.uiFontSize)} onChange={value => writeChatPreferences({ uiFontSize: Number(value) as UiFontSize })}
          options={UI_FONT_SIZES.map(size => ({ value: String(size), label: `${size}px` }))} />} />
      <SettingsRow anchor="appearance-interface-font" title={t('界面字体', 'Interface font')} description={t('用于界面和对话正文。填写字体名称，如 "Inter"；未安装或缺少中文字形时回退到系统字体。', 'Used for the interface and chat text. Enter a font name such as "Inter"; falls back to the system font when it is missing or lacks CJK glyphs.')}
        control={<FontFamilyField field="uiFont" value={prefs.uiFont} en={en} />} />
      <SettingsRow anchor="appearance-code-font" title={t('代码字体', 'Code font')} description={t('用于代码块、Diff 等等宽文本（终端除外）。填写字体名称，如 "JetBrains Mono"；未安装时回退到系统等宽字体。', 'Used for code blocks, diffs and other monospace text (not the terminal). Enter a font name such as "JetBrains Mono"; falls back to the system monospace font.')}
        control={<FontFamilyField field="codeFont" value={prefs.codeFont} en={en} />} />
      <SettingsRow anchor="appearance-motion" title={t('减少动态效果', 'Reduce motion')} description={t('关闭过渡与展开动画。系统的“减少动态效果”设置始终优先。', 'Turns off transitions and expand animations. The system reduce-motion setting always wins.')}
        control={<Switch ariaLabel={t('减少动态效果', 'Reduce motion')} checked={motion === 'reduce'} onCheckedChange={on => setMotionPreference(on ? 'reduce' : 'system')} />} />
      <SettingsRow anchor="appearance-workspaces" title={t('显示对话和比赛工作区', 'Show chat and competition workspaces')} description={t('默认只显示做题模式。开启后，首页、导航和搜索会加入对话与比赛工作区。', 'Only the solve workspace is shown by default. Turn this on to add chat and competition workspaces to the home page, navigation and search.')}
        control={<Switch ariaLabel={t('显示对话和比赛工作区', 'Show chat and competition workspaces')} checked={!solveOnly} onCheckedChange={on => setSolveOnlyMode(!on)} />} />
    </SettingsSection>

    <SettingsSection anchor="appearance-accent" title={t('主色', 'Accent color')} description={t('主色生成全套强调色，状态色保持不变。', 'The accent generates the highlight palette; status colors stay fixed.')}>
      <div className="grid grid-cols-2 gap-2 p-4 sm:grid-cols-4">
        {SCHEMES.map(scheme => {
          const accent = buildPalette(scheme.id, theme)['--accent'];
          const on = selection.kind === 'preset' && selection.id === scheme.id;
          return <button key={scheme.id} type="button" aria-pressed={on} onClick={() => choose({ kind: 'preset', id: scheme.id })}
            className={cn('cx-press flex items-center gap-2.5 rounded-xl border px-3 py-2.5 text-left outline-none transition-colors focus-visible:outline-2 focus-visible:outline-[var(--cx-focus)]',
              on ? 'border-cx-accent bg-cx-accent-soft' : 'border-cx-border hover:bg-cx-hover')}>
            <span className="size-4 shrink-0 rounded-full" style={{ background: accent }} />
            <span className="min-w-0 text-[13px] font-medium text-cx-fg">{SCHEME_NAMES[scheme.id]?.[en ? 1 : 0] ?? scheme.id}</span>
          </button>;
        })}
      </div>
      <SettingsRow title={t('自定义色相', 'Custom hue')} description={selection.kind === 'custom' ? `${hue}° · ${customAccent}` : t('拖动滑杆选择任意主色。', 'Drag to pick any accent.')}
        control={<span className="block size-6 rounded-full border border-cx-border" style={{ background: customAccent }} aria-hidden />}>
        <div className="cx-hue-slider"><Slider min={0} max={359} value={hue} onValueChange={value => choose({ kind: 'custom', hue: value })} /></div>
      </SettingsRow>
    </SettingsSection>

    <SettingsSection anchor="appearance-reading" title={t('对话阅读', 'Reading')} description={t('只影响聊天正文。', 'Applies to chat messages only.')}>
      <SettingsRow title={t('正文字号', 'Text size')} control={<SegmentedControl<ConversationFontScale> size="md" ariaLabel={t('正文字号', 'Text size')} value={reading.fontScale} onChange={fontScale => writeConversationReadingPrefs({ fontScale })}
        options={fontScales.map(value => ({ value, label: value === 'sm' ? t('小', 'Small') : value === 'md' ? t('标准', 'Default') : t('大', 'Large') }))} />} />
      <SettingsRow title={t('内容宽度', 'Content width')} control={<SegmentedControl<ConversationContentWidth> size="md" ariaLabel={t('内容宽度', 'Content width')} value={reading.contentWidth} onChange={contentWidth => writeConversationReadingPrefs({ contentWidth })}
        options={[{ value: 'narrow', label: t('窄', 'Narrow') }, { value: 'default', label: t('标准', 'Default') }, { value: 'wide', label: t('宽', 'Wide') }]} />} />
      <SettingsRow title={t('紧凑排版', 'Compact layout')} description={t('缩小消息之间的间距。', 'Tightens spacing between messages.')}
        control={<Switch ariaLabel={t('紧凑排版', 'Compact layout')} checked={reading.density === 'compact'} onCheckedChange={on => writeConversationReadingPrefs({ density: on ? 'compact' : 'comfortable' })} />} />
    </SettingsSection>
  </>;
}
