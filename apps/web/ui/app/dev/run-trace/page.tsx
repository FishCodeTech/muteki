import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { RunTraceLab } from "@/components/runtime-lab/RunTraceLab";

export const metadata: Metadata = { title: "活动时间线实验室", robots: { index: false, follow: false } };

export default function RunTracePage() {
  if (process.env.NODE_ENV === "production") notFound();
  return <RunTraceLab />;
}
