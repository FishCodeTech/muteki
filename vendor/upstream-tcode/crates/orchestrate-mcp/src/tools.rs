use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ProtocolVersion, ServerCapabilities, ServerConfig,
};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;

use crate::{Broker, OrchestrateOp, ThreadPurpose};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct DispatchParams {
    #[schemars(
        description = "Provider name from the current Orchestrate configuration, for example codex or claude."
    )]
    provider: String,
    #[serde(default)]
    #[schemars(
        description = "Model ID from the current Orchestrate configuration, for example gpt-6.1-sol. This is separate from the provider endpoint profile ID."
    )]
    model: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Reasoning effort for this call. Choose any available effort listed for this model in the current Orchestrate configuration (for example low, medium, high, xhigh, max, ultra, or ultracode when supported). Use the model description and task difficulty; the model is not pinned to a preset effort. Omit to use medium when available, otherwise the provider default. Unsupported values are rejected."
    )]
    effort: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Provider endpoint profile ID. Set only when the chosen configuration entry explicitly lists a profile ID; otherwise omit for the built-in endpoint. Do not put the model name here; use model instead."
    )]
    profile: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Native permission value from the target profile's permission list in the current Orchestrate configuration. Omit to use Settings → Orchestrate child approval policy (default: recommended). Unsupported values are rejected; providers without a permission control accept no value."
    )]
    permission: Option<String>,
    #[schemars(description = "Short title for the new child thread.")]
    title: String,
    #[schemars(
        description = "Self-contained execution assignment with context, scope, constraints, and expected result."
    )]
    brief: String,
    #[serde(default)]
    #[schemars(
        description = "Working directory for the child, absolute or relative to the parent directory. Omit to inherit the parent directory."
    )]
    cwd: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Override Settings → Orchestrate child-worktree isolation for this dispatch. When true and cwd resolves to a Git repository root, the child runs on branch tcode/<thread-id> in a dedicated worktree. The response includes its path and branch. Non-Git cwd or creation failure falls back to cwd and reports a warning."
    )]
    worktree: Option<bool>,
    #[serde(default)]
    #[schemars(
        description = "Character cap for the inline result text in the completion callback (default 1200; 0 = unlimited). Raise it or pass 0 when you will need the full report anyway — cheaper than a follow-up result call."
    )]
    result_max_chars: Option<u32>,
    #[serde(default)]
    #[schemars(
        description = "Override the child profile's fast-mode setting for this dispatch (true = on, false = off). Pass it only when the user explicitly asked for fast mode on or off; otherwise omit it and the profile decides. Ignored by providers without a fast mode."
    )]
    fast: Option<bool>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum CollaborationEffort {
    Medium,
    High,
}
impl CollaborationEffort {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CollaborateParams {
    #[schemars(
        description = "Provider name from the current Orchestrate configuration, for example codex or claude."
    )]
    provider: String,
    #[serde(default)]
    #[schemars(
        description = "Model ID from the current Orchestrate configuration, for example gpt-6.1-sol. This is separate from the provider endpoint profile ID."
    )]
    model: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Peer reasoning effort: medium for focused consultation, high for difficult synthesis or tradeoffs. Defaults to medium when available, otherwise high. Other efforts are not allowed for collaboration."
    )]
    effort: Option<CollaborationEffort>,
    #[serde(default)]
    #[schemars(
        description = "Provider endpoint profile ID. Set only when the chosen configuration entry explicitly lists a profile ID; otherwise omit for the built-in endpoint. Do not put the model name here; use model instead."
    )]
    profile: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Native permission value from the target profile's permission list in the current Orchestrate configuration. Omit to use Settings → Orchestrate child approval policy (default: recommended). Unsupported values are rejected; providers without a permission control accept no value."
    )]
    permission: Option<String>,
    #[schemars(description = "Short title for the new child thread.")]
    title: String,
    #[schemars(
        description = "Self-contained discussion: context, open question, current alternatives, constraints, and the independent perspective requested. Concrete implementation belongs to execution models."
    )]
    brief: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct StatusParams {
    #[serde(default)]
    thread_id: Option<String>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SendParams {
    thread_id: String,
    message: String,
    #[serde(default)]
    #[schemars(
        description = "Switch the child's fast mode (true = on, false = off) before delivering this message. Takes effect from the child's next turn: a turn already running keeps its speed, so to speed up work in progress cancel the child first, then send with fast set — it resumes its transcript on a fresh process. Pass it only when the user explicitly asks; omit it to leave the setting alone."
    )]
    fast: Option<bool>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct DispatchBatch {
    #[schemars(
        description = "One entry per child thread to open. All entries start concurrently; the response lists one object per entry in the same order, each carrying the entry's title and either its thread_id or an error."
    )]
    children: Vec<DispatchParams>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SendBatch {
    #[schemars(
        description = "One entry per message, each to one child thread. The response lists one object per entry in the same order, each carrying the entry's thread_id and either its delivery or an error."
    )]
    messages: Vec<SendParams>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ThreadParams {
    thread_id: String,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ThreadsParams {
    #[schemars(
        description = "Child thread ids. The response lists one object per id in the same order, each carrying the thread_id and either the outcome or an error."
    )]
    thread_ids: Vec<String>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ApproveParams {
    #[schemars(description = "Child thread id from the pending approval callback.")]
    thread_id: String,
    #[serde(default)]
    #[schemars(
        description = "Pending approval request id from the callback. May be omitted only when the child has exactly one pending request."
    )]
    request_id: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Exact native option id from the pending approval callback. Supply option or cancel: true, never both."
    )]
    option: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Cancel the pending request using the provider's native cancellation behavior. Supply cancel: true or option, never both."
    )]
    cancel: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AnswerParams {
    #[schemars(description = "Child thread id from the pending question callback.")]
    thread_id: String,
    #[serde(default)]
    #[schemars(
        description = "Pending question request id from the callback. May be omitted; a child has at most one pending question."
    )]
    request_id: Option<String>,
    #[schemars(
        description = "One entry per question, keyed by the question id from the callback: a string (an option label or free text) for a single-choice question, an array of strings for a multi_select one."
    )]
    answers: serde_json::Map<String, serde_json::Value>,
}

#[derive(Clone)]
pub struct OrchestrateTools {
    broker: Broker,
    parent_id: String,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl OrchestrateTools {
    fn new(broker: Broker, parent_id: String) -> Self {
        Self {
            broker,
            parent_id,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Dispatch concrete execution work to enabled execution-model profiles, one new child Tcode thread per entry in children. Use collaborate for peer decision discussions. Entries start concurrently, so put every child that can advance at once in one call; the response lists one object per entry in order with its title and thread_id, or an error for that entry alone (the call fails only when every entry failed). profile is the provider-profile id from the fleet table, required when the entry names one. permission selects an exact native value listed for the target profile; omit it to use the child approval setting. Each entry's response records the resolved permission value. worktree optionally isolates the child in tcode/<thread-id> and overrides the Orchestrate setting; the response identifies the path and branch or explains fallback. When you accept a child's result, settle it: an unsettled finished child is not delivered and keeps your thread waiting. fast overrides the profile's fast-mode setting for this child; use it only on the user's explicit instruction."
    )]
    async fn dispatch(&self, Parameters(p): Parameters<DispatchBatch>) -> CallToolResult {
        let ops = p
            .children
            .into_iter()
            .map(|p| {
                let op = OrchestrateOp::Dispatch {
                    purpose: ThreadPurpose::Execution,
                    parent_id: self.parent_id.clone(),
                    provider: p.provider,
                    model: p.model,
                    effort: p.effort,
                    profile: p.profile,
                    permission: p.permission,
                    title: p.title.clone(),
                    brief: p.brief,
                    cwd: p.cwd,
                    worktree: p.worktree,
                    result_max_chars: p.result_max_chars,
                    fast: p.fast,
                };
                (("title", p.title), op)
            })
            .collect();
        run_ops(&self.broker, ops).await
    }

    #[tool(
        description = "Open a peer discussion with an enabled collaboration model from Settings → Orchestrate (bundled: Astra and Fable 5.1). Use for independent approaches, architecture, assumptions, and review of decisions. This is a discussion, not an implementation assignment; dispatch concrete work to execution models. Prefer a complementary provider when it adds a useful perspective. Returns thread_id; use send for further discussion. permission selects an exact native value listed for the target profile; omit it to use the child approval setting. The response records the resolved permission value. The peer's report arrives through the normal completion callback."
    )]
    async fn collaborate(&self, Parameters(p): Parameters<CollaborateParams>) -> CallToolResult {
        run_op(
            &self.broker,
            OrchestrateOp::Dispatch {
                purpose: ThreadPurpose::Collaboration,
                parent_id: self.parent_id.clone(),
                provider: p.provider,
                model: p.model,
                effort: p.effort.map(|effort| effort.as_str().to_string()),
                profile: p.profile,
                permission: p.permission,
                title: p.title,
                brief: p.brief,
                cwd: None,
                worktree: Some(false),
                result_max_chars: Some(0),
                fast: None,
            },
        )
        .await
    }

    #[tool(
        description = "List child thread status, optionally for one thread. state is running, reported, unreported, failed, or idle. report is what the child owes for your latest message: none, expected (your message is out and no report_result has come back), reported, unreported (it went quiet twice without reporting; its final message was delivered as a fallback), or failed (its provider could not deliver). delivery is running, awaiting_settle (finished, its result not yet accepted with settle), settled, or not_delivered (cancelled or archived)."
    )]
    async fn status(&self, Parameters(p): Parameters<StatusParams>) -> CallToolResult {
        run_op(
            &self.broker,
            OrchestrateOp::Status {
                parent_id: self.parent_id.clone(),
                thread_id: p.thread_id,
            },
        )
        .await
    }

    #[tool(
        description = "Send follow-up messages to this session's child threads, one entry per message in messages. If a child has a turn in flight the message is steered into it immediately; otherwise it is queued and sent as the child's next turn. Each message opens a new expectation: you receive one completion callback for it, carrying the child's report_result text; a report not yet delivered for the previous message is superseded. Progress inside the child never reaches you, so poll status or send when you need to know. The response lists one object per entry in order with its thread_id and which happened (delivery: steered | queued), or an error for that entry alone. A settled, cancelled or archived child reopens and must be settled again once you accept its new result."
    )]
    async fn send(&self, Parameters(p): Parameters<SendBatch>) -> CallToolResult {
        let ops = p
            .messages
            .into_iter()
            .map(|p| {
                let op = OrchestrateOp::Send {
                    parent_id: self.parent_id.clone(),
                    thread_id: p.thread_id.clone(),
                    message: p.message,
                    fast: p.fast,
                };
                (("thread_id", p.thread_id), op)
            })
            .collect();
        run_ops(&self.broker, ops).await
    }

    #[tool(
        description = "Read a child's latest report_result text. Falls back to its final assistant message, marked source: fallback, when it never reported. Works while the child is still running."
    )]
    async fn result(&self, Parameters(p): Parameters<ThreadParams>) -> CallToolResult {
        run_op(
            &self.broker,
            OrchestrateOp::Result {
                parent_id: self.parent_id.clone(),
                thread_id: p.thread_id,
            },
        )
        .await
    }

    #[tool(
        description = "Cancel and shut down this session's child threads named in thread_ids. Their results are kept but not delivered, and they no longer keep your thread waiting. The response lists one object per id in order, or an error for that id alone. To accept a result, use settle instead."
    )]
    async fn cancel(&self, Parameters(p): Parameters<ThreadsParams>) -> CallToolResult {
        let ops = p
            .thread_ids
            .into_iter()
            .map(|thread_id| {
                let op = OrchestrateOp::Cancel {
                    parent_id: self.parent_id.clone(),
                    thread_id: thread_id.clone(),
                };
                (("thread_id", thread_id), op)
            })
            .collect();
        run_ops(&self.broker, ops).await
    }

    #[tool(
        description = "Settle this session's finished child threads named in thread_ids once you have accepted their results. Settling is the delivery: until then a finished child is not delivered and keeps your thread waiting. Each child's provider stops; send reopens it. The response lists one object per id in order; an id is refused with an error of its own while that child still runs or waits for an answer."
    )]
    async fn settle(&self, Parameters(p): Parameters<ThreadsParams>) -> CallToolResult {
        let ops = p
            .thread_ids
            .into_iter()
            .map(|thread_id| {
                let op = OrchestrateOp::Settle {
                    parent_id: self.parent_id.clone(),
                    thread_id: thread_id.clone(),
                };
                (("thread_id", thread_id), op)
            })
            .collect();
        run_ops(&self.broker, ops).await
    }

    #[tool(
        description = "Settle every finished child of this lead thread that is awaiting_settle, accepting all of their results at once. Settling is the delivery and stops each child's provider; send reopens it. No thread ids are needed. Returns settled entries with thread_id and delivery, plus skipped children with their current delivery state. Running or waiting children are skipped, as are already settled or cancelled children."
    )]
    async fn settle_all(&self) -> CallToolResult {
        run_op(
            &self.broker,
            OrchestrateOp::SettleAll {
                parent_id: self.parent_id.clone(),
            },
        )
        .await
    }

    #[tool(
        description = "Answer a child thread's pending permission approval. Supply option with an exact native option id from the approval callback, or cancel: true to cancel; never both. request_id may be omitted when the child has exactly one pending request."
    )]
    async fn approve(&self, Parameters(p): Parameters<ApproveParams>) -> CallToolResult {
        run_op(
            &self.broker,
            OrchestrateOp::Approve {
                parent_id: self.parent_id.clone(),
                thread_id: p.thread_id,
                request_id: p.request_id,
                option: p.option,
                cancel: p.cancel,
            },
        )
        .await
    }

    #[tool(
        description = "Answer a child thread's pending question (its request_user_input, blocking or async) from the question callback. answers has one entry per question keyed by question id: a string for a single-choice question (an option label or free text), an array of strings for a multi_select one. Decide from the brief and the user's instructions."
    )]
    async fn answer(&self, Parameters(p): Parameters<AnswerParams>) -> CallToolResult {
        run_op(
            &self.broker,
            OrchestrateOp::Answer {
                parent_id: self.parent_id.clone(),
                thread_id: p.thread_id,
                request_id: p.request_id,
                answers: p.answers,
            },
        )
        .await
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReportResultParams {
    #[schemars(
        description = "The complete final report, verbatim. Do not summarize or truncate; this exact text is what the orchestrator receives."
    )]
    text: String,
}

/// The single-tool server registered with child threads: a channel for pushing
/// the full RESULT text to the orchestrator instead of relying on whatever the
/// child's last message happens to be.
#[derive(Clone)]
pub struct ChildReportTools {
    broker: Broker,
    child_id: String,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl ChildReportTools {
    fn new(broker: Broker, child_id: String) -> Self {
        Self {
            broker,
            child_id,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Send your complete final report (RESULT) to the thread that initiated this work or discussion. The orchestrator sees nothing of your turns, wake-ups, Monitor or background work — only this call reaches it, delivered when your current turn ends — so make the report self-contained: your reasoning, recommendations, disagreements, and open questions for a discussion; files changed and commands actually run with outcomes for execution; evidence for your conclusions in either case. Call it once when the work is complete, before ending your turn; calling again replaces the previous report (last call wins). Only if this call fails, write the same complete report as your final message instead — it is sent back as the fallback, truncated when long."
    )]
    async fn report_result(&self, Parameters(p): Parameters<ReportResultParams>) -> CallToolResult {
        run_op(
            &self.broker,
            OrchestrateOp::ReportResult {
                child_id: self.child_id.clone(),
                text: p.text,
            },
        )
        .await
    }
}

/// Run one op per entry concurrently and answer with one object per entry in
/// the same order, tagged with the entry's `(key, label)`. An entry that fails
/// carries its own `error`; the call as a whole fails only when every entry
/// did, so one bad entry does not hide the children that did start.
async fn run_ops(
    broker: &Broker,
    ops: Vec<((&'static str, String), OrchestrateOp)>,
) -> CallToolResult {
    if ops.is_empty() {
        return CallToolResult::error(vec![ContentBlock::text("no entries")]);
    }
    let items = futures::future::join_all(ops.into_iter().map(|((key, label), op)| async move {
        let mut item = broker
            .invoke(|reply| crate::BrokerRequest { op, reply })
            .await
            .unwrap_or_else(|error| serde_json::json!({ "error": error }));
        if let serde_json::Value::Object(map) = &mut item {
            map.insert(key.to_string(), serde_json::Value::String(label));
        }
        item
    }))
    .await;
    let all_failed = items.iter().all(|item| item.get("error").is_some());
    let text = ContentBlock::text(serde_json::Value::Array(items).to_string());
    if all_failed {
        CallToolResult::error(vec![text])
    } else {
        CallToolResult::success(vec![text])
    }
}

async fn run_op(broker: &Broker, op: OrchestrateOp) -> CallToolResult {
    match broker
        .invoke(|reply| crate::BrokerRequest { op, reply })
        .await
    {
        Ok(value) => CallToolResult::success(vec![ContentBlock::text(value.to_string())]),
        Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
    }
}

pub fn tool_descriptions() -> Vec<(String, String)> {
    OrchestrateTools::tool_router()
        .list_all()
        .into_iter()
        .map(|tool| {
            (
                tool.name.to_string(),
                tool.description.as_deref().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

/// The orchestrating thread's server `instructions`, read at initialization.
pub const INSTRUCTIONS: &str = "Prefer Tcode Orchestrate for cross-provider peer collaboration and execution dispatch. Use collaborate for decision discussions, dispatch for implementation, send to continue either thread, and settle each child whose result you have accepted, or use settle_all to accept every finished child at once. Children report through report_result; you receive one completion callback per message you sent a child, carrying that report (or a labelled fallback or failure), and nothing of its progress in between. dispatch, send, cancel and settle take a list, so act on every child that is ready in one call.";

/// A dispatched child's server `instructions`, read at initialization.
pub const REPORT_INSTRUCTIONS: &str = "Report this thread's final result to the orchestrator that dispatched it with report_result; it sees nothing else of this thread.";

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ChildReportTools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_server_info(Implementation::from_build_env())
            .with_instructions(REPORT_INSTRUCTIONS)
    }
}

pub type ChildService = StreamableHttpService<ChildReportTools, LocalSessionManager>;

pub fn child_service(broker: Broker, child_id: String) -> ChildService {
    StreamableHttpService::new(
        move || Ok(ChildReportTools::new(broker.clone(), child_id.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    )
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for OrchestrateTools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_server_info(Implementation::from_build_env())
            .with_instructions(INSTRUCTIONS)
    }
}

pub type Service = StreamableHttpService<OrchestrateTools, LocalSessionManager>;

pub fn service(broker: Broker, parent_id: String) -> Service {
    StreamableHttpService::new(
        move || Ok(OrchestrateTools::new(broker.clone(), parent_id.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn broker(
        requests: async_channel::Sender<crate::BrokerRequest>,
        timeout: std::time::Duration,
    ) -> Broker {
        Broker::new(
            requests,
            timeout,
            mcp_host::BrokerErrors {
                unavailable: "tcode orchestrator is not available",
                dropped: "tcode orchestrator dropped the request",
                timed_out: "orchestrator operation timed out",
            },
        )
    }

    #[tokio::test]
    async fn collaboration_tool_routes_peer_purpose_and_native_permission() {
        let schema = serde_json::to_value(schemars::schema_for!(CollaborationEffort)).unwrap();
        assert_eq!(schema["enum"], serde_json::json!(["medium", "high"]));
        for effort in ["medium", "high"] {
            let params: CollaborateParams = serde_json::from_value(serde_json::json!({"provider":"codex", "effort":effort, "title":"Review", "brief":"Compare alternatives"})).unwrap();
            assert_eq!(params.effort.unwrap().as_str(), effort);
        }
        for effort in ["low", "xhigh", "max", "ultra"] {
            assert!(serde_json::from_value::<CollaborateParams>(serde_json::json!({"provider":"codex", "effort":effort, "title":"Review", "brief":"Compare alternatives"})).is_err());
        }
        let (tx, rx) = async_channel::unbounded();
        let broker = broker(tx, std::time::Duration::from_secs(30));
        let resolver = tokio::spawn(async move {
            let request = rx.recv().await.unwrap();
            assert!(matches!(request.op, OrchestrateOp::Dispatch {
                purpose: ThreadPurpose::Collaboration,
                parent_id, provider, permission: Some(permission), worktree: Some(false), result_max_chars: Some(0), ..
            } if parent_id == "parent" && provider == "codex" && permission == "ask"));
            request
                .reply
                .send(Ok(serde_json::json!({"thread_id": "peer"})))
                .await
                .unwrap();
        });
        let result = OrchestrateTools::new(broker, "parent".into())
            .collaborate(Parameters(CollaborateParams {
                provider: "codex".into(),
                model: None,
                effort: None,
                profile: None,
                permission: Some("ask".into()),
                title: "Design discussion".into(),
                brief: "Compare the alternatives".into(),
            }))
            .await;
        assert_eq!(result.is_error, Some(false));
        resolver.await.unwrap();
    }

    #[tokio::test]
    async fn native_permission_and_approval_parameters_reach_runtime() {
        let (tx, rx) = async_channel::unbounded();
        let tools = OrchestrateTools::new(
            broker(tx, std::time::Duration::from_secs(30)),
            "parent".into(),
        );
        let resolver = tokio::spawn(async move {
            let request = rx.recv().await.unwrap();
            assert!(matches!(request.op, OrchestrateOp::Dispatch {
                parent_id, permission: Some(permission), purpose: ThreadPurpose::Execution, ..
            } if parent_id == "parent" && permission == "auto_review"));
            request
                .reply
                .send(Ok(
                    serde_json::json!({"thread_id":"child", "permission":"auto_review"}),
                ))
                .await
                .unwrap();
            for expected in [Some("Allow:Session"), None] {
                let request = rx.recv().await.unwrap();
                assert!(matches!(request.op, OrchestrateOp::Approve {
                    parent_id, thread_id, option, cancel, request_id: None,
                } if parent_id == "parent" && thread_id == "child" && option.as_deref() == expected && cancel == expected.is_none()));
                request
                    .reply
                    .send(Ok(serde_json::json!({"ok":true})))
                    .await
                    .unwrap();
            }
        });
        let result = tools.dispatch(Parameters(serde_json::from_value(serde_json::json!({
            "children": [{"provider":"codex", "permission":"auto_review", "title":"Inspect", "brief":"Inspect the code"}]
        })).unwrap())).await;
        assert_eq!(result.is_error, Some(false));
        assert_eq!(
            text(&result),
            serde_json::json!([{"thread_id":"child", "permission":"auto_review", "title":"Inspect"}])
        );
        for params in [
            serde_json::json!({"thread_id":"child", "option":"Allow:Session"}),
            serde_json::json!({"thread_id":"child", "cancel":true}),
        ] {
            let result = tools
                .approve(Parameters(serde_json::from_value(params).unwrap()))
                .await;
            assert_eq!(result.is_error, Some(false));
        }
        resolver.await.unwrap();
    }

    #[tokio::test]
    async fn question_answers_reach_runtime_keyed_by_question_id() {
        let (tx, rx) = async_channel::unbounded();
        let tools = OrchestrateTools::new(
            broker(tx, std::time::Duration::from_secs(30)),
            "parent".into(),
        );
        let resolver = tokio::spawn(async move {
            let request = rx.recv().await.unwrap();
            assert!(matches!(request.op, OrchestrateOp::Answer {
                parent_id, thread_id, request_id: Some(request_id), answers,
            } if parent_id == "parent" && thread_id == "child" && request_id == "q-1"
                && answers == serde_json::json!({"lang": "Rust", "targets": ["ios", "android"]}).as_object().cloned().unwrap()));
            request
                .reply
                .send(Ok(serde_json::json!({"ok":true, "request_id":"q-1"})))
                .await
                .unwrap();
        });
        let result = tools
            .answer(Parameters(
                serde_json::from_value(serde_json::json!({
                    "thread_id":"child", "request_id":"q-1",
                    "answers": {"lang": "Rust", "targets": ["ios", "android"]}
                }))
                .unwrap(),
            ))
            .await;
        assert_eq!(result.is_error, Some(false));
        resolver.await.unwrap();
    }

    fn text(result: &CallToolResult) -> serde_json::Value {
        let [block] = result.content.as_slice() else {
            panic!("one content block");
        };
        serde_json::from_str(&block.as_text().unwrap().text).unwrap()
    }

    /// One bad entry answers for itself: the others still run, the response
    /// keeps the request order, and the call fails only when every entry did.
    #[tokio::test]
    async fn batch_entries_run_together_and_fail_alone() {
        let (tx, rx) = async_channel::unbounded();
        let tools = OrchestrateTools::new(
            broker(tx, std::time::Duration::from_secs(30)),
            "parent".into(),
        );
        let resolver = tokio::spawn(async move {
            // Both requests are already queued before either is answered, and
            // the second is answered first.
            let first = rx.recv().await.unwrap();
            let second = rx.recv().await.unwrap();
            assert!(
                matches!(&second.op, OrchestrateOp::Send { thread_id, message, fast: None, .. }
                if thread_id == "gone" && message == "retry")
            );
            second
                .reply
                .send(Err("unknown child: gone".into()))
                .await
                .unwrap();
            assert!(
                matches!(&first.op, OrchestrateOp::Send { thread_id, message, fast: Some(true), .. }
                if thread_id == "alive" && message == "go on")
            );
            first
                .reply
                .send(Ok(serde_json::json!({"ok":true, "delivery":"steered"})))
                .await
                .unwrap();
            let request = rx.recv().await.unwrap();
            assert!(
                matches!(&request.op, OrchestrateOp::Cancel { thread_id, .. } if thread_id == "gone")
            );
            request
                .reply
                .send(Err("unknown child: gone".into()))
                .await
                .unwrap();
        });
        let result = tools
            .send(Parameters(
                serde_json::from_value(serde_json::json!({"messages": [
                    {"thread_id":"alive", "message":"go on", "fast":true},
                    {"thread_id":"gone", "message":"retry"},
                ]}))
                .unwrap(),
            ))
            .await;
        assert_eq!(result.is_error, Some(false));
        assert_eq!(
            text(&result),
            serde_json::json!([
                {"ok":true, "delivery":"steered", "thread_id":"alive"},
                {"error":"unknown child: gone", "thread_id":"gone"},
            ])
        );
        let result = tools
            .cancel(Parameters(ThreadsParams {
                thread_ids: vec!["gone".into()],
            }))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            text(&result),
            serde_json::json!([{"error":"unknown child: gone", "thread_id":"gone"}])
        );
        let empty = tools
            .settle(Parameters(ThreadsParams { thread_ids: vec![] }))
            .await;
        assert_eq!(empty.is_error, Some(true));
        resolver.await.unwrap();
    }

    #[tokio::test]
    async fn settle_all_is_parameter_free_scoped_and_preserves_outcomes() {
        let (tx, rx) = async_channel::unbounded();
        let tools = OrchestrateTools::new(
            broker(tx, std::time::Duration::from_secs(30)),
            "lead".into(),
        );
        let schema = tools
            .tool_router
            .list_all()
            .into_iter()
            .find(|tool| tool.name == "settle_all")
            .unwrap();
        assert!(
            schema
                .input_schema
                .get("required")
                .is_none_or(|value| value.as_array().unwrap().is_empty())
        );
        assert!(
            schema
                .input_schema
                .get("properties")
                .is_none_or(|value| value.as_object().unwrap().is_empty())
        );
        let outcomes = serde_json::json!({
            "settled": [{"thread_id":"finished", "delivery":"settled"}],
            "skipped": [{"thread_id":"working", "delivery":"running"}],
        });
        let reply = outcomes.clone();
        let resolver = tokio::spawn(async move {
            let request = rx.recv().await.unwrap();
            assert_eq!(
                request.op,
                OrchestrateOp::SettleAll {
                    parent_id: "lead".into()
                }
            );
            request.reply.send(Ok(reply)).await.unwrap();
        });
        let result = tools.settle_all().await;
        assert_eq!(result.is_error, Some(false));
        assert_eq!(text(&result), outcomes);
        resolver.await.unwrap();
    }

    #[tokio::test]
    async fn report_result_carries_child_scope() {
        let (tx, rx) = async_channel::unbounded();
        let broker = broker(tx, std::time::Duration::from_secs(30));
        let resolver = tokio::spawn(async move {
            let request = rx.recv().await.unwrap();
            assert!(
                matches!(request.op, OrchestrateOp::ReportResult { child_id, text }
                    if child_id == "child" && text == "full report")
            );
            request
                .reply
                .send(Ok(serde_json::json!({ "ok": true })))
                .await
                .unwrap();
        });
        let tools = ChildReportTools::new(broker, "child".into());
        let names: Vec<_> = tools
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert_eq!(names, ["report_result"]);
        let result = tools
            .report_result(Parameters(ReportResultParams {
                text: "full report".into(),
            }))
            .await;
        assert_eq!(result.is_error, Some(false));
        resolver.await.unwrap();
    }
}
