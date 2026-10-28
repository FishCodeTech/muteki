"use client";

import { useMemo, useState, type CSSProperties } from "react";
import { ActivityConsole } from "@/components/activity-timeline/ActivityConsole";
import { SCENARIOS, type ScenarioId } from "./traceMock";
import "./runtimeLab.css";

const WIDTHS = [
  { id: "full", label: "全宽", value: "100%" },
  { id: "mid", label: "900px", value: "900px" },
  { id: "narrow", label: "480px", value: "480px" },
] as const;

const KEYS: [string, string][] = [
  ["空格", "播放/暂停"],
  ["j / k", "上/下一条事件"],
  ["[ / ]", "上/下一个失败"],
  ["L", "回到最新"],
  ["/", "搜索"],
  ["Esc", "逐级清除"],
  ["z", "放大选中行"],
  ["⌘/Ctrl + 滚轮", "缩放时间轴"],
];

export function RunTraceLab() {
  const [scenarioId, setScenarioId] = useState<ScenarioId>("standard");
  const [mode, setMode] = useState<"live" | "replay">("live");
  const [autoFold, setAutoFold] = useState(true);
  const [reduced, setReduced] = useState(false);
  const [width, setWidth] = useState<(typeof WIDTHS)[number]["id"]>("full");
  const [restart, setRestart] = useState(0);

  const scenario = useMemo(() => SCENARIOS.find((item) => item.id === scenarioId)!.build(), [scenarioId]);

  return (
    <main className="rtl-page">
      <div className="rtl-bar">
        <h1>活动时间线实验室</h1>
        <label className="rtl-group">
          场景
          <select value={scenarioId} onChange={(event) => setScenarioId(event.target.value as ScenarioId)}>
            {SCENARIOS.map((item) => (
              <option key={item.id} value={item.id}>{item.title}</option>
            ))}
          </select>
        </label>
        <span className="rtl-group" role="group" aria-label="数据来源">
          <button type="button" className="rtl-btn" aria-pressed={mode === "live"} onClick={() => setMode("live")}>实时运行</button>
          <button type="button" className="rtl-btn" aria-pressed={mode === "replay"} onClick={() => setMode("replay")}>历史回放</button>
        </span>
        <span className="rtl-group" role="group" aria-label="容器宽度">
          {WIDTHS.map((item) => (
            <button type="button" key={item.id} className="rtl-btn" aria-pressed={width === item.id} onClick={() => setWidth(item.id)}>{item.label}</button>
          ))}
        </span>
        <button type="button" className="rtl-btn" aria-pressed={autoFold} onClick={() => setAutoFold((value) => !value)}>完成后自动折叠</button>
        <button type="button" className="rtl-btn" aria-pressed={reduced} onClick={() => setReduced((value) => !value)}>减少动效</button>
        <button type="button" className="rtl-btn" onClick={() => setRestart((value) => value + 1)}>重新开始</button>
        <small>全部数据为本页自造，不读取任何运行记录</small>
      </div>
      <div className="rtl-keys" aria-label="快捷键">
        {KEYS.map(([key, label]) => (
          <span key={key}><kbd>{key}</kbd>{label}</span>
        ))}
      </div>
      <div className="rtl-stage" style={{ "--rtl-width": WIDTHS.find((item) => item.id === width)!.value } as CSSProperties}>
        <ActivityConsole
          scenario={scenario}
          clock={mode === "live" ? { mode: "live", cap: scenario.duration } : { mode: "replay", startAt: "begin" }}
          resetKey={`${scenario.id}:${mode}:${restart}`}
          autoFold={autoFold}
          reducedMotion={reduced}
        />
      </div>
    </main>
  );
}
