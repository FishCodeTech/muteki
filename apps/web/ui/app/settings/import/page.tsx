"use client";

import { useEffect, useState } from "react";
import { Icon } from "@/components/Icon";
import { ConversationImportModal } from "@/components/conversation/ConversationImportModal";
import { fetchConversationProjects, type ConversationProject } from "@/lib/useConversation";

export default function ImportSettingsPage() {
  const [projects, setProjects] = useState<ConversationProject[]>([]);
  const [open, setOpen] = useState(false);
  const [notice, setNotice] = useState("");

  useEffect(() => {
    void fetchConversationProjects().then(setProjects).catch(() => {
      // Import still works without an optional project assignment.
    });
  }, []);

  return (
    <section className="settings-data-page" aria-label="历史会话导入">
      <div className="settings-import-card">
        <span className="settings-import-icon"><Icon name="download" size={20} /></span>
        <div>
          <h2>从本机导入历史会话</h2>
          <p>选择 Claude 或 Codex 的历史目录，预览可导入的会话，再决定导入哪些内容。</p>
          <button type="button" className="settings-data-primary" onClick={() => setOpen(true)}>选择历史会话</button>
        </div>
      </div>
      {notice ? <p className="settings-data-feedback" role="status">{notice}</p> : null}
      {open ? (
        <ConversationImportModal
          projects={projects}
          onClose={() => setOpen(false)}
          onImportDone={() => setNotice("历史会话已导入，可返回对话页面查看。")}
        />
      ) : null}
    </section>
  );
}
