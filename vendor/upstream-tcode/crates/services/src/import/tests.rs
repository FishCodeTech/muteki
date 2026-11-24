use std::fs;
use std::path::{Path, PathBuf};

use agent::{ItemContent, ItemStatus};
use serde_json::json;

use super::*;
use tcode_core::session::{EntryContent, Timeline};

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("tcode-import-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_lines(path: &Path, lines: &[serde_json::Value]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let text = lines
        .iter()
        .map(serde_json::Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(path, format!("{text}\n")).unwrap();
}

fn item_contents(converted: &ConvertedThread) -> Vec<&ItemContent> {
    converted
        .entries
        .iter()
        .filter_map(|entry| match entry {
            ConvertedEntry::Item { item, .. } => Some(&item.content),
            ConvertedEntry::Compacted { .. } => None,
        })
        .collect()
}

#[test]
fn claude_converter_maps_items_and_excludes_noise() {
    let temp = TestDir::new();
    let file = temp.path().join("claude.jsonl");
    let base =
        json!({"timestamp":"2026-01-02T03:04:05.006Z","cwd":"/synthetic","sessionId":"claude-1"});
    let lines = vec![
        json!({"type":"user","message":{"role":"user","content":"Hello Claude"},"timestamp":base["timestamp"],"cwd":base["cwd"],"sessionId":base["sessionId"]}),
        json!({"type":"assistant","message":{"role":"assistant","content":[
            {"type":"text","text":"Hello human"},
            {"type":"thinking","thinking":"Consider the request"},
            {"type":"tool_use","id":"tool-1","name":"Read","input":{"file_path":"demo.txt"}},
            {"type":"tool_use","id":"image-read","name":"Read","input":{"file_path":"image.png"}}
        ]},"timestamp":"2026-01-02T03:04:06.006Z","cwd":"/synthetic","sessionId":"claude-1"}),
        json!({"type":"user","message":{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"tool-1","content":[{"type":"text","text":"permission denied"}],"is_error":true},
            {"type":"tool_result","tool_use_id":"image-read","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AQID"}}]}
        ]},"timestamp":"2026-01-02T03:04:07.006Z","cwd":"/synthetic","sessionId":"claude-1"}),
        json!({"type":"user","message":{"role":"user","content":"sidechain secret"},"isSidechain":true,"timestamp":"2026-01-02T03:04:08.006Z","cwd":"/synthetic"}),
        json!({"type":"queue-operation","operation":"enqueue","timestamp":"2026-01-02T03:04:09.006Z","cwd":"/synthetic"}),
        json!({"type":"user","message":{"role":"user","content":"<command-message>noise</command-message>"},"timestamp":"2026-01-02T03:04:10.006Z","cwd":"/synthetic"}),
    ];
    write_lines(&file, &lines);

    let converted = claude::convert(&file, "claude:claude-1").unwrap().unwrap();
    let contents = item_contents(&converted);
    assert_eq!(contents.len(), 5);
    assert!(
        matches!(contents[4], ItemContent::ToolCall { image_reads, .. } if image_reads.len() == 1 && image_reads[0].data_base64 == "AQID")
    );
    assert!(matches!(contents[0], ItemContent::UserMessage { text, .. } if text == "Hello Claude"));
    assert!(matches!(contents[1], ItemContent::AssistantMessage { text } if text == "Hello human"));
    assert!(
        matches!(contents[2], ItemContent::Reasoning { text } if text == "Consider the request")
    );
    assert!(matches!(contents[3], ItemContent::ToolCall {
        name, input, output: Some(output), status: ItemStatus::Failed, ..
    } if name == "Read" && input == &json!({"file_path":"demo.txt"}) && output == "permission denied"));
}

#[test]
fn codex_converter_maps_external_history_and_excludes_harness_and_own_sessions() {
    let temp = TestDir::new();
    let file = temp.path().join("rollout.jsonl");
    write_lines(
        &file,
        &[
            json!({"timestamp":"2026-01-02T03:04:05.006Z","type":"session_meta","payload":{"id":"codex-1","cwd":"/synthetic","originator":"codex_exec"}}),
            json!({"timestamp":"2026-01-02T03:04:06.006Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>noise</environment_context>"},{"type":"input_text","text":"Build it"}]}}),
            json!({"timestamp":"2026-01-02T03:04:07.006Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Working on it"}]}}),
            json!({"timestamp":"2026-01-02T03:04:08.006Z","type":"response_item","payload":{"type":"reasoning","summary":[{"text":"First"},{"text":"Second"}]}}),
            json!({"timestamp":"2026-01-02T03:04:09.006Z","type":"response_item","payload":{"type":"function_call","name":"read_file","arguments":"{\"path\":\"a.rs\"}","call_id":"call-1"}}),
            json!({"timestamp":"2026-01-02T03:04:10.006Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"{\"output\":\"file body\"}"}}),
            json!({"timestamp":"2026-01-02T03:04:11.006Z","type":"event_msg","payload":{"type":"token_count","input_tokens":10}}),
        ],
    );

    let converted = codex::convert(&file, "codex:codex-1").unwrap().unwrap();
    let contents = item_contents(&converted);
    assert_eq!(contents.len(), 4);
    assert!(matches!(contents[0], ItemContent::UserMessage { text, .. } if text == "Build it"));
    assert!(
        matches!(contents[1], ItemContent::AssistantMessage { text } if text == "Working on it")
    );
    assert!(matches!(contents[2], ItemContent::Reasoning { text } if text == "First\nSecond"));
    assert!(matches!(contents[3], ItemContent::ToolCall {
        name, input, output: Some(output), status: ItemStatus::Completed, ..
    } if name == "read_file" && input == &json!({"path":"a.rs"}) && output == "file body"));
    write_lines(
        &file,
        &[json!({
            "timestamp":"2026-01-02T03:04:05.006Z",
            "type":"session_meta",
            "payload":{"id":"ours","cwd":"/synthetic","originator":"tcode"}
        })],
    );
    assert!(codex::convert(&file, "codex:ours").unwrap().is_none());
}

#[test]
fn scanner_groups_attributes_orders_and_excludes() {
    let temp = TestDir::new();
    let dirs = temp.path().join("working-dirs");
    let desktop_cwd = dirs.join("desktop");
    let shared_cwd = dirs.join("shared");
    let excluded_cwd = dirs.join("excluded");
    fs::create_dir_all(&desktop_cwd).unwrap();
    fs::create_dir_all(&shared_cwd).unwrap();
    fs::create_dir_all(&excluded_cwd).unwrap();
    let missing_cwd = dirs.join("missing");

    let roots = ExternalRoots {
        claude_projects: temp.path().join("claude/projects"),
        claude_desktop_meta: temp.path().join("desktop-meta"),
        codex_session_roots: vec![temp.path().join("codex/sessions")],
    };
    write_lines(
        &roots.claude_projects.join("munged/desktop-id.jsonl"),
        &[json!({
            "type":"user","message":{"content":"desktop"},"cwd":desktop_cwd,"sessionId":"desktop-id",
            "entrypoint":"cli","timestamp":"2099-01-01T00:00:00Z"
        })],
    );
    write_lines(
        &roots.claude_projects.join("munged/t3-id.jsonl"),
        &[json!({
            "type":"user","message":{"content":"t3"},"cwd":shared_cwd,"sessionId":"t3-id",
            "entrypoint":"sdk-ts","timestamp":"2098-01-01T00:00:00Z"
        })],
    );
    write_lines(
        &roots.claude_projects.join("munged/missing-id.jsonl"),
        &[json!({
            "type":"user","message":{"content":"missing"},"cwd":missing_cwd,"sessionId":"missing-id","timestamp":"2097-01-01T00:00:00Z"
        })],
    );
    // Subagent material must never surface as threads: transcripts nested below
    // the project dir, and sidechain-flagged files at the top level.
    write_lines(
        &roots
            .claude_projects
            .join("munged/desktop-id/subagents/agent-1.jsonl"),
        &[json!({
            "type":"user","isSidechain":true,"message":{"content":"nested"},"cwd":desktop_cwd,
            "sessionId":"nested-id","timestamp":"2099-06-01T00:00:00Z"
        })],
    );
    write_lines(
        &roots.claude_projects.join("munged/sidechain-id.jsonl"),
        &[json!({
            "type":"user","isSidechain":true,"message":{"content":"sidechain"},"cwd":desktop_cwd,
            "sessionId":"sidechain-id","timestamp":"2099-06-01T00:00:00Z"
        })],
    );
    fs::create_dir_all(roots.claude_desktop_meta.join("a/b")).unwrap();
    fs::write(roots.claude_desktop_meta.join("a/b/local_one.json"), json!({
        "cliSessionId":"desktop-id","cwd":desktop_cwd,"title":"Desktop title","lastActivityAt":4_200_000_000_000_u64
    }).to_string()).unwrap();
    write_lines(
        &roots.codex_session_roots[0].join("2026/01/rollout-shared.jsonl"),
        &[json!({
            "timestamp":"2096-01-01T00:00:00Z","type":"session_meta",
            "payload":{"id":"codex-shared","cwd":shared_cwd,"originator":"Codex Desktop"}
        })],
    );
    write_lines(
        &roots.codex_session_roots[0].join("2026/01/rollout-excluded.jsonl"),
        &[json!({
            "timestamp":"2095-01-01T00:00:00Z","type":"session_meta",
            "payload":{"id":"codex-excluded","cwd":excluded_cwd,"originator":"codex_exec"}
        })],
    );

    // A link back to an ancestor must not list the sessions below it again.
    #[cfg(unix)]
    std::os::unix::fs::symlink(
        roots.codex_session_roots[0].join("2026"),
        roots.codex_session_roots[0].join("2026/01/loop"),
    )
    .unwrap();

    let recent = scan_recent_dirs(&roots, std::slice::from_ref(&excluded_cwd));
    assert_eq!(recent.len(), 2);
    assert_eq!(recent[0].path, desktop_cwd);
    assert_eq!(
        recent[0].threads.len(),
        1,
        "sidechain files must not surface"
    );
    assert_eq!(recent[0].threads[0].source, SourceTool::ClaudeDesktop);
    assert_eq!(
        recent[0].threads[0].title_hint.as_deref(),
        Some("Desktop title")
    );
    let shared = recent.iter().find(|dir| dir.path == shared_cwd).unwrap();
    assert_eq!(shared.threads.len(), 2);
    assert!(
        shared
            .threads
            .iter()
            .any(|thread| thread.source == SourceTool::T3Code)
    );
    assert!(
        shared
            .threads
            .iter()
            .any(|thread| thread.source == SourceTool::CodexDesktop)
    );
    assert!(
        recent
            .iter()
            .all(|dir| dir.path != excluded_cwd && dir.path != missing_cwd)
    );
}

#[test]
fn import_is_idempotent_and_replays_into_timeline() {
    let temp = TestDir::new();
    let cwd = temp.path().join("project");
    fs::create_dir_all(&cwd).unwrap();
    let transcript = temp.path().join("session.jsonl");
    write_lines(
        &transcript,
        &[
            json!({"type":"user","message":{"role":"user","content":"Imported question"},"timestamp":"2026-01-02T03:04:05.006Z","cwd":cwd,"sessionId":"session-1","entrypoint":"cli"}),
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Imported answer"}]},"timestamp":"2026-01-02T03:04:06.006Z","cwd":cwd,"sessionId":"session-1"}),
        ],
    );
    let store = SessionStore::open_at(temp.path().join("store")).unwrap();
    let project = Project {
        id: "project-1".into(),
        name: "Project".into(),
        root: cwd,
        quick_chat: false,
        icon_path: None,
        permission_defaults: Default::default(),
        created_at: 1,
    };
    store.upsert_project(&project).unwrap();
    let thread = ExternalThread {
        source: SourceTool::ClaudeCode,
        file: transcript,
        external_id: "claude:session-1".into(),
        title_hint: None,
        last_active_ms: 0,
    };
    let mut existing = existing_external_ids(&store.load_index().unwrap());
    assert_eq!(
        import_thread(&store, &project, &thread, &mut existing),
        ImportOutcome::Imported
    );
    assert_eq!(
        import_thread(&store, &project, &thread, &mut existing),
        ImportOutcome::SkippedDuplicate
    );

    let index = store.load_index().unwrap();
    assert_eq!(index.len(), 1);
    let meta = &index[0];
    assert_eq!(meta.project_id.as_deref(), Some("project-1"));
    assert_eq!(meta.imported_from.as_deref(), Some("claude:session-1"));
    assert_eq!(
        meta.resume_cursor.as_ref().unwrap().0["session_id"],
        "session-1"
    );
    let timeline = Timeline::fold_events(store.read_events(&meta.id).unwrap());
    assert!(timeline.entries.iter().any(
        |entry| matches!(&entry.content, EntryContent::Item(ItemContent::UserMessage { text, .. }) if text == "Imported question")
    ));
    assert!(timeline.entries.iter().any(|entry| matches!(&entry.content, EntryContent::Item(ItemContent::AssistantMessage { text }) if text == "Imported answer")));

    let mut imported = meta.clone();
    imported.resume_cursor = None;
    store.upsert_meta(&imported).unwrap();
    let mut existing = existing_external_ids(&store.load_index().unwrap());
    assert_eq!(
        import_thread(&store, &project, &thread, &mut existing),
        ImportOutcome::SkippedDuplicate
    );
    assert_eq!(store.load_index().unwrap().len(), 1);

    for (index, (provider, source, external_id, cursor, lines)) in [
        (ProviderKind::ClaudeCode, SourceTool::ClaudeCode, "claude:native-claude",
            json!({"session_id":"native-claude"}), vec![
                json!({"type":"user","message":{"role":"user","content":"Native Claude question"},"timestamp":"2026-01-02T03:04:05.006Z","cwd":project.root,"sessionId":"native-claude"}),
            ]),
        (ProviderKind::Codex, SourceTool::CodexCli, "codex:native-codex",
            json!({"thread_id":"native-codex"}), vec![
                json!({"timestamp":"2026-01-02T03:04:05.006Z","type":"session_meta","payload":{"id":"native-codex","cwd":project.root,"originator":"codex_exec"}}),
                json!({"timestamp":"2026-01-02T03:04:06.006Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Native Codex question"}]}}),
            ]),
    ].into_iter().enumerate() {
        let mut native = SessionMeta::new(provider, project.root.clone(), None);
        native.resume_cursor = Some(ResumeCursor(cursor));
        store.upsert_meta(&native).unwrap();
        let file = temp.path().join(format!("native-{index}.jsonl"));
        write_lines(&file, &lines);
        let thread = ExternalThread {
            source, file, external_id: external_id.into(), title_hint: None, last_active_ms: 0,
        };
        let mut existing = existing_external_ids(&store.load_index().unwrap());
        assert_eq!(import_thread(&store, &project, &thread, &mut existing),
            ImportOutcome::SkippedDuplicate, "{external_id}");
        assert_eq!(store.load_index().unwrap().len(), index + 2);
    }
}
