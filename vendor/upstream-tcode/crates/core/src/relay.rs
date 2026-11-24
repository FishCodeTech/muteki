//! Provider-independent conversation handoff rendering.

use std::path::Path;

use agent::{ItemContent, ItemStatus, PlanStepStatus, ProviderKind};

use crate::session::{EntryContent, Timeline, TimelineEntry};

pub const RELAY_TRANSCRIPT_MAX_CHARS: usize = 60_000;
pub const RELAY_PREAMBLE: &str = "You are taking over an ongoing conversation another agent worked on. The transcript follows; continue seamlessly, do not re-introduce yourself or re-do completed work.";

/// True when the timeline carries work worth handing over: a turn that
/// completed, or one that produced provider work before it was interrupted,
/// failed, or was still running when the user switched provider. A draft, or a
/// history whose only turn holds the opening user message and nothing back,
/// needs no relay.
pub fn has_meaningful_history(timeline: &Timeline) -> bool {
    timeline.turns.iter().enumerate().any(|(index, turn)| {
        turn.status == Some(agent::TurnStatus::Completed)
            || timeline
                .entries
                .iter()
                .any(|entry| entry.turn == index && is_provider_work(&entry.content))
    })
}

fn is_provider_work(content: &EntryContent) -> bool {
    match content {
        EntryContent::Item(ItemContent::AssistantMessage { text }) => !text.trim().is_empty(),
        EntryContent::Item(
            ItemContent::CommandExecution { .. }
            | ItemContent::FileChange { .. }
            | ItemContent::ToolCall { .. }
            | ItemContent::Subagent { .. }
            | ItemContent::ImageRead { .. }
            | ItemContent::WebSearch { .. }
            | ItemContent::Other { .. },
        ) => true,
        EntryContent::Item(ItemContent::UserMessage { .. } | ItemContent::Reasoning { .. })
        | EntryContent::Steer { .. }
        | EntryContent::Error { .. }
        | EntryContent::ProviderStartError { .. }
        | EntryContent::ProviderRelay { .. }
        | EntryContent::ContextCompacted(_)
        | EntryContent::ContextWindowChanged { .. }
        | EntryContent::ControlAction(_)
        | EntryContent::ModelChanged { .. } => false,
    }
}

/// Render the canonical folded timeline as a compact markdown handoff.
pub fn render_relay_transcript(
    timeline: &Timeline,
    project_path: &Path,
    original_provider: ProviderKind,
    original_model: Option<&str>,
    max_chars: usize,
) -> String {
    if !has_meaningful_history(timeline) {
        return String::new();
    }

    let completed_turns = timeline
        .turns
        .iter()
        .filter(|turn| turn.status == Some(agent::TurnStatus::Completed))
        .count();
    let (original_provider, original_model) = timeline
        .entries
        .iter()
        .find_map(|entry| match &entry.content {
            EntryContent::ProviderRelay {
                from_provider,
                from_model,
                ..
            } => Some((*from_provider, from_model.as_deref())),
            _ => None,
        })
        .unwrap_or((original_provider, original_model));
    let model = original_model.unwrap_or("provider default");
    let header = format!(
        "# Conversation relay\n\n- Project: `{}`\n- Original provider/model: {} / {}\n- Completed turns: {}\n",
        project_path.display(),
        original_provider.display_name(),
        model,
        completed_turns
    );
    let closing = "\n---\nThis is where the previous agent left off.\n";
    let mut turn_blocks = Vec::new();
    for turn in 0..timeline.turns.len() {
        let entries: Vec<&TimelineEntry> = timeline
            .entries
            .iter()
            .filter(|entry| entry.turn == turn)
            .map(AsRef::as_ref)
            .collect();
        let mut block = render_turn(turn + 1, &entries);
        if block.is_empty() {
            continue;
        }
        match timeline.turns[turn].status {
            Some(agent::TurnStatus::Completed) => {}
            Some(agent::TurnStatus::Interrupted) => {
                block.push_str("> This turn was interrupted before it completed.\n\n");
            }
            Some(agent::TurnStatus::Failed) => {
                block.push_str("> This turn failed before it completed.\n\n");
            }
            None => {
                block.push_str(
                    "> This turn was still running when the conversation was handed over.\n\n",
                );
            }
        }
        turn_blocks.push(block);
    }

    if !timeline.plan_steps.is_empty() {
        let mut plan = String::from("### Current todo state\n\n");
        if let Some(explanation) = timeline.plan_explanation.as_deref() {
            plan.push_str(explanation.trim());
            plan.push_str("\n\n");
        }
        for step in &timeline.plan_steps {
            let marker = match step.status {
                PlanStepStatus::Completed => "x",
                PlanStepStatus::Pending | PlanStepStatus::InProgress => " ",
            };
            let suffix = if step.status == PlanStepStatus::InProgress {
                " (in progress)"
            } else {
                ""
            };
            plan.push_str(&format!("- [{marker}] {}{suffix}\n", step.step));
        }
        if let Some(last) = turn_blocks.last_mut() {
            last.push_str(&plan);
        }
    }

    let full = format!("{}\n{}{}", header, turn_blocks.join("\n"), closing);
    if full.chars().count() <= max_chars {
        return full;
    }

    render_elided(timeline, &header, closing, &turn_blocks, max_chars)
}

pub fn assemble_relay_prompt(transcript: &str, user_message: &str) -> String {
    format!(
        "{RELAY_PREAMBLE}\n\n<conversation-transcript>\n{}\n</conversation-transcript>\n\n<new-user-message>\n{}\n</new-user-message>",
        transcript.trim(),
        user_message
    )
}

fn render_elided(
    timeline: &Timeline,
    header: &str,
    closing: &str,
    blocks: &[String],
    max_chars: usize,
) -> String {
    let first_user = timeline.first_user_message().unwrap_or("");
    let first = if first_user.is_empty() {
        String::new()
    } else {
        format!("## First user message\n\n{}\n\n", first_user)
    };
    let mut kept = Vec::new();
    let mut used = header.chars().count() + first.chars().count() + closing.chars().count() + 64;
    for block in blocks.iter().rev() {
        let len = block.chars().count() + 1;
        if used + len > max_chars && !kept.is_empty() {
            break;
        }
        if used + len > max_chars {
            kept.push(truncate_chars(block, max_chars.saturating_sub(used)));
            break;
        }
        kept.push(block.clone());
        used += len;
    }
    kept.reverse();
    let elided = blocks.len().saturating_sub(kept.len());
    let marker = format!("[... {elided} earlier turns elided ...]\n\n");
    let mut out = format!("{header}\n{first}{marker}{}{closing}", kept.join("\n"));
    if out.chars().count() > max_chars {
        out = truncate_chars(&out, max_chars);
    }
    out
}

fn render_turn(number: usize, entries: &[&TimelineEntry]) -> String {
    let mut body = String::new();
    for entry in entries {
        match &entry.content {
            EntryContent::Item(ItemContent::UserMessage {
                text, context_len, ..
            })
            | EntryContent::Steer {
                text, context_len, ..
            } => {
                let visible = context_len.and_then(|len| text.get(len..)).unwrap_or(text);
                if !visible.trim().is_empty() {
                    body.push_str("### User\n\n");
                    body.push_str(visible);
                    body.push_str("\n\n");
                }
            }
            EntryContent::Item(ItemContent::AssistantMessage { text })
                if !text.trim().is_empty() =>
            {
                body.push_str("### Assistant\n\n");
                body.push_str(text);
                body.push_str("\n\n");
            }
            EntryContent::Item(ItemContent::AssistantMessage { .. }) => {}
            EntryContent::Item(ItemContent::CommandExecution {
                command,
                output,
                exit_code,
                status,
            }) => {
                let outcome = if let Some(code) = exit_code {
                    format!("exit {code}: {}", one_line(output))
                } else {
                    status_outcome(*status, output)
                };
                activity(&mut body, "command", &one_line(command), &outcome);
            }
            EntryContent::Item(ItemContent::FileChange { changes, .. }) => {
                let target = changes
                    .iter()
                    .map(|change| change.path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                activity(
                    &mut body,
                    "file_change",
                    &target,
                    &format!("{} file(s) changed", changes.len()),
                );
            }
            EntryContent::Item(ItemContent::ToolCall {
                name,
                input,
                output,
                status,
                ..
            }) => {
                let target = tool_target(input);
                let outcome = status_outcome(*status, output.as_deref().unwrap_or(""));
                activity(&mut body, name, &target, &outcome);
            }
            EntryContent::Item(ItemContent::Subagent {
                agent_type,
                description,
                status,
                summary,
                ..
            }) => {
                let outcome = status_outcome(*status, summary.as_deref().unwrap_or(""));

                activity(&mut body, agent_type, &one_line(description), &outcome);
            }
            EntryContent::Error { message, .. }
            | EntryContent::ProviderStartError { error: message } => {
                activity(&mut body, "error", "provider", &one_line(message));
            }
            EntryContent::ProviderRelay {
                from_provider,
                to_provider,
                ..
            } => {
                body.push_str(&format!(
                    "> Relayed from {} to {}.\n\n",
                    from_provider.display_name(),
                    to_provider.display_name()
                ));
            }
            EntryContent::ContextCompacted(c) => activity(
                &mut body,
                "context",
                "provider",
                &format!(
                    "{}; trigger={:?}; pre_tokens={:?}",
                    if c.in_progress {
                        "compacting"
                    } else {
                        "compacted"
                    },
                    c.trigger,
                    c.pre_tokens
                ),
            ),
            EntryContent::ContextWindowChanged { window } => activity(
                &mut body,
                "context",
                "provider",
                &format!("window set to {}", agent::context_window::format(*window)),
            ),
            EntryContent::ModelChanged { .. } => {}
            EntryContent::ControlAction(action) => activity(
                &mut body,
                "control",
                &action.actor_title,
                &match &action.detail {
                    Some(detail) => format!("{}: {detail}", action.action),
                    None => action.action.clone(),
                },
            ),
            EntryContent::Item(ItemContent::ImageRead { path, .. }) => activity(
                &mut body,
                "image_view",
                path,
                &status_outcome(ItemStatus::Completed, ""),
            ),
            EntryContent::Item(ItemContent::WebSearch { query }) => activity(
                &mut body,
                "web_search",
                &tool_target(&serde_json::json!({ "query": query })),
                &status_outcome(ItemStatus::Completed, ""),
            ),
            EntryContent::Item(ItemContent::Other {
                provider_kind,
                summary,
            }) => activity(
                &mut body,
                provider_kind,
                &tool_target(&serde_json::json!({ "summary": summary })),
                &status_outcome(ItemStatus::Completed, ""),
            ),
            EntryContent::Item(ItemContent::Reasoning { .. }) => {}
        }
    }
    if body.is_empty() {
        String::new()
    } else {
        format!("## Turn {number}\n\n{body}")
    }
}

fn activity(out: &mut String, name: &str, target: &str, outcome: &str) {
    out.push_str(&format!(
        "- `{}` — {} — {}\n",
        one_line(name),
        if target.is_empty() {
            "(no target)"
        } else {
            target
        },
        if outcome.is_empty() {
            "completed"
        } else {
            outcome
        }
    ));
}

fn tool_target(input: &serde_json::Value) -> String {
    const KEYS: [&str; 6] = ["path", "file", "command", "query", "url", "target"];
    KEYS.iter()
        .find_map(|key| input.get(key).and_then(|value| value.as_str()))
        .map(one_line)
        .unwrap_or_else(|| one_line(&input.to_string()))
}

fn status_outcome(status: ItemStatus, output: &str) -> String {
    let output = one_line(output);
    let label = match status {
        ItemStatus::Failed => "failed",
        ItemStatus::Interrupted => "interrupted",
        ItemStatus::InProgress => "in progress",
        ItemStatus::Completed if !output.is_empty() => return output,
        ItemStatus::Completed => "completed",
        ItemStatus::Declined => "declined",
    };
    if output.is_empty() {
        label.into()
    } else {
        format!("{label}: {output}")
    }
}

fn one_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use agent::{AgentEvent, ItemContent, ItemStatus, ThreadItem, TurnStatus};

    use super::*;

    fn item(id: &str, content: ItemContent) -> AgentEvent {
        AgentEvent::ItemCompleted(ThreadItem {
            id: id.into(),
            parent_item_id: None,
            content,
        })
    }

    fn user(id: &str, text: &str) -> AgentEvent {
        item(
            id,
            ItemContent::UserMessage {
                text: text.into(),
                context_len: None,
                attachments: Vec::new(),
            },
        )
    }

    fn assistant(id: &str, text: &str) -> AgentEvent {
        item(id, ItemContent::AssistantMessage { text: text.into() })
    }

    fn completed(id: &str) -> AgentEvent {
        AgentEvent::TurnCompleted {
            turn_id: id.into(),
            status: TurnStatus::Completed,
            usage: None,
        }
    }

    fn render(timeline: &Timeline, max_chars: usize) -> String {
        render_relay_transcript(
            timeline,
            Path::new("/work/project"),
            ProviderKind::ClaudeCode,
            Some("opus"),
            max_chars,
        )
    }

    #[test]
    fn transcript_preserves_turn_and_message_order() {
        let timeline = Timeline::fold_events([
            user("u1", "first request"),
            assistant("a1", "first answer"),
            completed("t1"),
            user("u2", "second request"),
            assistant("a2", "second answer"),
            completed("t2"),
        ]);

        let transcript = render(&timeline, 60_000);
        let positions = [
            "first request",
            "first answer",
            "second request",
            "second answer",
        ]
        .map(|needle| transcript.find(needle).unwrap());
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(transcript.contains("Original provider/model: Claude Code / opus"));
        assert!(transcript.contains("Completed turns: 2"));
    }

    #[test]
    fn work_activity_is_compacted_to_one_line_with_error_outcome() {
        let timeline = Timeline::fold_events([
            user("u1", "run it"),
            item(
                "tool",
                ItemContent::ToolCall {
                    presentation: None,
                    image_reads: Vec::new(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "src/main.rs"}),
                    output: Some("line one\nline two".into()),
                    status: ItemStatus::Completed,
                },
            ),
            item(
                "failed",
                ItemContent::ToolCall {
                    presentation: None,
                    image_reads: Vec::new(),
                    name: "build".into(),
                    input: serde_json::json!({"target": "workspace"}),
                    output: Some("compiler error\nmore detail".into()),
                    status: ItemStatus::Failed,
                },
            ),
            completed("t1"),
        ]);

        let transcript = render(&timeline, 60_000);
        assert!(transcript.contains("`read_file` — src/main.rs — line one line two"));
        assert!(transcript.contains("`build` — workspace — failed: compiler error more detail"));
        assert!(!transcript.contains("line one\nline two"));
    }

    #[test]
    fn oversized_transcript_keeps_first_user_and_recent_turns() {
        let huge = "middle ".repeat(2_000);
        let timeline = Timeline::fold_events([
            user("u1", "keep this first request"),
            assistant("a1", "old answer"),
            completed("t1"),
            user("u2", "middle request"),
            assistant("a2", &huge),
            completed("t2"),
            user("u3", "keep this recent request"),
            assistant("a3", "keep this recent answer"),
            completed("t3"),
        ]);

        let transcript = render(&timeline, 1_000);
        assert!(transcript.chars().count() <= 1_000);
        assert!(transcript.contains("keep this first request"));
        assert!(transcript.contains("keep this recent request"));
        assert!(transcript.contains("keep this recent answer"));
        assert!(transcript.contains("[... 2 earlier turns elided ...]"));
        assert!(!transcript.contains("middle request"));
    }

    #[test]
    fn empty_or_incomplete_history_has_no_transcript() {
        let empty = Timeline::default();
        assert_eq!(render(&empty, 60_000), "");

        let incomplete = Timeline::fold_events([user("u1", "not completed")]);
        assert_eq!(render(&incomplete, 60_000), "");
    }

    /// Switching provider while the only turn is still streaming, or after it
    /// was interrupted, hands over what that turn produced and says it was
    /// cut short; an opening message that got nothing back hands over nothing.
    #[test]
    fn an_unfinished_turn_with_provider_work_is_relayed_with_a_note() {
        let started = AgentEvent::TurnStarted {
            turn_id: "t1".into(),
        };
        let running = Timeline::fold_events([
            started.clone(),
            user("u1", "compare them"),
            assistant("a1", "They differ in"),
        ]);
        assert!(has_meaningful_history(&running));
        let transcript = render(&running, 60_000);
        assert!(transcript.contains("They differ in"));
        assert!(transcript.contains("still running when the conversation was handed over"));

        let interrupted = Timeline::fold_events([
            started.clone(),
            user("u1", "compare them"),
            assistant("a1", "They differ in"),
            AgentEvent::TurnCompleted {
                turn_id: "t1".into(),
                status: TurnStatus::Interrupted,
                usage: None,
            },
        ]);
        assert!(has_meaningful_history(&interrupted));
        assert!(render(&interrupted, 60_000).contains("interrupted before it completed"));

        let opening = Timeline::fold_events([started, user("u1", "compare them")]);
        assert!(!has_meaningful_history(&opening));
        assert_eq!(render(&opening, 60_000), "");
    }
}
