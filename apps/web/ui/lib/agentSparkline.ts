/**
 * 12 × 30s activity / token-rate buckets for collaboration cards.
 * Window end is the agent's own finish (or the run's, or now) so a completed
 * worker still shows the last six minutes of its life rather than an empty
 * window anchored at Date.now().
 */

import { toEpochMs } from "./format";
import type { AgentRuntimeFeedEvent, ChatMessage, CostHistorySample } from "./events";

export const SPARK_BUCKETS = 12;
export const SPARK_BUCKET_MS = 30_000;
export const RUNTIME_EVENT_CAP = 500;

export type CostSample = CostHistorySample;

export function sparklineWindowEnd(
  agent: { endedAt?: number; finishedAt?: number },
  deckFinishedAt: number | undefined,
  now: number,
): number {
  return toEpochMs(agent.endedAt) || toEpochMs(agent.finishedAt) || toEpochMs(deckFinishedAt) || now;
}

export function indexActivityTimestamps(
  runtimeEvents: AgentRuntimeFeedEvent[],
  chat: ChatMessage[],
): Map<string, number[]> {
  const byId = new Map<string, number[]>();
  const push = (id: string | undefined, ts: number) => {
    if (!id || !ts) return;
    const list = byId.get(id);
    if (list) list.push(ts);
    else byId.set(id, [ts]);
  };
  for (const event of runtimeEvents) push(event.solverId, event.ts);
  for (const message of chat) push(message.solverId, message.ts);
  return byId;
}

export function bucketCounts(
  timestamps: number[],
  end: number,
  buckets = SPARK_BUCKETS,
  bucketMs = SPARK_BUCKET_MS,
): number[] {
  const start = end - buckets * bucketMs;
  const counts = Array<number>(buckets).fill(0);
  for (const ts of timestamps) {
    const ms = toEpochMs(ts);
    if (ms < start || ms > end) continue;
    counts[Math.min(buckets - 1, Math.floor((ms - start) / bucketMs))] += 1;
  }
  return counts;
}

/** Per-bucket token delta from a cumulative cost series. */
export function bucketTokenDiffs(
  history: CostSample[],
  end: number,
  buckets = SPARK_BUCKETS,
  bucketMs = SPARK_BUCKET_MS,
): number[] {
  const start = end - buckets * bucketMs;
  const diffs = Array<number>(buckets).fill(0);
  const lastInBucket: Array<number | undefined> = Array(buckets);
  let prev = 0;
  const ordered = history.slice().sort((a, b) => toEpochMs(a.ts) - toEpochMs(b.ts));
  for (const sample of ordered) {
    const ms = toEpochMs(sample.ts);
    if (ms < start) prev = sample.tokens;
    else if (ms <= end) lastInBucket[Math.min(buckets - 1, Math.floor((ms - start) / bucketMs))] = sample.tokens;
  }
  for (let i = 0; i < buckets; i += 1) {
    const last = lastInBucket[i];
    if (last === undefined) continue;
    diffs[i] = Math.max(0, last - prev);
    prev = last;
  }
  return diffs;
}

/** True when the global 500-event cap has dropped samples that belong in this window. */
export function isSparklinePartial(events: Array<{ ts: number }>, windowStart: number): boolean {
  if (events.length < RUNTIME_EVENT_CAP) return false;
  let oldest = Infinity;
  for (const event of events) {
    const ms = toEpochMs(event.ts);
    if (ms && ms < oldest) oldest = ms;
  }
  return oldest > windowStart;
}

export type AgentSparklineSeries = {
  events: number[];
  tokens: number[];
  partial: boolean;
};

/** 12-bucket event and token-rate series keyed by agent id. */
export function sparklineSeriesByAgent(
  agents: Array<{ id: string; endedAt?: number; finishedAt?: number }>,
  runtimeEvents: AgentRuntimeFeedEvent[],
  chat: ChatMessage[],
  costHistory: Record<string, CostHistorySample[]>,
  deckFinishedAt: number | undefined,
  now: number,
): Map<string, AgentSparklineSeries> {
  const index = indexActivityTimestamps(runtimeEvents, chat);
  const result = new Map<string, AgentSparklineSeries>();
  for (const agent of agents) {
    const end = sparklineWindowEnd(agent, deckFinishedAt, now);
    const start = end - SPARK_BUCKETS * SPARK_BUCKET_MS;
    result.set(agent.id, {
      events: bucketCounts(index.get(agent.id) ?? [], end),
      tokens: bucketTokenDiffs(costHistory[agent.id] ?? [], end),
      partial: isSparklinePartial(runtimeEvents, start),
    });
  }
  return result;
}
