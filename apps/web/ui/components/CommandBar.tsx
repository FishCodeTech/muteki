"use client";

import { useState } from "react";

/** HITL command input (§14.3 #7): send a hint / pause / submit / reject to the
 *  global run or a specific solver. Syntax: "/action rest" or plain hint text. */
export function CommandBar({ onSend }: { onSend: (target: string, action: string, text: string) => void }) {
  const [v, setV] = useState("");
  const submit = () => {
    const text = v.trim();
    if (!text) return;
    let action = "hint";
    let payload = text;
    if (text.startsWith("/")) {
      const [a, ...rest] = text.slice(1).split(" ");
      action = a;
      payload = rest.join(" ");
    }
    onSend("global", action, payload);
    setV("");
  };
  return (
    <div className="cmd">
      <input
        value={v}
        onChange={(e) => setV(e.target.value)}
        onKeyDown={(e) => e.key === "Enter" && submit()}
        placeholder="command the swarm:  hint text…   or  /pause   /submit flag{…}   /reject"
      />
      <button onClick={submit}>Send</button>
    </div>
  );
}
