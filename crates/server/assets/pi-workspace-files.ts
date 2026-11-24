import { readFileSync, unlinkSync, writeFileSync } from "node:fs";

export default function register(pi: import("@earendil-works/pi-coding-agent").ExtensionAPI) {
  const index = new URL("./attachments.json", import.meta.url);
  pi.on("before_agent_start", (event) => {
    if (event.messageType === "custom") return;
    let contents: string;
    try {
      contents = readFileSync(index, "utf8");
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ENOENT") return;
      throw error;
    }
    const paths = JSON.parse(contents);
    unlinkSync(index);
    return {
      message: {
        customType: "muteki.workspace-files",
        content: JSON.stringify({ workspace_files: paths }),
        display: false,
      },
    };
  });
  writeFileSync(new URL("./ready", import.meta.url), "");
}
