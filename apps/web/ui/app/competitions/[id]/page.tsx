"use client";

/**
 * /competitions/[id]：比赛工作区（COMP-10，设计 14.2）。
 *
 * 页面负责比赛清单加载；共享布局负责顶栏与登录，工作区本体在
 * components/CompetitionWorkspace.tsx。子 Run 详情仍走 /run/[id]，其
 * SSE 由子 Run 页面自己订阅，本页只维护一条比赛 SSE（设计 14.3）。
 */

import { useEffect, useState } from "react";
import { useParams } from "next/navigation";
import { CompetitionWorkspace } from "@/components/CompetitionWorkspace";
import type { CompetitionInfo } from "@/lib/competition-events";
import { fetchCompetitions } from "@/lib/useCompetition";

function CompetitionPageInner() {
  const params = useParams<{ id: string }>();
  const competitionId = decodeURIComponent(String(params?.id ?? ""));
  const [competitions, setCompetitions] = useState<CompetitionInfo[]>([]);

  useEffect(() => {
    let cancelled = false;
    fetchCompetitions()
      .then((list) => {
        if (!cancelled) setCompetitions(list);
      })
      .catch(() => {
        /* 左栏列表加载失败不影响工作区主体 */
      });
    return () => {
      cancelled = true;
    };
  }, []);

  if (!competitionId) return null;
  return (
    <CompetitionWorkspace
      competitionId={competitionId}
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
