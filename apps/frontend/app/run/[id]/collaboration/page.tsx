"use client";

import dynamic from "next/dynamic";
import Link from "next/link";
import { useParams, useRouter } from "next/navigation";
import { useCallback, useEffect, useRef, useState } from "react";
import { Button, Dialog, Tabs, TabsList, TabsTrigger, toast } from "@muteki/ui";

import { CollaborationSkeleton } from "@/components/collaboration/CollaborationSkeleton";
import { Icon } from "@muteki/ui";
import { RunCategoryTag } from "@/components/run-view/RunCategoryTag";
import { RunStatePill } from "@/components/run-view/RunStatePill";
import { clearActiveRunWorkspace, publishActiveRunWorkspace } from "@/lib/activeRunWorkspace";
import { readCollabUrlState } from "@/lib/collabUrlState";
import { isRunActive, workerIds } from "@/lib/events";
import { useT } from "@/lib/i18n";
import {
  asRunWorkspaceMode,
  conversationHref,
  reportHref,
  runtimeHref,
  type RunWorkspaceMode,
} from "@/lib/runWorkspaceRoutes";
import { killWorker, spawnWorker, useRun } from "@/lib/useRun";
import { CTF_DEMO_NOTICE, ctfDemoDeck } from "@/lib/ctfDemoRun";
import { toWorkerIdentity, workerDisplayName } from "@/lib/workers";


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
  const demoDeck = ctfDemoDeck(runId);
  const liveRun = useRun(demoDeck ? "" : runId);
  const deck = demoDeck ?? liveRun.deck;
  const connected = demoDeck ? true : liveRun.connected;
  const demoNotice = () => toast(CTF_DEMO_NOTICE, { icon: "info" });
  const running = isRunActive(deck);
  const loading = !deck.started;
  const mode: RunWorkspaceMode = asRunWorkspaceMode(deck.mode) ?? "ctf";
  const [killConfirm, setKillConfirm] = useState<{ id: string; label: string } | null>(null);
  const [killBusy, setKillBusy] = useState(false);
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
  useEffect(() => {
    if (deck.driverKind === "local_pi" && conversationPath) {
      router.replace(conversationPath);
    }
  }, [deck.driverKind, conversationPath, router]);
  const onSpawnWorker = async (engine?: string) => {
    if (!runId) return;
    if (demoDeck) { demoNotice(); return; }
    const ok = await spawnWorker(runId, engine);
    if (ok === null) return;
    toast(t(ok ? "toast.workerSpawned" : "toast.actionFailed"), { tone: ok ? "success" : "danger", icon: "cpu" });
  };
  const onKillWorker = (solverId: string) => {
    if (demoDeck) { demoNotice(); return; }
    const siblings = workerIds(deck).map((id) => toWorkerIdentity(id, deck.lanes[id]));
    setKillConfirm({ id: solverId, label: workerDisplayName(solverId, toWorkerIdentity(solverId, deck.lanes[solverId]), siblings).title });
  };
  const confirmKill = async () => {
    if (!killConfirm || killBusy || !runId) return;
    setKillBusy(true);
    try {
      const ok = await killWorker(runId, killConfirm.id);
      toast(t(ok ? "toast.workerKilled" : "toast.actionFailed"), { tone: ok ? "neutral" : "danger", icon: "cpu" });
    } finally {
      setKillBusy(false);
      setKillConfirm(null);
    }
  };

  const title = deck.challengeName || t("convo.run");

  return (
    <div className="collab-page">
      <div className="collab-page-bar">
        <Link href={conversationPath} className="collab-page-back" aria-label={t("collabPage.back")} title={t("collabPage.back")}>
          <Icon name="chevronLeft" size={15} />
        </Link>
        <div className="collab-page-context">
          <span className="collab-page-title" title={title}>{title}</span>
          {mode === "ctf" && <RunCategoryTag category={deck.category} />}
        </div>
        <Tabs
          value="collaboration"
          variant="segmented"
          size="sm"
          onChange={(key: string) => {
            if (key === "conversation") go(conversationPath);
            else if (key === "runtime") go(toRuntime("timeline"));
            else if (key === "report" && mode === "pentest") go(reportHref(runId));
          }}
        >
          <TabsList className="convo-view-switch" ariaLabel={t("collabPage.nav")}>
            <TabsTrigger value="conversation" icon="rows">{t("convo.viewConversation")}</TabsTrigger>
            <TabsTrigger value="runtime" icon="panel">{t("convo.viewRuntime")}</TabsTrigger>
            <TabsTrigger value="collaboration" icon="network">{t("convo.viewCollaboration")}</TabsTrigger>
            {mode === "pentest" ? (
              <TabsTrigger value="report" icon="rows">报告</TabsTrigger>
            ) : null}
          </TabsList>
        </Tabs>
        <div className="collab-page-status">
          <RunStatePill deck={deck} running={running} connected={connected} />
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
      <Dialog
        open={Boolean(killConfirm)}
        onOpenChange={(open) => !open && setKillConfirm(null)}
        dismissable={!killBusy}
        className="cx-settings-dialog"
        size="sm"
        tone="danger"
        title={t("worker.confirmKillTitle")}
        description={t("worker.confirmKill")}
        footer={<>
          <Button variant="ghost" onClick={() => setKillConfirm(null)} disabled={killBusy}>{t("dialog.cancel")}</Button>
          <Button variant="danger" agentConfirm loading={killBusy} onClick={() => void confirmKill()}>{t("worker.killTitle")}</Button>
        </>}
      >
        {killConfirm ? <div className="action-dialog-summary"><span><strong>{killConfirm.label}</strong></span><code>{killConfirm.id}</code></div> : null}
      </Dialog>
    </div>
  );
}
