"use client";

import { useState } from "react";

/**
 * Launch a run — a "new project" form, scoped to a CTF challenge spec.
 * Two modes: a real swarm (needs MUTEKI_DEEPSEEK_API_KEY on the backend) or a
 * keyless mock stream for UI/e2e. The body shape matches drivers.build_driver.
 */
export function LaunchForm({
  onStart,
  disabled,
}: {
  onStart: (body: Record<string, any>) => void;
  disabled?: boolean;
}) {
  const [name, setName] = useState("target");
  const [category, setCategory] = useState("web");
  const [target, setTarget] = useState("http://127.0.0.1:8000");
  const [description, setDescription] = useState("Solve the web challenge.");
  const [hints, setHints] = useState("");
  const [nSolvers, setNSolvers] = useState(2);

  const launch = (kind: "swarm" | "mock") => {
    if (kind === "mock") {
      onStart({ kind: "mock" });
      return;
    }
    const desc = hints.trim()
      ? `${description.trim()}\n\nHints:\n${hints.trim()}`
      : description.trim();
    onStart({
      kind: "swarm",
      n_solvers: nSolvers,
      challenge: { name: name.trim() || "target", category, description: desc, target: target.trim() || undefined },
    });
  };

  return (
    <div className="launch">
      <div className="launch-grid">
        <label>
          <span>Name</span>
          <input value={name} onChange={(e) => setName(e.target.value)} placeholder="challenge name" />
        </label>
        <label>
          <span>Category</span>
          <select value={category} onChange={(e) => setCategory(e.target.value)}>
            {["web", "crypto", "rev", "pwn", "forensics", "misc"].map((c) => (
              <option key={c} value={c}>{c}</option>
            ))}
          </select>
        </label>
        <label className="wide">
          <span>Target (origin / URL / host)</span>
          <input value={target} onChange={(e) => setTarget(e.target.value)} placeholder="http://host:port or host" />
        </label>
        <label className="wide">
          <span>Goal / description</span>
          <textarea value={description} onChange={(e) => setDescription(e.target.value)} rows={2}
            placeholder="what does solving look like? what's the objective?" />
        </label>
        <label className="wide">
          <span>Hints (optional, one per line)</span>
          <textarea value={hints} onChange={(e) => setHints(e.target.value)} rows={2}
            placeholder="known facts to seed the swarm with" />
        </label>
        <label>
          <span>Solvers</span>
          <input type="number" min={1} max={6} value={nSolvers}
            onChange={(e) => setNSolvers(Math.max(1, Math.min(6, Number(e.target.value) || 1)))} />
        </label>
      </div>
      <div className="launch-actions">
        <button className="primary" disabled={disabled} onClick={() => launch("swarm")}>▶ Launch swarm</button>
        <button disabled={disabled} onClick={() => launch("mock")} title="keyless demo stream">Run mock</button>
      </div>
    </div>
  );
}
