//! In-process MCP server for dispatching work to child tcode threads.

use std::time::Duration;

mod tools;

pub use tools::{INSTRUCTIONS, REPORT_INSTRUCTIONS, tool_descriptions};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadPurpose {
    Execution,
    Collaboration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrchestrateOp {
    Dispatch {
        purpose: ThreadPurpose,
        parent_id: String,
        provider: String,
        model: Option<String>,
        effort: Option<String>,
        profile: Option<String>,
        permission: Option<String>,
        title: String,
        brief: String,
        cwd: Option<String>,
        worktree: Option<bool>,
        result_max_chars: Option<u32>,
        /// Per-dispatch override of the profile's fast-mode setting.
        fast: Option<bool>,
    },
    Status {
        parent_id: String,
        thread_id: Option<String>,
    },
    Send {
        parent_id: String,
        thread_id: String,
        message: String,
        /// Switch the child's fast mode before delivering the message.
        fast: Option<bool>,
    },
    Result {
        parent_id: String,
        thread_id: String,
    },
    Cancel {
        parent_id: String,
        thread_id: String,
    },
    /// The lead accepts a finished child's result: settling it is the delivery.
    Settle {
        parent_id: String,
        thread_id: String,
    },
    SettleAll {
        parent_id: String,
    },
    Approve {
        parent_id: String,
        thread_id: String,
        request_id: Option<String>,
        option: Option<String>,
        cancel: bool,
    },
    /// The lead answers a child's pending question, keyed by question id.
    Answer {
        parent_id: String,
        thread_id: String,
        request_id: Option<String>,
        answers: serde_json::Map<String, serde_json::Value>,
    },
    /// A child thread pushing its final RESULT text up to the orchestrator.
    ReportResult {
        child_id: String,
        text: String,
    },
}

#[derive(Debug)]
pub struct BrokerRequest {
    pub op: OrchestrateOp,
    pub reply: async_channel::Sender<Result<serde_json::Value, String>>,
}

pub type Broker = mcp_host::Broker<BrokerRequest>;
pub type TokenRegistry = mcp_host::TokenRegistry<tools::Service>;
pub type ChildTokenRegistry = mcp_host::TokenRegistry<tools::ChildService>;

pub struct OrchestrateMcpServer {
    pub url: String,
    pub tokens: TokenRegistry,
    /// Child-thread endpoint exposing only `report_result`, scoped per child.
    pub child_url: String,
    pub child_tokens: ChildTokenRegistry,
    pub requests: async_channel::Receiver<BrokerRequest>,
}

pub fn start(host: &mut mcp_host::Host) -> OrchestrateMcpServer {
    let url = host.url("/orchestrate");
    let child_url = host.url("/orchestrate/report");
    let (req_tx, req_rx) = async_channel::unbounded();
    let broker = Broker::new(
        req_tx,
        Duration::from_secs(30),
        mcp_host::BrokerErrors {
            unavailable: "tcode orchestrator is not available",
            dropped: "tcode orchestrator dropped the request",
            timed_out: "orchestrator operation timed out",
        },
    );
    let child_broker = broker.clone();
    let tokens = TokenRegistry::new(move |parent_id| tools::service(broker.clone(), parent_id));
    host.mount(mcp_host::route("/orchestrate", &tokens));
    let child_tokens = ChildTokenRegistry::new(move |child_id| {
        tools::child_service(child_broker.clone(), child_id)
    });
    host.mount(mcp_host::route("/orchestrate/report", &child_tokens));

    log::info!("orchestrate-mcp: serving at {url}");
    OrchestrateMcpServer {
        url,
        tokens,
        child_url,
        child_tokens,
        requests: req_rx,
    }
}
