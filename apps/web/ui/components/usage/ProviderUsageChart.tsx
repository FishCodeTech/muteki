"use client";

import { useId, useMemo, useState, type KeyboardEvent, type PointerEvent } from "react";
import { engineColor, engineName, exactMoney, moneyValue, slotLabel, tokenValue, type UsageSeries } from "@/lib/providerUsage";
import { niceScale } from "./UsageAreaChart";
import styles from "../UsageDashboard.module.css";

const WIDTH = 920;
const HEIGHT = 260;
const DASHES = [undefined, "7 3", "2 3", "10 3 2 3", "5 2 2 2"];

export function ProviderUsageChart({ series, metric, engines }: { series: UsageSeries[]; metric: "cost" | "tokens"; engines: string[] }) {
  const id = useId();
  const [activeIndex, setActiveIndex] = useState<number | null>(null);
  const [focused, setFocused] = useState(false);
  const field = metric === "cost" ? "cost" : "total_tokens";
  const format = metric === "cost" ? moneyValue : tokenValue;
  const chart = useMemo(() => {
    const peak = Math.max(0, ...series.map(row => row[field] ?? 0));
    const scale = niceScale(peak, 4);
    const y = (value: number) => HEIGHT - value / (scale.max || 1) * (HEIGHT - 12);
    const x = (index: number) => metric === "tokens" ? (index + .5) * WIDTH / Math.max(1, series.length) : series.length <= 1 ? WIDTH / 2 : index * WIDTH / (series.length - 1);
    const lines = engines.map((engine, engineIndex) => {
      const segments: { x: number; y: number }[][] = [];
      let segment: { x: number; y: number }[] = [];
      series.forEach((row, index) => {
        const value = row.providers[engine]?.[field];
        if (value == null) { if (segment.length) segments.push(segment); segment = []; }
        else segment.push({ x: x(index), y: y(value) });
      });
      if (segment.length) segments.push(segment);
      return { engine, dash: DASHES[engineIndex % DASHES.length], segments };
    });
    return { ...scale, x, y, lines };
  }, [series, engines, field, metric]);
  const active = activeIndex === null ? null : series[activeIndex];
  const known = engines.length > 0 && series.some(row => row[field] !== null);
  const track = (event: PointerEvent<HTMLDivElement>) => {
    if (series.length === 0) return;
    const bounds = event.currentTarget.getBoundingClientRect();
    const ratio = Math.max(0, Math.min(1, (event.clientX - bounds.left) / bounds.width));
    setActiveIndex(metric === "tokens" ? Math.min(series.length - 1, Math.floor(ratio * series.length)) : Math.round(ratio * (series.length - 1)));
  };
  const navigate = (event: KeyboardEvent<HTMLDivElement>) => {
    if (!series.length) return;
    let next = activeIndex ?? series.length - 1;
    if (event.key === "ArrowRight") next += 1;
    else if (event.key === "ArrowLeft") next -= 1;
    else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = series.length - 1;
    else return;
    event.preventDefault();
    setActiveIndex(Math.max(0, Math.min(series.length - 1, next)));
  };

  return <div className={styles.chartSection}>
    <div className={styles.chartLegend} aria-label="图例">
      {engines.map((engine, index) => <span key={engine}><svg width="18" height="8" aria-hidden="true"><line x1="0" x2="18" y1="4" y2="4" stroke={engineColor(engine)} strokeWidth="2.5" strokeDasharray={metric === "cost" ? DASHES[index % DASHES.length] : undefined} /></svg>{engineName(engine)}</span>)}
    </div>
    {!known ? <div className={styles.chartEmpty}>这个范围暂无已知{metric === "cost" ? "费用" : " Token"}趋势</div> : <>
      <div className={styles.chartFrame}>
        <div className={styles.chartAxis} aria-hidden="true">{chart.ticks.map(tick => <span key={tick} style={{ top: `${chart.y(tick) / HEIGHT * 100}%` }}>{format(tick)}</span>)}</div>
        <div className={styles.chartPlot} tabIndex={0} role="group" aria-label={`${metric === "cost" ? "费用" : "Token"}趋势。使用左右方向键查看相邻时间，Home 和 End 跳至首尾。`} aria-describedby={id}
          onKeyDown={navigate} onFocus={() => { setFocused(true); setActiveIndex(series.length - 1); }} onBlur={() => { setFocused(false); setActiveIndex(null); }}
          onPointerMove={track} onPointerLeave={() => { if (!focused) setActiveIndex(null); }}>
          <svg viewBox={`0 0 ${WIDTH} ${HEIGHT}`} preserveAspectRatio="none" aria-hidden="true" className={styles.chartSvg}>
            {chart.ticks.map(tick => <line key={tick} x1={0} x2={WIDTH} y1={chart.y(tick)} y2={chart.y(tick)} stroke="var(--cx-border-subtle)" strokeDasharray={metric === "tokens" ? "4 4" : undefined} vectorEffect="non-scaling-stroke" />)}
            {metric === "cost" ? chart.lines.map(line => <g key={line.engine}>
              {line.segments.map((points, index) => {
                const path = points.map((point, index) => `${index === 0 ? "M" : "L"}${point.x.toFixed(2)},${point.y.toFixed(2)}`).join(" ");
                return <g key={index}>{points.length > 1 ? <path d={`${path} L${points[points.length - 1].x},${HEIGHT} L${points[0].x},${HEIGHT} Z`} fill={engineColor(line.engine)} fillOpacity=".08" /> : null}
                  <path d={path} fill="none" stroke={engineColor(line.engine)} strokeWidth="2" strokeDasharray={line.dash} vectorEffect="non-scaling-stroke" strokeLinejoin="round" />
                  {points.length === 1 ? <circle cx={points[0].x} cy={points[0].y} r="3" fill={engineColor(line.engine)} /> : null}</g>;
              })}
            </g>) : series.map((row, index) => {
              const width = Math.max(1, WIDTH / series.length * .64);
              let sum = 0;
              return <g key={row.slot}>{engines.map(engine => {
                const value = row.providers[engine]?.total_tokens;
                if (value == null || value === 0) return null;
                sum += value;
                return <rect key={engine} x={chart.x(index) - width / 2} y={chart.y(sum)} width={width} height={HEIGHT - chart.y(value)} fill={engineColor(engine)} opacity=".85" />;
              })}</g>;
            })}
            {activeIndex !== null ? <line x1={chart.x(activeIndex)} x2={chart.x(activeIndex)} y1={0} y2={HEIGHT} stroke="var(--cx-fg-3)" strokeDasharray="3 3" vectorEffect="non-scaling-stroke" /> : null}
          </svg>
          {active ? <div className={styles.chartTooltip} style={{ left: `${Math.max(0, Math.min(100, chart.x(activeIndex!) / WIDTH * 100))}%`, transform: `translateX(${activeIndex! > (series.length - 1) / 2 ? "calc(-100% - 8px)" : "8px"})` }}>
            <strong>{slotLabel(active.slot)}</strong><div className={styles.tooltipTotal}>已知合计 <b title={metric === "cost" ? exactMoney(active[field]) : undefined}>{format(active[field])}</b></div>
            {engines.filter(engine => active.providers[engine]?.[field] != null).map(engine => <div key={engine}><span>{engineName(engine)}</span><b title={metric === "cost" ? exactMoney(active.providers[engine][field]) : undefined}>{format(active.providers[engine][field])}</b></div>)}
          </div> : null}
        </div>
      </div>
      <div className={styles.chartLabels} aria-hidden="true">{[...new Set([0, Math.floor((series.length - 1) / 2), series.length - 1])].map(index => <span key={index}>{slotLabel(series[index].slot)}</span>)}</div>
    </>}
    <p id={id} className={styles.srOnly} aria-live="polite">{active ? `${slotLabel(active.slot)}，已知合计 ${format(active[field])}。${engines.map(engine => `${engineName(engine)} ${format(active.providers[engine]?.[field])}`).join("；")}` : "聚焦图表后使用左右方向键查看数据，也可以展开图表数据表。"}</p>
    {known ? <details className={styles.chartData}><summary>查看图表数据</summary><div className={styles.tableScroll}><table><thead><tr><th>时间</th><th>已知合计</th>{engines.map(engine => <th key={engine}>{engineName(engine)}</th>)}</tr></thead><tbody>{series.map(row => <tr key={row.slot}><th scope="row">{slotLabel(row.slot)}</th><td title={metric === "cost" ? exactMoney(row[field]) : undefined}>{format(row[field])}</td>{engines.map(engine => <td key={engine} title={metric === "cost" ? exactMoney(row.providers[engine]?.[field]) : undefined}>{format(row.providers[engine]?.[field])}</td>)}</tr>)}</tbody></table></div></details> : null}
  </div>;
}
