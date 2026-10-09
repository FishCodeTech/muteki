"use client";

import { useEffect, useState } from "react";
import { Icon } from "@/components/Icon";
import { SettingsNote, SettingsRow, SettingsSection } from "@/components/settings/primitives";
import { ConversationImportModal } from "@/components/conversation/ConversationImportModal";
import { fetchConversationProjects, type ConversationProject } from "@/lib/useConversation";

export default function ImportSettings() {
  const [projects, setProjects] = useState<ConversationProject[]>([]);
  const [open, setOpen] = useState(false);
  const [notice, setNotice] = useState("");

  useEffect(() => {
    void fetchConversationProjects().then(setProjects).catch(() => {
      // Import still works without an optional project assignment.
    });
  }, []);

  return (
    <section className="cx-settings-data flex flex-col gap-6" aria-label="历史会话导入">
      <SettingsSection>
        <SettingsRow
          leading={<span className="cx-import-icon"><Icon name="download" size={18} /></span>}
          title="从服务宿主导入历史会话"
          description="选择当前服务可访问的 Claude 或 Codex 历史目录，预览可导入的会话，再决定导入哪些内容。"
          control={<button type="button" className="cx-data-btn is-primary" onClick={() => setOpen(true)}>选择历史会话</button>}
        />
      </SettingsSection>
      {notice ? <SettingsNote>{notice}</SettingsNote> : null}
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
