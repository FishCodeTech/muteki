import type { BlackboardVulnReport } from "@/lib/events";

const MISSING = "（未填写）";

export type CvssRating = "critical" | "high" | "medium" | "low";

const FINDING_CLASS_LABELS: Record<string, string> = {
  sqli: "SQL 注入",
  xss: "跨站脚本",
  rce: "远程代码执行",
  idor: "越权",
  ssrf: "服务端请求伪造",
  csrf: "跨站请求伪造",
  lfi: "本地文件包含",
  upload: "文件上传",
  generic: "其他",
  other: "其他",
};

const CVSS_RATING_LABELS: Record<CvssRating, string> = {
  critical: "严重",
  high: "高危",
  medium: "中危",
  low: "低危",
};

type CvssPair = readonly [number, string];

const CVSS_BY_CLASS: Record<string, readonly [CvssPair, CvssPair]> = {
  rce: [
    [9.8, "AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"],
    [8.8, "AV:N/AC:L/PR:L/UI:N/S:U/C:H/I:H/A:H"],
  ],
  upload: [
    [9.8, "AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"],
    [8.8, "AV:N/AC:L/PR:L/UI:N/S:U/C:H/I:H/A:H"],
  ],
  sqli: [
    [9.1, "AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:N"],
    [8.1, "AV:N/AC:L/PR:L/UI:N/S:U/C:H/I:H/A:N"],
  ],
  ssrf: [
    [9.8, "AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H"],
    [8.8, "AV:N/AC:L/PR:L/UI:N/S:U/C:H/I:H/A:H"],
  ],
  idor: [
    [7.5, "AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:N/A:N"],
    [6.5, "AV:N/AC:L/PR:L/UI:N/S:U/C:H/I:N/A:N"],
  ],
  lfi: [
    [7.5, "AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:N/A:N"],
    [6.5, "AV:N/AC:L/PR:L/UI:N/S:U/C:H/I:N/A:N"],
  ],
  xss: [
    [6.1, "AV:N/AC:L/PR:N/UI:R/S:C/C:L/I:L/A:N"],
    [6.1, "AV:N/AC:L/PR:N/UI:R/S:C/C:L/I:L/A:N"],
  ],
  csrf: [
    [6.5, "AV:N/AC:L/PR:N/UI:R/S:U/C:N/I:H/A:N"],
    [6.5, "AV:N/AC:L/PR:N/UI:R/S:U/C:N/I:H/A:N"],
  ],
  generic: [
    [5.3, "AV:N/AC:L/PR:N/UI:N/S:U/C:L/I:N/A:N"],
    [5.3, "AV:N/AC:L/PR:L/UI:N/S:U/C:L/I:N/A:N"],
  ],
  other: [
    [5.3, "AV:N/AC:L/PR:N/UI:N/S:U/C:L/I:N/A:N"],
    [5.3, "AV:N/AC:L/PR:L/UI:N/S:U/C:L/I:N/A:N"],
  ],
};

const UNAUTH_HINTS = [
  "unauthenticated", "unauth", "without authentication", "no authentication",
  "no login", "anonymous", "public endpoint", "未授权", "无需认证", "无需登录",
  "公开端点", "无需登陆",
];

export type CvssEstimate = {
  score: number;
  vector: string;
  rating: CvssRating;
  label: string;
  badge: string;
  estimated: true;
};

export const REPRO_INTENT_PREFIX = "I-repro-";

export function reproIntentId(reportId: string): string {
  return `${REPRO_INTENT_PREFIX}${reportId}`;
}

function inlineText(value: string | undefined): string {
  return (value ?? "").replace(/\r\n/g, "\n").split(/\s+/).join(" ").trim();
}

function blockText(value: string | undefined): string {
  return (value ?? "").replace(/\r\n/g, "\n").trim();
}

function orMissing(value: string | undefined): string {
  const text = blockText(value);
  return text || MISSING;
}

export function findingClassLabel(cls: string | undefined): string {
  const key = (cls ?? "").trim().toLowerCase();
  if (!key) return MISSING;
  return FINDING_CLASS_LABELS[key] ?? cls ?? MISSING;
}

export function reportLocationLabel(resourceId: string | undefined): string {
  const raw = (resourceId ?? "").trim();
  if (!raw) return "";
  try {
    const url = new URL(raw);
    const path = `${url.pathname}${url.search}`;
    if (path && path !== "/") return path;
    return raw;
  } catch {
    return raw;
  }
}

function cvssRating(score: number): CvssRating {
  if (score >= 9.0) return "critical";
  if (score >= 7.0) return "high";
  if (score >= 4.0) return "medium";
  return "low";
}

function requiresPrivileges(row: Pick<BlackboardVulnReport, "preconditions" | "title" | "affectedRole">): boolean {
  const blob = `${row.preconditions ?? ""} ${row.title ?? ""} ${row.affectedRole ?? ""}`.toLowerCase();
  return !UNAUTH_HINTS.some((hint) => blob.includes(hint));
}

export function estimateCvss(row: Pick<BlackboardVulnReport, "findingClass" | "preconditions" | "title" | "affectedRole">): CvssEstimate {
  const cls = (row.findingClass ?? "").trim().toLowerCase();
  const pair = CVSS_BY_CLASS[cls] ?? CVSS_BY_CLASS.other;
  const [score, vector] = requiresPrivileges(row) ? pair[1] : pair[0];
  const rating = cvssRating(score);
  const label = CVSS_RATING_LABELS[rating];
  return {
    score,
    vector,
    rating,
    label,
    badge: `估算 · ${label} ${score.toFixed(1)}`,
    estimated: true,
  };
}

export function reportSummary(row: BlackboardVulnReport): string {
  const narrative = blockText(row.narrative);
  if (narrative) return narrative;
  return [row.impactWho, row.impactWhat].map((part) => blockText(part)).filter(Boolean).join("。");
}

function looksLikeReportMarkdown(markdown: string): boolean {
  if (!markdown.startsWith("# ")) return false;
  return markdown.includes("## 漏洞概要")
    && markdown.includes("**严重程度**")
    && markdown.includes("## 复现步骤")
    && markdown.includes("## PoC")
    && markdown.includes("## 证明输出")
    && markdown.includes("## 影响");
}

function rewriteEstimatedCvssLines(markdown: string): string {
  return markdown
    .replace(/^- \*\*严重程度\*\*：([^\n]+)$/m, (_all, label: string) => {
      const text = String(label).replace(/\s*（类型估算）\s*$/, "").trim();
      return `- **严重程度**：${text}（类型估算）`;
    })
    .replace(/^- \*\*CVSS 3\.1\*\*：/m, "- **参考向量**：");
}

export function reportToMarkdown(row: BlackboardVulnReport): string {
  const existing = blockText(row.markdown);
  if (looksLikeReportMarkdown(existing)) {
    const rewritten = rewriteEstimatedCvssLines(existing);
    return rewritten.endsWith("\n") ? rewritten : `${rewritten}\n`;
  }
  const title = inlineText(row.title) || "未命名漏洞";
  const cls = inlineText(row.findingClass);
  const typeLine = cls ? `${findingClassLabel(cls)} (\`${cls}\`)` : findingClassLabel(cls);
  const resource = inlineText(row.resourceId) || MISSING;
  const command = blockText(row.replayCommand) || `# ${MISSING}`;
  const proof = blockText(row.witness) || MISSING;
  const steps = row.steps?.map((step) => blockText(step)).filter(Boolean) ?? [];
  const who = orMissing(row.impactWho);
  const what = orMissing(row.impactWhat);
  const cvss = estimateCvss(row);
  const lines = [
    `# ${title}`,
    "",
    "## 漏洞概要",
    "",
    `- **类型**：${typeLine}`,
    `- **位置**：\`${resource}\``,
    `- **严重程度**：${cvss.label}（类型估算）`,
    `- **参考向量**：${cvss.score.toFixed(1)}（\`${cvss.vector}\`）`,
    `- **先决条件**：${orMissing(row.preconditions)}`,
    `- **影响对象**：${orMissing(row.affectedRole)}`,
  ];
  const narrative = blockText(row.narrative);
  if (narrative) lines.push("", narrative);
  lines.push("", "## 复现步骤", "");
  if (steps.length) {
    steps.forEach((step, index) => lines.push(`${index + 1}. ${step}`));
  } else {
    lines.push(MISSING);
  }
  lines.push(
    "",
    "## PoC",
    "",
    "```bash",
    command,
    "```",
    "",
    "## 证明输出",
    "",
    "```",
    proof,
    "```",
    "",
    "## 影响",
    "",
    `${who} ${what}`.trim(),
    "",
  );
  return lines.join("\n");
}

function demoteHeadings(markdown: string, index: number): string {
  const lines: string[] = [];
  let inFence = false;
  let firstTitle = true;
  for (const line of markdown.split("\n")) {
    const stripped = line.trimStart();
    if (stripped.startsWith("```")) {
      inFence = !inFence;
      lines.push(line);
      continue;
    }
    if (!inFence) {
      if (firstTitle && line.startsWith("# ")) {
        lines.push(`## ${index}. ${line.slice(2)}`);
        firstTitle = false;
        continue;
      }
      if (line.startsWith("#")) {
        lines.push(`#${line}`);
        continue;
      }
    }
    lines.push(line);
  }
  return lines.join("\n");
}

export function reportsToCollectionMarkdown(
  rows: BlackboardVulnReport[],
  title = "漏洞报告集",
): string {
  const accepted = rows.filter((row) => row.status === "accepted");
  const heading = inlineText(title) || "漏洞报告集";
  if (!accepted.length) return `# ${heading}\n\n（尚无已入库报告）\n`;
  const bodies = accepted.map((row, index) => demoteHeadings(reportToMarkdown(row), index + 1));
  return [`# ${heading}`, "", `共 ${accepted.length} 份已入库报告。`, "", bodies.join("\n\n---\n\n"), ""].join("\n");
}
