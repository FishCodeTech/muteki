"use client";

/**
 * /competitions：比赛大厅。
 *
 * 平台连接、凭据轮换 / 撤销、浏览器会话和比赛登记都在这一页完成。
 * 打开具体比赛后进入 /competitions/[id] 工作区。
 */

import { CompetitionCenter } from "@/components/CompetitionCenter";

export default function CompetitionsPage() {
  return <CompetitionCenter />;
}
