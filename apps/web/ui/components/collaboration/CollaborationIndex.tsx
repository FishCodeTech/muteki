"use client";

import { Dropdown } from "@heroui/react";
import { useCallback, useState, type ReactNode } from "react";

import { Icon } from "@/components/Icon";
import type { CollaborationKnowledgeItem, CollaborationKnowledgeKind } from "@/lib/agentCollaboration";
import { LIST_LIMITS } from "@/lib/agentCollaborationLayout";
import type { BlackboardIntent } from "@/lib/events";
import type { RunCanvasMode } from "@/lib/swarmProjection";
import { useT } from "@/lib/i18n";
import { SPAWN_ENGINES } from "@/lib/workers";

import { EventStamp } from "./EventStamp";
import { KnowledgeKindChips } from "./KnowledgeKindFilter";
import { KnowledgeRow } from "./KnowledgeRow";
import { useFeedUnread } from "./useFeedUnread";

function QueueSpawnControl({ onSpawnWorker }: { onSpawnWorker: (engine?: string) => void }) {
  const t = useT();
  return (
    <Dropdown>
      <Dropdown.Trigger className="collab-queue-spawn" aria-label={t("collab.action.spawn")}>
        <Icon name="plus" size={12} />
        <span>{t("collab.action.spawn")}</span>
      </Dropdown.Trigger>
      <Dropdown.Popover placement="bottom end" className="collab-context-menu">
        <Dropdown.Menu aria-label={t("collab.spawn.engine")} onAction={(key) => {
          const engine = String(key);
          onSpawnWorker(engine === "auto" ? undefined : engine);
        }}>
          <Dropdown.Item id="auto" textValue={t("collab.spawn.auto")}>{t("collab.spawn.auto")}</Dropdown.Item>
          {SPAWN_ENGINES.map((engine) => (
            <Dropdown.Item key={engine} id={engine} textValue={engine}>{engine}</Dropdown.Item>
          ))}
        </Dropdown.Menu>
      </Dropdown.Popover>
    </Dropdown>
  );
}

export function CollaborationIndex({
  totalIntents,
  totalKnowledge,
  intents,
  knowledge,
  hasQuery,
  selectedKnowledgeId,
  actorName,
  running,
  kindCounts,
  knowledgeKinds,
  onToggleKnowledgeKind,
  canvasMode,
  onSpawnWorker,
  onSelectIntent,
  onSelectKnowledge,
  onClosePanel,
  id,
  resizer,
  resetKey,
}: {
  totalIntents: number;
  totalKnowledge: number;
  intents: BlackboardIntent[];
  knowledge: CollaborationKnowledgeItem[];
  hasQuery: boolean;
  selectedKnowledgeId: string | null;
  actorName: (id?: string) => string;
  running: boolean;
  kindCounts?: Map<CollaborationKnowledgeKind, number>;
  knowledgeKinds?: Set<CollaborationKnowledgeKind>;
  onToggleKnowledgeKind?: (kind: CollaborationKnowledgeKind) => void;
  canvasMode?: RunCanvasMode;
  onSpawnWorker?: (engine?: string) => void;
  onSelectIntent: (intent: BlackboardIntent) => void;
  onSelectKnowledge: (itemId: string) => void;
  onClosePanel?: () => void;
  id?: string;
  resizer?: ReactNode;
  resetKey: string;
}) {
  const t = useT();
  // Page the feed in LIST_LIMITS.knowledge steps so a long run stays scrollable.
  const [visibleLimit, setVisibleLimit] = useState<number>(LIST_LIMITS.knowledge);
  const { listRef, lastSeenTs, freshCount, showJump, onScroll, jumpToLatest, markSeen } = useFeedUnread(knowledge, resetKey);
  const onSelectRow = useCallback((itemId: string) => {
    markSeen();
    onSelectKnowledge(itemId);
  }, [markSeen, onSelectKnowledge]);
  // An empty queue folds to its heading (count kept, hint inline) so the
  // knowledge list takes the remaining height.
  const queueEmpty = intents.length === 0;
  const visibleKnowledge = knowledge.slice(0, visibleLimit);
  return (
    <aside className="collab-index" id={id} aria-label={t("collab.index")}>
      {resizer}
      <div className="collab-panel-head">
        <span><Icon name="list" size={14} /><strong>{t("collab.index")}</strong></span>
        <button type="button" className="collab-panel-close" aria-label={t("collab.hideIndex")} onClick={onClosePanel}>
          <Icon name="x" size={14} />
        </button>
      </div>
      <section className={`collab-index-section queue ${queueEmpty ? "empty" : ""}`}>
        <h2>
          <span>{t(canvasMode === "ctf" ? "collab.queueCtf" : "collab.queue")}</span>
          {queueEmpty && !(running && totalIntents > 0 && onSpawnWorker) && (
            <small>{hasQuery ? t("collab.noMatches") : t(canvasMode === "ctf" ? "collab.queueEmptyCtf" : "collab.queueEmpty")}</small>
          )}
          {running && totalIntents > 0 && onSpawnWorker && (
            <QueueSpawnControl onSpawnWorker={onSpawnWorker} />
          )}
          <b>{totalIntents}</b>
        </h2>
        {!queueEmpty && (
          <div className="collab-queue-list">
            {intents.map((intent) => {
              const selected = selectedKnowledgeId === `intent:${intent.id}`;
              return (
                <button
                  type="button"
                  key={intent.id}
                  className={`collab-queue-row ${selected ? "selected" : ""}`}
                  aria-current={selected ? "true" : undefined}
                  onClick={() => onSelectIntent(intent)}
                >
                  <span><Icon name="target" size={12} />{intent.workerClass}</span>
                  <strong>{intent.summary || intent.goal}</strong>
                  <small>{intent.id} · <EventStamp ts={intent.proposedTs} /></small>
                </button>
              );
            })}
          </div>
        )}
      </section>
      <section className="collab-index-section knowledge">
        <h2><span>{t("collab.knowledgeFeed")}</span><b>{totalKnowledge}</b></h2>
        {showJump && (
          <button type="button" className="collab-feed-new" onClick={jumpToLatest}>
            {t("collab.feedNew", { n: freshCount })}
          </button>
        )}
        {kindCounts && knowledgeKinds && onToggleKnowledgeKind && (
          <KnowledgeKindChips counts={kindCounts} selected={knowledgeKinds} onToggle={onToggleKnowledgeKind} mode={canvasMode} />
        )}
        <div className="collab-index-list" ref={listRef} onScroll={onScroll}>
          {visibleKnowledge.length ? (
            <>
              {visibleKnowledge.map((item) => (
                <KnowledgeRow
                  key={item.id}
                  item={item}
                  actor={actorName(item.agentId)}
                  selected={selectedKnowledgeId === item.id}
                  fresh={item.ts > lastSeenTs}
                  onSelect={onSelectRow}
                  mode={canvasMode}
                />
              ))}
              {knowledge.length > visibleLimit && (
                <button
                  type="button"
                  className="collab-index-more"
                  onClick={() => setVisibleLimit((n) => n + LIST_LIMITS.knowledge)}
                >
                  {t("collab.knowledgeMore", { shown: visibleKnowledge.length, total: knowledge.length })}
                </button>
              )}
            </>
          ) : (
            <div className="collab-index-empty">
              {hasQuery ? t("collab.noMatches") : (
                <>
                  <strong>{t("collab.knowledgeEmpty")}</strong>
                  <span>{t("collab.knowledgeEmptyHint")}</span>
                </>
              )}
            </div>
          )}
        </div>
      </section>
    </aside>
  );
}
