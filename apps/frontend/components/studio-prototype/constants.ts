import type { StudioReportTemplate } from "./model";

export const STUDIO_DEFAULT_THREAD_ID = "src-demo-thread-baidu";
export const STUDIO_DEFAULT_PROJECT_ID = "src-demo-baidu";

export const STUDIO_TEAM = [
  "资料负责人（演示）",
  "人工复核组（演示）",
  "资产整理 Agent（模拟）",
  "证据整理 Agent（模拟）",
];

export const REPORT_TEMPLATES: StudioReportTemplate[] = [
  {
    id: "bsrc",
    name: "百度安全应急响应中心 (BSRC) 标准模板",
    platform: "BSRC",
    version: "2026.2",
    requiredFields: ["漏洞标题", "目标与归属系统", "漏洞类型与CVSS", "前置条件与最低权限", "详细复现步骤", "原值PoC与请求报文", "业务影响说明", "修复建议"],
    description: "适配国内大型企业 SRC 提交流程，要求区分已证实影响与条件性影响，并附带独立可复现的原始请求字节与截图。",
  },
  {
    id: "general_src",
    name: "中国企业 SRC 通用交付模板",
    platform: "通用企业 SRC",
    version: "1.4",
    requiredFields: ["标题", "受影响资产", "机制说明", "逐步复现", "决定性证据", "修复建议"],
    description: "包含规范结构、主张映射、反证说明与陌生审阅者复现清单，支持一键转换到各平台。",
  },
  {
    id: "butian",
    name: "补天众测 / 专属 SRC 报告模板",
    platform: "补天平台",
    version: "3.1",
    requiredFields: ["厂商与域名", "漏洞等级", "漏洞描述", "复现步骤與PoC", "截图附件", "修复方案"],
    description: "强调资产归属备案证明、复现步骤清晰度与无害化验证说明。",
  },
  {
    id: "edusrc",
    name: "教育行业漏洞报告平台 (EduSRC) 模板",
    platform: "EduSRC",
    version: "2.0",
    requiredFields: ["单位名称", "受影响URL/系统", "漏洞详情", "测试时间与IP", "修复建议"],
    description: "要求明确记录测试时间窗口、仅只读验证承诺及测试数据恢复结果。",
  },
  {
    id: "hackerone-en-v1",
    name: "HackerOne Submission Standard (English)",
    platform: "HackerOne",
    version: "1.0",
    requiredFields: ["Title", "Asset & Endpoint", "Vulnerability Details & Root Cause", "Steps to Reproduce", "Verified Impact", "Remediation"],
    description: "Concise, zero-fluff English report tailored for HackerOne triage: raw HTTP artifacts, verified vs conditional impact, and concrete fix.",
  },
  {
    id: "bugcrowd-en-v1",
    name: "Bugcrowd VRT Standard (English)",
    platform: "Bugcrowd",
    version: "1.0",
    requiredFields: ["Submission Title", "Target URL / VRT Scope", "Technical Root Cause", "Technical Walkthrough (PoC)", "Business Impact", "Fix Recommendation"],
    description: "Structured English walkthrough aligned with Bugcrowd VRT triage criteria and reproducible HTTP PoC evidence.",
  },
  {
    id: "bilingual-src-v1",
    name: "国际 / 合资 SRC 中英双语交付模板",
    platform: "Global / Bilingual SRC",
    version: "1.0",
    requiredFields: ["漏洞标题 / Title", "受影响端点 / Target", "根因机制 / Root Cause", "复现步骤 / Steps", "已证实影响 / Impact", "修复方案 / Remediation"],
    description: "面向跨国企业与双语审核团队的确定性中英对照报告模板。",
  },
];
