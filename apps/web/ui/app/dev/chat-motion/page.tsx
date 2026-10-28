import type { Metadata } from "next";
import { notFound } from "next/navigation";
import { ChatMotionLab } from "@/components/conversation/dev/ChatMotionLab";

export const metadata: Metadata = { title: "动效实验室", robots: { index: false, follow: false } };

export default function ChatMotionPage() {
  if (process.env.NODE_ENV === "production") notFound();
  return <ChatMotionLab />;
}
