use std::path::PathBuf;

use agent::AgentEvent;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;

use super::*;
use tcode_core::settlement::{AgentDelivery, AgentExecution, AgentReport};

#[test]
fn provider_update_command_and_toasts_have_literal_wire_contracts() {
    let command = Command::UpdateProviders {
        providers: vec![agent::ProviderKind::ClaudeCode, agent::ProviderKind::Codex],
    };
    let wire =
        json!({"type": "update_providers", "content": {"providers": ["claude_code", "codex"]}});
    assert_eq!(serde_json::to_value(&command).unwrap(), wire);
    assert_eq!(serde_json::from_value::<Command>(wire).unwrap(), command);
    let updates = RuntimeToast::ProviderUpdatesAvailable {
        updates: vec![ProviderUpdateAvailable {
            provider: agent::ProviderKind::Codex,
            version: "2.0.0".into(),
            automatic: true,
        }],
    };
    let wire = json!({"type": "provider_updates_available", "content": {"updates": [{"provider": "codex", "version": "2.0.0", "automatic": true}]}});
    assert_eq!(serde_json::to_value(&updates).unwrap(), wire);
    assert_eq!(
        serde_json::from_value::<RuntimeToast>(wire).unwrap(),
        updates
    );
    let run = ProviderUpdateRun {
        total: 2,
        completed: 1,
        current: Some(agent::ProviderKind::Codex),
        failed: vec![agent::ProviderKind::ClaudeCode],
    };
    for (name, toast) in [
        (
            "provider_update_started",
            RuntimeToast::ProviderUpdateStarted {
                operation: RuntimeOperationId(7),
                run: run.clone(),
            },
        ),
        (
            "provider_update_progress",
            RuntimeToast::ProviderUpdateProgress {
                operation: RuntimeOperationId(7),
                run: run.clone(),
            },
        ),
        (
            "provider_update_finished",
            RuntimeToast::ProviderUpdateFinished {
                operation: RuntimeOperationId(7),
                run,
            },
        ),
    ] {
        let wire = json!({"type": name, "content": {"operation": 7, "run": {"total": 2, "completed": 1, "current": "codex", "failed": ["claude_code"]}}});
        assert_eq!(serde_json::to_value(&toast).unwrap(), wire);
        assert_eq!(serde_json::from_value::<RuntimeToast>(wire).unwrap(), toast);
    }
}

#[test]
fn provider_update_status_accepts_older_hosts_and_preserves_terminal_requirement() {
    let older = json!({
        "installed": "1.0.0", "latest": "1.0.1", "update_available": true,
        "checking": false, "updating": false, "update_command": "brew upgrade codex"
    });
    let status: ProviderVersionStatus = serde_json::from_value(older.clone()).unwrap();
    assert!(!status.update_requires_terminal);
    let mut terminal = older;
    terminal["update_command"] = json!("sudo apt-get install --only-upgrade claude-code");
    terminal["update_requires_terminal"] = json!(true);
    let status: ProviderVersionStatus = serde_json::from_value(terminal.clone()).unwrap();
    assert!(status.update_requires_terminal);
    assert_eq!(serde_json::to_value(status).unwrap(), terminal);
}

#[test]
fn client_ndjson_preserves_ids_text_and_record_boundaries() {
    let message = ClientMessage {
        principal: None,
        key: None,
        id: u64::MAX,
        payload: ClientPayload::Command(Command::SendTurn {
            session_id: "session-1".into(),
            text: "第一行\n\"quoted\"".into(),
            attachment_paths: vec![PathBuf::from("/tmp/image.png")],
        }),
    };
    let line = encode_line(&message).unwrap();
    assert!(line.ends_with('\n'));
    assert_eq!(line.lines().count(), 1);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&line).unwrap(),
        json!({
            "id": u64::MAX,
            "payload": {
                "type": "command",
                "content": {
                    "type": "send_turn",
                    "content": {
                        "session_id": "session-1",
                        "text": "第一行\n\"quoted\"",
                        "attachment_paths": ["/tmp/image.png"],
                    },
                },
            },
        })
    );
    assert_eq!(decode_client_line(&line).unwrap(), message);
}

#[test]
fn host_responses_preserve_errors_and_correlation() {
    let error = ProtocolError {
        code: "not_found".into(),
        message: "missing session".into(),
    };
    for (message, kind) in [
        (
            HostMessage::Ack {
                id: 7,
                result: Err(error.clone()),
            },
            "ack",
        ),
        (
            HostMessage::QueryResult {
                id: 7,
                result: Err(error),
            },
            "query_result",
        ),
    ] {
        let line = encode_line(&message).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap(),
            json!({
                "type": kind,
                "content": {
                    "id": 7,
                    "result": {"Err": {"code": "not_found", "message": "missing session"}},
                },
            })
        );
        assert_eq!(decode_host_line(&line).unwrap(), message);
    }
}

#[test]
fn event_envelopes_keep_stored_record_shape_and_optional_request_id() {
    let mut envelope = EventEnvelope {
        request_id: None,
        topic: Topic::SessionEvents {
            session_id: "session-1".into(),
        },
        event: ServerEvent::SessionEvent(SessionEventRecord {
            origin: None,
            author: None,
            ts: Some(123),
            event: AgentEvent::TurnStarted {
                turn_id: "turn-1".into(),
            },
            elided: None,
        }),
    };
    let expected = json!({
        "topic": {"type": "session_events", "content": {"session_id": "session-1"}},
        "event": {
            "type": "session_event",
            "content": {"ts": 123, "event": {"type": "turn_started", "turn_id": "turn-1"}},
        },
    });
    assert_eq!(serde_json::to_value(&envelope).unwrap(), expected);
    assert_eq!(
        serde_json::from_value::<EventEnvelope>(expected).unwrap(),
        envelope
    );

    if let ServerEvent::SessionEvent(record) = &mut envelope.event {
        record.author = Some(tcode_core::session::Author {
            device_id: "d".into(),
            name: "Alice".into(),
        });
    }
    assert_eq!(
        serde_json::to_value(&envelope).unwrap()["event"]["content"]["author"],
        json!({"device_id":"d","name":"Alice"})
    );
    envelope.request_id = Some(42);
    let message = HostMessage::Event(envelope);
    let line = encode_line(&message).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&line).unwrap()["content"]["request_id"],
        42
    );
    assert_eq!(decode_host_line(&line).unwrap(), message);
}

fn assert_binary_wire<T>(message: T, pointer: &str)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let mut value = serde_json::to_value(&message).unwrap();
    assert_eq!(value.pointer(pointer).unwrap(), "AP8KGw==");
    assert_eq!(serde_json::from_value::<T>(value.clone()).unwrap(), message);
    *value.pointer_mut(pointer).unwrap() = json!("not base64!");
    assert!(serde_json::from_value::<T>(value).is_err());
}

#[test]
fn binary_payloads_use_base64_and_reject_corrupt_input() {
    let bytes = vec![0, 0xff, b'\n', b'\x1b'];
    assert_binary_wire(
        Command::TerminalInput {
            terminal_id: 9,
            bytes: bytes.clone(),
        },
        "/content/bytes",
    );
    assert_binary_wire(
        Query::SaveAttachment {
            dir: PathBuf::from("/tmp/attachments"),
            bytes: bytes.clone(),
            ext: "png".into(),
        },
        "/content/bytes",
    );
    assert_binary_wire(QueryResponse::FileBytes(bytes), "/content");
}

#[test]
fn omitted_optional_subscription_index_and_queue_fields_default() {
    let subscription: Subscription =
        serde_json::from_value(json!({"topic": {"type": "index"}})).unwrap();
    assert_eq!(
        subscription,
        Subscription {
            known_index: None,
            topic: Topic::Index,
            after: None
        }
    );
    let index: IndexSnapshot = serde_json::from_value(
        json!({"sessions": [], "projects": [], "worktree_shared": [], "archived_revision": 0}),
    )
    .unwrap();
    assert_eq!(index.summary, IndexSummary::default());
    let queued: QueuedMessageStatus =
        serde_json::from_value(json!({"id": 1, "text": "next", "editable": true})).unwrap();
    assert_eq!(queued.fire_at_unix_secs, None);
    let command = decode_client_line(r#"{"id":1,"payload":{"type":"command","content":{"type":"capture_terminal_selection","content":{"session_id":"s","terminal_id":9}}}}"#).unwrap();
    assert!(matches!(
        command.payload,
        ClientPayload::Command(Command::CaptureTerminalSelection {
            selection: None,
            ..
        })
    ));
}

/// Import progress and content search are replicated host state and a host-run
/// query. Their literal shapes are the contract a non-Rust or older client
/// depends on, so assert the JSON rather than a round trip.
#[test]
fn import_status_and_content_search_use_their_documented_wire_shapes() {
    let start = ClientMessage {
        principal: None,
        key: None,
        id: 3,
        payload: ClientPayload::Command(Command::StartExternalImport {
            project_id: "project-1".into(),
            threads: vec![ExternalThread {
                source: SourceTool::ClaudeCode,
                file: PathBuf::from("/history/a.jsonl"),
                external_id: "claude:abc".into(),
                title_hint: None,
                last_active_ms: 17,
            }],
        }),
    };
    assert_eq!(
        serde_json::to_value(&start).unwrap(),
        json!({
            "id": 3,
            "payload": {
                "type": "command",
                "content": {
                    "type": "start_external_import",
                    "content": {
                        "project_id": "project-1",
                        "threads": [{
                            "source": "claude_code",
                            "file": "/history/a.jsonl",
                            "external_id": "claude:abc",
                            "title_hint": null,
                            "last_active_ms": 17,
                        }],
                    },
                },
            },
        })
    );
    assert_eq!(
        decode_client_line(&encode_line(&start).unwrap()).unwrap(),
        start
    );

    assert_eq!(
        serde_json::to_value(CommandResponse::ExternalImportStarted(false)).unwrap(),
        json!({"type": "external_import_started", "content": false})
    );

    for (status, expected) in [
        (None, json!(null)),
        (
            Some(ExternalImportStatus {
                run_id: 4,
                state: ExternalImportState::Progress {
                    done: 1,
                    total: 2,
                    tool: "Codex CLI".into(),
                },
            }),
            json!({
                "run_id": 4,
                "state": {
                    "type": "progress",
                    "content": {"done": 1, "total": 2, "tool": "Codex CLI"},
                },
            }),
        ),
        (
            Some(ExternalImportStatus {
                run_id: 4,
                state: ExternalImportState::Finished {
                    imported: 2,
                    skipped: 1,
                },
            }),
            json!({
                "run_id": 4,
                "state": {
                    "type": "finished",
                    "content": {"imported": 2, "skipped": 1},
                },
            }),
        ),
    ] {
        let envelope = EventEnvelope {
            request_id: None,
            topic: Topic::ExternalImport {
                project_id: "project-1".into(),
            },
            event: ServerEvent::ExternalImportStatusReplaced {
                project_id: "project-1".into(),
                status,
            },
        };
        let value = json!({
            "topic": {"type": "external_import", "content": {"project_id": "project-1"}},
            "event": {
                "type": "external_import_status_replaced",
                "content": {"project_id": "project-1", "status": expected},
            },
        });
        assert_eq!(serde_json::to_value(&envelope).unwrap(), value);
        assert_eq!(
            serde_json::from_value::<EventEnvelope>(value).unwrap(),
            envelope
        );
    }

    let search = ClientMessage {
        principal: None,
        key: None,
        id: 9,
        payload: ClientPayload::Query(Query::SearchSessionContent {
            query: "auth.rs".into(),
            limit: 50,
        }),
    };
    assert_eq!(
        serde_json::to_value(&search).unwrap(),
        json!({
            "id": 9,
            "payload": {
                "type": "query",
                "content": {
                    "type": "search_session_content",
                    "content": {"query": "auth.rs", "limit": 50},
                },
            },
        })
    );
    assert_eq!(
        decode_client_line(&encode_line(&search).unwrap()).unwrap(),
        search
    );

    let hits = QueryResponse::SessionContentHits(vec![SessionSearchHit {
        session_id: "session-1".into(),
        session_title: "Authentication cleanup".into(),
        entry_id: "entry-1".into(),
        turn: 2,
        snippet: "…updated auth.rs…".into(),
    }]);
    let hits_value = json!({
        "type": "session_content_hits",
        "content": [{
            "session_id": "session-1",
            "session_title": "Authentication cleanup",
            "entry_id": "entry-1",
            "turn": 2,
            "snippet": "…updated auth.rs…",
        }],
    });
    assert_eq!(serde_json::to_value(&hits).unwrap(), hits_value);
    assert_eq!(
        serde_json::from_value::<QueryResponse>(hits_value).unwrap(),
        hits
    );
}

#[test]
fn unknown_data_variants_and_malformed_records_return_decode_errors() {
    for line in [
        "",
        "not JSON",
        r#"{"id":7,"payload":{"type":"command","content":{"type":"future_command","content":{"value":1}}}}"#,
        r#"{"id":7,"payload":{"type":"future_payload"}}"#,
    ] {
        assert_eq!(decode_client_line(line).unwrap_err().code, "decode_error");
    }
    for line in [
        "{",
        r#"{"type":"future_host_message","content":{}}"#,
        r#"{"type":"event","content":{"topic":{"type":"index"},"event":{"type":"future_event","content":{}}}}"#,
    ] {
        assert_eq!(decode_host_line(line).unwrap_err().code, "decode_error");
    }
    assert_eq!(
        serde_json::from_value::<GitDiffScope>(json!("future_scope")).unwrap(),
        GitDiffScope::Unknown
    );
    assert_eq!(
        serde_json::from_value::<SourceTool>(json!("future_tool")).unwrap(),
        SourceTool::Unknown
    );
}

/// The replicated terminal grid is the whole client contract, so its literal
/// shape is asserted rather than round-tripped. Everything that is at its
/// default is omitted: a blank cell is `{}` and a blank row is `{}`.
#[test]
fn terminal_frame_wire_shape_omits_defaults() {
    use crate::terminal::{
        CellWidth, CursorShape, TerminalCell, TerminalColor, TerminalCursor, TerminalFrame,
        TerminalLink, TerminalModes, TerminalRow, TerminalStyle,
    };

    let frame = TerminalFrame {
        cols: 4,
        rows: 2,
        modes: TerminalModes {
            mode: 0b1_0000_0001_0001,
            keyboard: 1,
            modify_other_keys: Some(2),
        },
        cursor: Some(TerminalCursor {
            row: 1,
            col: 2,
            shape: CursorShape::Beam,
            blinking: true,
        }),
        styles: vec![
            TerminalStyle::default(),
            TerminalStyle {
                fg: TerminalColor::Rgb { r: 1, g: 2, b: 3 },
                bg: TerminalColor::Indexed(9),
                underline_color: None,
                flags: 0b110,
            },
        ],
        visible: vec![
            TerminalRow {
                cells: vec![
                    TerminalCell {
                        text: "中".into(),
                        width: CellWidth::Wide,
                        style: 1,
                        link: None,
                    },
                    TerminalCell {
                        width: CellWidth::Spacer,
                        ..TerminalCell::default()
                    },
                    TerminalCell {
                        text: "e\u{301}".into(),
                        link: Some(TerminalLink {
                            uri: "https://example.com".into(),
                            id: None,
                        }),
                        ..TerminalCell::default()
                    },
                ],
                wrapped: true,
            },
            TerminalRow::default(),
        ],
        history: vec![TerminalRow::default()],
        lines_evicted: 7,
        title: "zsh".into(),
        working_directory: Some(PathBuf::from("/tmp/project")),
        exited: false,
        exit_code: None,
    };
    let event = ServerEvent::TerminalFrame {
        terminal_id: 3,
        frame: Box::new(frame.clone()),
    };
    assert_eq!(
        serde_json::to_value(&event).unwrap(),
        json!({
            "type": "terminal_frame",
            "content": {
                "terminal_id": 3,
                "frame": {
                    "cols": 4,
                    "rows": 2,
                    "modes": {"mode": 4113, "keyboard": 1, "modify_other_keys": 2},
                    "cursor": {"row": 1, "col": 2, "shape": "beam", "blinking": true},
                    "styles": [
                        {"fg": {"type": "foreground"}, "bg": {"type": "background"}},
                        {
                            "fg": {"type": "rgb", "content": {"r": 1, "g": 2, "b": 3}},
                            "bg": {"type": "indexed", "content": 9},
                            "flags": 6,
                        },
                    ],
                    "visible": [
                        {
                            "cells": [
                                {"text": "中", "width": "wide", "style": 1},
                                {"width": "spacer"},
                                {"text": "e\u{301}", "link": {"uri": "https://example.com"}},
                            ],
                            "wrapped": true,
                        },
                        {},
                    ],
                    "history": [{}],
                    "lines_evicted": 7,
                    "title": "zsh",
                    "working_directory": "/tmp/project",
                },
            },
        })
    );
    assert_eq!(
        serde_json::from_value::<ServerEvent>(json!({
            "type": "terminal_frame",
            "content": {"terminal_id": 3, "frame": serde_json::to_value(&frame).unwrap()},
        }))
        .unwrap(),
        event
    );
}

#[test]
fn terminal_delta_wire_shape_carries_only_what_changed() {
    use crate::terminal::{
        TerminalCell, TerminalDelta, TerminalHistoryUpdate, TerminalModes, TerminalRow,
        TerminalRowUpdate, TerminalStyle,
    };

    let delta = TerminalDelta {
        cols: 80,
        rows: 24,
        modes: TerminalModes::default(),
        cursor: None,
        lines_evicted: 0,
        styles: vec![TerminalStyle::default()],
        rows_replaced: vec![TerminalRowUpdate {
            index: 5,
            row: TerminalRow {
                cells: vec![TerminalCell {
                    text: "y".into(),
                    ..TerminalCell::default()
                }],
                wrapped: false,
            },
        }],
        history: Some(TerminalHistoryUpdate::Appended(
            vec![TerminalRow::default()],
        )),
        title: None,
        working_directory: None,
        exit: None,
        bell: true,
        clipboard: None,
    };
    let event = ServerEvent::TerminalDelta {
        terminal_id: 3,
        delta: Box::new(delta.clone()),
    };
    assert_eq!(
        serde_json::to_value(&event).unwrap(),
        json!({
            "type": "terminal_delta",
            "content": {
                "terminal_id": 3,
                "delta": {
                    "cols": 80,
                    "rows": 24,
                    "modes": {"mode": 0, "keyboard": 0},
                    "styles": [{"fg": {"type": "foreground"}, "bg": {"type": "background"}}],
                    "rows_replaced": [{"index": 5, "row": {"cells": [{"text": "y"}]}}],
                    "history": {"type": "appended", "content": [{}]},
                    "bell": true,
                },
            },
        })
    );
    assert_eq!(
        serde_json::from_value::<ServerEvent>(serde_json::to_value(&event).unwrap()).unwrap(),
        event
    );
}

/// A delta advances the frame exactly as the host's own retained projection
/// does: rows are replaced in place, history appends and trims at the cap, and
/// message-local style ids are remapped into the frame's own table.
#[test]
fn applying_a_delta_replaces_rows_and_trims_history_at_the_cap() {
    use crate::terminal::{
        HISTORY_LIMIT, TerminalCell, TerminalColor, TerminalDelta, TerminalFrame,
        TerminalHistoryUpdate, TerminalRow, TerminalRowUpdate, TerminalStyle,
    };

    let red = TerminalStyle {
        fg: TerminalColor::Indexed(1),
        ..TerminalStyle::default()
    };
    let blue = TerminalStyle {
        fg: TerminalColor::Indexed(4),
        ..TerminalStyle::default()
    };
    let row = |text: String, style| TerminalRow {
        cells: vec![TerminalCell {
            text,
            style,
            ..TerminalCell::default()
        }],
        wrapped: false,
    };
    let mut frame = TerminalFrame {
        cols: 2,
        rows: 2,
        styles: vec![TerminalStyle::default(), blue],
        visible: vec![row("unchanged".into(), 1), TerminalRow::default()],
        history: (0..HISTORY_LIMIT)
            .map(|index| row(format!("old-{index}"), 0))
            .collect(),
        ..TerminalFrame::default()
    };
    frame.apply(&TerminalDelta {
        cols: 2,
        rows: 2,
        // Local style 1 is red here but blue in the retained frame.
        styles: vec![TerminalStyle::default(), red],
        rows_replaced: vec![TerminalRowUpdate {
            index: 1,
            row: TerminalRow {
                cells: vec![TerminalCell {
                    text: "x".into(),
                    style: 1,
                    ..TerminalCell::default()
                }],
                wrapped: false,
            },
        }],
        history: Some(TerminalHistoryUpdate::Appended(vec![row("new".into(), 0)])),
        ..TerminalDelta::default()
    });

    assert_eq!(frame.visible[0].cells[0].text, "unchanged");
    assert_eq!(frame.style(&frame.visible[0].cells[0]), blue);
    assert_eq!(frame.visible[1].cells[0].text, "x");
    assert_eq!(frame.style(&frame.visible[1].cells[0]), red);
    assert_eq!(frame.history.len(), HISTORY_LIMIT);
    assert_eq!(frame.history[0].cells[0].text, "old-1");
    assert_eq!(
        frame.history[HISTORY_LIMIT - 2].cells[0].text,
        format!("old-{}", HISTORY_LIMIT - 1)
    );
    assert_eq!(
        frame.history[HISTORY_LIMIT - 1].cells[0].text,
        "new",
        "the newest scrollback row is kept and the oldest dropped"
    );
}

/// Command output is rendered by the host and pushed on a topic, so the
/// subscription and the pushed screen are what an older or non-Rust client has
/// to speak.
#[test]
fn command_output_subscription_uses_its_documented_wire_shape() {
    use crate::terminal::{TerminalCell, TerminalFrame, TerminalRow, TerminalStyle};

    let topic = Topic::CommandOutput {
        session_id: "session-1".into(),
        item_id: "cmd-1".into(),
        cols: 40,
    };
    let request = ClientMessage {
        principal: None,
        key: None,
        id: 11,
        payload: ClientPayload::Subscribe(Subscription {
            known_index: None,
            topic: topic.clone(),
            after: None,
        }),
    };
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        json!({
            "id": 11,
            "payload": {
                "type": "subscribe",
                "content": {
                    "topic": {
                        "type": "command_output",
                        "content": {"session_id": "session-1", "item_id": "cmd-1", "cols": 40},
                    },
                    "after": null,
                },
            },
        })
    );
    assert_eq!(
        decode_client_line(&encode_line(&request).unwrap()).unwrap(),
        request
    );

    // A rendered screen: no cursor, no scrollback, no session behind it.
    let pushed = HostMessage::Event(EventEnvelope {
        request_id: None,
        topic,
        event: ServerEvent::CommandOutputReplaced(Box::new(TerminalFrame {
            cols: 40,
            rows: 1,
            styles: vec![
                TerminalStyle::default(),
                TerminalStyle {
                    fg: crate::terminal::TerminalColor::Indexed(1),
                    flags: 0b10,
                    ..TerminalStyle::default()
                },
            ],
            visible: vec![TerminalRow {
                cells: vec![TerminalCell {
                    text: "x".into(),
                    style: 1,
                    ..TerminalCell::default()
                }],
                wrapped: false,
            }],
            ..TerminalFrame::default()
        })),
    });
    assert_eq!(
        serde_json::to_value(&pushed).unwrap(),
        json!({
            "type": "event",
            "content": {
                "topic": {
                    "type": "command_output",
                    "content": {"session_id": "session-1", "item_id": "cmd-1", "cols": 40},
                },
                "event": {
                    "type": "command_output_replaced",
                    "content": {
                        "cols": 40,
                        "rows": 1,
                        "modes": {"mode": 0, "keyboard": 0},
                        "styles": [
                            {"fg": {"type": "foreground"}, "bg": {"type": "background"}},
                            {
                                "fg": {"type": "indexed", "content": 1},
                                "bg": {"type": "background"},
                                "flags": 2,
                            },
                        ],
                        "visible": [{"cells": [{"text": "x", "style": 1}]}],
                        "title": "",
                    },
                },
            },
        })
    );
    assert_eq!(
        decode_host_line(&encode_line(&pushed).unwrap()).unwrap(),
        pushed
    );
}

/// Rendering an export and registering a project both moved a decision to the
/// host, so their literal shapes are what an older or non-Rust client sees.
#[test]
fn thread_export_and_project_creation_use_their_documented_wire_shapes() {
    let request = ClientMessage {
        principal: None,
        key: None,
        id: 7,
        payload: ClientPayload::Query(Query::RenderThreadExport {
            session_id: "session-1".into(),
            format: ThreadExportFormat::Markdown,
        }),
    };
    assert_eq!(
        serde_json::to_value(&request).unwrap(),
        json!({
            "id": 7,
            "payload": {
                "type": "query",
                "content": {
                    "type": "render_thread_export",
                    "content": {"session_id": "session-1", "format": "markdown"},
                },
            },
        })
    );
    assert_eq!(
        decode_client_line(&encode_line(&request).unwrap()).unwrap(),
        request
    );

    // Bytes travel base64, so the artifact survives a transport that is text.
    let answer = HostMessage::QueryResult {
        id: 7,
        result: Ok(QueryResponse::ThreadExport {
            bytes: b"# Title\n".to_vec(),
            suggested_name: "Title.md".into(),
            mime: "text/markdown".into(),
        }),
    };
    assert_eq!(
        serde_json::to_value(&answer).unwrap(),
        json!({
            "type": "query_result",
            "content": {
                "id": 7,
                "result": {"Ok": {
                    "type": "thread_export",
                    "content": {
                        "bytes": "IyBUaXRsZQo=",
                        "suggested_name": "Title.md",
                        "mime": "text/markdown",
                    },
                }},
            },
        })
    );
    assert_eq!(
        decode_host_line(&encode_line(&answer).unwrap()).unwrap(),
        answer
    );

    // A root the host will not accept comes back as a typed protocol error, so
    // the client can show the host's reason instead of guessing one.
    let rejected = HostMessage::Ack {
        id: 8,
        result: Err(ProtocolError {
            code: "invalid_project_root".into(),
            message: r"C:\Users\dev\src is not an absolute path on this host".into(),
        }),
    };
    assert_eq!(
        serde_json::to_value(&rejected).unwrap(),
        json!({
            "type": "ack",
            "content": {
                "id": 8,
                "result": {"Err": {
                    "code": "invalid_project_root",
                    "message": r"C:\Users\dev\src is not an absolute path on this host",
                }},
            },
        })
    );
    assert_eq!(
        decode_host_line(&encode_line(&rejected).unwrap()).unwrap(),
        rejected
    );
}

#[test]
fn history_paging_literal_json_contract() {
    let query = r#"{"type":"session_history_page","content":{"session_id":"thread","before":1800,"limit":200}}"#;
    assert_eq!(
        serde_json::from_str::<Query>(query).unwrap(),
        Query::SessionHistoryPage {
            session_id: "thread".into(),
            before: 1800,
            limit: 200
        }
    );
    let response = QueryResponse::SessionHistoryPage {
        records: vec![],
        from: 1600,
        end: 1800,
        truncated: true,
    };
    assert_eq!(
        serde_json::to_string(&response).unwrap(),
        r#"{"type":"session_history_page","content":{"records":[],"from":1600,"end":1800,"truncated":true}}"#
    );
    let snapshot = ServerEvent::SessionSnapshot {
        from: 1800,
        end: 2000,
        records: vec![],
        total: 2000,
        total_turns: 0,
        truncated: false,
    };
    assert_eq!(
        serde_json::to_string(&snapshot).unwrap(),
        r#"{"type":"session_snapshot","content":{"from":1800,"end":2000,"records":[],"total":2000,"total_turns":0,"truncated":false}}"#
    );
    assert!(matches!(
        serde_json::from_str::<ServerEvent>(
            r#"{"type":"session_snapshot","content":{"from":0,"end":0,"records":[]}}"#
        )
        .unwrap(),
        ServerEvent::SessionSnapshot {
            total: 0,
            total_turns: 0,
            truncated: false,
            ..
        }
    ));
}

#[test]
fn version_six_index_visits_output_and_elision_literal_json() {
    let index = IndexSnapshot {
        summary: IndexSummary {
            archived_counts: [("p".to_string(), 2)].into(),
            activity: [
                (
                    "cold".into(),
                    SessionActivity {
                        working: false,
                        turn_running: false,
                        waiting: false,
                        waiting_for_approval: false,
                        waiting_for_input: false,
                        failed: false,
                        unread: true,
                        fork: ForkAvailability::Available,
                        agent: None,
                    },
                ),
                (
                    "child".into(),
                    SessionActivity {
                        working: false,
                        turn_running: false,
                        waiting: false,
                        waiting_for_approval: false,
                        waiting_for_input: false,
                        failed: false,
                        unread: false,
                        fork: ForkAvailability::Available,
                        agent: Some(AgentStatus {
                            execution: AgentExecution::Finished,
                            delivery: AgentDelivery::AwaitingSettle,
                            report: AgentReport::Reported,
                            run_started_at: Some(1_000),
                            run_completed_at: Some(81_000),
                        }),
                    },
                ),
            ]
            .into(),
            archived_revision: 7,
            ..IndexSummary::default()
        },
        sessions: vec![],
        projects: vec![],
    };
    assert_eq!(
        serde_json::to_value(&index).unwrap(),
        json!({"activity": {"cold": {"working":false,"turn_running":false,"waiting":false,
            "waiting_for_approval":false,"waiting_for_input":false,"failed":false,"unread":true,
            "fork":"available"},
            "child": {"working":false,"turn_running":false,"waiting":false,
            "waiting_for_approval":false,"waiting_for_input":false,"failed":false,"unread":false,
            "fork":"available","agent":{"execution":"finished","delivery":"awaiting_settle","report":"reported",
            "run_started_at":1000,"run_completed_at":81000}}},
            "title_generating": [], "archived_counts": {"p": 2},
            "worktree_shared": [], "archived_revision": 7, "sessions": [], "projects": []})
    );
    assert_eq!(
        serde_json::to_value(ServerEvent::LastVisitedChanged(
            [("s".to_string(), 5)].into()
        ))
        .unwrap(),
        json!({"type": "last_visited_changed", "content": {"s": 5}})
    );
    assert_eq!(
        serde_json::from_str::<Query>(
            r#"{"type":"read_item_output","content":{"session_id":"s","item_id":"i"}}"#
        )
        .unwrap(),
        Query::ReadItemOutput {
            session_id: "s".into(),
            item_id: "i".into()
        }
    );
    assert_eq!(
        serde_json::from_str::<Query>(r#"{"type":"archived_sessions"}"#).unwrap(),
        Query::ArchivedSessions
    );
    let status_wire: serde_json::Value = serde_json::from_str(r#"{"type":"session_status_replaced", "content": {
        "session_id":"s", "title":"Thread", "cwd":"/workspace", "attachments_dir":"/attachments",
        "provider":"codex", "requested_model":null, "requested_profile_id":null,
        "acp_agent_id":null, "project_id":null,
        "queued_messages":[{"id":3,"delivery_key":"send-key","text":"Next","fire_at_unix_secs":null,"editable":false}],
        "review_comment_drafts":[], "terminals":[], "active_terminal_id":null, "terminal_splits":[],
        "terminal_contexts":[], "terminal_open":false, "terminal_height":240.0, "delivery_in_flight":3,
        "activity":{"working":true,"turn_running":false,"waiting":false,"waiting_for_approval":false,
            "waiting_for_input":false,"failed":false,"unread":false,"fork":"available"},
        "stopping":false,"native_rewind_blocked":true,"checkout_blocked":false,"conversation_read_only":false,
        "terminal_limit_reached":false,"terminal_split_available":false,"usage":null,"context_window":200000,
        "running_turn":null,"pending_approvals":[],"pending_user_input":null,"supports_steering":true,
        "provider_option_descriptors":[],"provider_option_selections":[],"provider_option_requested_selections":[],"provider_commands":[],
        "git_branch":null,"branches":[],"draft":false,"draft_workspace":{"kind":"local_checkout"},"worktree":null,
        "preparing_worktree":false,"relay_confirmation":null,"native_rewind_pending":false,
        "native_rewind_prefill_available":false,"model_pending_restart":false,"options_pending_restart":false
    }}"#).unwrap();
    let status = serde_json::from_value::<ServerEvent>(status_wire.clone()).unwrap();
    assert_eq!(serde_json::to_value(status).unwrap(), status_wire);
    let archive_wire = json!({"type":"archived_sessions", "content": {"sessions":[], "worktree_shared":["owner"], "revision":7}});
    let archived = QueryResponse::ArchivedSessions(ArchivedSessions {
        sessions: Vec::new(),
        worktree_shared: ["owner".into()].into(),
        revision: 7,
    });
    assert_eq!(serde_json::to_value(&archived).unwrap(), archive_wire);
    assert_eq!(
        serde_json::from_value::<QueryResponse>(archive_wire).unwrap(),
        archived
    );
    assert_eq!(
        serde_json::to_value(Topic::SessionPlan {
            session_id: "s".into()
        })
        .unwrap(),
        json!({"type":"session_plan", "content":{"session_id":"s"}})
    );
    let turn_started = AgentEvent::TurnStarted {
        turn_id: "t".into(),
    };
    let record = SessionEventRecord {
        origin: None,
        author: None,
        ts: Some(1),
        event: turn_started.clone(),
        elided: Some(9000),
    };
    assert_eq!(
        serde_json::to_value(&record).unwrap(),
        json!({"ts": 1, "event": {"type": "turn_started", "turn_id": "t"}, "elided": 9000})
    );
    assert_eq!(
        serde_json::to_value(SessionEventRecord::from(turn_started)).unwrap(),
        json!({"ts": null, "event": {"type": "turn_started", "turn_id": "t"}})
    );
}

#[test]
fn command_key_is_optional_for_v3_and_preserved_for_v4() {
    let legacy = decode_client_line(
        r#"{"id":9,"payload":{"type":"command","content":{"type":"cycle_project_sort"}}}"#,
    )
    .unwrap();
    assert_eq!(legacy.key, None);
    assert_eq!(legacy.principal, None);
    let scoped = decode_client_line(r#"{"id":9,"principal":{"type":"space","content":{"space_id":"s","space_name":"Shared","project_ids":["p"],"device_id":"d","device_name":"Phone"}},"payload":{"type":"command","content":{"type":"cycle_project_sort"}}}"#).unwrap();
    assert_eq!(
        scoped.principal,
        Some(Principal::Space {
            policy_revision: 0,
            space_id: "s".into(),
            space_name: "Shared".into(),
            project_ids: vec!["p".into()],
            device_id: "d".into(),
            device_name: "Phone".into(),
        })
    );
    assert_eq!(
        serde_json::to_value(&scoped).unwrap()["principal"],
        json!({"type":"space","content":{"policy_revision":0,"space_id":"s","space_name":"Shared","project_ids":["p"],"device_id":"d","device_name":"Phone"}})
    );
    let keyed = decode_client_line(r#"{"id":9,"key":"951925af-31f3-4f1c-a57b-dac199d82ad7","payload":{"type":"command","content":{"type":"cycle_project_sort"}}}"#).unwrap();
    assert_eq!(
        keyed.key.as_deref(),
        Some("951925af-31f3-4f1c-a57b-dac199d82ad7")
    );
    assert_eq!(keyed.payload, legacy.payload);
}

/// The hosting reply is read by browsers and phones that may be older or
/// newer than the machine: a machine from the six-digit-code era (its
/// `code` field is ignored) or without device paths must still be
/// understood, and a machine that has the link sends it whole so a scanner
/// needs nothing else to reach it off the LAN. Each Traverse source's status
/// is spelled out for the settings page that draws it.
#[test]
fn hosting_state_keeps_older_machines_readable_and_carries_the_invite_link() {
    let older: HostingState = serde_json::from_value(json!({
        "enabled": true,
        "code": "123456",
        "expires_in_secs": 280,
        "host_id": "ab".repeat(32),
        "host_name": "Studio",
        "devices": [{"id": "cd".repeat(32), "name": "Phone", "created_unix": 1}]
    }))
    .unwrap();
    assert_eq!(older.invite, None);
    assert_eq!(older.created_space_id, None);
    assert_eq!(older.devices[0].path, None);
    assert_eq!(
        serde_json::from_value::<HostingAction>(json!({"type": "new_code"})).unwrap(),
        HostingAction::NewInvitation
    );
    assert_eq!(
        serde_json::to_value(HostingAction::NewInvitation).unwrap(),
        json!({"type": "new_invitation"})
    );

    assert!(older.spaces.is_empty());
    assert!(older.traverse.is_empty());
    assert_eq!(older.devices[0].access, DeviceAccess::Full);
    for (action, wire) in [
        (
            SpaceAction::Create {
                name: "Shared".into(),
            },
            json!({"type":"create","content":{"name":"Shared"}}),
        ),
        (
            SpaceAction::Rename {
                id: "s".into(),
                name: "Renamed".into(),
            },
            json!({"type":"rename","content":{"id":"s","name":"Renamed"}}),
        ),
        (
            SpaceAction::Delete { id: "s".into() },
            json!({"type":"delete","content":{"id":"s"}}),
        ),
        (
            SpaceAction::SetProjects {
                id: "s".into(),
                project_ids: vec!["p".into()],
            },
            json!({"type":"set_projects","content":{"id":"s","project_ids":["p"]}}),
        ),
        (
            SpaceAction::RegenerateLink { id: "s".into() },
            json!({"type":"regenerate_link","content":{"id":"s"}}),
        ),
        (
            SpaceAction::SetLinkEnabled {
                id: "s".into(),
                enabled: false,
            },
            json!({"type":"set_link_enabled","content":{"id":"s","enabled":false}}),
        ),
        (
            SpaceAction::MoveMember {
                device_id: "d".into(),
                space_id: "s".into(),
            },
            json!({"type":"move_member","content":{"device_id":"d","space_id":"s"}}),
        ),
        (
            SpaceAction::RemoveMember {
                device_id: "d".into(),
                regenerate_link: true,
            },
            json!({"type":"remove_member","content":{"device_id":"d","regenerate_link":true}}),
        ),
    ] {
        let action = HostingAction::Spaces(action);
        let wire = json!({"type":"spaces","content":wire});
        assert_eq!(serde_json::to_value(&action).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<HostingAction>(wire).unwrap(),
            action
        );
    }
    for (action, wire) in [
        (
            GroupAction::Create {
                name: "Home".into(),
            },
            json!({"type":"create","content":{"name":"Home"}}),
        ),
        (
            GroupAction::Join {
                link: "tcode://pair?v=3".into(),
                address: Some("192.168.1.2".into()),
            },
            json!({"type":"join","content":{"link":"tcode://pair?v=3","address":"192.168.1.2"}}),
        ),
        (GroupAction::Leave, json!({"type":"leave"})),
        (
            GroupAction::RemoveMachine("m".into()),
            json!({"type":"remove_machine","content":"m"}),
        ),
        (
            GroupAction::Admit("d".into()),
            json!({"type":"admit","content":"d"}),
        ),
    ] {
        let action = HostingAction::Group(action);
        let wire = json!({"type":"group","content":wire});
        assert_eq!(serde_json::to_value(&action).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<HostingAction>(wire).unwrap(),
            action
        );
    }
    let mut state = HostingState {
        group: None,
        created_space_id: None,
        spaces: Vec::new(),
        enabled: true,
        expires_in_secs: 280,
        host_id: "ab".repeat(32),
        host_name: "Studio".into(),
        invite: Some("tcode://pair?v=2&id=abab".into()),
        devices: vec![
            HostedDevice {
                access: DeviceAccess::Full,
                id: "cd".repeat(32),
                name: "Phone".into(),
                created_unix: 1,
                platform: Some("iOS 26".into()),
                path: Some(PathInfo {
                    direct: false,
                    relay: Some("https://relay.example/".into()),
                    lan: false,
                    probing_direct: true,
                }),
            },
            HostedDevice {
                access: DeviceAccess::Space {
                    space_id: "s".into(),
                },
                id: "ef".repeat(32),
                name: "Laptop".into(),
                created_unix: 2,
                platform: None,
                path: None,
            },
        ],
        traverse: vec![
            TraverseSourceStatus {
                source: "official".into(),
                manifest: TraverseManifestStatus {
                    state: TraverseManifestState::Cached,
                    fetched_unix: Some(1_760_000_000),
                    error: Some("timed out".into()),
                },
                relays: vec![
                    TraverseRelayStatus {
                        url: "https://euc1-1.relay.n0.iroh.link./".into(),
                        region: Some("eu".into()),
                        latency_ms: Some(42),
                        home: true,
                        error: None,
                    },
                    TraverseRelayStatus {
                        url: "https://use1-1.relay.n0.iroh.link./".into(),
                        region: None,
                        latency_ms: None,
                        home: false,
                        error: None,
                    },
                ],
                lookups: vec![TraverseLookupStatus {
                    url: "https://dns.iroh.link/pkarr".into(),
                    state: TraverseLookupState::Ok,
                    checked_unix: Some(1_760_000_100),
                    error: None,
                }],
            },
            TraverseSourceStatus {
                source: "https://traverse.example/".into(),
                manifest: TraverseManifestStatus {
                    state: TraverseManifestState::Failed,
                    fetched_unix: None,
                    error: Some("connection refused".into()),
                },
                relays: Vec::new(),
                lookups: Vec::new(),
            },
        ],
    };
    state.spaces.push(SpaceInfo {
        id: "s".into(),
        name: "Shared".into(),
        created_unix: 3,
        project_ids: vec!["p".into()],
        link: Some("tcode://pair?v=2&id=abab&space=s".into()),
        link_enabled: true,
        link_dead: false,
        members: vec![state.devices[1].clone()],
    });
    assert_eq!(
        serde_json::to_value(&state).unwrap(),
        json!({
            "enabled": true,
            "expires_in_secs": 280,
            "host_id": "ab".repeat(32),
            "host_name": "Studio",
            "invite": "tcode://pair?v=2&id=abab",
            "devices": [
                {
                    "id": "cd".repeat(32),
                    "name": "Phone",
                    "created_unix": 1,
                    "access": {"type":"full"},
                    "platform": "iOS 26",
                    "path": {"direct": false, "relay": "https://relay.example/", "probing_direct": true}
                },
                {"id": "ef".repeat(32), "name": "Laptop", "created_unix": 2, "access":{"type":"space","content":{"space_id":"s"}}}
            ],
            "spaces": [{"id":"s","name":"Shared","created_unix":3,"project_ids":["p"],
                "link":"tcode://pair?v=2&id=abab&space=s","link_enabled":true,"link_dead":false,
                "members":[{"id":"ef".repeat(32),"name":"Laptop","created_unix":2,"access":{"type":"space","content":{"space_id":"s"}}}]}],
            "traverse": [
                {
                    "source": "official",
                    "manifest": {"state": "cached", "fetched_unix": 1_760_000_000_u64, "error": "timed out"},
                    "relays": [
                        {"url": "https://euc1-1.relay.n0.iroh.link./", "region": "eu", "latency_ms": 42, "home": true},
                        {"url": "https://use1-1.relay.n0.iroh.link./"}
                    ],
                    "lookups": [
                        {"url": "https://dns.iroh.link/pkarr", "state": "ok", "checked_unix": 1_760_000_100_u64}
                    ]
                },
                {
                    "source": "https://traverse.example/",
                    "manifest": {"state": "failed", "error": "connection refused"},
                    "relays": [],
                    "lookups": []
                }
            ]
        })
    );
}

/// A path is direct or relayed on every peer; whether a direct path stays
/// on the LAN, and whether a relay is still waiting for one, are newer
/// flags that a machine from before them never sends. Such a machine's
/// direct path reads as punched, since that is what it could not tell.
#[test]
fn path_info_kinds_read_older_peers_and_spell_the_flags() {
    let older: PathInfo = serde_json::from_value(json!({"direct": true})).unwrap();
    assert_eq!(older.kind(), PathKind::Tunnel);
    let lan = PathInfo {
        direct: true,
        relay: None,
        lan: true,
        probing_direct: false,
    };
    assert_eq!(lan.kind(), PathKind::Lan);
    assert_eq!(
        serde_json::to_value(&lan).unwrap(),
        json!({"direct": true, "lan": true})
    );
    let relayed: PathInfo =
        serde_json::from_value(json!({"direct": false, "relay": "https://relay.example/"}))
            .unwrap();
    assert_eq!(
        relayed.kind(),
        PathKind::Relay {
            url: Some("https://relay.example/")
        }
    );
    assert!(!relayed.probing_direct);
}

/// A provider-plugin catalog and a plugin command are read by clients that
/// render the actions the host computed, so their literal shapes are the
/// contract; an older providers snapshot still decodes with no catalogs.
#[test]
fn provider_plugin_catalog_and_commands_use_their_documented_wire_shapes() {
    let older: ProvidersStatus = serde_json::from_value(json!({
        "model_catalogs": {}, "models_loading": {}, "provider_versions": {},
        "tcode_update": {"current": "1.0.0", "latest": null, "release_url": null,
            "update_available": false, "checking": false},
        "provider_snapshots": {}, "acp_marketplace_items": [], "acp_registry_loading": false,
        "acp_registry_error": null, "acp_installing": [], "providers_checked_at": null,
        "providers_checking": false, "secret_names": {}
    }))
    .unwrap();
    assert!(older.plugins.is_empty());

    let catalog: ProviderPluginCatalog = serde_json::from_value(json!({
        "profile_id": "claude",
        "context_cwd": "/work/proj",
        "marketplaces": [{"name": "tcode-probe", "source": "/tmp/tcode-probe/mkt",
            "kind": "local_path", "location": "/tmp/tcode-probe/mkt"}],
        "marketplace_actions": [
            {"type": "add"},
            {"type": "remove", "marketplace": "tcode-probe", "uninstalls": ["alpha@tcode-probe"]}
        ],
        "entries": [{
            "id": "alpha@tcode-probe",
            "name": "alpha",
            "version": "1.0.0",
            "source": {"marketplace": "tcode-probe", "kind": "local_path"},
            "installations": [{"scope": "project", "location": "/cache/alpha/1.0.0",
                "version": "1.0.0", "scope_enabled": true}],
            "enabled": "yes",
            "declared": {"skills": ["greet", "hello"], "hooks": ["SessionStart"],
                "mcp_servers": ["alpha-dead"]},
            "actions": [{"type": "disable", "scope": "project"},
                {"type": "update", "scope": "project"}],
            "diagnostics": [["message", "Restart to apply changes."]]
        }],
        "errors": ["failed to load marketplace /work/proj/.agents/plugins/marketplace.json"],
        "state": {"type": "stale", "content": {"reason": "changed"}},
        "loading": true,
        "pending": ["gamma@tcode-probe"],
        "challenges": [{
            "op_id": 12,
            "profile_id": "claude",
            "entry_id": "gamma@tcode-probe",
            "kind": {"type": "accept_command", "content": {
                "command": "echo /tmp/tcode-probe/gamma-src", "sha256": "b9c02c85", "mode": "copy"}},
            "native_text": "\"gamma\" is installed by running a command"
        }]
    }))
    .unwrap();
    assert_eq!(
        catalog.errors,
        ["failed to load marketplace /work/proj/.agents/plugins/marketplace.json"]
    );
    let alpha = &catalog.entries[0];
    assert_eq!(alpha.enabled, agent::Tri::Yes);
    assert_eq!(alpha.description, None);
    assert!(alpha.errors.is_empty());
    assert_eq!(
        alpha.installations[0],
        agent::PluginInstallation {
            scope: agent::PluginScope::Project,
            location: Some(PathBuf::from("/cache/alpha/1.0.0")),
            version: Some("1.0.0".into()),
            scope_enabled: Some(true),
        }
    );
    let declared = alpha.declared.as_ref().unwrap();
    assert_eq!(declared.agents, None);
    assert_eq!(
        declared.mcp_servers.as_deref(),
        Some(&["alpha-dead".to_string()][..])
    );
    assert_eq!(
        alpha.actions,
        [
            agent::PluginAction::Disable {
                scope: agent::PluginScope::Project
            },
            agent::PluginAction::Update {
                scope: agent::PluginScope::Project
            },
        ]
    );
    assert_eq!(
        catalog.marketplace_actions[1],
        agent::MarketplaceAction::Remove {
            marketplace: "tcode-probe".into(),
            uninstalls: vec!["alpha@tcode-probe".into()],
        }
    );
    assert_eq!(
        catalog.state,
        PluginCatalogState::Stale {
            reason: PluginStaleReason::Changed
        }
    );
    assert_eq!(
        catalog.challenges[0].kind,
        PluginChallengeKind::AcceptCommand {
            command: "echo /tmp/tcode-probe/gamma-src".into(),
            sha256: "b9c02c85".into(),
            mode: Some("copy".into()),
        }
    );
    assert_eq!(catalog.challenges[0].op_id, RuntimeOperationId(12));

    let command = decode_client_line(r#"{"id":4,"payload":{"type":"command","content":{"type":"install_provider_plugin","content":{"profile_id":"claude","entry_id":"gamma@tcode-probe","scope":"project","cwd":"/work/proj"}}}}"#).unwrap();
    assert_eq!(
        command.payload,
        ClientPayload::Command(Command::InstallProviderPlugin {
            profile_id: "claude".into(),
            entry_id: "gamma@tcode-probe".into(),
            scope: agent::PluginScope::Project,
            cwd: Some(PathBuf::from("/work/proj")),
        })
    );
    let resolve = decode_client_line(r#"{"id":5,"payload":{"type":"command","content":{"type":"resolve_plugin_challenge","content":{"op_id":12,"accept":true}}}}"#).unwrap();
    assert_eq!(
        resolve.payload,
        ClientPayload::Command(Command::ResolvePluginChallenge {
            op_id: RuntimeOperationId(12),
            accept: true,
        })
    );
}

#[test]
fn scope_subscriptions_and_provider_choices_have_literal_wire_contracts() {
    for (topic, wire) in [
        (Topic::Scope, json!({"type":"scope"})),
        (
            Topic::SpaceIndex {
                space_id: "s".into(),
            },
            json!({"type":"space_index","content":{"space_id":"s"}}),
        ),
    ] {
        let wire = json!({"id":1,"principal":{"type":"full"},"payload":{"type":"subscribe","content":{"topic":wire,"after":null}}});
        let request = ClientMessage {
            id: 1,
            key: None,
            principal: Some(Principal::Full),
            payload: ClientPayload::Subscribe(Subscription {
                known_index: None,
                topic,
                after: None,
            }),
        };
        assert_eq!(serde_json::to_value(&request).unwrap(), wire);
        assert_eq!(decode_client_line(&wire.to_string()).unwrap(), request);
    }
    let scope = Scope::Space {
        space_id: "s".into(),
        space_name: "Shared".into(),
        projects: vec![],
        providers: vec![ScopedProviderChoice {
            provider: agent::ProviderKind::Codex,
            profile_id: Some("codex".into()),
            name: "Codex".into(),
            models: vec![agent::ModelSpec {
                id: "model".into(),
                display_name: "Model".into(),
                is_default: true,
                options: vec![],
            }],
        }],
    };
    let scope_wire = json!({"type":"space","content":{"space_id":"s","space_name":"Shared","projects":[],"providers":[{"provider":"codex","profile_id":"codex","name":"Codex","models":[{"id":"model","display_name":"Model","is_default":true,"options":[]}]}]}});
    for (event, wire) in [
        (
            ServerEvent::ScopeSnapshot(Scope::Full),
            json!({"type":"scope_snapshot","content":{"type":"full"}}),
        ),
        (
            ServerEvent::ScopeSnapshot(scope.clone()),
            json!({"type":"scope_snapshot","content":scope_wire.clone()}),
        ),
    ] {
        let response = HostMessage::Event(EventEnvelope {
            request_id: Some(1),
            topic: Topic::Scope,
            event,
        });
        let wire = json!({"type":"event","content":{"request_id":1,"topic":{"type":"scope"},"event":wire}});
        assert_eq!(serde_json::to_value(&response).unwrap(), wire);
        assert_eq!(decode_host_line(&wire.to_string()).unwrap(), response);
    }
}
