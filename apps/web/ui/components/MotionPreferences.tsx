"use client";

import { Radio, RadioGroup } from "@heroui/react";
import { useLang } from "@/lib/i18n";
import { setMotionPreference, useMotionPreference } from "@/lib/motionPreference";

export function MotionPreferences({ clientContext = "web" }: { clientContext?: "web" | "desktop" }) {
  const preference = useMotionPreference();
  const { lang } = useLang();
  const zh = lang === "zh";
  return (
    <section className="wappearance-card wappearance-choice-card" aria-labelledby="appearance-motion">
      <header><h3 id="appearance-motion">{zh ? "动态效果" : "Motion"}</h3></header>
      <p>{zh ? `调整整个工作台的过渡、展开和加载动效。系统的减少动态效果设置始终优先，选择会保存在${clientContext === "desktop" ? "当前桌面客户端" : "此浏览器"}。` : `Adjust transitions, panels and loading effects across the workspace. Your system’s reduced motion setting always takes priority. Saved in ${clientContext === "desktop" ? "this desktop client" : "this browser"}.`}</p>
      <RadioGroup className="wappearance-modes" orientation="horizontal" value={preference}
        onChange={(value) => setMotionPreference(value === "reduce" ? "reduce" : "system")}
        aria-labelledby="appearance-motion">
        {(["system", "reduce"] as const).map((value) => (
          <Radio key={value} value={value} className={preference === value ? "on" : ""}>
            <Radio.Content><Radio.Control><Radio.Indicator /></Radio.Control>
              {value === "system" ? (zh ? "跟随系统" : "Follow system") : (zh ? "减少动态效果" : "Reduce motion")}
            </Radio.Content>
          </Radio>
        ))}
      </RadioGroup>
    </section>
  );
}
