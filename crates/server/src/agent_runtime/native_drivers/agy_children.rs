//! AGY exposes delegated conversation identities in structured render blocks.
//! Each identity is observed through its own native state subscription.
use super::*;

type Update = (String, Result<Value, RuntimeError>);

struct Child {
    native: Native,
    parent_turn: String,
    parent: Option<String>,
    call: String,
    title: String,
    model: Option<String>,
    request: Option<String>,
    trajectory: String,
    invocation: Option<(u64, usize)>,
    steps: BTreeMap<usize, Value>,
    items: HashMap<String, (ItemContent, bool)>,
    approvals: HashMap<String, usize>,
    requested: HashSet<usize>,
    inputs: HashMap<String, (usize, Vec<Value>)>,
    running: bool,
    cancel_requested: bool,
    cancellation_seen: bool,
    cancellation_unconfirmed: bool,
    reader: Option<tokio::task::JoinHandle<()>>,
}

impl Child {
    fn invocation_cancelled(&self) -> bool {
        let start = self.invocation.map_or(0, |(_, start)| start);
        self.steps
            .range(start..)
            .any(|(_, step)| step["status"] == "CORTEX_STEP_STATUS_CANCELED")
    }
    fn invocation_failure(&self) -> Option<String> {
        let start = self.invocation.map_or(0, |(_, start)| start);
        let (_, step) = self.steps.range(start..).next_back()?;
        let status = step["status"].as_str()?;
        matches!(
            status,
            "CORTEX_STEP_STATUS_ERROR" | "CORTEX_STEP_STATUS_HALTED"
        )
        .then(|| {
            step["error"]["fullError"]
                .as_str()
                .unwrap_or(status)
                .to_owned()
        })
    }
}

pub(super) struct Children {
    nodes: HashMap<String, Child>,
    tx: tokio::sync::mpsc::Sender<Update>,
    pub(super) rx: tokio::sync::mpsc::Receiver<Update>,
    output: NativeOutput,
}

impl Drop for Children {
    fn drop(&mut self) {
        for child in self.nodes.values() {
            if let Some(reader) = &child.reader {
                reader.abort();
            }
        }
    }
}

impl Children {
    pub(super) fn new(output: NativeOutput) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        Self {
            nodes: HashMap::new(),
            tx,
            rx,
            output,
        }
    }

    pub(super) async fn discover(
        &mut self,
        native: &Native,
        steps: &BTreeMap<usize, Value>,
        turn: &str,
        parent: Option<&str>,
    ) -> Result<(), RuntimeError> {
        for step in steps.values() {
            let call = &step["metadata"]["toolCall"];
            if call["name"] != "swarm" {
                continue;
            }
            let input = arguments(call)?;
            if input["action"] != "add" {
                continue;
            }
            for block in step["generic"]["result"]["stepRenderInfo"]["contentBlocks"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let Some(id) = block["agent"]["conversationId"].as_str() else {
                    continue;
                };
                if self.nodes.contains_key(id) {
                    continue;
                }
                let call = call["id"]
                    .as_str()
                    .ok_or_else(|| failure("Native swarm spawn has no call identity"))?
                    .to_owned();
                let call = parent.map_or_else(|| call.clone(), |parent| format!("{parent}:{call}"));
                let child_native = Native {
                    session: id.into(),
                    ..native.clone()
                };
                let mut child = Child {
                    native: child_native,
                    parent_turn: turn.into(),
                    parent: parent.map(str::to_owned),
                    call,
                    title: block["agent"]["title"].as_str().unwrap_or(id).into(),
                    model: input["model"].as_str().map(str::to_owned),
                    request: input["brief"].as_str().map(str::to_owned),
                    trajectory: String::new(),
                    invocation: None,
                    steps: BTreeMap::new(),
                    items: HashMap::new(),
                    approvals: HashMap::new(),
                    requested: HashSet::new(),
                    inputs: HashMap::new(),
                    running: false,
                    cancel_requested: false,
                    cancellation_seen: false,
                    cancellation_unconfirmed: false,
                    reader: None,
                };
                Self::patch(&self.output, &child, "unknown", None).await;
                match child.native.subscribe().await {
                    Ok(response) => {
                        let output = self.tx.clone();
                        let id = id.to_owned();
                        child.reader = Some(tokio::spawn(async move {
                            let (tx, mut rx) = tokio::sync::mpsc::channel(32);
                            tokio::join!(read_stream(response, tx), async move {
                                while let Some(update) = rx.recv().await {
                                    if output.send((id.clone(), update)).await.is_err() {
                                        break;
                                    }
                                }
                            });
                        }));
                    }
                    Err(error) => {
                        Self::patch(&self.output, &child, "unknown", Some(&error.message)).await
                    }
                }
                self.nodes.insert(id.into(), child);
            }
        }
        Ok(())
    }

    async fn emit(output: &NativeOutput, child: &Child, event: AgentEvent) {
        output
            .send(AgentEvent::Origin {
                turn_id: child.parent_turn.clone(),
                agent_id: Some(child.native.session.clone()),
                event: Box::new(event),
            })
            .await;
    }

    async fn patch(output: &NativeOutput, child: &Child, status: &str, error: Option<&str>) {
        let patch = json!({"agent_id":child.native.session,"parent_id":child.parent,"call_id":child.call,"session_ref":child.native.session,
            "title":child.title,"model":child.model,"request":child.request,"status":status,"error":error,
            "cancel_supported":status == "running" && !child.cancel_requested,"cancel_requested":child.cancel_requested,
            "cancel_reason":if status == "waiting" {Some("The native conversation is idle; cancellation stops an active invocation, not swarm membership")}else if status == "unknown"{Some("Native execution state is unconfirmed")}else{None}});
        output
            .send(AgentEvent::Origin {
                turn_id: child.parent_turn.clone(),
                agent_id: None,
                event: Box::new(AgentEvent::NativeAgentUpdate { patch }),
            })
            .await;
    }

    pub(super) async fn update(&mut self, update: Update) {
        let id = update.0.clone();
        if let Err(error) = self.apply(update).await
            && let Some(child) = self.nodes.get(&id)
        {
            Self::patch(&self.output, child, "unknown", Some(&error.message)).await;
        }
    }

    async fn apply(&mut self, (id, update): Update) -> Result<(), RuntimeError> {
        let Some(child) = self.nodes.get_mut(&id) else {
            return Ok(());
        };
        let message = match update {
            Ok(message) => message,
            Err(error) => {
                Self::patch(&self.output, child, "unknown", Some(&error.message)).await;
                return Ok(());
            }
        };
        let update = &message["update"];
        if update["conversationId"] != id {
            return Err(failure(
                "Native child update belongs to a different conversation",
            ));
        }
        let trajectory = update["trajectoryId"]
            .as_str()
            .ok_or_else(|| failure("Missing native child trajectory identity"))?;
        if child.trajectory != trajectory {
            child.trajectory = trajectory.into();
            child.invocation = None;
            child.steps.clear();
            child.items.clear();
            child.approvals.clear();
            child.requested.clear();
            let withdrawn: Vec<String> = child.inputs.keys().cloned().collect();
            child.inputs.clear();
            for request_id in withdrawn {
                Self::emit(
                    &self.output,
                    child,
                    AgentEvent::UserInputResolved {
                        request_id,
                        answers: serde_json::Map::new(),
                    },
                )
                .await;
            }
        }
        let idle = update["fullyIdle"].as_bool().unwrap_or(false);
        if !idle && !child.running {
            child.running = true;
            if !child.cancel_requested {
                child.cancellation_seen = false;
                child.cancellation_unconfirmed = false;
            }
            Self::emit(
                &self.output,
                child,
                AgentEvent::TurnStarted {
                    turn_id: child.trajectory.clone(),
                },
            )
            .await;
        }
        for turn in update["turnsUpdate"]["turns"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let index = turn["index"].as_u64().unwrap_or(0);
            let start = turn["startStepIndex"].as_u64().unwrap_or(0) as usize;
            if child
                .invocation
                .is_none_or(|(previous, _)| index >= previous)
            {
                child.invocation = Some((index, start));
            }
        }
        let patch = &update["mainTrajectoryUpdate"]["stepsUpdate"];
        if let (Some(indices), Some(steps)) =
            (patch["indices"].as_array(), patch["steps"].as_array())
        {
            if indices.len() != steps.len() {
                return Err(failure("Native child step indices and payloads differ"));
            }
            for (index, step) in indices.iter().zip(steps) {
                let index = index
                    .as_u64()
                    .ok_or_else(|| failure("Invalid native child step index"))?
                    as usize;
                if child.cancel_requested
                    && index >= child.invocation.map_or(0, |(_, start)| start)
                    && step["status"] == "CORTEX_STEP_STATUS_CANCELED"
                    && child
                        .steps
                        .get(&index)
                        .is_none_or(|previous| previous["status"] != "CORTEX_STEP_STATUS_CANCELED")
                {
                    child.cancellation_seen = true;
                }
                child.steps.insert(index, step.clone());
            }
        }
        for metadata in
            update["mainTrajectoryUpdate"]["generatorMetadatasUpdate"]["generatorMetadatas"]
                .as_array()
                .into_iter()
                .flatten()
        {
            if let Some(model) = metadata["chatModel"]["responseModel"].as_str() {
                child.model = Some(model.into());
            }
        }
        let withdrawn: Vec<String> = child
            .inputs
            .iter()
            .filter(|(_, (index, _))| !child.steps.get(index).is_some_and(question_pending))
            .map(|(id, _)| id.clone())
            .collect();
        for request_id in withdrawn {
            child.inputs.remove(&request_id);
            Self::emit(
                &self.output,
                child,
                AgentEvent::UserInputResolved {
                    request_id,
                    answers: serde_json::Map::new(),
                },
            )
            .await;
        }
        for (&index, step) in &child.steps {
            for (item_id, content, done) in items(index, step)? {
                let item_id = format!("{id}:{item_id}");
                let mut content = content;
                if let Some(error) = step["error"]["fullError"].as_str() {
                    match &mut content {
                        ItemContent::CommandExecution { output, .. } if output.is_empty() => {
                            *output = error.into()
                        }
                        ItemContent::ToolCall { output, .. }
                            if output.as_ref().is_none_or(String::is_empty) =>
                        {
                            *output = Some(error.into())
                        }
                        _ => {}
                    }
                }
                if child
                    .items
                    .get(&item_id)
                    .is_some_and(|previous| previous == &(content.clone(), done))
                {
                    continue;
                }
                let existed = child
                    .items
                    .insert(item_id.clone(), (content.clone(), done))
                    .is_some();
                let item = ThreadItem {
                    id: item_id,
                    parent_item_id: Some(id.clone()),
                    content,
                };
                let event = if done {
                    AgentEvent::ItemCompleted(item)
                } else if existed {
                    AgentEvent::ItemUpdated(item)
                } else {
                    AgentEvent::ItemStarted(item)
                };
                Self::emit(&self.output, child, event).await;
            }
            let settled_requests: Vec<String> = child
                .approvals
                .iter()
                .filter(|(_, pending)| {
                    **pending == index && !step["requestedInteraction"]["permission"].is_object()
                })
                .map(|(id, _)| id.clone())
                .collect();
            for request_id in settled_requests {
                child.approvals.remove(&request_id);
                Self::emit(
                    &self.output,
                    child,
                    AgentEvent::ApprovalResolved {
                        request_id,
                        decision: ApprovalDecision::Cancel,
                    },
                )
                .await;
            }
            if step["requestedInteraction"]["permission"].is_object() {
                let request = permission_request(
                    step,
                    &child.trajectory,
                    index,
                    &child.parent_turn,
                    Some(id.clone()),
                )?;
                if child.requested.insert(index) {
                    child.approvals.insert(request.id.clone(), index);
                    Self::emit(&self.output, child, AgentEvent::ApprovalRequested(request)).await;
                }
            }
        }
        for (&index, step) in &child.steps {
            if question_pending(step) && child.requested.insert(index) {
                let raw = step["requestedInteraction"]["askQuestion"]["questions"]
                    .as_array()
                    .ok_or_else(|| failure("Missing native child questions"))?
                    .clone();
                let questions = raw
                    .iter()
                    .enumerate()
                    .map(|(i, q)| UserInputQuestion {
                        id: i.to_string(),
                        header: String::new(),
                        question: q["question"].as_str().unwrap_or("").into(),
                        options: q["options"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|o| agent::UserInputOption {
                                label: o["text"].as_str().unwrap_or("").into(),
                                description: String::new(),
                            })
                            .collect(),
                        multi_select: q["isMultiSelect"].as_bool().unwrap_or(false),
                        prefill: None,
                        contract: None,
                    })
                    .collect();
                let request_id = format!("agy-question-{}-{index}", child.trajectory);
                child.inputs.insert(request_id.clone(), (index, raw));
                Self::emit(
                    &self.output,
                    child,
                    AgentEvent::UserInputRequested {
                        request_id,
                        questions,
                        delivery: UserInputDelivery::Blocking,
                    },
                )
                .await;
            }
        }
        if let Some((&index, _)) = child.steps.iter().find(|(_, step)| {
            step["requestedInteraction"]
                .as_object()
                .is_some_and(|value| !value.is_empty())
                && !step["requestedInteraction"]["permission"].is_object()
                && !step["requestedInteraction"]["askQuestion"].is_object()
        }) {
            return Err(failure(format!(
                "Unsupported native child interaction at step {index}"
            )));
        }
        let failure = child.invocation_failure();
        if idle && child.running && child.inputs.is_empty() {
            child.running = false;
            if let Some(message) = &failure {
                Self::emit(
                    &self.output,
                    child,
                    AgentEvent::TurnFailed {
                        code: "agy.native_child_failed".into(),
                        message: message.clone(),
                    },
                )
                .await;
            }
            Self::emit(
                &self.output,
                child,
                AgentEvent::TurnCompleted {
                    turn_id: child.trajectory.clone(),
                    status: if failure.is_some() {
                        TurnStatus::Failed
                    } else if child.invocation_cancelled() {
                        TurnStatus::Interrupted
                    } else {
                        TurnStatus::Completed
                    },
                    usage: None,
                },
            )
            .await;
        }
        if idle && child.cancel_requested && !child.cancellation_seen {
            child.cancellation_unconfirmed = true;
        }
        let unconfirmed_cancel = idle && child.cancellation_unconfirmed;
        if idle {
            child.cancel_requested = false;
        }
        let status = if idle && failure.is_some() {
            "failed"
        } else if unconfirmed_cancel {
            "unknown"
        } else if idle && child.invocation_cancelled() {
            "cancelled"
        } else if idle {
            "waiting"
        } else {
            "running"
        };
        Self::patch(&self.output, child, status, if status == "unknown" && unconfirmed_cancel {
            Some("The provider became idle without reporting cancellation; termination was not confirmed")
        } else if idle { failure.as_deref() } else { None }).await;
        let native = child.native.clone();
        let steps = child.steps.clone();
        let turn = child.parent_turn.clone();
        self.discover(&native, &steps, &turn, Some(&id)).await
    }

    pub(super) async fn answer(
        &mut self,
        request: &str,
        answers: &serde_json::Map<String, Value>,
    ) -> Result<bool, RuntimeError> {
        let Some(child) = self
            .nodes
            .values_mut()
            .find(|child| child.inputs.contains_key(request))
        else {
            return Ok(false);
        };
        let (index, questions) = &child.inputs[request];
        let responses: Vec<Value> = questions.iter().enumerate().map(|(i, question)| {
            let answer = answers.get(&i.to_string()).unwrap_or(&Value::Null);
            let texts: Vec<&str> = answer.as_array().into_iter().flatten().filter_map(Value::as_str).chain(answer.as_str()).collect();
            let options: Vec<&Value> = question["options"].as_array().into_iter().flatten().collect();
            let selected: Vec<Value> = options.iter().filter(|v| texts.iter().any(|t| v["text"] == *t || v["id"] == *t)).map(|v| v["id"].clone()).collect();
            let written: Vec<&str> = texts.into_iter().filter(|t| !options.iter().any(|v| v["text"] == *t || v["id"] == *t)).collect();
            json!({"question":question["question"],"selectedOptionIds":selected,"writeInResponse":written.join("\n"),"skipped":answer.is_null()})
        }).collect();
        if let Err(error) = child.native.interact(&child.trajectory, *index, json!({"askQuestion":{"responses":if answers.is_empty(){Vec::new()}else{responses},"cancelled":answers.is_empty()}})).await {
            Self::emit(&self.output, child, AgentEvent::ControlDelivery {request_id:request.into(),operation:"user_input".into(),status:"failed".into(),error:Some(json!({"code":error.code,"message":error.message}))}).await;
        } else {
            child.inputs.remove(request);
            Self::emit(&self.output, child, AgentEvent::UserInputResolved {request_id:request.into(),answers:answers.clone()}).await;
        }
        Ok(true)
    }

    pub(super) async fn cancel(&mut self, id: Option<&str>) {
        if let Some(id) = id
            && let Some(child) = self.nodes.get_mut(id)
            && !child.running
        {
            child.cancel_requested = false;
            self.output.send(AgentEvent::Origin {
                turn_id: child.parent_turn.clone(), agent_id: None,
                event: Box::new(AgentEvent::NativeAgentUpdate { patch: json!({
                    "agent_id":id,"cancel_requested":false,"cancel_supported":false,
                    "cancel_reason":"No active native invocation was observed when cancellation was delivered"
                }) }),
            }).await;
            return;
        }
        for child in self
            .nodes
            .values_mut()
            .filter(|child| id.is_none_or(|id| child.native.session == id) && child.running)
        {
            child.cancel_requested = true;
            child.cancellation_seen = false;
            child.cancellation_unconfirmed = false;
            Self::patch(&self.output, child, "running", None).await;
            if let Err(error) = child
                .native
                .call(
                    "CancelCascadeInvocation",
                    json!({"cascadeId":child.native.session,"killBackgroundTasks":true}),
                )
                .await
            {
                child.cancel_requested = false;
                Self::patch(&self.output, child, "unknown", Some(&error.message)).await;
            }
        }
    }

    pub(super) async fn disconnected(&self) {
        for child in self.nodes.values().filter(|child| {
            child.running
                || child.cancel_requested
                || (!child.invocation_cancelled() && child.invocation_failure().is_none())
        }) {
            Self::patch(
                &self.output,
                child,
                "unknown",
                Some("The provider disconnected before native execution termination was confirmed"),
            )
            .await;
        }
    }

    pub(super) async fn approval(
        &mut self,
        request: &str,
        decision: &ApprovalDecision,
    ) -> Result<bool, RuntimeError> {
        let Some(child) = self
            .nodes
            .values_mut()
            .find(|child| child.approvals.contains_key(request))
        else {
            return Ok(false);
        };
        let index = child.approvals[request];
        let allow = matches!(decision, ApprovalDecision::Option(id) if id == "allow");
        if let Err(error) = child
            .native
            .interact(
                &child.trajectory,
                index,
                json!({"permission":{"allow":allow,"persistGrants":{}}}),
            )
            .await
        {
            Self::emit(
                &self.output,
                child,
                AgentEvent::ControlDelivery {
                    request_id: request.into(),
                    operation: "approval".into(),
                    status: "failed".into(),
                    error: Some(json!({"code":error.code,"message":error.message})),
                },
            )
            .await;
            return Ok(true);
        }
        child.approvals.remove(request);
        Self::emit(
            &self.output,
            child,
            AgentEvent::ApprovalResolved {
                request_id: request.into(),
                decision: decision.clone(),
            },
        )
        .await;
        Ok(true)
    }
}

pub(super) fn permission_request(
    step: &Value,
    trajectory: &str,
    index: usize,
    turn: &str,
    parent: Option<String>,
) -> Result<ApprovalRequest, RuntimeError> {
    let permission = &step["requestedInteraction"]["permission"];
    let call = &step["metadata"]["toolCall"];
    let input = arguments(call)?;
    let name = call["name"].as_str().unwrap_or("native_tool");
    let target = permission["resource"]["target"].as_str().unwrap_or("");
    let kind = match permission["resource"]["action"].as_str() {
        Some("command") => ApprovalKind::ExecCommand {
            command: target.into(),
            cwd: None,
            reason: None,
        },
        Some("write_file") => ApprovalKind::FileChange {
            changes: Vec::new(),
            reason: Some(target.into()),
        },
        Some("read_file") => ApprovalKind::FileRead {
            detail: target.into(),
        },
        _ => ApprovalKind::ToolUse {
            name: name.into(),
            input: input.clone(),
            detail: permission["actionDescription"]
                .as_str()
                .unwrap_or(name)
                .into(),
        },
    };
    Ok(ApprovalRequest {
        id: format!("agy-native-{trajectory}-{index}"),
        turn_id: Some(turn.into()),
        parent_item_id: parent,
        tool_call_id: call["id"].as_str().map(str::to_owned),
        native_execution: Some(
            json!({"tool_name":name,"action":name,"arguments":input,"resource_kind":match permission["resource"]["action"].as_str(){Some("command")=>"process",Some("read_file"|"write_file")=>"file",_=>"mcp"},"native_interaction":step["requestedInteraction"]}),
        ),
        kind,
        options: vec![
            ApprovalOption {
                id: "allow".into(),
                label: "Allow".into(),
                kind: ApprovalOptionKind::AllowOnce,
            },
            ApprovalOption {
                id: "deny".into(),
                label: "Deny".into(),
                kind: ApprovalOptionKind::RejectOnce,
            },
        ],
    })
}
