use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ProtocolVersion, ServerCapabilities, ServerConfig,
};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::json;

use crate::backend::{
    ActionOutcome, ActionRequest, ActionResult, CapturePolicy, Delivery, ObserveRequest,
    RootFilters, RootInfo, RootObservation,
};
use crate::outline::{self, UiNode};

pub use crate::backend::{ActionKind as UiActionKind, MouseButton, RootKind};

/// Observation source to use for a desktop root.
#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ObserveMode {
    Semantic,
    Visual,
    Fused,
}

/// Input action to apply to a state-scoped UI element or screen coordinate.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct UiAction {
    /// Action to perform: press, click, set_text, type_text, keypress, scroll, drag, or move_mouse.
    pub action: UiActionKind,
    /// State-scoped element reference to target, when the action targets an element.
    #[serde(default, rename = "ref")]
    pub r#ref: Option<String>,
    /// Absolute screen x-coordinate, when targeting a coordinate.
    #[serde(default)]
    pub x: Option<f64>,
    /// Absolute screen y-coordinate, when targeting a coordinate.
    #[serde(default)]
    pub y: Option<f64>,
    /// Text to set or type for text-entry actions.
    #[serde(default)]
    pub text: Option<String>,
    /// Key names or chord components for a keypress action.
    #[serde(default)]
    pub keys: Option<Vec<String>>,
    /// Horizontal scroll delta for a scroll action.
    #[serde(default)]
    pub scroll_x: Option<f64>,
    /// Vertical scroll delta for a scroll action.
    #[serde(default)]
    pub scroll_y: Option<f64>,
    /// Absolute screen points describing a drag path.
    #[serde(default)]
    pub path: Option<Vec<[f64; 2]>>,
    /// Mouse button to use for click or drag actions.
    #[serde(default)]
    pub button: Option<MouseButton>,
    /// Number of clicks to issue for a click action.
    #[serde(default)]
    pub click_count: Option<u32>,
}

/// Condition evaluated against a UI state.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct UiCondition {
    /// State-scoped element reference the condition should match.
    #[serde(default, rename = "ref")]
    pub r#ref: Option<String>,
    /// State-scoped ancestor reference that limits the search scope.
    #[serde(default)]
    pub scope_ref: Option<String>,
    /// Text the matching element should contain.
    #[serde(default)]
    pub text: Option<String>,
    /// Accessibility role the matching element should have.
    #[serde(default)]
    pub role: Option<String>,
    /// Accessibility value the matching element should have.
    #[serde(default)]
    pub value: Option<String>,
    /// Whether the matching element must become present or absent.
    #[serde(default)]
    pub until: Option<ConditionUntil>,
    /// Maximum time to wait for the condition, in milliseconds.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// Desired presence state for a UI condition.
#[derive(Debug, Clone, Copy, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ConditionUntil {
    Present,
    Absent,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ObserveUiParams {
    /// Desktop root reference to observe; omitted selects the frontmost root.
    #[serde(default)]
    root: Option<String>,
    /// Observation source: semantic, visual, or fused.
    #[serde(default)]
    mode: Option<ObserveMode>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SearchUiParams {
    /// Identifier of the cached UI state to search.
    state_id: String,
    /// Text to rank against element names, values, and descriptions.
    #[serde(default)]
    text: Option<String>,
    /// Accessibility role to match.
    #[serde(default)]
    role: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ExpandUiParams {
    /// Identifier of the cached UI state containing the element.
    state_id: String,
    /// State-scoped element reference to expand.
    #[serde(rename = "ref")]
    r#ref: String,
    /// Maximum descendant depth to include.
    #[serde(default)]
    depth: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct InspectUiParams {
    /// Identifier of the cached UI state containing the element.
    state_id: String,
    /// State-scoped element reference to inspect.
    #[serde(rename = "ref")]
    r#ref: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ActUiParams {
    /// Identifier of the cached UI state against which actions are resolved.
    state_id: String,
    /// Ordered actions to execute as one transaction.
    actions: Vec<UiAction>,
    /// Optional postcondition used to verify the transaction outcome.
    #[serde(default)]
    expect: Option<UiCondition>,
    /// Deliver each action at once instead of after the on-screen agent cursor, which shows the user where the agent acts, reaches its point. Set only when the input's timing matters more than showing it.
    #[serde(default)]
    immediate: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReadTextParams {
    /// Identifier of the cached UI state containing the text; omitted uses the owning state encoded by the continuation.
    #[serde(default)]
    state_id: Option<String>,
    /// State-scoped element or continuation reference whose text should be read.
    #[serde(rename = "ref")]
    r#ref: String,
    /// Byte offset at which to continue reading.
    #[serde(default)]
    offset: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WaitForParams {
    /// Identifier of the cached UI state that scopes the condition.
    state_id: String,
    /// Condition fields to wait for.
    #[serde(flatten)]
    condition: UiCondition,
}

#[derive(Clone)]
pub struct ComputerUseTools {
    tool_router: ToolRouter<Self>,
    feedback: Arc<crate::feedback::FeedbackSession>,
}

#[tool_router]
impl ComputerUseTools {
    fn new(feedback: Arc<crate::feedback::FeedbackSession>) -> Self {
        Self {
            tool_router: Self::tool_router(),
            feedback,
        }
    }

    #[tool(
        description = "Find and rank desktop window roots, returning @rN references; call this before observe_ui when the target is not the frontmost window. \
                       This server exposes desktop windows only, so use the tcode_preview tools for web pages. \
                       On macOS, grant Accessibility for all tools and Screen Recording for screenshots in Tcode Settings → Computer Use; Windows needs no permissions, and other platforms are unsupported."
    )]
    async fn find_roots(&self, Parameters(params): Parameters<RootFilters>) -> CallToolResult {
        let permissions = permissions();
        if let Some(result) = permission_gate(permissions, true, false) {
            return result;
        }
        let discovered = match crate::backend::list_roots(&params) {
            Ok(roots) => roots,
            Err(error) => return backend_error(error),
        };
        let roots = roots().lock().unwrap().refresh(discovered);
        let mut lines = vec![format!("roots: {} (platform-native order)", roots.len())];
        for root in roots {
            lines.push(format!(
                "{} {} app=\"{}\" bundle_id=\"{}\" pid={} title=\"{}\" window_id={} frame=({:.0},{:.0},{:.0},{:.0})",
                root.ref_id,
                root.kind,
                escaped(&root.app_name),
                escaped(&root.bundle_id),
                root.pid,
                escaped(&root.title),
                root.window_id,
                root.frame.x,
                root.frame.y,
                root.frame.w,
                root.frame.h
            ));
        }
        bounded_success(None, lines.join("\n"), Vec::new())
    }

    #[tool(
        description = "Observe a desktop root and return a folded outline, state_id, @e element refs, and a screenshot when requested by the observation mode. \
                       Use find_roots → observe_ui, query that cached state with search_ui/expand_ui/inspect_ui, then use act_ui/wait_for and observe again; those cached queries do not touch the live UI. \
                       Every @e ref belongs to its producing state_id; observations are immutable and kept in a bounded LRU (default 8), so an evicted or stale state or a ref from another state requires a fresh observe_ui. \
                       This server exposes desktop windows only, so use the tcode_preview tools for web pages. \
                       On macOS, grant Accessibility for all tools and Screen Recording for screenshots in Tcode Settings → Computer Use; Windows needs no permissions, and other platforms are unsupported."
    )]
    async fn observe_ui(&self, Parameters(params): Parameters<ObserveUiParams>) -> CallToolResult {
        let permissions = permissions();
        let config = crate::config::get();
        let needs_accessibility = !matches!(params.mode, Some(ObserveMode::Visual));
        let needs_screen_recording = config.image_mode != crate::config::ImageMode::Never
            && matches!(params.mode, Some(ObserveMode::Visual | ObserveMode::Fused));
        if let Some(result) =
            permission_gate(permissions, needs_accessibility, needs_screen_recording)
        {
            return result;
        }
        if config.image_mode == crate::config::ImageMode::Always
            && let Some(result) = permission_gate(permissions, false, true)
        {
            return result;
        }
        let root = match resolve_root(params.root.as_deref()) {
            Ok(root) => root,
            Err(result) => return *result,
        };
        let lane = observation_lane(root.pid);
        let _transaction = lane.lock().await;
        let capture = capture_policy(config.image_mode, params.mode, &permissions);
        let request = ObserveRequest {
            semantic: !matches!(params.mode, Some(ObserveMode::Visual)),
            capture,
        };
        let observed = match crate::backend::observe(&root, request) {
            Ok(observed) => observed,
            Err(error) => return backend_error(error),
        };
        save_observation(observed, capture_warning(config.image_mode, &permissions))
    }

    #[tool(
        description = "Search and rank elements by text and accessibility role in an immutable cached state, without touching the live UI. \
                       The ref and state_id must come from the same observe_ui result; an evicted state needs a fresh observe_ui. \
                       Oversized results return a preview and continuation ref for read_text."
    )]
    async fn search_ui(&self, Parameters(params): Parameters<SearchUiParams>) -> CallToolResult {
        let permissions = permissions();
        if let Some(result) = permission_gate(permissions, true, false) {
            return result;
        }
        let observation = match crate::state::global().lock().unwrap().get(&params.state_id) {
            Ok(observation) => observation,
            Err(error) => return tool_error(&error.to_string()),
        };
        let results = outline::search(
            &observation.tree,
            params.text.as_deref(),
            params.role.as_deref(),
        );
        let mut lines = vec![format!(
            "state_id: {}\nmatches: {} (showing {})",
            observation.state_id,
            results.total,
            results.matches.len()
        )];
        for result in results.matches {
            lines.push(format!(
                "score={} {}",
                result.score,
                outline::render_line(result.node, 0)
            ));
        }
        bounded_success(Some(&observation.state_id), lines.join("\n"), Vec::new())
    }

    #[tool(
        description = "Expand local outline context around an @e element in an immutable cached state, without touching the live UI. \
                       The ref and state_id must come from the same observe_ui result; an evicted state needs a fresh observe_ui. \
                       Oversized results return a preview and continuation ref for read_text."
    )]
    async fn expand_ui(&self, Parameters(params): Parameters<ExpandUiParams>) -> CallToolResult {
        let permissions = permissions();
        if let Some(result) = permission_gate(permissions, true, false) {
            return result;
        }
        let observation = match crate::state::global().lock().unwrap().get(&params.state_id) {
            Ok(observation) => observation,
            Err(error) => return tool_error(&error.to_string()),
        };
        let depth = params.depth.unwrap_or(3).min(12) as usize;
        let expanded = match outline::render_expanded(&observation.tree, &params.r#ref, depth) {
            Ok(expanded) => expanded,
            Err(error) => return tool_error(&error),
        };
        bounded_success(
            Some(&observation.state_id),
            format!("state_id: {}\n{expanded}", observation.state_id),
            Vec::new(),
        )
    }

    #[tool(
        description = "Inspect an @e element's full accessibility attributes, frame, and supported actions in an immutable cached state, without touching the live UI. \
                       The ref and state_id must come from the same observe_ui result; an evicted state needs a fresh observe_ui. \
                       Oversized results return a preview and continuation ref for read_text."
    )]
    async fn inspect_ui(&self, Parameters(params): Parameters<InspectUiParams>) -> CallToolResult {
        let permissions = permissions();
        if let Some(result) = permission_gate(permissions, true, false) {
            return result;
        }
        let observation = match crate::state::global().lock().unwrap().get(&params.state_id) {
            Ok(observation) => observation,
            Err(error) => return tool_error(&error.to_string()),
        };
        let Some(node) = observation.tree.find(&params.r#ref) else {
            return tool_error(&crate::state::StateError::UnknownElement(params.r#ref).to_string());
        };
        let value = json!({
            "state_id": observation.state_id,
            "ref": node.ref_id,
            "role": node.role,
            "title": node.title,
            "value": node.value,
            "description": node.description,
            "frame": node.frame,
            "actions": node.actions,
            "enabled": node.enabled,
            "focused": node.focused,
            "child_count": node.children.len(),
        });
        let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
        bounded_success(Some(&observation.state_id), text, Vec::new())
    }

    #[tool(
        description = "Execute a transaction of desktop input actions against an observed state, optionally using expect to verify semantic success. \
                       Every @e ref must belong to the supplied state_id; an evicted or stale state or a ref from another state errors and requires a fresh observe_ui, while a successful call returns a successor state_id that must replace the old one. \
                       Results report worked/didnt/unknown per step, stop at the first failure with stopped_at, and use expect as a postcondition rather than treating event delivery as success. \
                       Input is refused when observe-only mode is enabled (allow_input=false) in Settings → Computer Use, and oversized output returns a preview plus a continuation ref for read_text."
    )]
    async fn act_ui(
        &self,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
        Parameters(params): Parameters<ActUiParams>,
    ) -> CallToolResult {
        let mut feedback = self.feedback.begin(Some(context.ct.clone()));
        let permissions = permissions();
        if let Some(result) = permission_gate(permissions, true, false) {
            return result;
        }
        if !crate::config::get().allow_input {
            return tool_error(
                "observe-only mode is enabled in Settings → Computer Use; input actions are disabled",
            );
        }
        if params.actions.is_empty() {
            return tool_error("act_ui requires at least one action");
        }
        let pid = match crate::state::global().lock().unwrap().get(&params.state_id) {
            Ok(observation) => observation.root.pid,
            Err(error) => return tool_error(&error.to_string()),
        };
        let lane = observation_lane(pid);
        let _transaction = lane.lock().await;
        let previous = match crate::state::global()
            .lock()
            .unwrap()
            .validate_for_action(&params.state_id)
        {
            Ok(observation) => observation,
            Err(error) => return tool_error(&error.to_string()),
        };
        let mut step_results = Vec::new();
        let mut action_descriptions = Vec::new();
        let mut stopped_at = None;
        let mut activation = "none";
        for (index, action) in params.actions.iter().enumerate() {
            if !feedback.is_current() {
                return tool_error("computer-use action cancelled");
            }
            let result = match prepare_action(&previous.tree, action) {
                Ok(request) => crate::backend::perform_action_with_feedback(
                    &previous.root,
                    &request,
                    &feedback,
                    !params.immediate,
                )
                .unwrap_or_else(|error| ActionResult::didnt(error.to_string(), Delivery::None)),
                Err(error) => ActionResult::didnt(error, Delivery::None),
            };
            let didnt = result.outcome == ActionOutcome::Didnt;
            action_descriptions.push(crate::state::harness_action_description(
                action_name(action.action),
                action.r#ref.as_deref(),
                &previous.tree,
            ));
            activation = match (activation, result.delivery) {
                (_, Delivery::ForegroundHid) => "foreground",
                ("none", Delivery::BackgroundPid) => "background",
                (current, _) => current,
            };
            step_results.push(json!({
                "index": index + 1,
                "action": action_name(action.action),
                "outcome": result.outcome,
                "message": result.message,
                "delivery": result.delivery,
            }));
            if didnt {
                stopped_at = Some(index + 1);
                break;
            }
        }
        crate::state::global()
            .lock()
            .unwrap()
            .record_actions(&previous.root, action_descriptions);

        let expectation_preexisting = params
            .expect
            .as_ref()
            .is_some_and(|condition| condition_satisfied(&previous.tree, condition));
        let successor_result = tokio::select! {
            _ = context.ct.cancelled() => return tool_error("computer-use action cancelled"),
            result = poll_successor(&previous, params.expect.as_ref(), expectation_preexisting) => result,
        };
        let (successor, expectation_status, root_changed) = match successor_result {
            Ok(result) => result,
            Err(error) => return backend_error(error),
        };
        let successor = crate::state::global()
            .lock()
            .unwrap()
            .insert_observation(successor.root, successor.tree);
        let diff = outline::diff_trees(&previous.tree, &successor.tree);
        let expectation_failed = expectation_status == "failed";
        let any_unknown = step_results
            .iter()
            .any(|result| result["outcome"] == "unknown");
        let outcome = if stopped_at.is_some() || expectation_failed {
            "didnt"
        } else if params.expect.is_some() && expectation_status == "verified" {
            "worked"
        } else if any_unknown {
            "unknown"
        } else {
            "worked"
        };
        let report = json!({
            "state_id": successor.state_id,
            "previous_state_id": previous.state_id,
            "outcome": outcome,
            "activation": activation,
            "stopped_at": stopped_at,
            "steps": step_results,
            "expect": expectation_status,
            "root_changed": root_changed,
            "diff_confidence": diff.confidence,
        });
        let mut text = serde_json::to_string_pretty(&report).unwrap_or_else(|_| report.to_string());
        text.push('\n');
        if diff.use_full_view || root_changed {
            text.push_str("successor full view:\n");
            text.push_str(&outline::render_folded(&successor.tree));
        } else {
            text.push_str(&diff.text);
        }
        text.push_str("\n\n");
        text.push_str(&successor.harness_annotation);
        if feedback.is_current() {
            feedback.complete();
        }
        bounded_success(Some(&successor.state_id), text, Vec::new())
    }

    #[tool(
        description = "Read a bounded page of long text from an @e element or from an @o continuation returned with an oversized result. \
                       An @e ref requires the state_id that produced it; an @o ref retains its owning state, and a mismatched owner or evicted ref errors. \
                       Continue with the returned ref and next offset until eof."
    )]
    async fn read_text(&self, Parameters(params): Parameters<ReadTextParams>) -> CallToolResult {
        let permissions = permissions();
        if let Some(result) = permission_gate(permissions, true, false) {
            return result;
        }
        let offset = match params.offset.map(usize::try_from).transpose() {
            Ok(offset) => offset,
            Err(_) => return tool_error("offset is too large for this platform"),
        };
        let page = if params.r#ref.starts_with("@o") {
            crate::state::global().lock().unwrap().read_output(
                &params.r#ref,
                params.state_id.as_deref(),
                offset,
            )
        } else if params.r#ref.starts_with("@e") {
            let Some(state_id) = params.state_id.as_deref() else {
                return tool_error("state_id is required when reading an @e element ref");
            };
            crate::state::global().lock().unwrap().page_element_text(
                state_id,
                &params.r#ref,
                offset.unwrap_or(0),
            )
        } else {
            return tool_error("read_text ref must be an @e element or @o continuation");
        };
        let page = match page {
            Ok(page) => page,
            Err(error) => return tool_error(&error.to_string()),
        };
        let owner = page.owner_state.as_deref().unwrap_or("none");
        let continuation = if page.eof {
            "eof".to_string()
        } else {
            format!(
                "continue with ref {} offset {}",
                page.output_ref, page.next_offset
            )
        };
        bounded_success(
            page.owner_state.as_deref(),
            format!(
                "ref: {}\nstate_id: {}\noffset: {}\nnext_offset: {}\ntotal_bytes: {}\neof: {}\n{}\n---\n{}",
                page.output_ref,
                owner,
                page.offset,
                page.next_offset,
                page.total_bytes,
                page.eof,
                continuation,
                page.text
            ),
            Vec::new(),
        )
    }

    #[tool(
        description = "After observe_ui, wait against that state for a text, role, value, or referenced UI element to become present or absent. \
                       Every @e ref must belong to the supplied state_id; an evicted or stale state or a ref from another state errors and requires a fresh observe_ui. \
                       The call touches the live UI and returns a successor state_id; reuse that instead of the old state, then observe again after later UI changes. \
                       Oversized output returns a preview plus a continuation ref for read_text."
    )]
    async fn wait_for(&self, Parameters(params): Parameters<WaitForParams>) -> CallToolResult {
        let permissions = permissions();
        if let Some(result) = permission_gate(permissions, true, false) {
            return result;
        }
        let pid = match crate::state::global().lock().unwrap().get(&params.state_id) {
            Ok(observation) => observation.root.pid,
            Err(error) => return tool_error(&error.to_string()),
        };
        let lane = observation_lane(pid);
        let _transaction = lane.lock().await;
        let previous = match crate::state::global()
            .lock()
            .unwrap()
            .validate_for_action(&params.state_id)
        {
            Ok(observation) => observation,
            Err(error) => return tool_error(&error.to_string()),
        };
        let timeout = Duration::from_millis(
            params
                .condition
                .timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .min(MAX_TIMEOUT_MS),
        );
        let deadline = Instant::now() + timeout;
        let mut polls = 0_u64;
        let (observed, root_changed, matched) = loop {
            polls += 1;
            let (mut observed, root_changed) = match observe_with_root_fallback(
                &previous.root,
                ObserveRequest {
                    semantic: true,
                    capture: CapturePolicy::Never,
                },
            ) {
                Ok(observed) => observed,
                Err(error) => return backend_error(error),
            };
            outline::assign_refs_from_previous(&previous.tree, &mut observed.tree);
            if condition_satisfied(&observed.tree, &params.condition) {
                break (observed, root_changed, true);
            }
            if Instant::now() >= deadline {
                break (observed, root_changed, false);
            }
            tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
        };
        let successor = crate::state::global()
            .lock()
            .unwrap()
            .insert_observation(observed.root, observed.tree);
        let status = if matched { "matched" } else { "timeout" };
        let report = json!({
            "state_id": successor.state_id,
            "previous_state_id": previous.state_id,
            "status": status,
            "until": until_name(params.condition.until),
            "polls": polls,
            "root_changed": root_changed,
        });
        let mut text = serde_json::to_string_pretty(&report).unwrap_or_else(|_| report.to_string());
        text.push('\n');
        text.push_str(&outline::render_folded(&successor.tree));
        bounded_success(Some(&successor.state_id), text, Vec::new())
    }
}

/// Every tool's name and the description its model reads.
pub fn tool_descriptions() -> Vec<(String, String)> {
    ComputerUseTools::tool_router()
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

/// The server's `instructions`, which the client reads at initialization.
pub const INSTRUCTIONS: &str = "Use find_roots → observe_ui, query the immutable cached state with \
                 search_ui/expand_ui/inspect_ui, then use act_ui/wait_for and observe again. \
                 Every @e ref belongs to the state_id that produced it, so use each returned \
                 successor state instead of an older state. This server handles desktop windows \
                 only; use the tcode_preview tools for web pages.";

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ComputerUseTools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::LATEST)
            .with_server_info(Implementation::from_build_env())
            .with_instructions(INSTRUCTIONS)
    }
}

pub type Service = StreamableHttpService<ComputerUseTools, LocalSessionManager>;

pub(crate) fn service(feedback: Arc<crate::feedback::FeedbackSession>) -> Service {
    StreamableHttpService::new(
        move || Ok(ComputerUseTools::new(feedback.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    )
}

const DEFAULT_TIMEOUT_MS: u64 = 3_000;
const MAX_TIMEOUT_MS: u64 = 30_000;
const POLL_INTERVAL_MS: u64 = 100;

#[derive(Default)]
struct RootRegistry {
    next_ref: u64,
    by_identity: HashMap<String, String>,
    by_ref: HashMap<String, RootInfo>,
}

impl RootRegistry {
    fn refresh(&mut self, roots: Vec<RootInfo>) -> Vec<RootInfo> {
        if self.next_ref == 0 {
            self.next_ref = 1;
        }
        roots
            .into_iter()
            .map(|mut root| {
                let identity = root.identity();
                let ref_id = self
                    .by_identity
                    .entry(identity)
                    .or_insert_with(|| {
                        let ref_id = format!("@r{}", self.next_ref);
                        self.next_ref += 1;
                        ref_id
                    })
                    .clone();
                root.ref_id.clone_from(&ref_id);
                self.by_ref.insert(ref_id, root.clone());
                root
            })
            .collect()
    }

    fn get(&self, ref_id: &str) -> Option<RootInfo> {
        self.by_ref.get(ref_id).cloned()
    }
}

static ROOTS: OnceLock<Mutex<RootRegistry>> = OnceLock::new();

#[derive(Default)]
struct ObservationLanes {
    by_pid: Mutex<HashMap<u32, Arc<tokio::sync::Mutex<()>>>>,
}

impl ObservationLanes {
    fn lane(&self, pid: u32) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(
            self.by_pid
                .lock()
                .unwrap()
                .entry(pid)
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }
}

static OBSERVATION_LANES: OnceLock<ObservationLanes> = OnceLock::new();

fn roots() -> &'static Mutex<RootRegistry> {
    ROOTS.get_or_init(|| Mutex::new(RootRegistry::default()))
}

fn observation_lane(pid: u32) -> Arc<tokio::sync::Mutex<()>> {
    OBSERVATION_LANES
        .get_or_init(ObservationLanes::default)
        .lane(pid)
}

#[cfg(target_os = "macos")]
type PermissionSnapshot = crate::permissions::PermissionStatus;

#[cfg(not(target_os = "macos"))]
#[derive(Clone, Copy)]
struct PermissionSnapshot;

#[cfg(target_os = "macos")]
fn permissions() -> PermissionSnapshot {
    crate::permissions::check()
}

#[cfg(not(target_os = "macos"))]
fn permissions() -> PermissionSnapshot {
    PermissionSnapshot
}

#[cfg(target_os = "macos")]
const STALE_CODE_MESSAGE: &str = "Tcode's executable on disk changed after this Tcode process started (it was rebuilt, re-signed or replaced), so macOS rejects its Accessibility and Screen Recording grants until Tcode restarts. The grants in System Settings are intact: do not request them again; ask the user to quit and reopen Tcode, then retry.";

#[cfg(target_os = "macos")]
fn permission_gate(
    permissions: PermissionSnapshot,
    needs_accessibility: bool,
    needs_screen_recording: bool,
) -> Option<CallToolResult> {
    if (needs_accessibility || needs_screen_recording) && crate::permissions::code_changed_on_disk()
    {
        return Some(tool_error(STALE_CODE_MESSAGE));
    }
    if needs_accessibility && !permissions.accessibility {
        return Some(tool_error(
            "Accessibility permission is missing; grant it in Tcode Settings → Computer Use.",
        ));
    }
    if needs_screen_recording && !permissions.screen_recording {
        return Some(tool_error(
            "Screen Recording permission is missing; grant it in Tcode Settings → Computer Use.",
        ));
    }
    None
}

#[cfg(target_os = "windows")]
fn permission_gate(
    _permissions: PermissionSnapshot,
    _needs_accessibility: bool,
    _needs_screen_recording: bool,
) -> Option<CallToolResult> {
    None
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn permission_gate(
    _permissions: PermissionSnapshot,
    _needs_accessibility: bool,
    _needs_screen_recording: bool,
) -> Option<CallToolResult> {
    Some(backend_error(crate::backend::BackendError::unsupported()))
}

fn resolve_root(requested: Option<&str>) -> Result<RootInfo, Box<CallToolResult>> {
    if let Some(requested) = requested
        && let Some(root) = roots().lock().unwrap().get(requested)
    {
        return Ok(root);
    }
    let discovered = crate::backend::list_roots(&RootFilters::default())
        .map_err(|error| Box::new(backend_error(error)))?;
    let discovered = roots().lock().unwrap().refresh(discovered);
    match requested {
        Some(requested) => discovered
            .into_iter()
            .find(|root| root.ref_id == requested)
            .ok_or_else(|| {
                Box::new(tool_error(&format!(
                    "root ref {requested} is no longer available; call find_roots again"
                )))
            }),
        None => discovered
            .into_iter()
            .next()
            .ok_or_else(|| Box::new(tool_error("no on-screen desktop roots were found"))),
    }
}

fn capture_policy(
    configured: crate::config::ImageMode,
    requested: Option<ObserveMode>,
    permissions: &PermissionSnapshot,
) -> CapturePolicy {
    #[cfg(not(target_os = "macos"))]
    let _ = permissions;
    if configured == crate::config::ImageMode::Never {
        return CapturePolicy::Never;
    }
    if matches!(requested, Some(ObserveMode::Visual | ObserveMode::Fused))
        || configured == crate::config::ImageMode::Always
    {
        return CapturePolicy::Always;
    }
    #[cfg(target_os = "macos")]
    if !permissions.screen_recording {
        return CapturePolicy::Never;
    }
    CapturePolicy::IfSparse
}

fn capture_warning(
    configured: crate::config::ImageMode,
    permissions: &PermissionSnapshot,
) -> Option<&'static str> {
    #[cfg(target_os = "macos")]
    if configured == crate::config::ImageMode::Auto && !permissions.screen_recording {
        return Some(
            "screenshot omitted in auto mode because Screen Recording permission is missing",
        );
    }
    let _ = (configured, permissions);
    None
}

fn save_observation(observed: RootObservation, warning: Option<&str>) -> CallToolResult {
    let RootObservation {
        root,
        tree,
        text_sparse,
        screenshot,
        screenshot_mime,
    } = observed;
    let observation = crate::state::global()
        .lock()
        .unwrap()
        .insert_observation(root, tree);
    let mut text = format!(
        "state_id: {}\nroot: {} app=\"{}\" title=\"{}\"\nelements: {} interactive: {}",
        observation.state_id,
        observation.root.ref_id,
        escaped(&observation.root.app_name),
        escaped(&observation.root.title),
        observation.tree.node_count(),
        outline::interactive_count(&observation.tree)
    );
    if text_sparse {
        text.push_str("\ntext_sparse: true");
    }
    if let Some(warning) = warning {
        text.push_str("\nwarning: ");
        text.push_str(warning);
    }
    text.push('\n');
    text.push_str(&outline::render_folded(&observation.tree));
    text.push_str("\n\n");
    text.push_str(&observation.harness_annotation);
    let extra = screenshot
        .map(|screenshot| {
            ContentBlock::image(
                base64::engine::general_purpose::STANDARD.encode(screenshot),
                screenshot_mime,
            )
        })
        .into_iter()
        .collect();
    bounded_success(Some(&observation.state_id), text, extra)
}

fn prepare_action(tree: &UiNode, action: &UiAction) -> Result<ActionRequest, String> {
    let target = action.r#ref.as_deref().map(|ref_id| {
        let node = tree.find(ref_id).ok_or_else(|| {
            crate::state::StateError::UnknownElement(ref_id.to_string()).to_string()
        })?;
        let path = outline::path_to_ref(tree, ref_id).ok_or_else(|| {
            crate::state::StateError::UnknownElement(ref_id.to_string()).to_string()
        })?;
        Ok::<_, String>((node, path))
    });
    let target = target.transpose()?;
    Ok(ActionRequest {
        kind: action.action,
        target_path: target.as_ref().map(|(_, path)| path.clone()),
        target_frame: target.as_ref().map(|(node, _)| node.frame),
        target_role: target.as_ref().map(|(node, _)| node.role.clone()),
        target_title: target.as_ref().map(|(node, _)| node.title.clone()),
        target_actions: target
            .as_ref()
            .map(|(node, _)| node.actions.clone())
            .unwrap_or_default(),
        x: action.x,
        y: action.y,
        text: action.text.clone(),
        keys: action.keys.clone(),
        scroll_x: action.scroll_x,
        scroll_y: action.scroll_y,
        path: action.path.clone(),
        button: action.button.unwrap_or(MouseButton::Left),
        click_count: action.click_count.unwrap_or(1),
    })
}

async fn poll_successor(
    previous: &crate::state::Observation,
    condition: Option<&UiCondition>,
    preexisting: bool,
) -> Result<(RootObservation, &'static str, bool), crate::backend::BackendError> {
    let timeout = Duration::from_millis(
        condition
            .and_then(|condition| condition.timeout_ms)
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS),
    );
    let deadline = Instant::now() + timeout;
    loop {
        let (mut observed, root_changed) = observe_with_root_fallback(
            &previous.root,
            ObserveRequest {
                semantic: true,
                capture: CapturePolicy::Never,
            },
        )?;
        outline::assign_refs_from_previous(&previous.tree, &mut observed.tree);
        let status = match condition {
            None => Some("not_requested"),
            Some(_) if preexisting => Some("preexisting"),
            Some(condition) if condition_satisfied(&observed.tree, condition) => Some("verified"),
            Some(_) if Instant::now() >= deadline => Some("failed"),
            Some(_) => None,
        };
        if let Some(status) = status {
            return Ok((observed, status, root_changed));
        }
        tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
}

fn observe_with_root_fallback(
    root: &RootInfo,
    request: ObserveRequest,
) -> Result<(RootObservation, bool), crate::backend::BackendError> {
    match crate::backend::observe(root, request) {
        Ok(observed) => Ok((observed, false)),
        Err(original_error) => {
            let discovered = crate::backend::list_roots(&RootFilters::default())?;
            let discovered = roots().lock().unwrap().refresh(discovered);
            let Some(successor_root) = discovered.into_iter().next() else {
                return Err(original_error);
            };
            crate::backend::observe(&successor_root, request)
                .map(|observed| (observed, successor_root.identity() != root.identity()))
        }
    }
}

fn condition_satisfied(tree: &UiNode, condition: &UiCondition) -> bool {
    let scope = match condition.scope_ref.as_deref() {
        Some(ref_id) => tree.find(ref_id),
        None => Some(tree),
    };
    let Some(scope) = scope else {
        return matches!(condition.until, Some(ConditionUntil::Absent));
    };
    let present = if let Some(ref_id) = condition.r#ref.as_deref() {
        scope
            .find(ref_id)
            .is_some_and(|node| node_matches(node, condition))
    } else {
        any_node_matches(scope, condition)
    };
    match condition.until.unwrap_or(ConditionUntil::Present) {
        ConditionUntil::Present => present,
        ConditionUntil::Absent => !present,
    }
}

fn any_node_matches(node: &UiNode, condition: &UiCondition) -> bool {
    node_matches(node, condition)
        || node
            .children
            .iter()
            .any(|child| any_node_matches(child, condition))
}

fn node_matches(node: &UiNode, condition: &UiCondition) -> bool {
    let text_matches = condition.text.as_deref().is_none_or(|text| {
        contains_case_insensitive(&node.title, text)
            || contains_case_insensitive(&node.value, text)
            || contains_case_insensitive(&node.description, text)
    });
    let role_matches = condition
        .role
        .as_deref()
        .is_none_or(|role| outline::canonical_role(&node.role) == outline::canonical_role(role));
    let value_matches = condition
        .value
        .as_deref()
        .is_none_or(|value| contains_case_insensitive(&node.value, value));
    text_matches && role_matches && value_matches
}

fn contains_case_insensitive(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

fn until_name(until: Option<ConditionUntil>) -> &'static str {
    match until.unwrap_or(ConditionUntil::Present) {
        ConditionUntil::Present => "present",
        ConditionUntil::Absent => "absent",
    }
}

fn action_name(action: UiActionKind) -> &'static str {
    match action {
        UiActionKind::Press => "press",
        UiActionKind::Click => "click",
        UiActionKind::SetText => "set_text",
        UiActionKind::TypeText => "type_text",
        UiActionKind::Keypress => "keypress",
        UiActionKind::Scroll => "scroll",
        UiActionKind::Drag => "drag",
        UiActionKind::MoveMouse => "move_mouse",
    }
}

fn escaped(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace(['\n', '\r'], " ")
}

fn bounded_success(
    owner_state: Option<&str>,
    text: String,
    mut extra: Vec<ContentBlock>,
) -> CallToolResult {
    let text = crate::state::global()
        .lock()
        .unwrap()
        .bound_model_text(owner_state, text);
    let mut content = vec![ContentBlock::text(text)];
    content.append(&mut extra);
    CallToolResult::success(content)
}

fn backend_error(error: crate::backend::BackendError) -> CallToolResult {
    let text = serde_json::to_string(&error).unwrap_or_else(|_| error.to_string());
    CallToolResult::error(vec![ContentBlock::text(text)])
}

fn tool_error(message: &str) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message)])
}

#[cfg(test)]
mod scheduling_tests {
    use super::*;

    #[test]
    fn per_pid_lanes_allow_other_pids_and_serialize_the_same_pid() {
        let lanes = ObservationLanes::default();
        let first_pid = lanes.lane(1001);
        let same_pid = lanes.lane(1001);
        let other_pid = lanes.lane(2002);
        assert!(Arc::ptr_eq(&first_pid, &same_pid));
        assert!(!Arc::ptr_eq(&first_pid, &other_pid));

        let first_guard = first_pid.try_lock().unwrap();
        let other_guard = other_pid.try_lock().unwrap();
        assert!(same_pid.try_lock().is_err());

        drop(first_guard);
        let same_pid_guard = same_pid.try_lock().unwrap();
        drop((same_pid_guard, other_guard));
    }
}

#[cfg(all(test, target_os = "macos"))]
mod observation_tests {
    use super::*;
    use crate::outline::Frame;

    fn sparse_observation(screenshot: Option<Vec<u8>>) -> RootObservation {
        RootObservation {
            root: RootInfo {
                ref_id: "@r1".into(),
                app_name: "Canvas App".into(),
                title: "Canvas".into(),
                frame: Frame {
                    x: 0.0,
                    y: 0.0,
                    w: 800.0,
                    h: 600.0,
                },
                ..RootInfo::default()
            },
            tree: UiNode {
                role: "window".into(),
                title: "Canvas".into(),
                frame: Frame {
                    x: 0.0,
                    y: 0.0,
                    w: 800.0,
                    h: 600.0,
                },
                ..UiNode::default()
            },
            text_sparse: true,
            screenshot,
            screenshot_mime: "image/jpeg",
        }
    }

    #[test]
    fn observations_apply_image_policy_permissions_and_preserve_sparse_diagnostics() {
        use crate::config::ImageMode;
        for (configured, requested, granted, expected) in [
            (
                ImageMode::Auto,
                ObserveMode::Semantic,
                true,
                CapturePolicy::IfSparse,
            ),
            (
                ImageMode::Auto,
                ObserveMode::Semantic,
                false,
                CapturePolicy::Never,
            ),
            (
                ImageMode::Never,
                ObserveMode::Visual,
                true,
                CapturePolicy::Never,
            ),
            (
                ImageMode::Always,
                ObserveMode::Semantic,
                true,
                CapturePolicy::Always,
            ),
            (
                ImageMode::Auto,
                ObserveMode::Visual,
                true,
                CapturePolicy::Always,
            ),
        ] {
            let permissions = crate::permissions::PermissionStatus {
                accessibility: true,
                screen_recording: granted,
            };
            let policy = capture_policy(configured, Some(requested), &permissions);
            assert_eq!(policy, expected);
            let screenshot = policy.should_capture(true).then(|| vec![0xff, 0xd8, 0xff]);
            let warning = capture_warning(configured, &permissions);
            let result = save_observation(sparse_observation(screenshot), warning);
            assert!(
                matches!(&result.content[0], ContentBlock::Text(text) if text.text.contains("text_sparse: true"))
            );
            if policy.should_capture(true) {
                assert!(
                    matches!(result.content.as_slice(), [ContentBlock::Text(_), ContentBlock::Image(image)] if image.mime_type == "image/jpeg" && image.data == "/9j/")
                );
            } else {
                assert_eq!(result.content.len(), 1);
            }
            if !granted {
                assert!(
                    matches!(&result.content[0], ContentBlock::Text(text) if text.text.contains("Screen Recording permission is missing"))
                );
            }
        }
    }
}
