import type { Metadata } from "next";
import "./globals.css";
import SchemeBoot from "../components/SchemeBoot";

export const metadata: Metadata = {
  title: "Project Muteki — Command Deck",
  description: "Observe and command the autonomous CTF solver swarm.",
};

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en" data-theme="dark">
      <body>
        <SchemeBoot />
        {children}
      </body>
    </html>
  );
}
