import type { Metadata } from "next";
import "./globals.css";

export const metadata: Metadata = {
  title: "Project Muteki — Command Deck",
  description: "Observe and command the autonomous CTF solver swarm.",
};

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  );
}
