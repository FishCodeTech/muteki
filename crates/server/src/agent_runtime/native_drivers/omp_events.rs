//! OMP control and delegated-agent frames, kept separate from the main turn.
use super::*;

pub(super) fn commands(value: &Value) -> AgentEvent {
    AgentEvent::ProviderCommands {
        commands: value["commands"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|row| {
                Some(agent::ProviderCommand {
                    name: row["name"].as_str()?.to_owned(),
                    description: row["description"].as_str().map(str::to_owned),
                    kind: if row["kind"] == "skill" || row["source"] == "skill" {
                        agent::ProviderCommandKind::Skill
                    } else {
                        agent::ProviderCommandKind::Command
                    },
                })
            })
            .collect(),
    }
}

#[derive(Default)]
pub(super) struct Delegation {
    agents: HashMap<String, ThreadItem>,
    tools: HashMap<(String, String), ThreadItem>,
    manual_compaction_settled: bool,
}
impl Delegation {
    pub fn project(&mut self, frame: &Value) -> Option<Vec<AgentEvent>> {
        let kind = frame["type"].as_str().unwrap_or("");
        let payload = &frame["payload"];
        let mut events = Vec::new();
        match kind {
            "response" if frame["command"] == "compact" && frame["success"] == true => {
                if std::mem::take(&mut self.manual_compaction_settled) {
                    return Some(events);
                }
                let result = &frame["data"];
                events.push(AgentEvent::ContextCompacted(Compaction {
                    in_progress: false,
                    trigger: Some("manual".into()),
                    pre_tokens: result["tokensBefore"].as_u64(),
                    post_tokens: result["estimatedTokensAfter"].as_u64(),
                    ..Default::default()
                }));
                events.push(AgentEvent::TokenUsage(TokenUsage {
                    used_tokens: result["estimatedTokensAfter"].as_u64(),
                    freshness: agent::ContextFreshness::AwaitingObservation,
                    ..Default::default()
                }));
            }
            "response" if frame["command"] == "compact" && frame["success"] == false => {
                if std::mem::take(&mut self.manual_compaction_settled) {
                    return Some(events);
                }
                events.push(AgentEvent::ContextCompacted(Compaction {
                    status: Some("failed".into()),
                    trigger: Some("manual".into()),
                    message: Some(
                        frame["error"]
                            .as_str()
                            .unwrap_or("OMP compaction failed")
                            .to_owned(),
                    ),
                    ..Default::default()
                }));
            }

            "subagent_lifecycle" | "subagent_progress" => {
                let progress = if kind == "subagent_progress" {
                    &payload["progress"]
                } else {
                    payload
                };
                let id = progress["id"].as_str()?;
                let item = self
                    .agents
                    .entry(id.to_owned())
                    .or_insert_with(|| ThreadItem {
                        id: id.to_owned(),
                        parent_item_id: None,
                        content: ItemContent::Subagent {
                            agent_type: payload["agent"].as_str().unwrap_or("omp").to_owned(),
                            description: progress["description"].as_str().unwrap_or(id).to_owned(),
                            status: ItemStatus::InProgress,
                            summary: None,
                            model: None,
                            effort: None,
                        },
                    });
                if let ItemContent::Subagent {
                    status,
                    description,
                    summary,
                    model,
                    ..
                } = &mut item.content
                {
                    let next = match progress["status"].as_str().unwrap_or("") {
                        "completed" => ItemStatus::Completed,
                        "failed" => ItemStatus::Failed,
                        "aborted" | "cancelled" => ItemStatus::Interrupted,
                        _ => ItemStatus::InProgress,
                    };
                    if kind == "subagent_lifecycle"
                        || *status == ItemStatus::InProgress
                        || next != ItemStatus::InProgress
                    {
                        *status = next;
                    }
                    if let Some(value) = progress["description"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                    {
                        *description = value.to_owned();
                    }
                    if let Some(value) = progress["resolvedModel"]
                        .as_str()
                        .or_else(|| progress["modelOverride"].as_str())
                        .or_else(|| progress["modelOverride"][0].as_str())
                    {
                        *model = Some(value.to_owned());
                    }
                    if let Some(value) = progress["retryFailure"]["errorMessage"]
                        .as_str()
                        .or_else(|| progress["lastIntent"].as_str())
                        .or_else(|| progress["currentTool"].as_str())
                    {
                        *summary = Some(value.to_owned());
                    }
                }
                events.push(AgentEvent::ItemUpdated(item.clone()));
            }
            "subagent_event" => {
                let id = payload["id"].as_str()?;
                let event = &payload["event"];
                match event["type"].as_str().unwrap_or("") {
                    "tool_execution_start" | "tool_execution_update" | "tool_execution_end" => {
                        let call = event["toolCallId"].as_str().unwrap_or("");
                        let key = (id.to_owned(), call.to_owned());
                        let ending = event["type"] == "tool_execution_end";
                        let item = self.tools.entry(key.clone()).or_insert_with(|| ThreadItem {
                            id: format!("{id}/{call}"),
                            parent_item_id: Some(id.to_owned()),
                            content: ItemContent::ToolCall {
                                presentation: None,
                                image_reads: Vec::new(),
                                name: event["toolName"].as_str().unwrap_or("").to_owned(),
                                input: event["args"].clone(),
                                output: None,
                                status: ItemStatus::InProgress,
                            },
                        });
                        if let ItemContent::ToolCall { output, status, .. } = &mut item.content {
                            if let Some(value) =
                                event.get("result").or_else(|| event.get("partialResult"))
                            {
                                *output = Some(value.to_string());
                            }
                            *status = if !ending {
                                ItemStatus::InProgress
                            } else if event["isError"] == true {
                                ItemStatus::Failed
                            } else {
                                ItemStatus::Completed
                            };
                        }
                        events.push(if ending {
                            AgentEvent::ItemCompleted(item.clone())
                        } else if event["type"] == "tool_execution_start" {
                            AgentEvent::ItemStarted(item.clone())
                        } else {
                            AgentEvent::ItemUpdated(item.clone())
                        });
                        if ending {
                            self.tools.remove(&key);
                        }
                    }
                    "message_end" if event["message"]["role"] == "assistant" => {
                        let message = &event["message"];
                        let summary = message["errorMessage"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| {
                                message["content"]
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .filter(|b| b["type"] == "text")
                                    .filter_map(|b| b["text"].as_str())
                                    .collect()
                            });
                        if let Some(item) = self.agents.get_mut(id) {
                            if let ItemContent::Subagent { summary: prior, .. } = &mut item.content
                            {
                                *prior = Some(summary);
                            }
                            events.push(AgentEvent::ItemUpdated(item.clone()));
                        }
                    }
                    _ => {}
                }
            }
            "available_commands_update" => events.push(commands(frame)),
            "compaction_start"
            | "auto_compaction_start"
            | "compaction_end"
            | "auto_compaction_end" => {
                let starting = kind.ends_with("_start");
                let manual = !kind.starts_with("auto_") && frame["reason"] != "auto";
                let trigger = if frame["reason"] == "manual" {
                    "manual"
                } else {
                    "auto"
                };
                if starting && manual {
                    self.manual_compaction_settled = false;
                }
                let successful = !starting
                    && frame["result"].is_object()
                    && frame["aborted"] != true
                    && frame["skipped"] != true
                    && frame["errorMessage"].as_str().is_none_or(str::is_empty);
                let message = if !starting && !successful {
                    frame["errorMessage"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .or_else(|| (frame["skipped"] == true).then_some("skipped"))
                        .or_else(|| (frame["aborted"] == true).then_some("aborted"))
                        .map(str::to_owned)
                } else {
                    None
                };
                events.push(AgentEvent::ContextCompacted(Compaction {
                    in_progress: starting,
                    status: (!starting && !successful).then(|| "failed".to_owned()),
                    trigger: Some(trigger.into()),
                    pre_tokens: frame["result"]["tokensBefore"].as_u64(),
                    post_tokens: frame["result"]["estimatedTokensAfter"].as_u64(),
                    summary: frame["result"]["shortSummary"].as_str().map(str::to_owned),
                    message,
                    ..Default::default()
                }));
                if successful {
                    events.push(AgentEvent::TokenUsage(TokenUsage {
                        used_tokens: frame["result"]["estimatedTokensAfter"].as_u64(),
                        freshness: agent::ContextFreshness::AwaitingObservation,
                        ..Default::default()
                    }));
                }
                if !starting && manual {
                    self.manual_compaction_settled = true;
                }
            }
            _ => return None,
        }
        Some(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_compaction_failure_and_abort_are_not_completed() {
        let mut projection = Delegation::default();
        projection.project(&json!({"type":"compaction_start","reason":"manual"}));
        let ended = projection
            .project(&json!({"type":"compaction_end","aborted":true,"errorMessage":"stopped"}))
            .unwrap();
        assert!(matches!(&ended[..], [AgentEvent::ContextCompacted(c)]
            if c.status.as_deref() == Some("failed") && c.message.as_deref() == Some("stopped")));
        assert!(projection.project(&json!({"type":"response","command":"compact","success":false,"error":"stopped"})).unwrap().is_empty());

        let rejected = projection.project(&json!({"type":"response","command":"compact","success":false,"error":"Nothing to compact"})).unwrap();
        assert!(matches!(&rejected[..], [AgentEvent::ContextCompacted(c)]
            if c.status.as_deref() == Some("failed") && c.message.as_deref() == Some("Nothing to compact")));
    }
}
