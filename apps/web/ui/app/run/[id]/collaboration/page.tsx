"use client";

import dynamic from "next/dynamic";
import Link from "next/link";
import { useParams, useRouter } from "next/navigation";
import { useCallback, useEffect, useRef, useState } from "react";
import { Button, Chip, Modal, Tabs, toast } from "@heroui/react";

import { CollaborationSkeleton } from "@/components/collaboration/CollaborationSkeleton";
import { Icon } from "@/components/Icon";
import { clearActiveRunWorkspace, publishActiveRunWorkspace } from "@/lib/activeRunWorkspace";
import { readCollabUrlState } from "@/lib/collabUrlState";
import { isRunActive, swarmDigest, workerIds } from "@/lib/events";
import { useT } from "@/lib/i18n";
import {
  asRunWorkspaceMode,
  conversationHref,
  reportHref,
  runtimeHref,
  type RunWorkspaceMode,
} from "@/lib/runWorkspaceRoutes";
import { killWorker, spawnWorker, useRun } from "@/lib/useRun";
import { toWorkerIdentity, workerDisplayName } from "@/lib/workers";

/**
 * Standalone collaboration page (/run/<id>/collaboration): the agent map gets
 * the whole workspace instead of a runtime-panel tab. The canvas component is
 * the same one the runtime panel used to embed; only the shell around it is new.
 * Cross-page jumps (worker lanes / timeline / evidence / PoC / report) go back
 * to the conversation route with `?view=…&<focus>=…`, which the deck reads on
 * mount (see components/RunWorkbench.tsx). Hrefs follow deck.mode so pentest
 * stays on /pentest?run=… instead of bouncing through /run/<id> + redirect.
 */

const AgentCollaborationCanvas = dynamic(
  () => import("@/components/AgentCollaborationCanvas").then((module) => module.AgentCollaborationCanvas),
  { ssr: false, loading: () => <CollaborationSkeleton /> },
);

type FocusTarget = { id: string; nonce: number };

export default function Page() {
  return <CollaborationPage />;
}

function CollaborationPage() {
  const t = useT();
  const router = useRouter();
  const params = useParams<{ id?: string | string[] }>();
  const rawId = Array.isArray(params?.id) ? params.id[0] : params?.id;
  const runId = rawId ? decodeURIComponent(rawId) : "";
  const { deck, connected } = useRun(runId);
  const running = isRunActive(deck);
  const loading = !deck.started;
  const mode: RunWorkspaceMode = asRunWorkspaceMode(deck.mode) ?? "ctf";
  const [killConfirm, setKillConfirm] = useState<{ id: string; label: string } | null>(null);
  const [killBusy, setKillBusy] = useState(false);
  // A deep link with ?agent= / ?k= is read once so the inspector opens on it
  // even when the canvas restores its own remembered selection for this run.
  const initialFocus = useRef<{ agent: FocusTarget | null; knowledge: FocusTarget | null } | null>(null);
  if (initialFocus.current === null) {
    const url = readCollabUrlState();
    initialFocus.current = {
      agent: url.agentId ? { id: url.agentId, nonce: 1 } : null,
      knowledge: url.knowledgeId ? { id: url.knowledgeId, nonce: 1 } : null,
    };
  }

  useEffect(() => {
    if (!runId || !deck.started) return;
    publishActiveRunWorkspace(runId, mode);
    return () => clearActiveRunWorkspace(runId);
  }, [runId, deck.started, mode]);

  const go = useCallback((href: string) => router.push(href), [router]);
  const toRuntime = useCallback(
    (view: string, focus?: [string, string | number]) => runtimeHref(runId, mode, view, focus),
    [runId, mode],
  );
  const conversationPath = conversationHref(runId, mode);
  const onSpawnWorker = async (engine?: string) => {
    if (!runId) return;
    const ok = await spawnWorker(runId, engine);
    toast(t(ok ? "toast.workerSpawned" : "toast.actionFailed"), { variant: ok ? "success" : "danger", indicator: <Icon name="cpu" size={15} /> });
  };
  const onKillWorker = (solverId: string) => {
    const siblings = workerIds(deck).map((id) => toWorkerIdentity(id, deck.lanes[id]));
    setKillConfirm({ id: solverId, label: workerDisplayName(solverId, toWorkerIdentity(solverId, deck.lanes[solverId]), siblings).title });
  };
  const confirmKill = async () => {
    if (!killConfirm || killBusy || !runId) return;
    setKillBusy(true);
    try {
      const ok = await killWorker(runId, killConfirm.id);
      toast(t(ok ? "toast.workerKilled" : "toast.actionFailed"), { variant: ok ? "default" : "danger", indicator: <Icon name="cpu" size={15} /> });
    } finally {
      setKillBusy(false);
      setKillConfirm(null);
    }
  };

  const digest = swarmDigest(deck);
  const stateClass = deck.preparing ? "live" : digest.phase === "paused" ? "paused" : running ? "live" : "done";
  const stateLabel = deck.preparing ? t("convo.preparing") : digest.phase === "paused" ? t("convo.paused") : running ? t("convo.live") : t("convo.finished");
  const title = deck.challengeName || t("convo.run");

  return (
    <div className="collab-page">
      <div className="collab-page-bar">
        <Link href={conversationPath} className="collab-page-back" aria-label={t("collabPage.back")}>
          <Icon name="chevronLeft" size={15} /><span>{t("collabPage.back")}</span>
        </Link>
        <div className="collab-page-context">
          <span className="collab-page-title" title={title}>{title}</span>
          {runId && <Chip size="sm" variant="secondary" className="rid"><span>sessions/{runId}</span></Chip>}
        </div>
        <Tabs
          selectedKey="collaboration"
          onSelectionChange={(key) => {
            if (key === "conversation") go(conversationPath);
            else if (key === "runtime") go(toRuntime("timeline"));
            else if (key === "report" && mode === "pentest") go(reportHref(runId));
          }}
        >
          <Tabs.List className="convo-view-switch" aria-label={t("collabPage.nav")}>
            <Tabs.Tab id="conversation"><Icon name="rows" size={13} /><span>{t("convo.viewConversation")}</span><Tabs.Indicator /></Tabs.Tab>
            <Tabs.Tab id="runtime"><Icon name="panel" size={13} /><span>{t("convo.viewRuntime")}</span><Tabs.Indicator /></Tabs.Tab>
            <Tabs.Tab id="collaboration"><Icon name="network" size={13} /><span>{t("convo.viewCollaboration")}</span><Tabs.Indicator /></Tabs.Tab>
            {mode === "pentest" ? (
              <Tabs.Tab id="report"><Icon name="rows" size={13} /><span>报告</span><Tabs.Indicator /></Tabs.Tab>
            ) : null}
          </Tabs.List>
        </Tabs>
        <div className="collab-page-status">
          {deck.started && (
            <Chip size="sm" variant={stateClass === "done" ? "secondary" : "soft"} color={stateClass === "live" ? "success" : stateClass === "paused" ? "warning" : "default"} className={`runstate ${stateClass}`}>
              <span className={`runstate-dot ${stateClass}`} aria-hidden="true" /><span>{stateLabel}</span>
            </Chip>
          )}
          <span className={`dot ${connected ? "on" : !deck.started || deck.finished ? "idle" : "off"}`} role="img" aria-label={connected ? t("convo.connected") : deck.finished ? t("convo.finished") : t("convo.disconnected")} />
        </div>
      </div>
      <div className="collab-page-body" aria-busy={loading || undefined}>
        {loading ? <CollaborationSkeleton /> : (
          <AgentCollaborationCanvas
            deck={deck}
            running={running}
            onKillWorker={onKillWorker}
            onSpawnWorker={onSpawnWorker}
            onOpenTimeline={(id) => go(toRuntime("timeline", ["speaker", id]))}
            onOpenWorker={(id) => go(toRuntime("workers", ["worker", id]))}
            onOpenFact={(seq) => go(toRuntime("evidence", ["fact", seq]))}
            onOpenPoc={(id) => go(toRuntime("pocs", ["poc", id]))}
            focusAgent={initialFocus.current.agent}
            focusKnowledge={initialFocus.current.knowledge}
          />
        )}
      </div>
      <Modal isOpen={Boolean(killConfirm)} onOpenChange={(open) => !open && setKillConfirm(null)}>
        <Modal.Backdrop isDismissable={!killBusy}><Modal.Container><Modal.Dialog>
          <Modal.Header className="flex flex-col gap-1"><Modal.Heading>{t("worker.confirmKillTitle")}</Modal.Heading><small className="font-normal text-muted">{t("worker.confirmKill")}</small></Modal.Header>
          <Modal.Body>{killConfirm ? <div className="action-dialog-summary"><span><strong>{killConfirm.label}</strong></span><code>{killConfirm.id}</code></div> : null}</Modal.Body>
          <Modal.Footer><Button variant="ghost" onPress={() => setKillConfirm(null)} isDisabled={killBusy}>{t("dialog.cancel")}</Button><Button variant="danger" isPending={killBusy} onPress={() => void confirmKill()}>{t("worker.killTitle")}</Button></Modal.Footer>
        </Modal.Dialog></Modal.Container></Modal.Backdrop>
      </Modal>
    </div>
  );
}
