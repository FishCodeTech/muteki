import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { P0GateLab } from "@/components/dev/P0GateLab";

export const metadata: Metadata = {
  title: "Muteki UI 内部预览",
  robots: { index: false, follow: false },
};

export default function MutekiUiPreviewPage() {
  if (process.env.NODE_ENV === "production" && process.env.MUTEKI_ENABLE_UI_PREVIEW !== "1") {
    notFound();
  }
  return <P0GateLab />;
}
