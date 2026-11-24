"use client";
import { useEffect, useState, type PointerEvent } from 'react';
import chroma from 'chroma-js';
import { generateColorScale, SHADE_STEPS } from '@/lib/color-scale';
import { Input } from "@muteki/ui";

export function AccentColorPicker({ value, onPreview, onChange, en }: {
  value: string; onPreview(hex: string): void; onChange(hex: string): void; en: boolean;
}) {
  const [hsv, setHsv] = useState(() => chroma(value).hsv());
  const [draft, setDraft] = useState(value);
  const [invalid, setInvalid] = useState(false);
  useEffect(() => { setHsv(chroma(value).hsv()); setDraft(value); setInvalid(false); }, [value]);
  const h = Number.isFinite(hsv[0]) ? hsv[0] : 0;
  const hex = chroma.hsv(h, hsv[1], hsv[2]).hex();
  const scale = generateColorScale(hex);
  const update = (next: [number, number, number], commit = false) => {
    const color = chroma.hsv(...next).hex(); setHsv(next); setDraft(color); setInvalid(false);
    if (commit) onChange(color); else onPreview(color);
  };
  const move = (event: PointerEvent<HTMLDivElement>, commit = false) => {
    const bounds = event.currentTarget.getBoundingClientRect();
    update([h, Math.max(0, Math.min(1, (event.clientX - bounds.left) / bounds.width)), Math.max(0, Math.min(1, 1 - (event.clientY - bounds.top) / bounds.height))], commit);
  };
  const commitHex = () => {
    if (!/^#?[0-9a-f]{6}$/i.test(draft)) { setInvalid(true); return; }
    const next = draft.startsWith('#') ? draft : `#${draft}`;
    setHsv(chroma(next).hsv()); setDraft(next); setInvalid(false); onChange(next);
  };
  return <div className="accent-picker">
    <div className="accent-picker-plane" role="group" aria-label={en ? 'Saturation and brightness; use the sliders below for keyboard control' : '饱和度与明度；键盘操作请使用下方滑杆'}
      style={{ backgroundColor: chroma.hsv(h, 1, 1).hex() }}
      onPointerDown={event => { event.currentTarget.setPointerCapture(event.pointerId); move(event); }}
      onPointerMove={event => { if (event.currentTarget.hasPointerCapture(event.pointerId)) move(event); }}
      onPointerUp={event => { move(event, true); event.currentTarget.releasePointerCapture(event.pointerId); }}
      onPointerCancel={() => onChange(hex)}>
      <span className="accent-picker-thumb" style={{ left: `${hsv[1] * 100}%`, top: `${(1 - hsv[2]) * 100}%`, background: hex }} />
    </div>
    <div className="accent-picker-controls">
      <label>{en ? 'Hue' : '色相'}<input aria-label={en ? 'Hue' : '色相'} className="accent-picker-hue" type="range" min="0" max="359" value={h} onChange={e => update([Number(e.target.value), hsv[1], hsv[2]], true)} /></label>
      <label>{en ? 'Saturation' : '饱和度'}<input type="range" min="0" max="100" value={Math.round(hsv[1] * 100)} onChange={e => update([h, Number(e.target.value) / 100, hsv[2]], true)} /><output>{Math.round(hsv[1] * 100)}%</output></label>
      <label>{en ? 'Brightness' : '明度'}<input type="range" min="0" max="100" value={Math.round(hsv[2] * 100)} onChange={e => update([h, hsv[1], Number(e.target.value) / 100], true)} /><output>{Math.round(hsv[2] * 100)}%</output></label>
      <label>HEX<span className="accent-picker-sample" style={{ background: hex }} /><Input className="w-32 flex-none font-cx-mono" aria-label="HEX" invalid={invalid} value={draft} onChange={e => { setDraft(e.target.value); setInvalid(false); }} onBlur={commitHex} onKeyDown={e => { if (e.key === 'Enter') commitHex(); if (e.key === 'Escape') { setDraft(value); setInvalid(false); } }} /></label>
      {invalid ? <p role="alert">{en ? 'Enter a six-digit HEX color, e.g. #6366f1.' : '请输入六位 HEX 颜色，例如 #6366f1。'}</p> : null}
    </div>
    <div className="accent-picker-scale" aria-label={en ? 'Color scale' : '颜色阶梯'}>{SHADE_STEPS.map(step => <div key={step}><span style={{ background: scale[step] }} /><strong>{step}</strong><code>{scale[step]}</code></div>)}</div>
  </div>;
}
