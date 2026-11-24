"use client";


import { useEffect, useState } from "react";
import { useParams } from "next/navigation";
import { CompetitionWorkspace } from "@/components/CompetitionWorkspace";
import type { CompetitionInfo } from "@/lib/competition-events";
import { fetchCompetitions } from "@/lib/useCompetition";

function CompetitionPageInner() {
  const params = useParams<{ id: string }>();
  const competitionId = decodeURIComponent(String(params?.id ?? ""));
  const [competitions, setCompetitions] = useState<CompetitionInfo[]>([]);

  const [listError, setListError] = useState("");
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    setListError("");
    let cancelled = false;
    fetchCompetitions()
      .then((list) => {
        if (!cancelled) setCompetitions(list);
      })
      .catch((error: unknown) => {
        if (!cancelled) setListError(error instanceof Error ? error.message || "请求失败" : String(error) || "请求失败");
      });
    return () => {
      cancelled = true;
    };
  }, [attempt]);

  if (!competitionId) return null;
  return (
    <CompetitionWorkspace
      competitionId={competitionId}
      listError={listError}
      retryList={() => setAttempt((value) => value + 1)}
      competitions={competitions.map((c) => ({
        competition_id: c.competition_id,
        title: c.title,
        external_competition_id: c.external_competition_id,
      }))}
    />
  );
}

export default function CompetitionPage() {
  return <CompetitionPageInner />;
}
